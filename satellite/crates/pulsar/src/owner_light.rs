//! Состояние голоса для подсветки, которой владеет чужой процесс.
//!
//! см. docs/station2.md, «Светодиоды и часы»

use std::path::Path;
use std::time::Duration;

use log::warn;
use pulsar_core::{UiState, actors};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{MissedTickBehavior, interval};

const DISPLAY: &str = "/data/local/opt/ap2/bin/owner-display";
const STATE: &str = "/run/ap2-voice-ui";
const STATE_TMP: &str = "/run/ap2-voice-ui.pulsar";
/// Панель считает файл старше 3 с брошенным: упавший демон не оставит анимацию навсегда.
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct OwnerLightHandle {
    states: Option<UnboundedSender<UiState>>,
}

impl OwnerLightHandle {
    pub fn set_ui_state(&self, state: UiState) {
        if let Some(states) = &self.states {
            let _ = states.send(state);
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
async fn run(mut states: UnboundedReceiver<UiState>) {
    let mut refresh = interval(REFRESH);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current = "idle";
    write_state(current);
    loop {
        tokio::select! {
            state = states.recv() => match state {
                Some(state) => {
                    let next = label(state);
                    if next != current {
                        current = next;
                        write_state(current);
                    }
                }
                None => return,
            },
            _ = refresh.tick() => {
                if current != "idle" {
                    write_state(current);
                }
            }
        }
    }
}

/// У ошибки и таймера своих анимаций в панели нет.
fn label(state: UiState) -> &'static str {
    match state {
        UiState::Listening => "listening",
        UiState::Thinking => "thinking",
        UiState::Speaking => "speaking",
        UiState::Idle | UiState::Error | UiState::TimerRing => "idle",
    }
}

/// Панель читает файл без блокировок: замена только через rename.
fn write_state(label: &str) {
    let result = std::fs::write(STATE_TMP, format!("{label}\n"))
        .and_then(|()| std::fs::rename(STATE_TMP, STATE));
    if let Err(e) = result {
        warn!("owner light: state write failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dialog_states_show_an_animation() {
        assert_eq!(label(UiState::Listening), "listening");
        assert_eq!(label(UiState::Thinking), "thinking");
        assert_eq!(label(UiState::Speaking), "speaking");
        assert_eq!(label(UiState::Idle), "idle");
        assert_eq!(label(UiState::Error), "idle");
        assert_eq!(label(UiState::TimerRing), "idle");
    }
}
