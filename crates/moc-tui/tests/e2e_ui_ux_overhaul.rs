//! E2E UI/UX Overhaul Test Suite for moc-tui
//! Covering Tiers 1-4: Feature Coverage, Boundary & Corner Cases,
//! Cross-Feature Combinations, and Real-World Application Scenarios.

pub mod e2e;

#[test]
fn test_harness_initialization() {
    let (_app, _rx, terminal) = e2e::common::create_test_terminal(80, 24);
    assert_eq!(terminal.backend().buffer().area().width, 80);
    assert_eq!(terminal.backend().buffer().area().height, 24);
}
