use hb_core::*;

fn engine() -> Engine {
    Engine::new(
        Settings {
            low_sound: true,
            ..Settings::default()
        },
        Estimator::default(),
    )
}
fn reading(level: Option<u8>) -> Reading {
    let mut r = Reading::new("mouse", "Mouse", "razer", 0);
    r.level = level;
    r.charging = Some(false);
    r
}
fn poll(e: &mut Engine, r: Reading, time: f64) -> Option<u8> {
    e.apply("razer", Ok(vec![r]), time, false);
    e.take_low_battery_sound()
}

#[test]
fn old_and_invalid_settings_default_off_and_enabled_settings_round_trip() {
    for value in [
        serde_json::json!({}),
        serde_json::json!({"low_sound":"yes"}),
    ] {
        assert!(!Settings::from_value(value).unwrap().low_sound);
    }
    let s = Settings::from_value(serde_json::json!({"low_sound":true})).unwrap();
    assert!(
        Settings::from_value(serde_json::to_value(s).unwrap())
            .unwrap()
            .low_sound
    );
    let mut e = Engine::new(Settings::default(), Estimator::default());
    assert_eq!(
        e.apply("razer", Ok(vec![reading(Some(15))]), 0.0, false)
            .len(),
        1
    );
    assert_eq!(e.take_low_battery_sound(), None);
}

#[test]
fn repeats_only_after_five_minutes_and_keeps_the_toast_once() {
    let mut e = engine();
    for (time, level, sound, notes) in [
        (0.0, 20, Some(20), 1),
        (299.0, 6, None, 0),
        (300.0, 5, Some(5), 0),
        (600.0, 0, Some(0), 0),
    ] {
        assert_eq!(
            e.apply("razer", Ok(vec![reading(Some(level))]), time, false)
                .len(),
            notes
        );
        assert_eq!(e.take_low_battery_sound(), sound);
        assert_eq!(e.take_low_battery_sound(), None);
    }
}

#[test]
fn quiet_holds_the_toast_but_sound_is_independent_of_popup_preferences() {
    let mut e = engine();
    assert!(
        e.apply("razer", Ok(vec![reading(Some(10))]), 0.0, true)
            .is_empty()
    );
    assert_eq!(e.take_low_battery_sound(), Some(10));
    assert_eq!(e.flush_held().len(), 1);
    e.settings.notify = false;
    assert_eq!(poll(&mut e, reading(Some(10)), 300.0), Some(10));
}

#[test]
fn unknown_sleep_stale_charging_and_recovered_readings_do_not_sound() {
    let mut e = engine();
    assert_eq!(poll(&mut e, reading(Some(15)), 0.0), Some(15));
    assert_eq!(poll(&mut e, reading(None), 300.0), None);
    for connection in [Connection::Sleeping, Connection::Stale] {
        let mut r = reading(Some(10));
        r.connection = connection;
        assert_eq!(poll(&mut e, r, 400.0), None);
    }
    let mut charging = reading(Some(15));
    charging.charging = Some(true);
    assert_eq!(poll(&mut e, charging, 401.0), None);
    assert_eq!(poll(&mut e, reading(Some(15)), 402.0), Some(15));
    assert_eq!(poll(&mut e, reading(Some(21)), 403.0), None);
    assert_eq!(poll(&mut e, reading(Some(20)), 404.0), Some(20));
}

#[test]
fn per_device_threshold_changes_disable_hide_and_reenable_are_respected() {
    let mut e = engine();
    assert_eq!(poll(&mut e, reading(Some(25)), 0.0), None);
    let mut settings = e.settings.clone();
    settings.devices.insert(
        "mouse".into(),
        DevicePreferences {
            low: Some(30),
            ..Default::default()
        },
    );
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(25)), 1.0), Some(25));
    let mut settings = e.settings.clone();
    settings.devices.get_mut("mouse").unwrap().low = Some(0);
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(0)), 400.0), None);
    let mut settings = e.settings.clone();
    settings.devices.get_mut("mouse").unwrap().low = Some(30);
    settings.devices.get_mut("mouse").unwrap().hidden = true;
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(10)), 401.0), None);
    let mut settings = e.settings.clone();
    settings.devices.get_mut("mouse").unwrap().hidden = false;
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(10)), 402.0), Some(10));
    let mut settings = e.settings.clone();
    settings.low_sound = false;
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(10)), 900.0), None);
    let mut settings = e.settings.clone();
    settings.low_sound = true;
    e.update_settings(settings);
    assert_eq!(poll(&mut e, reading(Some(10)), 901.0), Some(10));
}

#[test]
fn another_async_provider_cannot_replay_a_cached_sound_and_identity_aliases_deduplicate() {
    let mut e = engine();
    let mut r = reading(Some(10));
    r.serial = Some("Unit123".into());
    assert_eq!(poll(&mut e, r.clone(), 0.0), Some(10));
    e.apply("other", Ok(vec![]), 300.0, false);
    assert_eq!(e.take_low_battery_sound(), None);
    let mut alias = r.clone();
    alias.key = "bluetooth:mouse".into();
    alias.source = "bluetooth".into();
    alias.serial = Some("unit123".into());
    e.apply("bluetooth", Ok(vec![alias]), 300.0, false);
    assert_eq!(e.take_low_battery_sound(), None);
    assert_eq!(poll(&mut e, r, 300.0), Some(10));
}

#[test]
fn a_bluetooth_takeover_shares_the_cooldown_when_only_its_container_is_available() {
    let mut e = engine();
    let mut wired = reading(Some(10));
    wired.serial = Some("unit".into());
    wired.container = Some("container".into());
    assert_eq!(poll(&mut e, wired, 0.0), Some(10));
    e.apply("razer", Err(ProviderError::new("busy")), 10.0, false);
    let mut bluetooth = reading(Some(10));
    bluetooth.key = "bluetooth:mouse".into();
    bluetooth.source = "bluetooth".into();
    bluetooth.container = Some("CONTAINER".into());
    e.apply("bluetooth", Ok(vec![bluetooth.clone()]), 10.0, false);
    assert_eq!(e.take_low_battery_sound(), None);
    e.apply("bluetooth", Ok(vec![bluetooth.clone()]), 300.0, false);
    assert_eq!(e.take_low_battery_sound(), Some(10));
    bluetooth.charging = Some(true);
    e.apply("bluetooth", Ok(vec![bluetooth]), 301.0, false);
    assert_eq!(e.take_low_battery_sound(), None);
    let mut wired = reading(Some(10));
    wired.serial = Some("unit".into());
    wired.container = Some("container".into());
    assert_eq!(poll(&mut e, wired, 302.0), Some(10));
}

#[test]
fn suspend_resume_clock_reset_and_confirmed_reconnect_require_fresh_readings() {
    let mut e = engine();
    assert_eq!(poll(&mut e, reading(Some(10)), 1000.0), Some(10));
    e.suspend();
    assert_eq!(poll(&mut e, reading(Some(10)), 1400.0), None);
    e.resume();
    e.apply("other", Ok(vec![]), 1400.0, false);
    assert_eq!(e.take_low_battery_sound(), None);
    assert_eq!(poll(&mut e, reading(Some(10)), 1400.0), Some(10));
    assert_eq!(poll(&mut e, reading(Some(10)), 10.0), None);
    assert_eq!(poll(&mut e, reading(Some(10)), 309.0), None);
    assert_eq!(poll(&mut e, reading(Some(10)), 310.0), Some(10));
    for _ in 0..2 {
        e.apply("razer", Ok(vec![]), 311.0, false);
    }
    assert_eq!(poll(&mut e, reading(Some(10)), 312.0), Some(10));
}
