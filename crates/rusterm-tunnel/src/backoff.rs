//! Reconnection backoff with jitter. The supervisor uses this to pace
//! reconnect attempts so a flapping tunnel doesn't hammer the SSH server,
//! while still coming back quickly on a transient blip.
//!
//! Pure functions — the supervisor supplies `rand01` so tests get
//! deterministic schedules.

use std::time::Duration;

/// First retry after 1s, multiplier 1.5, capped at 60s, jitter ±20%.
pub const BASE_DELAY: Duration = Duration::from_millis(800);
pub const MAX_DELAY: Duration = Duration::from_secs(60);
pub const MULTIPLIER: f64 = 1.5;
pub const JITTER: f64 = 0.2; // ±20%

/// Compute the delay before reconnect attempt `attempt` (1-based).
///
/// `rand01` must lie in `[0.0, 1.0)` and is used only for jitter, so a
/// fixed value yields a fully deterministic answer (tests pass 0.5).
pub fn backoff_delay(attempt: u32, rand01: f64) -> Duration {
    let attempt = attempt.max(1);
    let exponent = (attempt - 1) as f64;
    let raw_ms = BASE_DELAY.as_millis() as f64 * MULTIPLIER.powf(exponent);
    let capped_ms = raw_ms.min(MAX_DELAY.as_millis() as f64);
    // Jitter in [1 - J, 1 + J].
    let jitter_span = JITTER * 2.0;
    let jitter_factor = 1.0 - JITTER + rand01.clamp(0.0, 1.0) * jitter_span;
    let ms = (capped_ms * jitter_factor).max(1.0);
    Duration::from_millis(ms as u64)
}

/// Cheap process-level entropy for jitter. Not cryptographic — this is
/// only smearing retry times.
pub fn rand01_now() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_attempt_is_near_base() {
        // Deterministic midpoint jitter keeps attempt-1 delay within ±20%
        // of 800ms.
        for r in [0.0, 0.25, 0.5, 0.75, 0.999] {
            let d = backoff_delay(1, r);
            assert!(d >= Duration::from_millis(640), "{d:?} < lower bound");
            assert!(d <= Duration::from_millis(960), "{d:?} > upper bound");
        }
    }

    #[test]
    fn delay_grows_geometrically() {
        let d1 = backoff_delay(1, 0.5).as_millis();
        let d2 = backoff_delay(2, 0.5).as_millis();
        let d3 = backoff_delay(3, 0.5).as_millis();
        assert!(d2 > d1);
        assert!(d3 > d2);
        // Ratio should track the multiplier (jitter is deterministic at
        // 0.5 → factor 1.0 exactly).
        assert!((d2 as f64 / d1 as f64 - MULTIPLIER).abs() < 0.01);
    }

    #[test]
    fn delay_is_capped() {
        let d20 = backoff_delay(20, 0.5);
        assert!(d20 <= MAX_DELAY);
        // Even without jitter going high.
        assert!(backoff_delay(30, 0.999) <= MAX_DELAY + MAX_DELAY / 4);
    }

    #[test]
    fn jitter_bounds() {
        // Attempt 15: raw 800ms·1.5¹⁴ ≈ 233s, so the 60s cap applies and
        // jitter spreads the delay into [48s, 72s].
        let lo = backoff_delay(15, 0.0);
        let hi = backoff_delay(15, 0.999);
        assert!(lo >= Duration::from_secs(48), "{lo:?}");
        assert!(hi <= Duration::from_secs(73), "{hi:?}");
    }

    #[test]
    fn attempt_zero_treated_as_one() {
        assert_eq!(backoff_delay(0, 0.5), backoff_delay(1, 0.5));
    }
}
