use super::*;

#[test]
fn bash_defaults_match_lifecycle_contract() {
    let config = BashConfig::default();
    assert_eq!(config.foreground_after_secs, 60);
    assert_eq!(config.monitored_timeout_secs, 1800);
    assert_eq!(config.stall_after_secs, 120);
    assert_eq!(config.max_timeout_secs, 3600);
    assert_eq!(config.max_output_bytes, 16 * 1024 * 1024);
    assert_eq!(config.persistent_log_bytes, 16 * 1024 * 1024);
}

#[test]
fn ptc_duration_limits_are_rejected_before_runtime() {
    let config = PtcConfig {
        max_timeout_secs: u64::MAX,
        ..Default::default()
    };
    assert!(validate_ptc(&config).is_err());

    let config = PtcConfig {
        host_idle_timeout_secs: u64::MAX,
        ..Default::default()
    };
    assert!(validate_ptc(&config).is_err());
}
