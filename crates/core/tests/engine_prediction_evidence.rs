use hb_core::*;

#[test]
fn snapshot_predictions_require_exact_explicit_discharge_evidence() {
    let mut estimator = Estimator::default();
    for index in 0..=6 {
        let mut reading = Reading::new("synthetic", "Synthetic", "razer", index * 600);
        reading.level = Some((90 - index) as u8);
        reading.charging = Some(false);
        estimator.record(&reading, (index * 600) as f64);
    }
    let mut engine = Engine::new(Settings::default(), estimator);
    let mut reading = Reading::new("synthetic", "Synthetic", "razer", 3601);
    reading.level = Some(84);
    reading.charging = Some(false);
    engine.apply("razer", Ok(vec![reading.clone()]), 3601.0, false);
    assert!(engine.snapshot(3601).devices[0].seconds_left.is_some());

    for (charging, inferred, precision) in [
        (None, false, Precision::Exact),
        (Some(false), true, Precision::Exact),
        (Some(false), false, Precision::Estimated),
        (Some(false), false, Precision::Coarse),
    ] {
        let mut uncertain = reading.clone();
        uncertain.charging = charging;
        uncertain.charging_inferred = inferred;
        uncertain.precision = precision;
        engine.apply("razer", Ok(vec![uncertain]), 3602.0, false);
        assert!(engine.snapshot(3602).devices[0].seconds_left.is_none());
    }
}
