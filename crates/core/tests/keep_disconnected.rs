use hb_core::*;

#[test]
fn retained_devices_are_offline_quiet_and_process_local() {
    for keep in [false, true] {
        let settings = Settings {
            keep_disconnected: keep,
            low_sound: true,
            ..Default::default()
        };
        let mut engine = Engine::new(settings, Estimator::default());
        let mut reading = Reading::new("test:pad", "Test controller", "xinput", 100);
        reading.level = Some(50);
        reading.charging = Some(true);
        reading.kind = "gamepad".into();
        engine.apply("xinput", Ok(vec![reading.clone()]), 0., false);
        for time in [1., 60., 86400.] {
            assert!(engine.apply("xinput", Ok(vec![]), time, false).is_empty());
            assert!(engine.take_low_battery_sound().is_none());
        }
        let rows = engine.readings();
        assert_eq!(rows.len(), usize::from(keep));
        if keep {
            assert_eq!(rows[0].level, Some(50));
            assert_eq!(rows[0].timestamp, 100);
            assert!(!rows[0].online());
            assert_eq!(rows[0].charging, Some(false));
            assert!(engine.snapshot(200).devices[0].seconds_left.is_none());
            reading.charging = Some(false);
            engine.apply("xinput", Ok(vec![reading]), 86401., false);
            assert_eq!(engine.readings().len(), 1);
            assert!(engine.readings()[0].online());
            let mut settings = engine.settings.clone();
            settings.disabled_providers.insert("xinput".into());
            engine.update_settings(settings);
            assert!(engine.readings().is_empty());
        }
    }
    assert!(
        Engine::new(
            Settings {
                keep_disconnected: true,
                ..Default::default()
            },
            Estimator::default()
        )
        .readings()
        .is_empty()
    );
}

#[test]
fn retention_setting_defaults_off_and_rejects_wrong_types() {
    assert!(!Settings::default().keep_disconnected);
    assert!(
        Settings::from_value(serde_json::json!({"keep_disconnected":true}))
            .unwrap()
            .keep_disconnected
    );
    assert!(
        !Settings::from_value(serde_json::json!({"keep_disconnected":"yes"}))
            .unwrap()
            .keep_disconnected
    );
}
