//! 生成具备足够碰撞安全性的实体标识。

use uuid::Uuid;

/// 会话 ID 前缀；会话目录的识别也以它为准（子代理 transcript 按父会话目录归档）。
pub const SESSION_PREFIX: &str = "ses_";

/// UUIDv7 的前缀仅包含时间；会话取随机尾部以避免同毫秒覆盖。
pub fn session_id() -> String {
    let raw = Uuid::now_v7().simple().to_string();
    format!("{SESSION_PREFIX}{}", &raw[20..])
}

/// 创建 8 位十六进制 entry 标识。
pub fn entry_id() -> String {
    short_uuid()
}

/// 创建计划标识。
pub fn plan_id() -> String {
    format!("plan_{}", short_uuid())
}

/// 创建当前会话内单调递增的提示标识。
pub fn prompt_id(sequence: u64) -> String {
    format!("prompt_{sequence}")
}

/// 创建当前会话内单调递增的后台任务标识。
pub fn job_id(sequence: u64) -> String {
    format!("job_{sequence}")
}

fn short_uuid() -> String {
    use std::sync::{
        LazyLock,
        atomic::{AtomicU32, Ordering},
    };
    static SEQUENCE: LazyLock<AtomicU32> =
        LazyLock::new(|| AtomicU32::new(Uuid::now_v7().as_u128() as u32));
    format!("{:08x}", SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_have_stable_protocol_shapes() {
        let session = session_id();
        assert!(session.starts_with(SESSION_PREFIX));
        assert_eq!(session.len(), 16);
        assert!(session[4..].bytes().all(|byte| byte.is_ascii_hexdigit()));
        for id in [entry_id(), plan_id().trim_start_matches("plan_").to_owned()] {
            assert_eq!(id.len(), 8);
            assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert_eq!(prompt_id(7), "prompt_7");
        assert_eq!(job_id(11), "job_11");
    }

    #[test]
    fn rapid_entries_and_sessions_do_not_alias() {
        let entries: std::collections::HashSet<_> = (0..1000).map(|_| entry_id()).collect();
        assert_eq!(entries.len(), 1000);
        let sessions: std::collections::HashSet<_> = (0..100).map(|_| session_id()).collect();
        assert_eq!(sessions.len(), 100);
    }
}
