//! Volume, mute, ducking, the HA media player and the Sendspin hooks (controller.cpp "media").

use log::{debug, info};
use pulsar_core::volume;
use pulsar_core::{ClockShot, now_ms};
use pulsar_esphome::{MediaCommand, MediaCommandRequest};
use pulsar_media::{Channel, PlayState};
use pulsar_sendspin::SendspinEvent;

use super::logic::{self, AirplayState, cxx_max, cxx_min};
use super::{
    AIRPLAY_EXACT_MAX_SHIFTS, AIRPLAY_EXACT_WAIT_MS, AIRPLAY_VOLUME_MAX_WAIT_MS,
    AIRPLAY_VOLUME_WAIT_MS, AMP_SYNC_MS, AirplayExactRun, AirplayVolume, AirplayVolumeRun,
    Controller, DUCK_GAIN, Pending, SOUNDS, Tk,
};

impl Controller {
    pub(super) fn update_duck(&mut self) {
        let want = logic::want_duck(
            self.phase,
            self.announcing,
            self.timer_ringing,
            self.closing_earcon,
        );
        if want == self.ducked {
            return;
        }
        self.ducked = want;
        self.apply_gains();
    }

    /// Mute: softvol raw 0 is the bottom of its dB range (-60 dB on the Midi), still audible, so
    /// mute also zeroes the software gains of both playback channels and Sendspin. On models
    /// without a usable ALSA master (the Mini) this is also where the master volume is applied.
    pub(super) fn apply_gains(&self) {
        let master = pulsar_core::volume::master_gain(self.model, self.st.volume);
        let music = if self.st.muted {
            0.0
        } else {
            master * if self.ducked { DUCK_GAIN } else { 1.0 }
        };
        self.player.set_channel_gain(Channel::Music, music);
        self.player
            .set_channel_gain(Channel::Announce, if self.st.muted { 0.0 } else { master });
        if let Some(ss) = &self.sendspin {
            ss.set_duck_gain(music);
        }
    }

    pub(super) fn set_muted(&mut self, m: bool) {
        self.st.muted = m;
        self.mixer.set_muted(m);
        self.apply_gains();
        if let Some(ss) = &self.sendspin {
            ss.report_volume(self.st.volume, self.st.muted);
        }
        self.publish_media();
        self.persist();
    }

    /// `feedback`: a local change (buttons, voice), shown on the ring and the clock; it also
    /// unmutes.
    pub(super) fn set_volume(&mut self, v: f32, feedback: bool) {
        let before = (self.st.volume, self.st.muted);
        self.st.volume = cxx_min(1.0, cxx_max(0.0, v));
        self.mixer.set_volume(self.st.volume);
        if self.st.muted && feedback {
            self.st.muted = false;
            self.mixer.set_muted(false);
        }
        self.apply_gains();
        if feedback {
            self.leds.show_volume(self.st.volume);
            // mul_add: the station C++ fuses `volume * 100 + 0.5` (fmadd on aarch64, vfma.f32 on
            // armhf, checked in the -O2 asm of Controller::set_volume); the rounding follows it.
            self.clock
                .flash_number(self.st.volume.mul_add(100.0, 0.5) as i32);
            self.volume_click(before);
        }
        if let Some(ss) = &self.sendspin {
            ss.report_volume(self.st.volume, self.st.muted);
        }
        self.publish_media();
        self.persist();
    }

    /// Громкость изменили мимо демона (ползунок AirPlay): показать её в HA и сохранить.
    pub(super) fn sync_amp(&mut self) {
        self.timers.set(Tk::AmpSync, AMP_SYNC_MS);
        self.sync_airplay_state();
        let Some(v) = self.mixer.take_external() else {
            return;
        };
        info!(
            "amplifier changed outside the daemon: raw {}",
            self.mixer.raw()
        );
        self.st.volume = v;
        self.st.muted = false;
        if let Some(ss) = &self.sendspin {
            ss.report_volume(self.st.volume, self.st.muted);
        }
        self.publish_media();
        self.persist();
    }

    /// Источник остановился сам (телефон) или отпустил паузу: показать это в HA.
    fn sync_airplay_state(&mut self) {
        let playing = self.airplay.playing();
        if playing {
            self.airplay_stopped_ms = None;
        } else if self.airplay_was_playing && !self.airplay_paused {
            self.airplay_stopped_ms = Some(now_ms());
        }
        self.airplay_was_playing = playing;
        let state = self.airplay_state();
        if state != self.airplay_published {
            self.publish_media();
        }
    }

    pub(super) fn airplay_state(&self) -> AirplayState {
        if self.airplay_paused || self.airplay.playing() {
            AirplayState::Playing
        } else if self
            .airplay_stopped_ms
            .is_some_and(|t| now_ms() - t < logic::PAUSED_MUSIC_MS)
        {
            AirplayState::Paused
        } else {
            AirplayState::None
        }
    }

    /// Музыкой сейчас распоряжается телефон: и громкость, и транспорт идут через него. Своя музыка
    /// HA и Sendspin важнее.
    pub(super) fn airplay_source(&self) -> bool {
        self.airplay_state() != AirplayState::None
            && !self.sendspin_streaming()
            && !matches!(
                self.player.state(Channel::Music),
                PlayState::Playing | PlayState::Loading | PlayState::Paused
            )
    }

    /// True, если музыка играла. Пауза диалога просто остаётся, снимать её будет некому.
    pub(super) fn airplay_pause(&mut self) -> bool {
        if std::mem::take(&mut self.airplay_paused) {
            self.airplay.keep_paused();
        } else if self.airplay.playing() {
            self.airplay.pause();
        } else {
            return false;
        }
        self.airplay_stopped_ms = Some(now_ms());
        true
    }

    /// Команды HA для музыки телефона. True: команда обработана.
    fn airplay_command(&mut self, c: MediaCommand) -> bool {
        if !self.airplay_source() {
            return false;
        }
        let paused = self.airplay_state() == AirplayState::Paused;
        match c {
            MediaCommand::Pause | MediaCommand::Stop => {
                if self.airplay_pause() {
                    self.clock.play(ClockShot::Pause);
                }
            }
            MediaCommand::Play | MediaCommand::Toggle if paused => {
                self.airplay_stopped_ms = None;
                self.airplay.play();
            }
            // Паузу диалога снимет конец диалога.
            MediaCommand::Play => {}
            MediaCommand::Toggle => {
                if self.airplay_pause() {
                    self.clock.play(ClockShot::Pause);
                }
            }
            MediaCommand::VolumeUp | MediaCommand::VolumeDown => {
                self.airplay_volume(AirplayVolume::Step(c == MediaCommand::VolumeUp));
            }
            _ => return false,
        }
        true
    }

    /// На время диалога источник на паузе, а его снятие вернёт прежний уровень: команда ждёт конца
    /// диалога и идёт в очереди после снятия.
    pub(super) fn airplay_volume(&mut self, volume: AirplayVolume) {
        if self.airplay_paused {
            self.airplay_volume_after_dialog = Some(volume);
            return;
        }
        self.sync_amp();
        let raw = self.mixer.raw();
        let target = match volume {
            AirplayVolume::Step(true) => raw + logic::AIRPLAY_WORD_RAW,
            AirplayVolume::Step(false) => raw - logic::AIRPLAY_WORD_RAW,
            AirplayVolume::Level(v) => volume::to_raw(self.model, v),
        };
        let target = logic::airplay_exact_target(raw, target);
        if (target - raw).abs() <= logic::AIRPLAY_EXACT_TOLERANCE_RAW {
            info!("airplay volume: stays at {raw}, target {target}");
            return;
        }
        info!("airplay volume: from {raw} to {target}");
        self.airplay_run = None;
        let delta_db = (target - raw) as f64 / logic::airplay_raw_per_db_estimate(raw);
        self.airplay_exact = Some(AirplayExactRun {
            request: volume,
            target,
            raw,
            delta_db,
            shifts: 1,
            waited_ms: 0,
        });
        self.airplay.shift_volume(delta_db);
    }

    /// Запасной путь для источников без точного уровня (Spotify): кнопки громкости.
    fn airplay_volume_steps(&mut self, volume: AirplayVolume, raw: i64, target: i64) {
        let Some(up) = logic::airplay_next_step(raw, target, logic::airplay_step_estimate(raw))
        else {
            info!("airplay volume: stays at {raw}, target {target}");
            return;
        };
        // Уровень запоминается до запроса: источник может ответить раньше, чем запрос завершится.
        self.airplay_run = Some(AirplayVolumeRun {
            request: volume,
            target,
            up,
            raw,
            steps: 0,
            waited_ms: 0,
        });
        self.airplay.step_volume(up);
    }

    /// Запасной путь, если источник не выполнил команду: прямо на усилителе.
    fn airplay_volume_direct(&mut self, request: AirplayVolume) {
        match request {
            AirplayVolume::Level(v) => self.set_volume(v, false),
            AirplayVolume::Step(up) => {
                let v = volume::step_alice(self.st.volume, up);
                self.set_volume(v, true);
            }
        }
    }

    pub(super) fn airplay_volume_sent(&mut self, sent: bool) {
        if let Some(run) = self.airplay_exact.take() {
            if sent {
                self.airplay_exact = Some(run);
                self.timers
                    .set(Tk::AirplayVolumeCheck, AIRPLAY_EXACT_WAIT_MS);
            } else {
                self.sync_amp();
                self.airplay_volume_steps(run.request, self.mixer.raw(), run.target);
            }
            return;
        }
        if sent {
            self.timers
                .set(Tk::AirplayVolumeCheck, AIRPLAY_VOLUME_WAIT_MS);
        } else if let Some(run) = self.airplay_run.take() {
            self.sync_amp();
            // Часть шагов уже прошла: усилитель не трогаем, источник вернул бы свой уровень.
            if run.steps == 0 {
                self.airplay_volume_direct(run.request);
            }
        }
    }

    /// Шаг телефона неравномерен, поэтому идём по одному и смотрим на усилитель. Не каждый источник
    /// выполняет команду: без отклика на первый шаг он делается прямо на усилителе.
    pub(super) fn airplay_volume_check(&mut self) {
        if self.airplay_exact.is_some() {
            return self.airplay_exact_check();
        }
        let Some(mut run) = self.airplay_run.take() else {
            return;
        };
        self.sync_amp();
        let raw = self.mixer.raw();
        if raw == run.raw {
            run.waited_ms += AIRPLAY_VOLUME_WAIT_MS;
            if run.waited_ms < AIRPLAY_VOLUME_MAX_WAIT_MS {
                self.airplay_run = Some(run);
                self.timers
                    .set(Tk::AirplayVolumeCheck, AIRPLAY_VOLUME_WAIT_MS);
                return;
            }
            info!("airplay: source ignored the volume step {}", run.steps);
            if run.steps == 0 {
                self.airplay_volume_direct(run.request);
            }
            return;
        }
        run.steps += 1;
        let step_raw = (raw - run.raw).abs();
        let next = logic::airplay_next_step(raw, run.target, step_raw);
        // Смена направления значила бы проскочить цель и вернуться на тот же уровень.
        if run.steps < logic::AIRPLAY_MAX_STEPS && next == Some(run.up) {
            info!("airplay volume: {raw} after step {}", run.steps);
            run.raw = raw;
            run.waited_ms = 0;
            let up = run.up;
            self.airplay_run = Some(run);
            self.airplay.step_volume(up);
        } else {
            info!(
                "airplay volume: done at {raw}, target {}, {} step(s)",
                run.target, run.steps
            );
        }
    }

    /// Кривая источника неровная и у каждой колонки своя: сдвигаемся по замеру усилителя.
    fn airplay_exact_check(&mut self) {
        let Some(mut run) = self.airplay_exact.take() else {
            return;
        };
        self.sync_amp();
        let raw = self.mixer.raw();
        if (run.target - raw).abs() <= logic::AIRPLAY_EXACT_TOLERANCE_RAW {
            info!(
                "airplay volume: done at {raw}, target {}, {} shift(s)",
                run.target, run.shifts
            );
            return;
        }
        if raw == run.raw {
            run.waited_ms += AIRPLAY_EXACT_WAIT_MS;
            if run.waited_ms < AIRPLAY_VOLUME_MAX_WAIT_MS {
                self.airplay_exact = Some(run);
                self.timers
                    .set(Tk::AirplayVolumeCheck, AIRPLAY_EXACT_WAIT_MS);
                return;
            }
            info!("airplay: source ignored the exact volume");
            return self.airplay_volume_steps(run.request, raw, run.target);
        }
        let slope = logic::airplay_raw_per_db(run.raw, raw, run.delta_db);
        let (Some(slope), true) = (slope, run.shifts < AIRPLAY_EXACT_MAX_SHIFTS) else {
            info!(
                "airplay volume: stopped at {raw}, target {}, {} shift(s)",
                run.target, run.shifts
            );
            return;
        };
        run.delta_db = (run.target - raw) as f64 / slope;
        run.raw = raw;
        run.shifts += 1;
        run.waited_ms = 0;
        let delta_db = run.delta_db;
        self.airplay_exact = Some(run);
        self.airplay.shift_volume(delta_db);
    }

    /// Stock click after a local change (keys, voice words); `before` = (volume, muted) before it.
    fn volume_click(&self, before: (f32, bool)) {
        if !logic::want_volume_click(
            before,
            (self.st.volume, self.st.muted),
            self.phase,
            self.arbitrating,
            self.announcing,
            self.timer_ringing,
            self.closing_earcon,
        ) {
            return;
        }
        // Music is not ducked under it; its Done has no Pending entry and is ignored.
        let sound = format!("{SOUNDS}{}", self.model.volume_sound);
        self.player.play(Channel::Announce, vec![sound]);
    }

    /// Reads the player state as it is now: right after a command to the player it is still the
    /// previous one (as in C++); the change comes as `PlayerEvent::State`, which publishes again.
    pub(super) fn publish_media(&mut self) {
        self.airplay_published = self.airplay_state();
        let s = logic::media_state(
            self.announcing,
            self.player.state(Channel::Music),
            self.sendspin_streaming(),
            self.airplay_published,
        );
        self.api.publish_media(s, self.st.volume, self.st.muted);
        self.update_music();
    }

    pub(super) fn update_music(&self) {
        let ps = self.player.state(Channel::Music);
        let media =
            matches!(ps, PlayState::Playing | PlayState::Loading) || self.sendspin_streaming();
        self.leds.set_media_playing(media);
        self.clock.set_media_playing(media);
        // The speaker-reference analysis only runs while its result can be shown.
        self.mic.set_music_analysis(
            media
                && ((self.st.music_viz != "Off" && !self.st.light.on)
                    || (self.st.clock_music != "Off" && self.st.clock_on)),
        );
    }

    pub(super) fn on_media_command(&mut self, cmd: MediaCommandRequest) {
        if let Some(url) = cmd.media_url {
            if cmd.announcement == Some(true) {
                // Media-player announcements report completion through state only.
                self.announcing = true;
                self.announce_seq += 1;
                let seq = self.announce_seq;
                self.update_duck();
                self.update_ui();
                let id = self.player.play(Channel::Announce, vec![url]);
                self.pending.insert(id, Pending::MediaAnnounce { seq });
            } else {
                if let Some(ss) = &self.sendspin {
                    ss.yield_to_local(); // HA media takes the speaker from Sendspin
                }
                self.player.play(Channel::Music, vec![url]);
            }
        }
        if let Some(v) = cmd.volume {
            if self.airplay_source() {
                self.airplay_volume(AirplayVolume::Level(v));
            } else {
                self.set_volume(v, false);
            }
        }
        if let Some(c) = cmd.command
            && !self.airplay_command(c)
        {
            let ss_on = self.sendspin_streaming();
            let music = || self.player.state(Channel::Music);
            match c {
                MediaCommand::Play => {
                    if ss_on
                        && music() == PlayState::Paused
                        && let Some(ss) = &self.sendspin
                    {
                        ss.yield_to_local();
                    }
                    self.player.resume(Channel::Music);
                }
                MediaCommand::Pause => {
                    if ss_on || matches!(music(), PlayState::Playing | PlayState::Loading) {
                        self.clock.play(ClockShot::Pause);
                    }
                    // While Sendspin plays, HA's pause/stop leave the Sendspin group (the
                    // speaker stops).
                    if ss_on
                        && music() != PlayState::Playing
                        && let Some(ss) = &self.sendspin
                    {
                        ss.yield_to_local();
                    }
                    self.player.pause(Channel::Music);
                }
                MediaCommand::Toggle => {
                    if music() == PlayState::Playing {
                        self.clock.play(ClockShot::Pause);
                        self.player.pause(Channel::Music);
                    } else if ss_on {
                        self.clock.play(ClockShot::Pause);
                        if let Some(ss) = &self.sendspin {
                            ss.yield_to_local();
                        }
                    } else {
                        self.player.resume(Channel::Music);
                    }
                }
                MediaCommand::Stop => {
                    self.player.stop(Channel::Music);
                    if ss_on && let Some(ss) = &self.sendspin {
                        ss.yield_to_local();
                    }
                }
                MediaCommand::Mute | MediaCommand::Unmute => {
                    self.set_muted(c == MediaCommand::Mute);
                }
                MediaCommand::VolumeUp | MediaCommand::VolumeDown => {
                    let up = c == MediaCommand::VolumeUp;
                    self.set_volume(
                        pulsar_core::volume::step_level(self.model, self.st.volume, up),
                        false,
                    );
                }
                _ => debug!("media command {} ignored", c as u32),
            }
        }
        self.publish_media();
    }

    pub(super) fn on_sendspin(&mut self, ev: SendspinEvent) {
        match ev {
            SendspinEvent::Stream(active) => {
                // Music Assistant took over the speaker: the HA media channel yields to it.
                if active && self.player.state(Channel::Music) != PlayState::Idle {
                    self.player.stop(Channel::Music);
                }
                // The HA media channel playing here took the speaker over (yield_to_local): not a
                // pause in Music Assistant. on_player clears the stamp if its state comes later.
                let taken_over = matches!(
                    self.player.state(Channel::Music),
                    PlayState::Playing | PlayState::Loading
                );
                self.sendspin_ended_ms = (!active && !taken_over).then(now_ms);
                self.publish_media();
            }
            SendspinEvent::Volume(v) => self.set_volume(v, false),
            SendspinEvent::Mute(m) => self.set_muted(m),
            SendspinEvent::PairingCode(code) => {
                if code.is_empty() {
                    self.clock.clear_code();
                } else {
                    self.clock.show_code(code);
                }
            }
            SendspinEvent::PairingWindow(open) => {
                self.sendspin_pairing_prompt = open;
                if open {
                    self.clock.show_code("----".into());
                } else {
                    self.clock.clear_code();
                }
            }
            SendspinEvent::Music(f) => {
                self.leds.music_frame(f, true);
                self.clock.music_frame(f, true);
            }
        }
    }
}
