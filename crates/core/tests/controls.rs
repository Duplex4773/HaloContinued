use hb_core::*;
#[test]
fn polling_is_opt_in_and_saved_intent_is_validated_independently() {
    assert!(!Settings::default().polling_controls);
    let settings = Settings::from_value(serde_json::json!({
        "polling_controls": true, "devices": {
            "a": {"requested_polling_rate": 8000},
            "b": {"requested_polling_rate": 7999, "low": 15}
        }
    }))
    .unwrap();
    assert!(settings.polling_controls);
    assert_eq!(
        settings.devices["a"].requested_polling_rate.unwrap().hz(),
        8000
    );
    assert!(settings.devices["b"].requested_polling_rate.is_none());
    assert_eq!(settings.devices["b"].low, Some(15));
    assert!(
        !Settings::from_value(serde_json::json!({"polling_controls":"yes"}))
            .unwrap()
            .polling_controls
    );
}
#[test]
fn rate_type_rejects_arbitrary_frequencies() {
    for hz in [125, 250, 500, 1000, 2000, 4000, 8000] {
        assert_eq!(PollingRate::try_from(hz).unwrap().hz(), hz);
    }
    for hz in [0, 1, 120, 999, 7999, 16000, u32::MAX] {
        assert!(PollingRate::try_from(hz).is_err());
    }
}
#[test]
fn only_verified_change_resets_learning() {
    let request = ControlRequest {
        request: 1,
        target: ControlTarget {
            device: ConfigurationDevice::from_reading(&Reading::new("a", "Mouse", "razer", 10)),
            generation: 2,
            connection_stamp: None,
        },
        action: ControlAction::Read,
    };
    let mut result = ControlOutcome::failed(&request, "busy");
    result.may_have_changed = true;
    result.previous = PollingRate::try_from(1000).ok();
    assert!(!result.confirmed_change());
    result.failure = None;
    assert!(!result.confirmed_change());
    result.observation = Some(PollingObservation {
        target: request.target,
        supported: vec![],
        rate: PollingRate::try_from(8000).ok(),
        timestamp: 11,
        evidence: String::new(),
    });
    assert!(result.confirmed_change());
    result.failure = Some("second exchange failed".into());
    assert!(!result.confirmed_change());
}
#[test]
fn changed_rate_clears_proven_aliases_and_preserves_unrelated_receiver_devices() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    let mut a = Reading::new("a", "Same name", "razer", 10);
    a.serial = Some("UNIT1234".into());
    a.container = Some("receiver".into());
    let mut b = a.clone();
    b.key = "b".into();
    b.source = "bluetooth".into();
    let mut c = a.clone();
    c.key = "c".into();
    c.serial = Some("OTHERUNIT".into());
    engine.apply("razer", Ok(vec![a, c]), 0.0, false);
    engine.apply("bluetooth", Ok(vec![b]), 0.0, false);
    for key in ["a", "b", "c"] {
        engine
            .estimator
            .devices
            .insert(key.into(), Default::default());
    }
    engine.reset_estimate("a");
    assert!(!engine.estimator.devices.contains_key("a"));
    assert!(!engine.estimator.devices.contains_key("b"));
    assert!(engine.estimator.devices.contains_key("c"));
}

#[test]
fn changing_rate_preserves_a_different_unit_with_the_same_serial() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    let mut a = Reading::new("razer:first", "Mouse", "razer", 100);
    a.serial = Some("SHARED1234".into());
    a.container = Some("first-physical-unit".into());
    let mut b = a.clone();
    b.key = "razer:second".into();
    b.container = Some("second-physical-unit".into());
    let mut ambiguous_bt = a.clone();
    ambiguous_bt.key = "bluetooth:ambiguous".into();
    ambiguous_bt.source = "bluetooth".into();
    ambiguous_bt.container = None;
    engine.apply("razer", Ok(vec![a.clone(), b.clone()]), 0.0, false);
    engine.apply("bluetooth", Ok(vec![ambiguous_bt.clone()]), 0.0, false);
    for r in [&a, &b, &ambiguous_bt] {
        engine
            .estimator
            .devices
            .insert(r.key.clone(), Default::default());
    }
    engine.reset_estimate(&a.key);
    assert!(!engine.estimator.devices.contains_key(&a.key));
    assert!(engine.estimator.devices.contains_key(&b.key));
    assert!(engine.estimator.devices.contains_key(&ambiguous_bt.key));
}
#[test]
fn changing_rate_clears_proven_wired_wireless_preference_aliases() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    let mut radio = Reading::new("razer:00bf:SYNTHETICUNIT", "Mouse", "razer", 100);
    radio.kind = "mouse".into();
    radio.via = "usb".into();
    radio.serial = Some("SYNTHETICUNIT".into());
    radio.container = Some("radio-container".into());
    let mut wired = radio.clone();
    wired.key = "razer:00be:SYNTHETICUNIT".into();
    wired.container = Some("wired-container".into());
    engine.apply("razer", Ok(vec![radio.clone(), wired.clone()]), 0.0, false);
    for r in [&radio, &wired] {
        engine
            .estimator
            .devices
            .insert(r.key.clone(), Default::default());
    }
    engine.reset_estimate(&radio.key);
    assert!(!engine.estimator.devices.contains_key(&radio.key));
    assert!(!engine.estimator.devices.contains_key(&wired.key));
}
#[test]
fn configuration_identity_is_independent_of_battery_state() {
    let mut reading = Reading::new("razer:026b:SYNTHETIC", "Keyboard", "razer", 10);
    reading.kind = "keyboard".into();
    reading.via = "usb".into();
    reading.serial = Some("SYNTHETIC".into());
    reading.container = Some("CONTAINER".into());
    let device = ConfigurationDevice::from_reading(&reading);
    assert!(device.online());
    assert_eq!(device.capability, PollingCapability::ReadWrite);
    reading.level = Some(50);
    reading.timestamp = 99;
    reading.name = "Renamed".into();
    reading.connection = Connection::Sleeping;
    assert!(device.matches_reading(&reading));
    for change in 0..6 {
        let mut other = reading.clone();
        match change {
            0 => other.key.push('x'),
            1 => other.source.push('x'),
            2 => other.via.push('x'),
            3 => other.serial = None,
            4 => other.container = None,
            _ => other.kind = "mouse".into(),
        };
        assert!(!device.matches_reading(&other));
    }
}
