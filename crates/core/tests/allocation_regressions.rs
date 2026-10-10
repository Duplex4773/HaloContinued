use hb_core::*;
use serde_json::json;

#[test]
fn stalled_endpoint_fit_matches_explicit_sample_and_preserves_history() {
    for stalled in [false, true] {
        let samples = vec![(0.0, 90), (600.0, 89), (1900.0, 86), (2400.0, 85)];
        let usage = if stalled { 3700.0 } else { 2400.0 };
        let estimator = Estimator::from_value(
            json!({"devices":{"device":{"usage":usage,"samples":samples,"seen":1}}}),
            1,
        );
        let mut explicit = estimator.clone();
        if stalled {
            explicit
                .devices
                .get_mut("device")
                .unwrap()
                .samples
                .push((usage, 85));
        }
        let before = serde_json::to_value(&estimator).unwrap();
        for level in [Some(85), Some(30), None, Some(101)] {
            assert_eq!(
                estimator.seconds_left("device", level),
                explicit.seconds_left("device", level)
            );
        }
        assert_eq!(serde_json::to_value(&estimator).unwrap(), before);
    }
}

#[test]
fn dedup_identity_matching_keeps_ascii_case_trim_and_nonascii_semantics() {
    for (first, second, matches) in [
        ("  Mouse-AbC  ", "mouse-abc", true),
        (" unknown ", "UNKNOWN", false),
        ("{000-000}", "{000-000}", false),
        ("ÄBC", "Äbc", true),
        ("ÄBC", "äbc", false),
    ] {
        let mut engine = Engine::new(Settings::default(), Estimator::default());
        let mut hid = Reading::new("hid", "HID", "razer", 1);
        hid.level = Some(80);
        hid.serial = Some(first.into());
        let mut bluetooth = Reading::new("bt", "Bluetooth", "bluetooth", 1);
        bluetooth.level = Some(70);
        bluetooth.serial = Some(second.into());
        engine.apply("razer", Ok(vec![hid]), 0.0, false);
        engine.apply("bluetooth", Ok(vec![bluetooth]), 0.0, false);
        let readings = engine.readings();
        assert_eq!(readings.len(), if matches { 1 } else { 2 });
        assert!(readings.iter().any(|reading| reading.key == "hid"));
    }
}
