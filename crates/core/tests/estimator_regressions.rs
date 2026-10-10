use hb_core::*;
use serde_json::json;

fn reading(level: u8) -> Reading {
    let mut reading = Reading::new("device", "Device", "test", 100);
    reading.level = Some(level);
    reading.charging = Some(false);
    reading
}

fn learned() -> Estimator {
    let mut estimator = Estimator::default();
    for i in 0..=180 {
        estimator.record(&reading(100 - (i / 12) as u8), i as f64 * 60.0);
    }
    estimator
}

#[test]
fn unobserved_loss_preserves_learned_slope_without_counting_boundary_time() {
    for boundary in ["pause", "gap", "restart", "backwards", "unknown"] {
        let mut estimator = learned();
        let before = estimator.seconds_left("device", Some(55)).unwrap();
        let usage = estimator.devices["device"].usage;
        let mut time = 10_860.0;
        match boundary {
            "pause" => estimator.pause("device"),
            "gap" => time = 100_000.0,
            "restart" => {
                estimator = Estimator::from_value(serde_json::to_value(estimator).unwrap(), 100);
            }
            "backwards" => time = 1.0,
            "unknown" => {
                let mut unknown = reading(80);
                unknown.charging = None;
                estimator.record(&unknown, 10_830.0);
            }
            _ => unreachable!(),
        }
        estimator.record(&reading(55), time);
        assert_eq!(estimator.devices["device"].usage, usage, "{boundary}");
        assert_eq!(
            estimator.seconds_left("device", Some(55)),
            Some(before),
            "{boundary}"
        );
        assert_eq!(estimator.devices["device"].samples.last().unwrap().1, 55);
    }
}

#[test]
fn frequent_short_awake_sessions_accumulate_only_observed_discharge() {
    let mut estimator = Estimator::default();
    let mut time = 0.0;
    let mut level = 100;
    estimator.record(&reading(level), time);
    for _ in 0..6 {
        // Each session contains one measured point over ten awake minutes.
        time += 600.0;
        level -= 1;
        estimator.record(&reading(level), time);
        estimator.pause("device");
        time += 36_000.0;
        level -= 2;
        estimator.record(&reading(level), time);
    }
    assert_eq!(estimator.devices["device"].usage, 3600.0);
    assert_eq!(
        estimator.devices["device"].samples.first().unwrap().1 - level,
        6
    );
    let seconds = estimator.seconds_left("device", Some(level)).unwrap();
    assert!((seconds - f64::from(level) * 600.0).abs() < 0.001);
}

#[test]
fn uncertain_readings_do_not_extend_discharge_evidence() {
    for uncertainty in [
        "charging",
        "inferred",
        "precision",
        "old_timestamp",
        "negative_timestamp",
        "clock",
    ] {
        let mut estimator = learned();
        let before = serde_json::to_value(&estimator).unwrap();
        let mut uncertain = reading(80);
        let mut time = 10_860.0;
        match uncertainty {
            "charging" => uncertain.charging = None,
            "inferred" => uncertain.charging_inferred = true,
            "precision" => uncertain.precision = Precision::Estimated,
            "old_timestamp" => uncertain.timestamp = 99,
            "negative_timestamp" => uncertain.timestamp = -1,
            "clock" => time = f64::NAN,
            _ => unreachable!(),
        }
        estimator.record(&uncertain, time);
        assert_eq!(
            serde_json::to_value(&estimator).unwrap(),
            before,
            "{uncertainty}"
        );
        estimator.record(&reading(80), 20_000.0);
        assert_eq!(estimator.devices["device"].usage, 10_800.0);
    }
}

#[test]
fn recent_drain_adapts_after_usage_conditions_change() {
    let mut estimator = Estimator::default();
    for i in 0..=120 {
        estimator.record(&reading(100 - (i / 12) as u8), i as f64 * 300.0);
    }
    let slow = estimator.seconds_left("device", Some(90)).unwrap();
    for i in 1..=60 {
        estimator.record(&reading(90 - (i / 6) as u8), 36_000.0 + i as f64 * 60.0);
    }
    let fast = estimator.seconds_left("device", Some(80)).unwrap();
    assert!(fast < slow / 5.0);
    assert!((fast / 3600.0 - 8.0).abs() < 0.75);
}

#[test]
fn a_single_large_drop_does_not_support_an_estimate() {
    let mut estimator = Estimator::default();
    for i in 0..=30 {
        estimator.record(&reading(if i == 30 { 90 } else { 100 }), i as f64 * 60.0);
    }
    assert_eq!(estimator.seconds_left("device", Some(90)), None);
}

#[test]
fn full_level_plateau_and_small_upper_edge_jitter_do_not_reset_usage() {
    let mut estimator = Estimator::default();
    for i in 0..=30 {
        estimator.record(&reading(100), i as f64 * 60.0);
    }
    assert_eq!(estimator.devices["device"].usage, 1800.0);
    estimator.record(&reading(99), 1860.0);
    estimator.record(&reading(100), 1920.0);
    assert_eq!(estimator.devices["device"].usage, 1920.0);
    assert_eq!(estimator.devices["device"].samples.len(), 2);
}

#[test]
fn invalid_future_entry_does_not_prevent_recovery_of_valid_history() {
    let estimator = Estimator::from_value(
        json!({"devices": {
            "good": {"usage":1800.0,"samples":[[0.0,90],[600.0,89],[1200.0,88],[1800.0,87]],"seen":100},
            "future": {"usage":1800.0,"samples":[[0.0,90],[600.0,89],[1200.0,88],[1800.0,87]],"seen":1000000}
        }}),
        100,
    );
    assert!(estimator.seconds_left("good", Some(87)).is_some());
    assert!(!estimator.devices.contains_key("future"));
}

#[test]
fn impossible_saved_drops_and_negative_times_are_recovered_independently() {
    let estimator = Estimator::from_value(
        json!({"devices": {
            "good": {"usage":1800.0,"samples":[[0.0,90],[600.0,89],[1200.0,88],[1800.0,87]],"seen":0},
            "zero_time_drop": {"usage":1800.0,"samples":[[0.0,90],[600.0,89],[600.0,88],[1800.0,87]],"seen":100},
            "negative_timestamp": {"usage":1800.0,"samples":[[0.0,90],[600.0,89],[1200.0,88],[1800.0,87]],"seen":-1}
        }}),
        100,
    );
    assert_eq!(estimator.devices.len(), 1);
    assert!(estimator.seconds_left("good", Some(87)).is_some());

    // Invalid observations never create a new record or reset valid evidence.
    let mut empty = Estimator::default();
    let mut negative = reading(80);
    negative.timestamp = -1;
    empty.record(&negative, 0.0);
    assert!(empty.devices.is_empty());
}
