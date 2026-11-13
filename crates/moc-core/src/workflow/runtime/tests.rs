use super::*;

#[test]
fn run_id_charset_rejects_traversal() {
    assert!(valid_run_id("run_1735_001"));
    assert!(valid_run_id("a-b_C1"));
    assert!(!valid_run_id("../x"));
    assert!(!valid_run_id("a/b"));
    assert!(!valid_run_id("a.b"));
    assert!(!valid_run_id(""));
    assert!(!valid_run_id(&"x".repeat(65)));
}
