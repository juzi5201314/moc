use super::ReconnectPolicy;
use super::qualified_tool_name;
use std::time::Duration;

#[test]
fn qualified_names_are_injective_and_independent_of_listing_order() {
    let names = ["a.b", "a_b", "a_2eb", "a-b", "工具", "工_具", "", "_", "\0"];
    let mapped: std::collections::BTreeSet<_> = names
        .iter()
        .map(|name| qualified_tool_name("demo", name))
        .collect();
    assert_eq!(mapped.len(), names.len());
    assert_ne!(
        qualified_tool_name("a_", "5fb"),
        qualified_tool_name("a", "_b")
    );
    for name in mapped {
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
    }
}

#[test]
fn backoff_is_exponential_and_capped() {
    let policy = ReconnectPolicy::default();
    assert_eq!(policy.backoff(1), Duration::from_millis(500));
    assert_eq!(policy.backoff(2), Duration::from_secs(1));
    assert_eq!(policy.backoff(3), Duration::from_secs(2));
    assert_eq!(policy.backoff(10), policy.max_backoff);
    assert_eq!(policy.max_attempts, 10);
    assert_eq!(policy.stable_runtime, Duration::from_secs(30));
}
