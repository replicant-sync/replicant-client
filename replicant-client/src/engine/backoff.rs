use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_deterministic_and_in_unit_range() {
        let mut a = Jitter::new(42);
        let mut b = Jitter::new(42);
        for _ in 0..1000 {
            let x = a.next_unit();
            assert_eq!(x, b.next_unit());
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn connect_delay_is_full_jitter_capped_at_60s() {
        assert_eq!(connect_delay(0, 0.0), Duration::ZERO);
        assert_eq!(connect_delay(0, 0.5), Duration::from_millis(500));
        assert_eq!(connect_delay(3, 0.5), Duration::from_millis(4000));
        assert_eq!(connect_delay(30, 0.999_999), Duration::from_millis(59_999));
    }

    #[test]
    fn doc_retry_doubles_to_cap() {
        assert_eq!(doc_retry_delay(1), Duration::from_secs(1));
        assert_eq!(doc_retry_delay(2), Duration::from_secs(2));
        assert_eq!(doc_retry_delay(10), Duration::from_secs(60));
    }

    #[test]
    fn catch_up_retry_is_1_2_4() {
        assert_eq!(catch_up_retry_delay(1), Duration::from_secs(1));
        assert_eq!(catch_up_retry_delay(2), Duration::from_secs(2));
        assert_eq!(catch_up_retry_delay(3), Duration::from_secs(4));
    }
}

const CAP_MS: f64 = 60_000.0;

/// xorshift64: deterministic randomness so the core stays pure and testable.
#[derive(Debug, Clone)]
pub struct Jitter(u64);

impl Jitter {
    pub fn new(seed: u64) -> Self {
        Jitter(seed.max(1))
    }

    pub fn next_unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn connect_delay(attempt: u32, unit: f64) -> Duration {
    let ceiling = (1000.0 * 2f64.powi(attempt.min(30) as i32)).min(CAP_MS);
    Duration::from_millis((ceiling * unit) as u64)
}

pub fn doc_retry_delay(failures: u32) -> Duration {
    let ms = (1000.0 * 2f64.powi(failures.saturating_sub(1).min(30) as i32)).min(CAP_MS);
    Duration::from_millis(ms as u64)
}

pub fn catch_up_retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(2))
}
