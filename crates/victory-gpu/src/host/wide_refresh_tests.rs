use super::wide_refresh_enabled;

#[test]
fn parallel_refresh_is_default_on_multicore_and_can_be_overridden() {
    assert!(wide_refresh_enabled(None, Some(4)));
    assert!(wide_refresh_enabled(None, Some(16)));
    assert!(!wide_refresh_enabled(None, Some(2)));
    assert!(!wide_refresh_enabled(None, None));
    assert!(wide_refresh_enabled(Some("1"), Some(1)));
    assert!(!wide_refresh_enabled(Some("0"), Some(16)));
    assert!(wide_refresh_enabled(Some("auto"), Some(4)));
}
