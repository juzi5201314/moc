use super::*;

#[test]
fn version_negotiation_compares_major_only() {
    assert!(version_compatible("1.0"));
    assert!(version_compatible("1.7"));
    assert!(version_compatible("1"));
    assert!(!version_compatible("2.0"));
    assert!(!version_compatible("0.9"));
    assert!(!version_compatible(""));
    assert_eq!(major_version("1.2.3"), "1");
    assert_eq!(major_version("1"), "1");
}
