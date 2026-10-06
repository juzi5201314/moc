//! 重试策略与退避计算。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 退避下限：抖动可能把时长压到接近 0，最低保留 1ms，避免紧凑重试风暴。
const MIN_DELAY_SECS: f64 = 0.001;

/// 重试策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    /// 总尝试次数（含首次）。
    pub max_attempts: u32,
    /// 首次退避时长（秒）。
    pub initial_backoff_secs: f64,
    /// 单次退避上限（秒）。
    pub max_backoff_secs: f64,
    /// 是否对退避时长做全抖动（`[0, base]` 均匀取值）。
    pub jitter: bool,
}

impl Default for RetryPolicy {
    /// 默认三次尝试、0.5s 起步、20s 封顶、开启抖动。
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_secs: 0.5,
            max_backoff_secs: 20.0,
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt` 次（1 基）尝试失败后的等待时长；已无重试机会时返回 `None`。
    ///
    /// 退避为 `initial * 2^(attempt-1)` 并以 `max_backoff_secs` 封顶；上游给出
    /// `retry_after` 时取两者较大值（上游指令优先）。
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        let exponent = attempt.saturating_sub(1).min(30);
        // exponent 已 clamp 到 30，`as i32` 不可能截断或溢出
        let base =
            (self.initial_backoff_secs * 2f64.powi(exponent as i32)).min(self.max_backoff_secs);
        let backoff = if self.jitter {
            base * jitter_fraction()
        } else {
            base
        };
        let seconds = backoff.max(retry_after.map_or(0.0, |value| value.as_secs_f64()));
        Some(Duration::from_secs_f64(seconds.max(MIN_DELAY_SECS)))
    }
}

/// 全抖动比例，取自系统时钟的亚秒部分：这里只需把重试风暴打散，
/// 不需要密码学随机数，也就不引入随机数依赖。
fn jitter_fraction() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    f64::from(nanos) / 1e9
}

/// 解析 `retry-after` 头：仅接受整数秒，其余取值（HTTP 日期、小数）返回 `None`。
pub(crate) fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let seconds = value?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(initial: f64, max: f64, attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts: attempts,
            initial_backoff_secs: initial,
            max_backoff_secs: max,
            jitter: false,
        }
    }

    /// `delay(attempt, retry_after)`：指数退避并在 max 封顶；`retry_after` 更大时优先；
    /// 0 退避钳制到 1ms；越过 max_attempts 后不再等待。
    #[test]
    fn delay_table() {
        let exp = policy(0.5, 20.0, 8);
        let expected_ms = [500u64, 1000, 2000, 4000, 8000, 16000, 20000];
        for (index, expected) in expected_ms.iter().enumerate() {
            let attempt = index as u32 + 1;
            assert_eq!(
                exp.delay(attempt, None),
                Some(Duration::from_millis(*expected)),
                "attempt {attempt} 应指数翻倍并在 max 封顶"
            );
        }
        assert_eq!(exp.delay(8, None), None);
        assert_eq!(exp.delay(9, None), None);

        // retry_after 更大时优先；更小时按退避（第 2 次尝试为 0.5s 翻倍后的 1s）。
        let retry = policy(0.5, 20.0, 3);
        assert_eq!(
            retry.delay(1, Some(Duration::from_secs(7))),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            retry.delay(2, Some(Duration::from_secs(0))),
            Some(Duration::from_secs(1))
        );

        let zero = policy(0.0, 20.0, 3);
        assert_eq!(zero.delay(1, None), Some(Duration::from_millis(1)));
        assert_eq!(
            zero.delay(1, Some(Duration::from_secs(0))),
            Some(Duration::from_millis(1))
        );
    }

    #[test]
    fn jitter_stays_within_zero_to_base_and_respects_retry_after() {
        let policy = RetryPolicy {
            jitter: true,
            ..policy(0.5, 20.0, 3)
        };
        for _ in 0..64 {
            let delay = policy.delay(1, None).unwrap();
            assert!(delay >= Duration::from_millis(1), "{delay:?}");
            assert!(delay <= Duration::from_millis(500), "{delay:?}");
        }
        for _ in 0..64 {
            let delay = policy.delay(2, Some(Duration::from_secs(5))).unwrap();
            assert!(delay >= Duration::from_secs(5), "{delay:?}");
        }
    }

    #[test]
    fn retry_after_parses_integer_seconds_only() {
        assert_eq!(parse_retry_after(Some("0")), Some(Duration::from_secs(0)));
        assert_eq!(
            parse_retry_after(Some(" 12 ")),
            Some(Duration::from_secs(12))
        );
        assert_eq!(parse_retry_after(Some("1.5")), None);
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(Some("-1")), None);
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn default_policy_matches_documented_values() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.initial_backoff_secs, 0.5);
        assert_eq!(policy.max_backoff_secs, 20.0);
        assert!(policy.jitter);
    }
}
