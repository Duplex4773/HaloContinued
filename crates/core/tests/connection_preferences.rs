use hb_core::*;

fn device(pid: &str, serial: &str) -> ConfigurationDevice {
    let mut r = Reading::new(format!("razer:{pid}:{serial}"), "Mouse", "razer", 100);
    r.kind = "mouse".into();
    r.via = "usb".into();
    r.serial = Some(serial.into());
    ConfigurationDevice::from_reading(&r)
}

#[test]
fn preferences_follow_the_unit_without_changing_hardware_identity() {
    let radio = device("00bf", "TESTUNIT1234");
    let wired = device("00be", "TESTUNIT1234");
    let mut s = Settings::default();
    s.device_mut(&radio.key).name = Some("My mouse".into());
    s.device_mut(&radio.key).low = Some(25);
    s.device_mut(&radio.key).requested_polling_rate = Some(8000.try_into().unwrap());
    s.link_device(&wired);
    assert_eq!(
        s.device(&wired.key).unwrap().name.as_deref(),
        Some("My mouse")
    );
    assert_eq!(s.low_for(&wired.key), 25);
    assert_eq!(
        s.device(&wired.key)
            .unwrap()
            .requested_polling_rate
            .unwrap()
            .hz(),
        8000
    );
    assert_ne!(wired.key, radio.key);
    assert!(wired.wired_mouse());
    assert!(!radio.wired_mouse());
    s.device_mut(&wired.key).hidden = true;
    assert!(s.device(&radio.key).unwrap().hidden);
    let mut restored = Settings::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert!(restored.device(&wired.key).unwrap().hidden);
    restored.reset_device(&wired.key);
    assert!(restored.device(&radio.key).is_none());
    assert!(restored.device(&wired.key).is_none());
}

#[test]
fn existing_wireless_preferences_win_and_wired_only_preferences_migrate() {
    let radio = device("00bf", "TESTUNIT1234");
    let wired = device("00be", "TESTUNIT1234");
    let mut s = Settings::default();
    s.device_mut(&wired.key).low = Some(15);
    s.link_device(&wired);
    assert_eq!(s.low_for(&radio.key), 15);
    assert!(!s.devices.contains_key(&wired.key));
    s.devices.entry(wired.key.clone()).or_default().low = Some(50);
    s.device_mut(&radio.key).low = Some(30);
    s.link_device(&wired);
    assert_eq!(s.low_for(&wired.key), 30);
    s.reset_device(&radio.key);
    s.link_device(&wired);
    assert!(s.device(&wired.key).is_none());
}

#[test]
fn never_link_by_name_container_only_or_unproven_serial() {
    let mut s = Settings::default();
    let radio = device("00bf", "TESTUNIT1234");
    s.device_mut(&radio.key).low = Some(35);
    for mut wired in [
        device("00be", "OTHERUNIT1234"),
        device("00b6", "TESTUNIT1234"),
        device("9999", "TESTUNIT1234"),
    ] {
        wired.name = radio.name.clone();
        wired.container = radio.container.clone();
        s.link_device(&wired);
        assert_eq!(s.low_for(&wired.key), s.low);
    }
    let mut wired = device("00be", "TESTUNIT1234");
    wired.serial = None;
    s.link_device(&wired);
    assert_eq!(s.low_for(&wired.key), s.low);
    wired.serial = Some("DIFFERENTUNIT".into());
    s.link_device(&wired);
    assert_eq!(s.low_for(&wired.key), s.low);
    for serial in ["", "UNKNOWN", "000000", "{container}", "path&interface"] {
        assert!(device("00be", serial).shared_preference_key().is_none());
    }
}

#[test]
fn malformed_persisted_links_cannot_redirect_unrelated_settings() {
    let s = Settings::from_value(serde_json::json!({"device_links": {
        "razer:00be:TESTUNIT1234": "razer:00bf:DIFFERENTUNIT",
        "unrelated": "razer:00bf:TESTUNIT1234",
        "razer:00bf:TESTUNIT1234": "razer:00be:TESTUNIT1234"
    }}))
    .unwrap();
    assert!(s.device_links.is_empty());
}

#[test]
fn engine_uses_shared_alert_and_display_preferences_on_first_wired_read() {
    let radio = device("00bf", "TESTUNIT1234");
    let mut settings = Settings::default();
    settings.device_mut(&radio.key).name = Some("My mouse".into());
    settings.device_mut(&radio.key).low = Some(30);
    let mut engine = Engine::new(settings, Estimator::default());
    let mut reading = Reading::new("razer:00be:TESTUNIT1234", "Wired mouse", "razer", 100);
    reading.kind = "mouse".into();
    reading.via = "usb".into();
    reading.serial = Some("TESTUNIT1234".into());
    reading.level = Some(80);
    engine.apply("razer", Ok(vec![reading]), 0.0, false);
    let view = engine.snapshot(100).devices.remove(0);
    assert_eq!(view.name, "My mouse");
    assert_eq!(view.low_alert_at, 30);
    assert_eq!(view.reading.key, "razer:00be:TESTUNIT1234");
}
