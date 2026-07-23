//! Suspend/resume detection without OS hooks — wake is an EVENT, not a
//! timeout.
//!
//! A 1s ticker compares wall-clock elapsed to monotonic elapsed since the
//! previous tick. Monotonic clocks (macOS `mach_absolute_time`, Linux
//! `CLOCK_MONOTONIC`) exclude suspend, so a wall jump far beyond the tick
//! means the process just woke from system sleep. Subscribers — room actors,
//! relay links, the token refresh loop — reconnect/refresh immediately
//! instead of discovering half-open sockets by silence-lease timeout
//! (Discord/Slack-style instant recovery; user report: "doesn't fix until I
//! restart the app" / "shouldn't take a minute").
//!
//! The detector task is a lazily-spawned process-wide singleton; `subscribe`
//! must first be called from within a tokio runtime (every caller is an
//! async context). Broadcast receivers that lag simply miss duplicate wake
//! events — each subscriber treats ANY received event as "reconnect now",
//! so a missed one is at worst covered by the next silence lease.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::broadcast;

const TICK: Duration = Duration::from_secs(1);
/// Wall time must outrun monotonic time by this much in one tick to count as
/// a suspend — far above scheduler jitter, far below any real sleep.
const JUMP_THRESHOLD: Duration = Duration::from_secs(5);

static CHANNEL: OnceLock<broadcast::Sender<()>> = OnceLock::new();

/// Subscribe to system-wake events (the detector spawns on first call).
pub fn subscribe() -> broadcast::Receiver<()> {
    CHANNEL
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(4);
            let detector = tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(TICK);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await; // consume the immediate first tick
                let mut wall = SystemTime::now();
                let mut mono = Instant::now();
                loop {
                    interval.tick().await;
                    let wall_elapsed = SystemTime::now()
                        .duration_since(wall)
                        .unwrap_or(Duration::ZERO);
                    let mono_elapsed = mono.elapsed();
                    if wall_elapsed > mono_elapsed + JUMP_THRESHOLD {
                        tracing::info!(
                            slept_s = wall_elapsed.as_secs(),
                            "wake: system resumed from suspend"
                        );
                        let _ = detector.send(());
                    }
                    wall = SystemTime::now();
                    mono = Instant::now();
                }
            });
            tx
        })
        .subscribe()
}
