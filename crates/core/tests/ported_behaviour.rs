use hb_core::history::format_left;
use hb_core::*;
use serde_json::json;
fn engine() -> Engine {
    Engine::new(Settings::default(), Estimator::default())
}
fn r(key: &str, level: Option<u8>, charging: Option<bool>) -> Reading {
    let mut r = Reading::new(key, "Mouse", "razer", 1_000_000);
    r.level = level;
    r.charging = charging;
    r.kind = "mouse".into();
    r
}
fn apply(
    e: &mut Engine,
    level: Option<u8>,
    charging: Option<bool>,
    quiet: bool,
) -> Vec<Notification> {
    e.apply("razer", Ok(vec![r("one", level, charging)]), 0.0, quiet)
}
fn drain(h: &mut Estimator, key: &str, start: f64, hours: f64, rate: f64) -> f64 {
    let n = (hours * 60.0) as usize;
    for i in 0..=n {
        let reading = r(
            key,
            Some((100.0 - rate * i as f64 / 60.0).round() as u8),
            Some(false),
        );
        h.record(&reading, start + i as f64 * 60.0);
    }
    start + (n + 1) as f64 * 60.0
}

#[test]
fn full_charge_observed_transition_stopping_charging_and_custom_name() {
    let mut e = engine();
    e.settings.devices.insert(
        "one".into(),
        DevicePreferences {
            name: Some("Work mouse".into()),
            ..Default::default()
        },
    );
    assert!(apply(&mut e, Some(97), Some(true), false).is_empty());
    let notes = apply(&mut e, Some(100), Some(false), false);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, NotificationKind::Full);
    assert_eq!(notes[0].text, "Work mouse is fully charged.");
    assert!(apply(&mut e, Some(100), Some(false), false).is_empty());
}
#[test]
fn full_charge_startup_and_unobserved_charging_do_not_alert() {
    for charging in [Some(true), Some(false), None] {
        let mut e = engine();
        assert!(apply(&mut e, Some(100), charging, false).is_empty());
        assert!(apply(&mut e, Some(99), Some(true), false).is_empty());
        assert!(apply(&mut e, Some(100), Some(true), false).is_empty());
    }
    let mut e = engine();
    apply(&mut e, Some(90), Some(false), false);
    assert!(apply(&mut e, Some(100), Some(false), false).is_empty());
}
#[test]
fn full_charge_unknown_and_sleep_leave_charge_cycle_armed() {
    let mut e = engine();
    apply(&mut e, Some(90), Some(true), false);
    let mut asleep = r("one", Some(90), Some(true));
    asleep.connection = Connection::Sleeping;
    e.apply("razer", Ok(vec![asleep]), 0.0, false);
    apply(&mut e, None, Some(true), false);
    assert_eq!(apply(&mut e, Some(100), Some(true), false).len(), 1);
}
#[test]
fn full_charge_jitter_and_next_charge_rearm() {
    let mut e = engine();
    let mut full = 0;
    for (level, charging) in [
        (95, true),
        (100, true),
        (99, true),
        (100, true),
        (95, true),
        (100, true),
        (94, true),
        (100, true),
        (80, false),
        (90, true),
        (100, true),
    ] {
        full += apply(&mut e, Some(level), Some(charging), false).len();
    }
    assert_eq!(full, 3);
}
#[test]
fn full_charge_setting_and_notify_setting_suppress() {
    for settings in [
        Settings {
            full_alert: false,
            ..Default::default()
        },
        Settings {
            notify: false,
            ..Default::default()
        },
    ] {
        let mut e = Engine::new(settings, Estimator::default());
        apply(&mut e, Some(90), Some(true), false);
        assert!(apply(&mut e, Some(100), Some(true), false).is_empty());
    }
}
#[test]
fn low_uses_device_threshold_default_and_off() {
    let mut e = engine();
    e.settings.low = 20;
    e.settings.devices.insert(
        "one".into(),
        DevicePreferences {
            low: Some(40),
            ..Default::default()
        },
    );
    assert!(apply(&mut e, Some(45), Some(false), false).is_empty());
    assert_eq!(apply(&mut e, Some(40), Some(false), false).len(), 1);
    let other = e.apply(
        "razer",
        Ok(vec![r("two", Some(40), Some(false))]),
        0.0,
        false,
    );
    assert!(other.is_empty());
    assert_eq!(
        e.apply(
            "razer",
            Ok(vec![r("two", Some(20), Some(false))]),
            0.0,
            false
        )
        .len(),
        1
    );
    e.settings.devices.entry("one".into()).or_default().low = Some(0);
    assert!(apply(&mut e, Some(0), Some(false), false).is_empty());
}
#[test]
fn changing_threshold_rearms_without_charge() {
    let mut e = engine();
    assert_eq!(apply(&mut e, Some(15), Some(false), false).len(), 1);
    e.settings.devices.entry("one".into()).or_default().low = Some(30);
    assert_eq!(apply(&mut e, Some(15), Some(false), false).len(), 1);
    assert!(apply(&mut e, Some(15), Some(false), false).is_empty());
    e.settings.devices.get_mut("one").unwrap().low = Some(0);
    assert!(apply(&mut e, Some(0), Some(false), false).is_empty());
    e.settings.devices.get_mut("one").unwrap().low = None;
    assert_eq!(apply(&mut e, Some(15), Some(false), false).len(), 1);
}
#[test]
fn low_boundary_hysteresis_requires_greater_than_five() {
    let mut e = engine();
    assert_eq!(apply(&mut e, Some(20), Some(false), false).len(), 1);
    for l in [19, 23, 25, 20] {
        assert!(apply(&mut e, Some(l), Some(false), false).is_empty());
    }
    apply(&mut e, Some(26), Some(false), false);
    assert_eq!(apply(&mut e, Some(20), Some(false), false).len(), 1);
    apply(&mut e, Some(10), Some(true), false);
    assert_eq!(apply(&mut e, Some(10), Some(false), false).len(), 1);
}
#[test]
fn notifications_use_shared_exact_and_coarse_text() {
    let mut e = engine();
    let exact = apply(&mut e, Some(15), Some(false), false);
    assert_eq!(exact[0].text, "Mouse: 15% left. Time to charge.");
    let mut e = engine();
    let mut coarse = r("one", Some(20), Some(false));
    coarse.precision = Precision::Coarse;
    let notes = e.apply("razer", Ok(vec![coarse]), 0.0, false);
    assert_eq!(notes[0].text, "Mouse: battery is low. Time to charge.");
}
#[test]
fn quiet_game_holds_once_per_device_and_kind() {
    let mut e = engine();
    assert!(apply(&mut e, Some(90), Some(true), true).is_empty());
    assert!(apply(&mut e, Some(100), Some(true), true).is_empty());
    assert!(apply(&mut e, Some(15), Some(false), true).is_empty());
    assert!(apply(&mut e, Some(12), Some(false), true).is_empty());
    let notes = e.flush_held();
    // A full-charge alert held during a game must expire once the same device
    // is observed discharging; only the current low warning remains relevant.
    assert_eq!(notes.len(), 1);
    assert!(notes.iter().any(|n| n.kind == NotificationKind::Low));
    assert!(!notes.iter().any(|n| n.kind == NotificationKind::Full));
    assert!(e.flush_held().is_empty());
}
#[test]
fn quiet_disabled_delivers_during_game() {
    let mut e = engine();
    e.settings.quiet_fullscreen = false;
    assert_eq!(apply(&mut e, Some(15), Some(false), true).len(), 1);
}
#[test]
fn held_low_drops_on_charge_recovery_hidden_or_threshold_off() {
    for change in 0..4 {
        let mut e = engine();
        apply(&mut e, Some(15), Some(false), true);
        match change {
            0 => {
                apply(&mut e, Some(16), Some(true), true);
            }
            1 => {
                apply(&mut e, Some(21), Some(false), true);
            }
            2 => e.settings.devices.entry("one".into()).or_default().hidden = true,
            _ => e.settings.devices.entry("one".into()).or_default().low = Some(0),
        }
        assert!(e.flush_held().is_empty());
    }
}
#[test]
fn held_notifications_drop_when_disabled_or_disconnected() {
    for change in 0..3 {
        let mut e = engine();
        apply(&mut e, Some(15), Some(false), true);
        match change {
            0 => e.settings.notify = false,
            1 => {
                let mut settings = e.settings.clone();
                settings.disabled_providers.insert("razer".into());
                e.update_settings(settings);
            }
            _ => {
                e.apply("razer", Ok(vec![]), 0.0, true);
                e.apply("razer", Ok(vec![]), 0.0, true);
            }
        }
        assert!(e.flush_held().is_empty());
    }
}
#[test]
fn held_full_drops_when_setting_disabled() {
    let mut e = engine();
    apply(&mut e, Some(90), Some(true), true);
    apply(&mut e, Some(100), Some(true), true);
    e.settings.full_alert = false;
    assert!(e.flush_held().is_empty());
}
#[test]
fn notification_failure_retries_bounded_and_still_relevant() {
    let mut e = engine();
    let note = apply(&mut e, Some(15), Some(false), false).pop().unwrap();
    for _ in 0..5 {
        e.notification_failed(note.clone());
    }
    let retry = e.flush_held();
    assert_eq!(retry.len(), 1);
    e.notification_failed(retry[0].clone());
    apply(&mut e, Some(30), Some(true), true);
    assert!(e.flush_held().is_empty());
}
#[test]
fn suspended_and_resumed_engine_excludes_sleep_and_waits_for_fresh_data() {
    let mut e = engine();
    e.apply(
        "razer",
        Ok(vec![r("one", Some(90), Some(false))]),
        0.0,
        false,
    );
    e.apply(
        "razer",
        Ok(vec![r("one", Some(89), Some(false))]),
        60.0,
        false,
    );
    let usage = e.estimator.devices["one"].usage;
    e.suspend();
    assert_eq!(e.readings()[0].connection, Connection::Sleeping);
    assert!(apply(&mut e, Some(15), Some(false), false).is_empty());
    e.resume();
    assert_eq!(e.readings()[0].connection, Connection::Stale);
    e.apply(
        "razer",
        Ok(vec![r("one", Some(88), Some(false))]),
        36_000.0,
        false,
    );
    assert_eq!(e.estimator.devices["one"].usage, usage);
}
#[test]
fn successful_absence_pauses_history_and_confirmed_disconnect_rearms_alert() {
    let mut e = engine();
    e.apply(
        "razer",
        Ok(vec![r("one", Some(15), Some(false))]),
        0.0,
        false,
    );
    e.apply("razer", Ok(vec![]), 60.0, false);
    assert_eq!(e.readings()[0].connection, Connection::Stale);
    e.apply("razer", Ok(vec![]), 120.0, false);
    assert!(e.readings().is_empty());
    assert_eq!(
        e.apply(
            "razer",
            Ok(vec![r("one", Some(14), Some(false))]),
            36_000.0,
            false
        )
        .len(),
        1
    );
    assert_eq!(e.estimator.devices["one"].usage, 0.0);
}
#[test]
fn provider_error_retains_stale_readings_and_error_until_success() {
    let mut e = engine();
    apply(&mut e, Some(75), Some(false), false);
    for _ in 0..3 {
        e.apply("razer", Err(ProviderError::new("busy")), 60.0, false);
    }
    assert_eq!(e.snapshot(100).errors["razer"], "busy");
    assert_eq!(e.readings()[0].connection, Connection::Stale);
    e.apply(
        "razer",
        Ok(vec![r("one", Some(74), Some(false))]),
        36_000.0,
        false,
    );
    assert_eq!(e.estimator.devices["one"].usage, 0.0);
    assert!(e.snapshot(100).errors.is_empty());
}
#[test]
fn polling_other_provider_does_not_advance_old_history() {
    let mut e = engine();
    e.apply(
        "razer",
        Ok(vec![r("one", Some(90), Some(false))]),
        0.0,
        false,
    );
    let mut other = r("two", Some(80), Some(false));
    other.source = "logitech".into();
    e.apply("logitech", Ok(vec![other]), 600.0, false);
    assert_eq!(e.estimator.devices["one"].usage, 0.0);
}
#[test]
fn disabled_provider_and_hidden_device_do_not_alert_or_learn() {
    let mut e = engine();
    e.settings.devices.entry("one".into()).or_default().hidden = true;
    assert!(apply(&mut e, Some(5), Some(false), false).is_empty());
    assert!(e.estimator.devices.is_empty());
    assert!(e.snapshot(0).devices[0].hidden);
    let mut settings = e.settings.clone();
    settings.disabled_providers.insert("razer".into());
    e.update_settings(settings);
    assert!(e.readings().is_empty());
    assert!(apply(&mut e, Some(5), Some(false), false).is_empty());
}
#[test]
fn unhide_restores_name_icon_and_alert_without_hidden_time() {
    let mut e = engine();
    e.settings.devices.insert(
        "one".into(),
        DevicePreferences {
            name: Some("Work mouse".into()),
            hidden: true,
            icon: Some("keyboard".into()),
            low: Some(30),
            ..Default::default()
        },
    );
    apply(&mut e, Some(29), Some(false), false);
    e.settings.devices.get_mut("one").unwrap().hidden = false;
    let notes = apply(&mut e, Some(29), Some(false), false);
    assert_eq!(notes[0].text, "Work mouse: 29% left. Time to charge.");
    let view = &e.snapshot(0).devices[0];
    assert_eq!(view.name, "Work mouse");
    assert_eq!(view.icon, "keyboard");
    assert_eq!(view.low_alert_at, 30);
    e.settings.devices.get_mut("one").unwrap().name = None;
    e.settings.devices.get_mut("one").unwrap().icon = None;
    assert_eq!(e.snapshot(0).devices[0].name, "Mouse");
    assert_eq!(e.snapshot(0).devices[0].icon, "mouse");
}
#[test]
fn rename_changes_text_keeps_detected_kind_and_other_device_icon() {
    let mut e = engine();
    e.settings.devices.insert(
        "one".into(),
        DevicePreferences {
            name: Some("Headset name".into()),
            icon: Some("headset".into()),
            ..Default::default()
        },
    );
    e.apply(
        "razer",
        Ok(vec![
            r("one", Some(70), Some(false)),
            r("two", Some(60), Some(false)),
        ]),
        0.0,
        false,
    );
    let snapshot = e.snapshot(0);
    assert!(snapshot.devices[0].text.starts_with("Headset name: 70%"));
    assert_eq!(snapshot.devices[0].reading.kind, "mouse");
    assert_eq!(snapshot.devices[1].icon, "mouse");
}
#[test]
fn estimator_steady_drain_minimum_span_and_drop() {
    let mut h = Estimator::default();
    drain(&mut h, "one", 0.0, 3.0, 5.0);
    let hours = h.seconds_left("one", Some(85)).unwrap() / 3600.0;
    assert!((hours - 17.0).abs() < 1.0);
    for (hours, rate, level) in [(0.25, 20.0, 95), (2.0, 1.0, 98)] {
        let mut h = Estimator::default();
        drain(&mut h, "one", 0.0, hours, rate);
        assert_eq!(h.seconds_left("one", Some(level)), None);
    }
    assert_eq!(h.seconds_left("one", None), None);
}
#[test]
fn estimator_charge_rise_reset_and_small_jitter_ignored() {
    let mut h = Estimator::default();
    let t = drain(&mut h, "one", 0.0, 3.0, 5.0);
    h.record(&r("one", Some(85), Some(true)), t);
    assert!(h.devices["one"].samples.is_empty());
    let t = drain(&mut h, "one", t + 60.0, 3.0, 5.0);
    h.record(&r("one", Some(95), Some(false)), t);
    assert_eq!(h.devices["one"].samples.len(), 1);
    assert_eq!(h.devices["one"].usage, 0.0);
    let mut h = Estimator::default();
    for (i, level) in [80, 81, 80, 79, 80, 79].into_iter().enumerate() {
        h.record(&r("one", Some(level), Some(false)), i as f64 * 60.0);
    }
    assert_eq!(
        h.devices["one"]
            .samples
            .iter()
            .map(|s| s.1)
            .collect::<Vec<_>>(),
        vec![80, 79]
    );
}
#[test]
fn estimator_sleep_long_gap_unknown_coarse_and_backward_clock() {
    let mut h = Estimator::default();
    h.record(&r("one", Some(90), Some(false)), 0.0);
    h.record(&r("one", Some(89), Some(false)), 28_800.0);
    // An eight-hour observation gap does not establish any awake time.
    assert_eq!(h.devices["one"].usage, 0.0);
    let usage = h.devices["one"].usage;
    let mut sleeping = r("one", Some(89), Some(false));
    sleeping.connection = Connection::Sleeping;
    h.record(&sleeping, 29_000.0);
    h.record(&r("one", Some(88), Some(false)), 60_000.0);
    assert_eq!(h.devices["one"].usage, usage);
    h.record(&r("one", None, Some(false)), 60_001.0);
    h.record(&r("one", Some(87), Some(false)), 70_000.0);
    assert_eq!(h.devices["one"].usage, usage);
    h.record(&r("one", Some(87), Some(false)), 1.0);
    assert_eq!(h.devices["one"].usage, usage);
    let mut h = Estimator::default();
    let mut coarse = r("one", Some(50), Some(false));
    coarse.precision = Precision::Coarse;
    h.record(&coarse, 0.0);
    h.record(&r("two", None, Some(false)), 0.0);
    assert!(h.devices.is_empty());
}
#[test]
fn estimator_stalled_level_slows_fit_and_samples_are_bounded() {
    let mut h = Estimator::default();
    let t = drain(&mut h, "one", 0.0, 3.0, 5.0);
    let fast = h.seconds_left("one", Some(85)).unwrap();
    for i in 0..180 {
        h.record(&r("one", Some(85), Some(false)), t + i as f64 * 60.0);
    }
    assert!(h.seconds_left("one", Some(85)).unwrap() > fast);
}
#[test]
fn estimator_roundtrip_damaged_entries_expiry_and_baseline_reset() {
    let mut h = Estimator::default();
    let t = drain(&mut h, "one", 0.0, 3.0, 5.0);
    let value = serde_json::to_value(&h).unwrap();
    let mut loaded = Estimator::from_value(value, 1_000_100);
    assert_eq!(
        loaded.seconds_left("one", Some(85)),
        h.seconds_left("one", Some(85))
    );
    let usage = loaded.devices["one"].usage;
    loaded.record(&r("one", Some(85), Some(false)), t + 100_000.0);
    assert_eq!(loaded.devices["one"].usage, usage);
    let parsed = Estimator::from_value(
        json!({"devices":{"good":{"usage":60.0,"samples":[[0.0,80],[60.0,79]],"seen":1_000_000},"bad":{"samples":[["x",1]]},"bad_level":{"usage":1.0,"samples":[[0.0,255]]},"expired":{"seen":1,"usage":1.0,"samples":[]}}}),
        6_000_000,
    );
    assert_eq!(parsed.devices.len(), 1);
    assert!(parsed.devices.contains_key("good"));
    assert!(Estimator::from_value(json!([]), 1).devices.is_empty());
}
#[test]
fn estimate_format_hours_days_and_less_than_hour() {
    assert_eq!(format_left(1800.0), "less than 1 h of use left");
    assert_eq!(format_left(5.4 * 3600.0), "about 5 h of use left");
    assert_eq!(format_left(72.0 * 3600.0), "about 3 days of use left");
}
#[test]
fn snapshot_estimate_only_awake_exact_battery_and_enabled() {
    let mut e = engine();
    for i in 0..=180 {
        let level = (100.0 - 5.0 * i as f64 / 60.0).round() as u8;
        e.apply(
            "razer",
            Ok(vec![r("one", Some(level), Some(false))]),
            i as f64 * 60.0,
            false,
        );
    }
    assert!(e.snapshot(0).devices[0].text.contains("h of use left"));
    let learned = e.estimator.clone();
    for change in 0..5 {
        let mut e = Engine::new(Settings::default(), learned.clone());
        // Every branch starts with a real learned rate; a previous charging
        // branch must not empty history and make later display assertions vacuous.
        e.apply(
            "razer",
            Ok(vec![r("one", Some(85), Some(false))]),
            10_800.0,
            false,
        );
        assert!(
            e.snapshot(0).devices[0].seconds_left.is_some(),
            "baseline for condition {change}"
        );
        let mut current = r("one", Some(85), Some(false));
        match change {
            0 => current.charging = Some(true),
            1 => current.connection = Connection::Sleeping,
            2 => current.precision = Precision::Coarse,
            3 => current.level = None,
            _ => e.settings.time_left = false,
        }
        e.apply("razer", Ok(vec![current]), 11_000.0, false);
        assert_eq!(e.snapshot(0).devices[0].seconds_left, None);
        if change != 0 {
            assert!(
                e.estimator.seconds_left("one", Some(85)).is_some(),
                "condition {change} must preserve the learned rate"
            );
        }
    }
}
#[test]
fn settings_independent_device_fields_and_unicode_cap() {
    let s=Settings::from_value(json!({"devices":{"good":{"name":"Мышь","hidden":true,"low":40,"icon":"headset"},"partial":{"name":"Keep","hidden":"yes","low":-1,"icon":"rocket"},"broken":5,"long":{"name":"🐭".repeat(121)}}})).unwrap();
    assert_eq!(s.devices["good"].name.as_deref(), Some("Мышь"));
    assert!(s.devices["good"].hidden);
    assert_eq!(s.low_for("good"), 40);
    assert_eq!(s.devices["partial"].name.as_deref(), Some("Keep"));
    assert!(!s.devices["partial"].hidden);
    assert_eq!(s.low_for("partial"), 20);
    assert!(s.devices["partial"].icon.is_none());
    assert!(!s.devices.contains_key("broken"));
    assert_eq!(
        s.devices["long"].name.as_ref().unwrap().chars().count(),
        120
    );
}
#[test]
fn settings_invalid_low_values_and_container_types_fall_back() {
    for value in [
        json!("40"),
        json!(true),
        json!(150),
        json!(-1),
        json!(null),
        json!(12.5),
    ] {
        let s =
            Settings::from_value(json!({"devices":{"one":{"low":value,"name":"Keep"}}})).unwrap();
        assert_eq!(s.low_for("one"), 20);
        assert_eq!(s.devices["one"].name.as_deref(), Some("Keep"));
    }
    for value in [json!("razer"), json!(5), json!(null), json!({"razer":1})] {
        let s = Settings::from_value(json!({"devices":value,"disabled_providers":value})).unwrap();
        assert!(s.devices.is_empty());
        assert!(s.enabled("razer"));
    }
}
#[test]
fn settings_defaults_individual_validation_and_update_repository() {
    let defaults = Settings::from_value(json!({})).unwrap();
    assert_eq!(defaults.interval, 60);
    assert!(!defaults.percent_in_icon);
    assert!(defaults.quiet_fullscreen);
    assert!(!defaults.update_check);
    for repository in [
        json!(null),
        json!("https://github.com/owner/repo"),
        json!("../repo"),
        json!("owner/repo/sub"),
        json!("owner/"),
    ] {
        let s=Settings::from_value(json!({"release_repository":repository,"update_check":true,"interval":0,"low":15,"badges":false,"bluetooth":1})).unwrap();
        assert!(!s.update_check);
        assert!(s.release_repository.is_none());
        assert_eq!(s.interval, 60);
        assert_eq!(s.low, 15);
        assert!(!s.badges);
        assert!(s.bluetooth);
    }
    let configured = Settings::from_value(
        json!({"release_repository":"owner/HaloBatteryNext","update_check":true}),
    )
    .unwrap();
    assert!(configured.update_check);
    assert!(Settings::from_value(json!([])).is_err());
}
#[test]
fn bluetooth_dedup_requires_identity_and_live_known_hid() {
    for (level, connection, should_drop) in [
        (Some(75), Connection::Online, true),
        (None, Connection::Online, false),
        (Some(75), Connection::Stale, false),
        (Some(75), Connection::Sleeping, false),
    ] {
        let mut e = engine();
        let mut hid = r("hid", level, Some(false));
        hid.connection = connection;
        hid.container = Some("physical-one".into());
        let mut bt = r("bt", Some(80), Some(false));
        bt.source = "bluetooth".into();
        bt.container = Some("physical-one".into());
        e.apply("razer", Ok(vec![hid]), 0.0, false);
        e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
        assert_eq!(e.readings().iter().any(|r| r.key == "bt"), !should_drop);
    }
}
#[test]
fn ambiguous_names_zero_and_placeholder_identities_stay_separate() {
    for identity in [
        None,
        Some(""),
        Some("00000000-0000-0000-0000-000000000000"),
        Some("unknown"),
        Some("n/a"),
    ] {
        let mut e = engine();
        let mut hid = r("hid", Some(70), Some(false));
        hid.serial = identity.map(str::to_owned);
        hid.container = identity.map(str::to_owned);
        let mut bt = hid.clone();
        bt.key = "bt".into();
        bt.source = "bluetooth".into();
        e.apply("razer", Ok(vec![hid]), 0.0, false);
        e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
        assert_eq!(e.readings().len(), 2, "{identity:?}");
    }
}
#[test]
fn controller_bluetooth_preferred_only_with_proven_live_identity() {
    for live in [true, false] {
        let mut e = engine();
        let mut controller = r("xinput:0", Some(55), Some(false));
        controller.source = "xinput".into();
        controller.kind = "gamepad".into();
        controller.via = "bluetooth".into();
        controller.serial = Some("AABBCCDDEEFF".into());
        let mut bt = controller.clone();
        bt.key = "bt".into();
        bt.source = "bluetooth".into();
        bt.level = Some(82);
        if !live {
            bt.connection = Connection::Stale;
        }
        e.apply("xinput", Ok(vec![controller]), 0.0, false);
        e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
        assert_eq!(e.readings().iter().any(|r| r.key == "xinput:0"), !live);
    }
}
#[test]
fn same_key_prefers_live_exact_known_usb_not_unknown_usb() {
    let mut e = engine();
    let mut bt = r("one", Some(80), Some(false));
    bt.via = "bluetooth".into();
    let mut usb = bt.clone();
    usb.via = "usb".into();
    usb.level = None;
    e.apply("razer", Ok(vec![bt.clone(), usb.clone()]), 0.0, false);
    assert_eq!(e.readings()[0].level, Some(80));
    usb.level = Some(75);
    e.apply("razer", Ok(vec![bt, usb]), 0.0, false);
    assert_eq!(e.readings()[0].via, "usb");
    assert_eq!(e.readings()[0].level, Some(75));
}
#[test]
fn snapshot_serializes_status_precision_inference_and_stale_semantics() {
    let mut e = engine();
    let mut reading = r("one", Some(55), Some(true));
    reading.precision = Precision::Coarse;
    reading.charging_inferred = true;
    reading.approx = Some("About 55% (Medium)".into());
    reading.connection = Connection::Sleeping;
    e.apply("razer", Ok(vec![reading]), 0.0, false);
    let value = serde_json::to_value(e.snapshot(42)).unwrap();
    let device = &value["devices"][0];
    assert_eq!(value["timestamp"], 42);
    assert_eq!(device["reading"]["precision"], "coarse");
    assert_eq!(device["reading"]["connection"], "sleeping");
    assert_eq!(device["reading"]["charging_inferred"], true);
    assert!(device["text"].as_str().unwrap().contains("asleep"));
    assert!(device["seconds_left"].is_null());
}
#[test]
fn invalid_battery_or_nonfinite_clock_never_creates_alert_or_estimate() {
    let mut e = engine();
    assert!(apply(&mut e, Some(255), Some(false), false).is_empty());
    assert!(e.readings()[0].level.is_none());
    let mut h = Estimator::default();
    h.record(&r("one", Some(90), Some(false)), f64::NAN);
    assert!(h.devices.is_empty());
}
#[test]
fn hidden_missing_device_and_disabled_errors_are_cleaned() {
    let mut e = engine();
    e.settings.devices.entry("one".into()).or_default().hidden = true;
    apply(&mut e, Some(75), Some(false), false);
    e.apply("razer", Ok(vec![]), 0.0, false);
    e.apply("razer", Ok(vec![]), 0.0, false);
    assert!(e.snapshot(0).devices.is_empty());
    e.apply("razer", Err(ProviderError::new("busy")), 0.0, false);
    assert!(!e.snapshot(0).errors.is_empty());
    let mut settings = e.settings.clone();
    settings.disabled_providers.insert("razer".into());
    e.update_settings(settings);
    assert!(e.snapshot(0).errors.is_empty());
}
#[test]
fn damaged_icon_values_and_bluetooth_override_keep_automatic_fallback() {
    for value in [json!("rocket"), json!(5), json!(null), json!(["mouse"])] {
        let settings = Settings::from_value(json!({"devices":{"one":{"icon":value}}})).unwrap();
        let mut e = Engine::new(settings, Estimator::default());
        apply(&mut e, Some(80), Some(false), false);
        assert_eq!(e.snapshot(0).devices[0].icon, "mouse");
    }
    let mut e = engine();
    let mut bt = r("one", Some(70), Some(false));
    bt.source = "bluetooth".into();
    bt.kind = String::new();
    e.apply("bluetooth", Ok(vec![bt]), 0.0, false);
    assert_eq!(e.snapshot(0).devices[0].icon, "bluetooth");
    e.settings.devices.entry("one".into()).or_default().icon = Some("gamepad".into());
    assert_eq!(e.snapshot(0).devices[0].icon, "gamepad");
}
#[test]
fn pending_notification_survives_read_error_then_refreshes_name_and_level() {
    let mut e = engine();
    let note = apply(&mut e, Some(15), Some(false), false).pop().unwrap();
    e.notification_failed(note);
    e.apply("razer", Err(ProviderError::new("busy")), 60.0, false);
    assert!(e.flush_held().is_empty());
    e.settings.devices.entry("one".into()).or_default().name = Some("Work mouse".into());
    let retry = apply(&mut e, Some(12), Some(false), false);
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].text, "Work mouse: 12% left. Time to charge.");
    assert!(e.flush_held().is_empty());
}
