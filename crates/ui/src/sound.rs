//! Session notification sounds — the herdr approach (state-transition chimes
//! played through the platform's own audio CLI, zero Rust audio deps):
//!
//! - two short chimes embedded in the binary (`assets/sounds/*.wav`, synthesized
//!   in-repo — no external assets): **done** (run finished) and **request**
//!   (agent is asking a question);
//! - playback = write to a temp file, hand it to the system player on a
//!   background thread: `afplay` (macOS), PowerShell `Media.SoundPlayer`
//!   (Windows), first of `paplay`/`pw-play`/`aplay`/`ffplay`/`mpv` (Linux —
//!   WAV, so even bare ALSA `aplay` decodes it);
//! - `COMET_DISABLE_SOUND` env kill-switch + the `soundEnabled` ui-setting;
//! - failures are logged and swallowed — a missing player must never bother
//!   the session flow.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DISABLE_ENV: &str = "COMET_DISABLE_SOUND";
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

static SOUND_DONE: &[u8] = include_bytes!("../assets/sounds/done.wav");
static SOUND_REQUEST: &[u8] = include_bytes!("../assets/sounds/request.wav");

/// Which notification chime to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// A run finished (Working → Idle).
    Done,
    /// The agent is waiting on a question (→ AwaitingInput).
    Request,
}

/// Play a chime on a background thread. Silently a no-op when disabled or no
/// player is available.
pub fn play(sound: Sound) {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    std::thread::spawn(move || {
        let data = match sound {
            Sound::Done => SOUND_DONE,
            Sound::Request => SOUND_REQUEST,
        };
        if let Err(err) = play_bytes(data) {
            tracing::debug!(?sound, error = %err, "notification sound playback failed");
        }
    });
}

fn play_bytes(data: &[u8]) -> Result<(), String> {
    // The system players want a file path; write the embedded bytes out.
    let tmp = temp_path();
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let result = run_player(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn temp_path() -> PathBuf {
    let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("comet-sound-{}-{id}.wav", std::process::id()))
}

#[cfg(target_os = "macos")]
fn run_player(path: &Path) -> Result<(), String> {
    run_checked("afplay", &[], path)
}

#[cfg(windows)]
fn run_player(path: &Path) -> Result<(), String> {
    // SoundPlayer handles WAV natively; PlaySync keeps the process alive for
    // the chime's duration.
    let script = format!(
        "(New-Object Media.SoundPlayer '{}').PlaySync()",
        path.display()
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| format!("powershell failed: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("powershell exited with {}", output.status))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_player(path: &Path) -> Result<(), String> {
    // WAV everywhere, so even bare ALSA aplay decodes it (herdr must exclude
    // aplay because it ships mp3s).
    let players: &[(&str, &[&str])] = &[
        ("paplay", &[]),
        ("pw-play", &[]),
        ("aplay", &["-q"]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("mpv", &["--no-video", "--really-quiet"]),
    ];
    let mut errors = Vec::new();
    for (program, args) in players {
        match run_checked(program, args, path) {
            Ok(()) => return Ok(()),
            Err(err) => errors.push(err),
        }
    }
    Err(format!("no audio player available: {}", errors.join("; ")))
}

fn run_checked(program: &str, args: &[&str], path: &Path) -> Result<(), String> {
    // Bounded wait: a wedged audio daemon must not accumulate zombie threads.
    let mut child = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{program} exited with {status}")),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} timed out"));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: {err}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transition mapping (pure — herdr's notification_sound_for_state_change)
// ---------------------------------------------------------------------------

use comet_proto::SessionStatus;

/// Which chime (if any) a session-status transition deserves. Same-state
/// updates never chime; a question always chimes; a completion chimes on the
/// Working→Idle edge.
pub fn sound_for_transition(prev: SessionStatus, new: SessionStatus) -> Option<Sound> {
    if prev == new {
        return None;
    }
    match new {
        SessionStatus::AwaitingInput => Some(Sound::Request),
        SessionStatus::Idle if prev == SessionStatus::Working => Some(Sound::Done),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_mapping_matches_herdr_semantics() {
        use SessionStatus::*;
        // A question always chimes, wherever it came from.
        assert_eq!(sound_for_transition(Working, AwaitingInput), Some(Sound::Request));
        assert_eq!(sound_for_transition(Idle, AwaitingInput), Some(Sound::Request));
        // Completion = the Working→Idle edge only.
        assert_eq!(sound_for_transition(Working, Idle), Some(Sound::Done));
        assert_eq!(sound_for_transition(AwaitingInput, Idle), None);
        assert_eq!(sound_for_transition(Errored, Idle), None);
        // Same state / other edges stay silent.
        assert_eq!(sound_for_transition(Working, Working), None);
        assert_eq!(sound_for_transition(Idle, Working), None);
        assert_eq!(sound_for_transition(Working, Errored), None);
    }

    #[test]
    fn temp_paths_are_unique() {
        assert_ne!(temp_path(), temp_path());
    }

    #[test]
    fn embedded_chimes_are_wav() {
        for data in [SOUND_DONE, SOUND_REQUEST] {
            assert!(data.len() > 1000);
            assert_eq!(&data[..4], b"RIFF");
            assert_eq!(&data[8..12], b"WAVE");
        }
    }
}
