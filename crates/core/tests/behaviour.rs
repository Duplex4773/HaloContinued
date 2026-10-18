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
fn delayed_failed_notifications_do_not_transfer_to_reused_device_keys() {
    for full in [false, true] {
        for changed in 0..4 {
            let mut e = engine();
            let mut first = reading(if full { 98 } else { 19 }, full, 0);
            let note = if full {
                assert!(
                    e.apply("razer", Ok(vec![first.clone()]), 0.0, false)
                        .is_empty()
                );
                first.level = Some(100);
                first.timestamp = 60;
                e.apply("razer", Ok(vec![first.clone()]), 60.0, false)
            } else {
                e.apply("razer", Ok(vec![first.clone()]), 0.0, false)
            }
            .pop()
            .unwrap();
            let mut replacement = first;
            replacement.timestamp = 120;
            replacement.charging = Some(false);
            match changed {
                0 => replacement.serial = Some("SYNTHETIC-REPLACEMENT".into()),
                1 => replacement.container = Some("SYNTHETIC-REPLACEMENT".into()),
                2 => replacement.source = "replacement-source".into(),
                _ => replacement.kind = "replacement-kind".into(),
            }
            let replacement_notes = e.apply("razer", Ok(vec![replacement]), 120.0, false);
            assert_eq!(replacement_notes.len(), usize::from(!full));
            // This models a delayed UI delivery failure arriving after discovery
            // already replaced the unit. It must not revive the prior unit's alert.
            e.notification_failed(note);
            assert!(
                e.flush_held().is_empty(),
                "full={full}, identity field={changed}"
            );
            assert!(!e.has_held_notifications());
        }
    }
}

#[test]
fn delayed_retry_cannot_replace_a_new_low_battery_episode() {
    let mut e = engine();
    let old = e
        .apply("razer", Ok(vec![reading(19, false, 0)]), 0.0, false)
        .pop()
        .unwrap();
    e.apply("razer", Ok(vec![reading(80, true, 60)]), 60.0, false);
    let current = e.apply("razer", Ok(vec![reading(15, false, 120)]), 120.0, true);
    assert!(current.is_empty());
    assert!(e.has_held_notifications());
    e.notification_failed(old.clone());
    let current = e.flush_held();
    assert_eq!(current.len(), 1);
    // A failure from the previous episode must not remain valid after this
    // current notification is delivered successfully.
    let current = current.into_iter().next().unwrap();
    e.notification_failed(old);
    assert!(e.flush_held().is_empty());
    e.notification_failed(current.clone());
    assert_eq!(e.flush_held().len(), 1);
    e.notification_failed(current);
    e.apply("razer", Ok(vec![reading(80, true, 180)]), 180.0, false);
    assert!(!e.has_held_notifications());
}

#[test]
fn reused_keys_do_not_inherit_another_units_alerts_or_sound_cooldown() {
    let mut e = engine();
    e.settings.low_sound = true;
    let mut first = reading(10, false, 0);
    first.serial = Some("SYNTHETIC-FIRST-UNIT".into());
    assert_eq!(e.apply("razer", Ok(vec![first]), 0.0, false).len(), 1);
    assert_eq!(e.take_low_battery_sound(), Some(10));
    let mut replacement = reading(10, false, 60);
    replacement.serial = Some("SYNTHETIC-SECOND-UNIT".into());
    let notes = e.apply("razer", Ok(vec![replacement]), 60.0, false);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, NotificationKind::Low);
    assert_eq!(e.take_low_battery_sound(), Some(10));

    let mut e = engine();
    let mut first = reading(98, true, 0);
    first.serial = Some("SYNTHETIC-FIRST-UNIT".into());
    e.apply("razer", Ok(vec![first.clone()]), 0.0, true);
    first.level = Some(100);
    first.timestamp = 60;
    e.apply("razer", Ok(vec![first]), 60.0, true);
    assert!(e.has_held_notifications());
    let mut replacement = reading(100, false, 120);
    replacement.serial = Some("SYNTHETIC-SECOND-UNIT".into());
    assert!(
        e.apply("razer", Ok(vec![replacement]), 120.0, false)
            .is_empty()
    );
    assert!(!e.has_held_notifications());
}
#[test]
fn reused_keys_do_not_inherit_another_units_live_estimate() {
    for field in 0..4 {
        let mut e = engine();
        let old = reading(80, false, 0);
        e.apply("razer", Ok(vec![old.clone()]), 0.0, false);
        let estimate = e.estimator.devices.get_mut(&old.key).unwrap();
        estimate.usage = 600.0;
        estimate.samples = vec![(0.0, 90), (600.0, 80)];
        e.estimator
            .devices
            .entry("unrelated".into())
            .or_default()
            .usage = 123.0;
        let mut replacement = reading(50, false, 60);
        match field {
            0 => replacement.source = "replacement-source".into(),
            1 => replacement.kind = "replacement-kind".into(),
            2 => replacement.serial = Some("replacement-unit".into()),
            _ => replacement.container = Some("replacement-container".into()),
        }
        e.apply("razer", Ok(vec![replacement]), 60.0, false);
        let estimate = &e.estimator.devices[&old.key];
        assert_eq!(estimate.usage, 0.0, "identity field {field}");
        assert_eq!(estimate.samples, [(0.0, 50)]);
        assert_eq!(e.estimator.devices["unrelated"].usage, 123.0);
    }
}
#[test]
fn conflicting_containers_and_ambiguous_serials_do_not_hide_bluetooth_devices() {
    for ambiguous in [false, true] {
        let mut e = engine();
        let mut first = reading(70, false, 100);
        first.key = "hid:first".into();
        first.serial = Some("SYNTHETIC-SHARED-SERIAL".into());
        first.container = Some("physical-first".into());
        let mut bt = first.clone();
        bt.key = "bluetooth:separate".into();
        bt.source = "bluetooth".into();
        if ambiguous {
            bt.container = None;
            let mut second = first.clone();
            second.key = "hid:second".into();
            second.container = Some("physical-second".into());
            e.apply("razer", Ok(vec![first, second]), 0.0, false);
        } else {
            bt.container = Some("physical-separate".into());
            e.apply("razer", Ok(vec![first]), 0.0, false);
        }
        e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
        assert!(e.readings().iter().any(|r| r.key == "bluetooth:separate"));
    }
}
#[test]
fn proven_container_match_wins_but_known_different_device_kinds_stay_separate() {
    for different_kind in [false, true] {
        let mut e = engine();
        let mut hid = reading(70, false, 100);
        hid.kind = "mouse".into();
        hid.container = Some("physical-unit".into());
        hid.serial = Some("shared-unit".into());
        let mut bt = hid.clone();
        bt.key = "bluetooth:unit".into();
        bt.source = "bluetooth".into();
        if different_kind {
            bt.kind = "gamepad".into();
        }
        let mut other = hid.clone();
        other.key = "other-hid".into();
        other.container = None;
        e.apply("razer", Ok(vec![hid, other]), 0.0, false);
        e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
        assert_eq!(
            e.readings().iter().any(|r| r.key == "bluetooth:unit"),
            different_kind
        );
    }
}
#[test]
fn inventory_presence_matches_resolved_readings_and_enabled_providers() {
    let mut e = engine();
    assert!(!e.has_readings());
    let mut hid = reading(80, false, 0);
    hid.serial = Some("SYNTHETIC-UNIT".into());
    let mut bluetooth = hid.clone();
    bluetooth.key = "bluetooth:one".into();
    bluetooth.source = "bluetooth".into();
    e.apply("razer", Ok(vec![hid]), 0.0, false);
    e.apply("bluetooth", Ok(vec![bluetooth]), 0.0, false);
    assert!(e.has_readings());
    assert_eq!(e.readings().len(), 1);
    let mut settings = e.settings.clone();
    settings.disabled_providers.insert("razer".into());
    e.update_settings(settings.clone());
    assert!(e.has_readings());
    settings.bluetooth = false;
    e.update_settings(settings);
    assert!(!e.has_readings());
    assert!(e.readings().is_empty());
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
