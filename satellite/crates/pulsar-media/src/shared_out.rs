//! Вывод звука демона через 64-битный `aplay` на общий dmix приёмника AirPlay.
//!
//! см. docs/station2-airplay.md, «Общий dmix с AirPlay»

use std::io::Write as _;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const APLAY: &str = "/data/local/opt/ap2/bin/aplay";
const ASOUND_CONF: &str = "/data/local/opt/ap2/etc/asound.conf";
const SHARED_PCM: &str = "ap2_loudness";
/// Приёмник переведён на общий dmix, только если эта цепочка есть в его конфигурации.
const SHARED_MARK: &str = "ap2_mix_dmix";
const APLAY_BUFFER_US: &str = "--buffer-time=100000";
const FRAMES_PER_MS: u128 = 48;

pub(crate) const PERIOD_FRAMES: usize = 960;
pub(crate) const BUFFER_FRAMES: usize = 7680;
/// Буфер `aplay` и буфер dmix: записанное уже посчитано сыгранным, а из динамика ещё не вышло.
const OUTPUT_LATENCY_FRAMES: u64 = 9600;
const FRAME_BYTES: usize = 4;

// Очередь в канале не должна блокировать запись; 65536 байт: размер канала в Linux.
const _: () = assert!(BUFFER_FRAMES * FRAME_BYTES < 65536);

/// Приёмник и `aplay` на этой станции делят один dmix.
pub fn available() -> bool {
    Path::new(APLAY).exists()
        && std::fs::read_to_string(ASOUND_CONF).is_ok_and(|text| text.contains(SHARED_MARK))
}

pub(crate) struct SharedOut {
    child: Child,
    stdin: ChildStdin,
    started: Option<Instant>,
    written: u64,
}

impl SharedOut {
    pub(crate) fn open() -> Result<Self, i32> {
        let mut child = Command::new(APLAY)
            .args([
                "-q", "-D", SHARED_PCM, "-f", "S16_LE", "-c", "2", "-r", "48000",
            ])
            .args(["-t", "raw", APLAY_BUFFER_US, "-"])
            .env("ALSA_CONFIG_PATH", ASOUND_CONF)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(errno)?;
        let stdin = child.stdin.take().ok_or(-libc::EPIPE)?;
        Ok(Self {
            child,
            stdin,
            started: None,
            written: 0,
        })
    }

    fn queued(&self) -> u64 {
        let elapsed = self.started.map_or(Duration::ZERO, |t| t.elapsed());
        queued_frames(self.written, elapsed)
    }

    pub(crate) fn avail_update(&mut self) -> Result<usize, i32> {
        let queued = usize::try_from(self.queued()).unwrap_or(usize::MAX);
        Ok(BUFFER_FRAMES.saturating_sub(queued))
    }

    /// Очередь не длиннее буфера, он меньше буфера канала: запись не блокируется.
    pub(crate) fn writei(&mut self, buf: &[u8], frames: usize) -> Result<usize, i32> {
        self.stdin
            .write_all(&buf[..frames * FRAME_BYTES])
            .map_err(errno)?;
        self.started.get_or_insert_with(Instant::now);
        self.written += u64::try_from(frames).unwrap_or(0);
        Ok(frames)
    }

    pub(crate) fn delay(&mut self) -> Result<i64, i32> {
        let queued = self.queued();
        let elapsed = self.started.map_or(Duration::ZERO, |t| t.elapsed());
        Ok(i64::try_from(delay_frames(self.written, queued, elapsed)).unwrap_or(i64::MAX))
    }
}

impl Drop for SharedOut {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn errno(e: std::io::Error) -> i32 {
    -e.raw_os_error().unwrap_or(libc::EIO)
}

fn played_frames(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis() * FRAMES_PER_MS).unwrap_or(u64::MAX)
}

/// Записано, но по часам ещё не сыграно.
fn queued_frames(written: u64, elapsed: Duration) -> u64 {
    written.saturating_sub(played_frames(elapsed))
}

/// Сколько ещё ждать последний кадр: очередь плюс задержка вывода, отсчитанная от конца записи.
fn delay_frames(written: u64, queued: u64, elapsed: Duration) -> u64 {
    if queued > 0 {
        return queued + OUTPUT_LATENCY_FRAMES;
    }
    (written + OUTPUT_LATENCY_FRAMES).saturating_sub(played_frames(elapsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_shrinks_with_time() {
        assert_eq!(queued_frames(4800, Duration::ZERO), 4800);
        assert_eq!(queued_frames(4800, Duration::from_millis(50)), 2400);
        assert_eq!(queued_frames(4800, Duration::from_millis(200)), 0);
    }

    #[test]
    fn delay_counts_the_output_latency_after_the_queue_runs_out() {
        let written = 4800;
        let at_start = Duration::ZERO;
        assert_eq!(
            delay_frames(written, queued_frames(written, at_start), at_start),
            4800 + OUTPUT_LATENCY_FRAMES
        );
        let empty = Duration::from_millis(100);
        assert_eq!(
            delay_frames(written, queued_frames(written, empty), empty),
            OUTPUT_LATENCY_FRAMES
        );
        let done = Duration::from_millis(100) + Duration::from_millis(200);
        assert_eq!(delay_frames(written, queued_frames(written, done), done), 0);
    }
}
