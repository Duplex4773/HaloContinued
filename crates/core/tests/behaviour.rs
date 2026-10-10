use hb_core::*;
fn reading(level: u8, charging: bool, timestamp: i64) -> Reading {
    let mut r = Reading::new("razer:one", "Mouse", "razer", timestamp);
    r.level = Some(level);
    r.charging = Some(charging);
    r
}
fn engine() -> Engine {
    Engine::new(Settings::default(), Estimator::default())
}
#[test]
fn low_alert_has_hysteresis_and_rearms_on_charge() {
    let mut e = engine();
    for (level, charge, expected) in [
        (20, false, 1),
        (19, false, 0),
        (23, false, 0),
        (26, false, 0),
        (20, false, 1),
        (15, true, 0),
        (15, false, 1),
    ] {
        assert_eq!(
            e.apply("razer", Ok(vec![reading(level, charge, 0)]), 0.0, false)
                .len(),
            expected
        );
    }
}
#[test]
fn full_alert_ignores_startup_and_charge_jitter() {
    let mut e = engine();
    for (level, charge, expected) in [
        (100, true, 0),
        (99, true, 0),
        (100, true, 0),
        (90, false, 0),
        (95, true, 0),
        (100, false, 1),
        (99, true, 0),
        (100, true, 0),
        (90, true, 0),
        (100, true, 1),
    ] {
        assert_eq!(
            e.apply("razer", Ok(vec![reading(level, charge, 0)]), 0.0, false)
                .len(),
            expected
        );
    }
}
#[test]
fn a_failure_is_not_a_confirmed_disconnect() {
    let mut e = engine();
    e.apply("razer", Ok(vec![reading(75, false, 0)]), 0.0, false);
    for _ in 0..3 {
        e.apply("razer", Err(ProviderError::new("busy")), 0.0, false);
    }
    assert_eq!(e.readings()[0].connection, Connection::Stale);
    e.apply("razer", Ok(vec![]), 0.0, false);
    assert_eq!(e.readings().len(), 1);
    e.apply("razer", Ok(vec![]), 0.0, false);
    assert!(e.readings().is_empty());
}
#[test]
fn held_low_notification_is_dropped_after_charge() {
    let mut e = engine();
    assert!(
        e.apply("razer", Ok(vec![reading(10, false, 0)]), 0.0, true)
            .is_empty()
    );
    e.apply("razer", Ok(vec![reading(30, true, 0)]), 0.0, true);
    assert!(e.flush_held().is_empty());
}
#[test]
fn delayed_full_alert_requires_a_current_known_full_reading() {
    for failed_delivery in [false, true] {
        let mut e = engine();
        e.apply("razer", Ok(vec![reading(95, true, 0)]), 0.0, false);
        let notes = e.apply(
            "razer",
            Ok(vec![reading(100, true, 1)]),
            1.0,
            !failed_delivery,
        );
        if failed_delivery {
            e.notification_failed(notes[0].clone());
        }
        let mut unknown = reading(100, false, 2);
        unknown.level = None;
        e.apply("razer", Ok(vec![unknown]), 2.0, true);
        assert!(e.flush_held().is_empty());
        assert!(e.has_held_notifications());
        e.apply("razer", Ok(vec![reading(80, false, 3)]), 3.0, true);
        assert!(e.flush_held().is_empty());
        assert!(!e.has_held_notifications());
    }
}
#[test]
fn display_name_is_bounded_single_line_and_blank_restores_detected_name() {
    use hb_core::settings::device_name;
    assert_eq!(
        device_name("  Work\0 mouse\r\n ").as_deref(),
        Some("Work mouse")
    );
    assert_eq!(device_name(" \r\n\t"), None);
    assert_eq!(device_name(&"🐭".repeat(121)).unwrap().chars().count(), 120);
    let settings =
        Settings::from_value(serde_json::json!({"devices":{"test":{"name":"  \u{0} "}}})).unwrap();
    assert!(settings.devices["test"].name.is_none());
}
#[test]
fn invalid_properties_use_individual_defaults() {
    let s = Settings::from_value(serde_json::json!({"interval":0,"low":15,"bluetooth":1,"animation":false,"icon_theme":"evil"})).unwrap();
    assert_eq!(s.interval, 60);
    assert_eq!(s.low, 15);
    assert!(s.bluetooth);
    assert!(!s.animation);
    assert_eq!(s.icon_theme, "auto");
    assert!(Settings::from_value(serde_json::json!([])).is_err());
}
#[test]
fn unknown_serials_do_not_merge_identical_names() {
    let mut e = engine();
    let a = reading(50, false, 0);
    let mut b = a.clone();
    b.key = "razer:two".into();
    e.apply("razer", Ok(vec![a, b]), 0.0, false);
    assert_eq!(e.readings().len(), 2);
}
#[test]
fn estimator_ignores_sleep_coarse_and_clock_backwards() {
    let mut e = Estimator::default();
    let mut r = reading(100, false, 0);
    for i in 0..5 {
        r.level = Some(100 - i);
        e.record(&r, f64::from(i) * 600.0);
    }
    assert!(e.seconds_left(&r.key, r.level).is_some());
    let usage = e.devices[&r.key].usage;
    e.record(&r, 1.0);
    assert_eq!(e.devices[&r.key].usage, usage);
    r.connection = Connection::Sleeping;
    e.record(&r, 10000.0);
    r.connection = Connection::Online;
    e.record(&r, 11000.0);
    assert_eq!(e.devices[&r.key].usage, usage);
    r.charging = Some(true);
    e.record(&r, 12000.0);
    assert!(e.seconds_left(&r.key, r.level).is_none());
}
#[test]
fn guid_identity_is_stable_and_distinct() {
    assert_eq!(stable_guid("one"), stable_guid("one"));
    assert_ne!(stable_guid("one"), stable_guid("two"));
}
