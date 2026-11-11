//! Состояние голоса для подсветки, которой владеет чужой процесс.
//!
//! см. docs/station2.md, «Светодиоды и часы»

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::warn;
use pulsar_core::{UiState, actors};

#[derive(Clone, Copy)]
enum Signal {
    State(UiState, bool),
    TimerStarted,
}
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{MissedTickBehavior, interval};

const DISPLAY: &str = "/data/local/opt/ap2/bin/owner-display";
const STATE: &str = "/run/ap2-voice-ui";
const STATE_TMP: &str = "/run/ap2-voice-ui.pulsar";
/// Панель считает файл старше 3 с брошенным: упавший демон не оставит анимацию навсегда.
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct OwnerLightHandle {
    states: Option<UnboundedSender<Signal>>,
}

impl OwnerLightHandle {
    pub fn set_ui_state(&self, state: UiState, muted: bool) {
        if let Some(states) = &self.states {
            let _ = states.send(Signal::State(state, muted));
        }
    }
    pub fn timer_started(&self) {
        if let Some(states) = &self.states {
            let _ = states.send(Signal::TimerStarted);
        }
    }
}

pub fn spawn() -> OwnerLightHandle {
    if !Path::new(DISPLAY).exists() {
        return OwnerLightHandle { states: None };
    }
    let (states, states_rx) = mpsc::unbounded_channel();
    actors::spawn_actor("owner_light", run(states_rx));
    OwnerLightHandle {
        states: Some(states),
    }
}

/// Отдельный актор: сервис `airplay` на время паузы занят ожиданием выхода, а свечение ждать не должно.
async fn run(mut states: UnboundedReceiver<Signal>) {
    let mut refresh = interval(REFRESH);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current = "idle";
    let mut muted = false;
    let mut timer = 0_u64;
    write_state(current, muted, timer);
    loop {
        tokio::select! {
            signal = states.recv() => match signal {
                Some(Signal::State(state, value)) => {
                    current = label(state);
                    muted = value;
                    write_state(current, muted, timer);
                }
                Some(Signal::TimerStarted) => {
                    timer = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                    write_state(current, muted, timer);
                }
                None => return,
            },
            _ = refresh.tick() => write_state(current, muted, timer),
        }
    }
}

/// Штатные голосовые клипы и служебные сигналы рисует единственный владелец framebuffer.
fn label(state: UiState) -> &'static str {
    match state {
        UiState::Listening => "listening",
        UiState::Thinking => "thinking",
        UiState::Speaking => "speaking",
        UiState::Idle => "idle",
        UiState::Error => "error",
        UiState::TimerRing => "timer",
    }
}

/// Панель читает файл без блокировок: замена только через rename.
fn write_state(label: &str, muted: bool, timer: u64) {
    let result = std::fs::write(STATE_TMP, format!("{label} {} {timer}\n", u8::from(muted)))
        .and_then(|()| std::fs::rename(STATE_TMP, STATE));
    if let Err(e) = result {
        warn!("owner light: state write failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_and_service_states_are_distinct() {
        assert_eq!(label(UiState::Listening), "listening");
        assert_eq!(label(UiState::Thinking), "thinking");
        assert_eq!(label(UiState::Speaking), "speaking");
        assert_eq!(label(UiState::Idle), "idle");
        assert_eq!(label(UiState::Error), "error");
        assert_eq!(label(UiState::TimerRing), "timer");
    }
}
