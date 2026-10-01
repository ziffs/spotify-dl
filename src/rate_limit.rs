use std::fmt;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use governor::DefaultDirectRateLimiter;
use governor::Quota;
use governor::RateLimiter;
use tokio::time::sleep;
use tracing::info;
use tracing::trace;
use tracing::warn;

/// How often the remaining wait time is checked and reported.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Lower bound for the poll interval, so a tiny `report_interval` can't cause
/// a busy loop.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Configuration for the persistent download rate limiter.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitConfig {
    /// Burst capacity: how many downloads may start back to back.
    pub max_downloads: NonZeroU32,
    /// Refill period for a single download token.
    pub period: Duration,
    /// Waits longer than this are reported to the user, followed by a
    /// countdown of the remaining wait time, updated once per interval.
    pub report_interval: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_downloads: NonZeroU32::new(30).expect("30 is non-zero"),
            period: Duration::from_secs(60),
            report_interval: Duration::from_secs(30),
        }
    }
}

/// A download rate limiter whose budget survives process restarts.
///
/// The in-memory [governor](https://docs.rs/governor) limiter enforces the
/// actual waiting, while a JSON state file on disk tracks the remaining budget
/// so that restarting the program does not reset the quota.
pub struct PersistentRateLimiter {
    state_path: PathBuf,
    config: RateLimitConfig,
    limiter: DefaultDirectRateLimiter,
    tracked: Mutex<TrackedState>,
}

impl fmt::Debug for PersistentRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentRateLimiter")
            .field("state_path", &self.state_path)
            .field("config", &self.config)
            .finish()
    }
}

#[derive(Debug)]
struct TrackedState {
    tokens: f64,
    last_refill_epoch_ms: u64,
}

struct PersistedState {
    tokens: f64,
    last_refill_epoch_ms: u64,
}

/// A snapshot of the rate limiter's budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitStatus {
    /// Full download tokens currently available.
    pub remaining: u32,
    /// Burst capacity (the maximum budget).
    pub capacity: u32,
    /// Time until the next full token is available (zero if one is available
    /// now or the budget is full).
    pub next_token_in: Duration,
}

impl PersistentRateLimiter {
    /// Create a limiter backed by the state file at `state_path`.
    ///
    /// The persisted budget is loaded, refilled by the time elapsed since the
    /// last write, and reconciled with a fresh in-memory limiter. A missing,
    /// corrupt or invalid state file is treated as a full budget.
    pub fn new(state_path: PathBuf, config: RateLimitConfig) -> Result<Self> {
        let quota = Quota::with_period(config.period)
            .ok_or_else(|| anyhow!("rate limit period must be greater than zero"))?
            .allow_burst(config.max_downloads);
        let limiter = RateLimiter::direct(quota);

        if let Some(parent) = state_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create directory for rate limit state file {}",
                        state_path.display()
                    )
                })?;
            }
        }

        let max = config.max_downloads.get() as f64;
        let period_secs = config.period.as_secs_f64();

        let (tokens, last_refill_epoch_ms) = match load_state(&state_path) {
            Some(state) => (state.tokens, state.last_refill_epoch_ms),
            None => (max, current_epoch_ms()),
        };

        let now = current_epoch_ms();
        let tokens = refill(tokens, last_refill_epoch_ms, now, max, period_secs);

        // Budget already spent in previous runs shows up as a deficit between
        // the persisted budget and the fresh in-memory limiter. Consuming it up
        // front is instantaneous (`check_n` never waits) and brings the
        // governor limiter back in sync with the persisted state.
        let deficit = (max - tokens).ceil();
        if deficit >= 1.0 {
            let n = NonZeroU32::new(deficit as u32)
                .expect("deficit is at least 1 and at most the burst capacity");
            match limiter.check_n(n) {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => {
                    warn!(
                        "failed to reconcile rate limiter with persisted budget (deficit {}); \
                         continuing with a slightly more permissive limiter",
                        deficit
                    );
                }
            }
        }

        let tracked = TrackedState {
            tokens,
            last_refill_epoch_ms: now,
        };
        write_state(&state_path, tracked.tokens, tracked.last_refill_epoch_ms)?;

        Ok(Self {
            state_path,
            config,
            limiter,
            tracked: Mutex::new(tracked),
        })
    }

    /// The current budget, with the refill up to the current time applied.
    pub fn status(&self) -> RateLimitStatus {
        let max = self.config.max_downloads.get();
        let tokens = {
            let tracked = self
                .tracked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            refill(
                tracked.tokens,
                tracked.last_refill_epoch_ms,
                current_epoch_ms(),
                max as f64,
                self.config.period.as_secs_f64(),
            )
        };

        let next_token_in = if tokens >= max as f64 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((tokens.ceil() - tokens) * self.config.period.as_secs_f64())
        };

        RateLimitStatus {
            remaining: tokens.floor() as u32,
            capacity: max,
            next_token_in,
        }
    }

    /// Wait until the rate limiter allows another download, then record the
    /// spent token in the persisted state file.
    ///
    /// Waits longer than [`RateLimitConfig::report_interval`] are announced
    /// once, followed by a countdown of the remaining wait time.
    pub async fn acquire(&self) -> Result<()> {
        self.wait_with_progress().await;

        let (tokens, last_refill_epoch_ms) = {
            let mut tracked = self
                .tracked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = current_epoch_ms();
            let refilled = refill(
                tracked.tokens,
                tracked.last_refill_epoch_ms,
                now,
                self.config.max_downloads.get() as f64,
                self.config.period.as_secs_f64(),
            );
            tracked.tokens = (refilled - 1.0).max(0.0);
            tracked.last_refill_epoch_ms = now;
            (tracked.tokens, tracked.last_refill_epoch_ms)
        };

        write_state(&self.state_path, tokens, last_refill_epoch_ms)?;
        trace!(
            "rate limit budget: {:.3} of {} downloads remaining",
            tokens, self.config.max_downloads
        );

        Ok(())
    }

    /// Poll the limiter until a token is available, logging the remaining wait
    /// time while waiting. A successful `check` consumes the token, exactly
    /// like `until_ready` would.
    async fn wait_with_progress(&self) {
        let mut last_report: Option<Instant> = None;
        loop {
            match self.limiter.check() {
                Ok(_) => return,
                Err(not_until) => {
                    let wait = not_until.wait_time_from(Instant::now());
                    let reporting = last_report.is_some();
                    if wait > self.config.report_interval || reporting {
                        let due = last_report
                            .map_or(true, |last| last.elapsed() >= self.config.report_interval);
                        if due {
                            info!(
                                "Rate limit reached: next download in {:.1}s",
                                wait.as_secs_f64()
                            );
                            last_report = Some(Instant::now());
                        }
                    }
                    let poll = POLL_INTERVAL
                        .min(self.config.report_interval)
                        .max(MIN_POLL_INTERVAL);
                    sleep(poll.min(wait)).await;
                }
            }
        }
    }
}

/// Refill `tokens` by the time elapsed since `last_refill_epoch_ms`, clamped to
/// `[0, max]`. Timestamps in the future count as zero elapsed time.
fn refill(
    tokens: f64,
    last_refill_epoch_ms: u64,
    now_epoch_ms: u64,
    max: f64,
    period_secs: f64,
) -> f64 {
    let elapsed_ms = now_epoch_ms.saturating_sub(last_refill_epoch_ms);
    let refilled = tokens + elapsed_ms as f64 / 1000.0 / period_secs;
    refilled.clamp(0.0, max)
}

fn load_state(path: &Path) -> Option<PersistedState> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(
                "failed to read rate limit state file {}: {}; starting with a full budget",
                path.display(),
                err
            );
            return None;
        }
    };

    match parse_state(&content) {
        Some(state) => Some(state),
        None => {
            warn!(
                "rate limit state file {} is corrupt or invalid; starting with a full budget",
                path.display()
            );
            None
        }
    }
}

fn parse_state(content: &str) -> Option<PersistedState> {
    let tokens = parse_field(content, "tokens")?;
    let last_refill_epoch_ms = parse_field(content, "last_refill_epoch_ms")?;

    if !tokens.is_finite() || tokens < 0.0 {
        return None;
    }
    if !last_refill_epoch_ms.is_finite()
        || last_refill_epoch_ms < 0.0
        || last_refill_epoch_ms > u64::MAX as f64
    {
        return None;
    }

    Some(PersistedState {
        tokens,
        last_refill_epoch_ms: last_refill_epoch_ms as u64,
    })
}

/// Extract the numeric value following `"key":` in the state file's JSON.
fn parse_field(content: &str, key: &str) -> Option<f64> {
    let key_pos = content.find(&format!("\"{}\"", key))?;
    let rest = &content[key_pos + key.len() + 2..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    let end = value
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(value.len());
    value[..end].parse::<f64>().ok()
}

fn write_state(path: &Path, tokens: f64, last_refill_epoch_ms: u64) -> Result<()> {
    let content = format!(
        "{{\"tokens\":{},\"last_refill_epoch_ms\":{}}}",
        tokens, last_refill_epoch_ms
    );

    // Write to a temporary file in the same directory and rename it over the
    // target, so a crash mid-write can never leave a partially written state
    // file behind (rename is atomic; the old file stays intact until it
    // completes).
    let tmp_path = path.with_extension(format!("tmp-{}", std::process::id()));

    let mut tmp = File::create(&tmp_path).with_context(|| {
        format!(
            "failed to create temporary state file {}",
            tmp_path.display()
        )
    })?;

    tmp.write_all(content.as_bytes())?;

    drop(tmp);

    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to persist rate limit state to {}", path.display()))?;

    Ok(())
}

fn current_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use tempfile::tempdir;

    use super::PersistentRateLimiter;
    use super::RateLimitConfig;
    use super::current_epoch_ms;
    use super::parse_state;

    fn test_config(max: u32, period_ms: u64) -> RateLimitConfig {
        RateLimitConfig {
            max_downloads: std::num::NonZeroU32::new(max).unwrap(),
            period: Duration::from_millis(period_ms),
            // Keep existing tests silent: only the dedicated reporting test
            // opts into short report intervals.
            report_interval: Duration::from_secs(3600),
        }
    }

    #[test]
    fn limiter_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PersistentRateLimiter>();
    }

    #[tokio::test]
    async fn burst_is_allowed_then_throttled() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let limiter = PersistentRateLimiter::new(state_path, test_config(3, 50)).unwrap();

        let start = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await.unwrap();
        }
        let burst_elapsed = start.elapsed();
        assert!(
            burst_elapsed < Duration::from_millis(250),
            "burst of 3 should complete quickly, took {:?}",
            burst_elapsed
        );

        let start = Instant::now();
        limiter.acquire().await.unwrap();
        let throttled_elapsed = start.elapsed();
        assert!(
            throttled_elapsed >= Duration::from_millis(40),
            "4th acquire should wait for a token, took {:?}",
            throttled_elapsed
        );
        assert!(
            throttled_elapsed < Duration::from_millis(500),
            "4th acquire should not wait much longer than one period, took {:?}",
            throttled_elapsed
        );
    }

    #[tokio::test]
    async fn budget_persists_across_instances() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let config = test_config(3, 50);

        let limiter = PersistentRateLimiter::new(state_path.clone(), config).unwrap();
        for _ in 0..3 {
            limiter.acquire().await.unwrap();
        }
        drop(limiter);

        // A new "run" on the same state file must inherit the spent budget.
        let limiter = PersistentRateLimiter::new(state_path, config).unwrap();
        let start = Instant::now();
        limiter.acquire().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "first acquire of a new instance should wait for a refilled token, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn elapsed_time_refills_budget() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let last_refill = current_epoch_ms() - 250;
        fs::write(
            &state_path,
            format!(
                "{{\"tokens\":0.0,\"last_refill_epoch_ms\":{}}}",
                last_refill
            ),
        )
        .unwrap();

        let limiter = PersistentRateLimiter::new(state_path, test_config(5, 50)).unwrap();

        // 250ms at 50ms/token refills the full budget of 5.
        let start = Instant::now();
        for _ in 0..5 {
            limiter.acquire().await.unwrap();
        }
        let burst_elapsed = start.elapsed();
        assert!(
            burst_elapsed < Duration::from_millis(250),
            "refilled budget should allow 5 quick acquires, took {:?}",
            burst_elapsed
        );

        let start = Instant::now();
        limiter.acquire().await.unwrap();
        let throttled_elapsed = start.elapsed();
        assert!(
            throttled_elapsed >= Duration::from_millis(40),
            "6th acquire should wait for a token, took {:?}",
            throttled_elapsed
        );
    }

    #[tokio::test]
    async fn corrupt_state_file_is_treated_as_full_budget() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        fs::write(&state_path, "this is definitely not json {{{").unwrap();

        let limiter = PersistentRateLimiter::new(state_path, test_config(3, 50)).unwrap();

        let start = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await.unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "corrupt state should behave like a fresh full budget"
        );

        let start = Instant::now();
        limiter.acquire().await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "budget should be the normal full one, not unlimited"
        );
    }

    #[tokio::test]
    async fn future_last_refill_is_clamped() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let future = current_epoch_ms() + 60 * 60 * 1000;
        fs::write(
            &state_path,
            format!("{{\"tokens\":2.0,\"last_refill_epoch_ms\":{}}}", future),
        )
        .unwrap();

        let limiter = PersistentRateLimiter::new(state_path.clone(), test_config(3, 50)).unwrap();

        // tokens 2.0 with zero elapsed time: one acquire is immediate.
        let start = Instant::now();
        limiter.acquire().await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "acquire with a future timestamp should not panic or wait"
        );

        let content = fs::read_to_string(&state_path).unwrap();
        let state = parse_state(&content).expect("state file should parse after acquire");
        assert!(
            (state.tokens - 1.0).abs() < 0.1,
            "expected ~1.0 remaining token, got {}",
            state.tokens
        );
    }

    #[tokio::test]
    async fn state_file_is_written_after_acquire() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let limiter = PersistentRateLimiter::new(state_path.clone(), test_config(3, 50)).unwrap();

        limiter.acquire().await.unwrap();

        let content = fs::read_to_string(&state_path).unwrap();
        let state = parse_state(&content).expect("state file should exist and parse");
        assert!(
            (state.tokens - 2.0).abs() < 0.1,
            "expected tokens == max - 1, got {}",
            state.tokens
        );
        assert!(
            state.last_refill_epoch_ms <= current_epoch_ms(),
            "last_refill should not be in the future"
        );
    }

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Waits longer than `report_interval` must produce an announcement with
    /// the remaining wait time, followed by countdown updates.
    #[tokio::test]
    async fn long_waits_are_reported_with_remaining_time() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("rate_limit.json");
        let config = RateLimitConfig {
            max_downloads: std::num::NonZeroU32::new(1).unwrap(),
            period: Duration::from_millis(300),
            report_interval: Duration::from_millis(50),
        };
        let limiter = PersistentRateLimiter::new(state_path, config).unwrap();

        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let capture = LogCapture(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        limiter.acquire().await.unwrap(); // consumes the burst token
        limiter.acquire().await.unwrap(); // must wait a full period

        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

        let remaining: Vec<f64> = logs
            .lines()
            .filter_map(|line| {
                let pos = line.find("next download in ")?;
                line[pos + "next download in ".len()..]
                    .trim_end_matches('s')
                    .parse::<f64>()
                    .ok()
            })
            .collect();

        assert!(
            remaining.len() >= 3,
            "expected an announcement plus countdown updates, got: {}",
            logs
        );
        assert!(
            remaining.first().unwrap() > remaining.last().unwrap(),
            "remaining wait time should count down, got: {:?}",
            remaining
        );
    }
}
