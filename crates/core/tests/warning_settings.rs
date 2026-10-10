use hb_core::Settings;
use serde_json::json;

#[test]
fn warning_threshold_defaults_validates_and_round_trips() {
    assert_eq!(Settings::default().warning_level, 30);
    assert_eq!(
        Settings::from_value(json!({"low": 15}))
            .unwrap()
            .warning_level,
        30
    );
    for warning_level in [0, 10, 30, 100] {
        let settings =
            Settings::from_value(json!({"low": 20, "warning_level": warning_level})).unwrap();
        assert_eq!(settings.warning_level, warning_level);
        assert_eq!(
            settings.low, 20,
            "Visual warning must not adjust notification settings"
        );
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            Settings::from_value(saved).unwrap().warning_level,
            warning_level
        );
    }
    for invalid in [
        json!(-1),
        json!(101),
        json!(300),
        json!(30.5),
        json!("30"),
        json!(null),
        json!(true),
    ] {
        let settings = Settings::from_value(json!({"warning_level": invalid, "low": 10})).unwrap();
        assert_eq!(settings.warning_level, 30);
        assert_eq!(settings.low, 10);
    }
}
