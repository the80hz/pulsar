//! Пауза AirPlay на время голосового диалога.
//!
//! см. docs/station2-airplay.md, «AirPlay и голос»

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use pulsar_alsa::{Format, HwRequest};
use pulsar_core::actors;
use pulsar_core::model::Model;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{MissedTickBehavior, interval, sleep};

const ACTION_SCRIPT: &str = "/data/local/opt/ap2/touch-action.sh";
const OUTPUT_STATUS: &str = "/proc/asound/card0/pcm0p/sub0/status";
const OUTPUT_HW_PARAMS: &str = "/proc/asound/card0/pcm0p/sub0/hw_params";
/// С `active_state_timeout = 0.5` выход освобождается примерно за 3 с.
const RELEASE_TIMEOUT: Duration = Duration::from_millis(3500);
const RELEASE_POLL: Duration = Duration::from_millis(50);
const WATCH_PERIOD: Duration = Duration::from_secs(1);
const RESET_RATE: u32 = 48_000;
const RESET_CHANNELS: u32 = 2;
const RESET_FRAME_BYTES: usize = 4;
const RESET_TIME: Duration = Duration::from_millis(200);
/// Приёмник раз в 100 мс смотрит сначала этот файл, потом сохранённый: см. docs/station2-equalizer.md.
const DSP_RUN: &str = "/run/ap2-dsp";
const DSP_RUN_TMP: &str = "/run/ap2-dsp.pulsar";
const DSP_SAVED: &str = "/data/local/opt/ap2/etc/dsp.conf";
const MUTE_LINES: &str = "#pulsar-mute\nmute=1\n";
/// Шкала источника AirPlay. Ниже неё только -144, выключенный звук.
const SOURCE_MIN_DB: f64 = -30.0;
const SOURCE_MAX_DB: f64 = 0.0;

pub enum AirplayEvent {
    /// Запрос громкости ушёл источнику. `sent`: приёмник его принял.
    VolumeSent { sent: bool },
    /// Пауза отработала. `closed`: выход свободен и такт захвата снова 48 кГц.
    Released { closed: bool },
}

enum Cmd {
    Pause,
    Resume,
    /// Диалог кончился, а музыка остаётся на паузе: вернуть только заглушение.
    KeepPaused,
    /// Диалог ведёт другая станция: заглушить музыку, пока её пауза не дойдёт сюда.
    MuteUntilClosed,
    Action(&'static str),
    Volume {
        up: bool,
    },
    ShiftVolume {
        delta_db: f64,
    },
}

#[derive(Clone)]
pub struct AirplayHandle {
    cmds: UnboundedSender<Cmd>,
    available: bool,
}

impl AirplayHandle {
    /// True, если музыка играла и пауза запрошена. Читает procfs: без ожидания устройства.
    pub fn pause_if_playing(&self) -> bool {
        if !self.available || !is_running(&read_status()) {
            return false;
        }
        let _ = self.cmds.send(Cmd::Pause);
        true
    }

    /// Играет ли музыка сейчас. Читает procfs, как `pause_if_playing`.
    pub fn playing(&self) -> bool {
        self.available && is_running(&read_status())
    }

    pub fn resume(&self) {
        let _ = self.cmds.send(Cmd::Resume);
    }

    pub fn keep_paused(&self) {
        let _ = self.cmds.send(Cmd::KeepPaused);
    }

    /// Пауза победившей станции доходит до группы только через 3 с: всё это время наша музыка
    /// попадала бы в её микрофон и VAD считал бы её речью. Не та группа: звук вернётся по таймауту.
    pub fn mute_until_closed(&self) {
        if self.playing() {
            let _ = self.cmds.send(Cmd::MuteUntilClosed);
        }
    }

    pub fn pause(&self) {
        let _ = self.cmds.send(Cmd::Action("pause"));
    }

    pub fn play(&self) {
        let _ = self.cmds.send(Cmd::Action("play"));
    }

    /// Трек меняет источник, на всю группу AirPlay. Нет приёмника: false.
    pub fn skip(&self, next: bool) -> bool {
        if !self.available {
            return false;
        }
        let action = if next { "next" } else { "previous" };
        let _ = self.cmds.send(Cmd::Action(action));
        true
    }

    /// Шаг громкости через источник: ползунок на телефоне двигается, а его уровень не отменит
    /// команду при следующем запуске потока. Идёт в очереди после паузы и её снятия.
    pub fn step_volume(&self, up: bool) {
        let _ = self.cmds.send(Cmd::Volume { up });
    }

    /// Сдвиг уровня источника на точную величину. Телефон двигает ползунок, и уровень держится после
    /// снятия паузы; см. docs/station2-airplay.md, «Команды и громкость под AirPlay».
    pub fn shift_volume(&self, delta_db: f64) {
        let _ = self.cmds.send(Cmd::ShiftVolume { delta_db });
    }
}

/// Есть ли на станции приёмник AirPlay.
pub fn receiver_installed() -> bool {
    Path::new(ACTION_SCRIPT).exists()
}

pub fn spawn(model: &'static Model) -> (AirplayHandle, UnboundedReceiver<AirplayEvent>) {
    let (cmds, cmds_rx) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    let available = receiver_installed();
    if available {
        info!("airplay: receiver found, pausing it for voice dialogs");
        actors::spawn_actor("airplay", run(model, cmds_rx, events));
    }
    (AirplayHandle { cmds, available }, events_rx)
}

async fn run(
    model: &'static Model,
    mut cmds: UnboundedReceiver<Cmd>,
    events: UnboundedSender<AirplayEvent>,
) {
    let mut watch = interval(WATCH_PERIOD);
    watch.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut was_running = is_running(&read_status());
    // На общем dmix выход и так на 48 кГц, а лишнее открытие из демона заняло бы его у приёмника.
    let mut reset_due = clock_differs(&read_hw_params());
    let mut muted = false;
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Pause) => {
                    muted = muted || mute_dsp();
                    let reset_due = clock_differs(&read_hw_params());
                    toggle().await;
                    let closed = wait_closed().await;
                    if !closed {
                        warn!("airplay: output still busy after {RELEASE_TIMEOUT:?}");
                    } else if reset_due {
                        reset_clock(model).await;
                    }
                    was_running = false;
                    let _ = events.send(AirplayEvent::Released { closed });
                }
                Some(Cmd::Resume) => {
                    toggle().await;
                    if std::mem::take(&mut muted) {
                        restore_dsp();
                    }
                }
                Some(Cmd::KeepPaused) => {
                    if std::mem::take(&mut muted) {
                        restore_dsp();
                    }
                }
                Some(Cmd::MuteUntilClosed) => {
                    if !muted && mute_dsp() {
                        wait_closed().await;
                        restore_dsp();
                    }
                }
                Some(Cmd::Action(action)) => {
                    run_action(action).await;
                }
                Some(Cmd::Volume { up }) => {
                    let sent = run_action(if up { "volume-up" } else { "volume-down" }).await;
                    let _ = events.send(AirplayEvent::VolumeSent { sent });
                }
                Some(Cmd::ShiftVolume { delta_db }) => {
                    let sent = shift_source_volume(delta_db).await;
                    let _ = events.send(AirplayEvent::VolumeSent { sent });
                }
                None => return,
            },
            _ = watch.tick() => {
                // Демон мог упасть посреди диалога, а панель могла записать файл в момент его конца.
                if !muted && std::fs::read_to_string(DSP_RUN).is_ok_and(|text| is_muted(&text)) {
                    restore_dsp();
                }
                let status = read_status();
                // Источник закончил сам: такт захвата остался бы от AirPlay.
                if was_running && reset_due && is_closed(&status) {
                    reset_clock(model).await;
                }
                was_running = is_running(&status);
                if was_running {
                    reset_due = clock_differs(&read_hw_params());
                }
            }
        }
    }
}

/// Глушит музыку сразу, пока буфер AirPlay ещё доигрывает. Панель может держать в `DSP_RUN`
/// несохранённые настройки: они основа.
fn mute_dsp() -> bool {
    let base =
        match std::fs::read_to_string(DSP_RUN).or_else(|_| std::fs::read_to_string(DSP_SAVED)) {
            Ok(text) => text,
            Err(e) => {
                warn!("airplay: no DSP settings to mute from: {e}");
                return false;
            }
        };
    if is_muted(&base) {
        return true;
    }
    match write_dsp(&muted_config(&base)) {
        Ok(()) => {
            info!("airplay: music muted");
            true
        }
        Err(e) => {
            warn!("airplay: mute failed: {e}");
            false
        }
    }
}

/// Панель могла поменять или сохранить настройки за время диалога: возвращается то, что в файле
/// сейчас, а не то, что было до диалога.
fn restore_dsp() {
    let Ok(current) = std::fs::read_to_string(DSP_RUN) else {
        return;
    };
    let saved = std::fs::read_to_string(DSP_SAVED).ok();
    let result = match restored_config(&current, saved.as_deref()) {
        Some(text) => write_dsp(&text),
        None => std::fs::remove_file(DSP_RUN),
    };
    if let Err(e) = result {
        warn!("airplay: DSP restore failed: {e}");
    }
}

/// `None`: настройки совпали с сохранёнными и отдельный файл не нужен.
fn restored_config(current: &str, saved: Option<&str>) -> Option<String> {
    let plain = current.replace(MUTE_LINES, "");
    if saved.is_some_and(|text| text.trim_end() == plain.trim_end()) {
        return None;
    }
    Some(plain)
}

/// Приёмник читает файл без блокировок: замена только через rename.
fn write_dsp(text: &str) -> std::io::Result<()> {
    std::fs::write(DSP_RUN_TMP, text)?;
    std::fs::rename(DSP_RUN_TMP, DSP_RUN)
}

/// Приёмник применяет последнее значение параметра.
fn muted_config(base: &str) -> String {
    let separator = if base.ends_with('\n') { "" } else { "\n" };
    format!("{base}{separator}{MUTE_LINES}")
}

fn is_muted(text: &str) -> bool {
    text.contains(MUTE_LINES)
}

async fn wait_closed() -> bool {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        if is_closed(&read_status()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(RELEASE_POLL).await;
    }
}

async fn toggle() {
    run_action("toggle").await;
}

async fn run_action(action: &'static str) -> bool {
    run_script(vec![action.to_owned()]).await.is_some()
}

async fn shift_source_volume(delta_db: f64) -> bool {
    let Some(reply) = run_script(vec!["volume-get".to_owned()]).await else {
        return false;
    };
    let Some(current) = parse_source_volume(&reply) else {
        warn!("airplay: no source volume in the reply");
        return false;
    };
    let level = shifted_source_volume(current, delta_db);
    run_script(vec!["volume-set".to_owned(), format!("{level:.2}")])
        .await
        .is_some()
}

/// Ответ `dbus-send`: `variant       double -26.55`.
fn parse_source_volume(reply: &str) -> Option<f64> {
    reply.split_whitespace().last()?.parse().ok()
}

fn shifted_source_volume(current: f64, delta_db: f64) -> f64 {
    (current.max(SOURCE_MIN_DB) + delta_db).clamp(SOURCE_MIN_DB, SOURCE_MAX_DB)
}

/// Stdout скрипта, если он завершился успешно.
async fn run_script(args: Vec<String>) -> Option<String> {
    let action = args.join(" ");
    let result = tokio::task::spawn_blocking(move || {
        Command::new("sh")
            .arg(ACTION_SCRIPT)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    })
    .await;
    match result {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(Ok(output)) => {
            warn!("airplay: {action} failed: {}", output.status);
            None
        }
        Ok(Err(e)) => {
            warn!("airplay: {action} failed: {e}");
            None
        }
        Err(e) => {
            warn!("airplay: {action} task failed: {e}");
            None
        }
    }
}

/// Реальный такт захвата равен частоте последнего открытия выхода: см. docs/station2.md.
async fn reset_clock(model: &'static Model) {
    let result = tokio::task::spawn_blocking(move || write_silence(model)).await;
    match result {
        Ok(Ok(())) => info!("airplay: capture clock back to {RESET_RATE} Hz"),
        Ok(Err((step, e))) => {
            error!(
                "airplay: clock reset {step} failed: {}",
                pulsar_alsa::strerror(e)
            );
        }
        Err(e) => warn!("airplay: clock reset task failed: {e}"),
    }
}

fn write_silence(model: &Model) -> Result<(), (&'static str, i32)> {
    let mut pcm = pulsar_alsa::open_playback(model).map_err(|e| ("open", e))?;
    let hw = pcm
        .set_hw_params(&HwRequest {
            format: Format::S16Le,
            channels: RESET_CHANNELS,
            rate: RESET_RATE,
            rate_exact: true,
            period_frames: None,
            buffer_frames: None,
        })
        .map_err(|e| ("hw_params", e))?;
    let period = hw.period_frames.max(1);
    pcm.set_sw_params(period, period)
        .map_err(|e| ("sw_params", e))?;
    pcm.prepare().map_err(|e| ("prepare", e))?;
    let frames = usize::try_from(period).unwrap_or(usize::MAX);
    let silence = vec![0_u8; frames * RESET_FRAME_BYTES];
    let started = Instant::now();
    while started.elapsed() < RESET_TIME {
        pcm.writei(&silence, frames).map_err(|e| ("write", e))?;
    }
    // Записанное лежит в буфере: выход должен успеть поработать на этой частоте.
    std::thread::sleep(RESET_TIME);
    Ok(())
}

fn read_status() -> String {
    std::fs::read_to_string(OUTPUT_STATUS).unwrap_or_default()
}

fn read_hw_params() -> String {
    std::fs::read_to_string(OUTPUT_HW_PARAMS).unwrap_or_default()
}

/// Открытый выход на другой частоте. Закрытый выход о такте не говорит.
fn clock_differs(hw_params: &str) -> bool {
    hw_params
        .lines()
        .find_map(|line| line.strip_prefix("rate: "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|rate| rate.parse::<u32>().ok())
        .is_some_and(|rate| rate != RESET_RATE)
}

fn first_line(status: &str) -> &str {
    status.lines().next().unwrap_or("").trim()
}

fn is_closed(status: &str) -> bool {
    first_line(status) == "closed"
}

fn is_running(status: &str) -> bool {
    first_line(status) == "state: RUNNING"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_volume_comes_from_the_last_word_of_the_reply() {
        let reply = "method return time=1.2 sender=:1.7 -> destination=:1.9 serial=4 reply_serial=2\n   variant       double -26.55\n";
        assert_eq!(parse_source_volume(reply), Some(-26.55));
        assert_eq!(parse_source_volume(""), None);
        assert_eq!(parse_source_volume("variant string \"x\""), None);
    }

    #[test]
    fn source_volume_stays_on_its_scale() {
        assert_eq!(shifted_source_volume(-26.0, -1.5), -27.5);
        assert_eq!(shifted_source_volume(-29.5, -2.0), -30.0);
        assert_eq!(shifted_source_volume(-1.0, 3.0), 0.0);
        // -144: звук выключен, отсчёт от низа шкалы.
        assert_eq!(shifted_source_volume(-144.0, 2.0), -28.0);
    }

    #[test]
    fn mute_goes_after_the_settings() {
        let text = muted_config("version=1\nvolume_limit=0\n");
        assert_eq!(text, "version=1\nvolume_limit=0\n#pulsar-mute\nmute=1\n");
        assert!(is_muted(&text));
    }

    #[test]
    fn mute_keeps_lines_apart_without_a_final_newline() {
        assert_eq!(
            muted_config("version=1"),
            "version=1\n#pulsar-mute\nmute=1\n"
        );
    }

    #[test]
    fn plain_settings_are_not_muted() {
        assert!(!is_muted("version=1\nvolume_limit=-36\n"));
    }

    #[test]
    fn untouched_settings_need_no_file_after_a_dialog() {
        let saved = "version=1\neq=1\n";
        assert_eq!(restored_config(&muted_config(saved), Some(saved)), None);
    }

    #[test]
    fn panel_preview_survives_a_dialog() {
        let saved = "version=1\neq=1\n";
        let preview = "version=1\neq=0\n";
        assert_eq!(
            restored_config(&muted_config(preview), Some(saved)).as_deref(),
            Some(preview)
        );
    }

    #[test]
    fn profile_saved_during_a_dialog_needs_no_file() {
        let saved = "version=1\neq=0\n";
        assert_eq!(restored_config(&muted_config(saved), Some(saved)), None);
    }

    #[test]
    fn settings_without_a_saved_file_are_kept() {
        let preview = "version=1\n";
        assert_eq!(
            restored_config(&muted_config(preview), None).as_deref(),
            Some(preview)
        );
    }

    #[test]
    fn only_another_open_rate_needs_a_reset() {
        assert!(clock_differs("format: S32_LE\nrate: 44100 (44100/1)\n"));
        assert!(!clock_differs("format: S32_LE\nrate: 48000 (48000/1)\n"));
        assert!(!clock_differs("closed\n"));
        assert!(!clock_differs(""));
    }

    #[test]
    fn status_of_a_playing_output() {
        let status = "state: RUNNING\nowner_pid   : 5784\n";
        assert!(is_running(status));
        assert!(!is_closed(status));
    }

    #[test]
    fn status_of_a_free_output() {
        assert!(is_closed("closed\n"));
        assert!(!is_running("closed\n"));
    }

    #[test]
    fn unreadable_status_is_neither() {
        assert!(!is_closed(""));
        assert!(!is_running(""));
    }

    #[test]
    fn prepared_output_is_not_running() {
        assert!(!is_running("state: PREPARED\n"));
        assert!(!is_closed("state: PREPARED\n"));
    }
}
