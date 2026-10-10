use hb_core::Settings;
use serde_json::json;

#[test]
fn startup_polling_restoration_defaults_off_for_existing_configs() {
    assert!(!Settings::default().restore_polling_on_startup);
    let old_config = json!({
        "polling_controls": true,
        "devices": {"mouse": {"requested_polling_rate": 8000}},
        "future_option": true
    });
    for settings in [
        Settings::from_value(old_config.clone()).unwrap(),
        serde_json::from_value::<Settings>(old_config).unwrap(),
    ] {
        assert!(!settings.restore_polling_on_startup);
        assert!(settings.polling_controls);
        assert_eq!(
            settings.devices["mouse"]
                .requested_polling_rate
                .unwrap()
                .hz(),
            8000
        );
    }
}

#[test]
fn startup_polling_restoration_round_trips_both_boolean_values() {
    for enabled in [false, true] {
        let settings = Settings::from_value(json!({
            "restore_polling_on_startup": enabled,
            "polling_controls": false
        }))
        .unwrap();
        assert_eq!(settings.restore_polling_on_startup, enabled);
        assert!(!settings.polling_controls);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["restore_polling_on_startup"], enabled);
        assert_eq!(
            Settings::from_value(saved.clone())
                .unwrap()
                .restore_polling_on_startup,
            enabled
        );
        assert_eq!(
            serde_json::from_value::<Settings>(saved)
                .unwrap()
                .restore_polling_on_startup,
            enabled
        );
    }
}

#[test]
fn invalid_startup_polling_restoration_keeps_default_and_other_preferences() {
    for invalid in [
        json!(null),
        json!("true"),
        json!(0),
        json!(1),
        json!([]),
        json!({}),
    ] {
        let settings = Settings::from_value(json!({
            "restore_polling_on_startup": invalid,
            "polling_controls": true,
            "low": 15,
            "devices": {"mouse": {"requested_polling_rate": 1000}}
        }))
        .unwrap();
        assert!(!settings.restore_polling_on_startup);
        assert!(settings.polling_controls);
        assert_eq!(settings.low, 15);
        assert_eq!(
            settings.devices["mouse"]
                .requested_polling_rate
                .unwrap()
                .hz(),
            1000
        );
    }
}
