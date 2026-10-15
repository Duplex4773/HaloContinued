mod support {
    pub mod allocation_counter;
}
use hb_core::*;
use support::allocation_counter::{TrackingAllocator, measure};

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn reading(key: &str, timestamp: i64, level: u8) -> Reading {
    let mut reading = Reading::new(key, "Invented mouse 雪", "test", timestamp);
    reading.level = Some(level);
    reading.charging = Some(false);
    reading.via = "receiver".into();
    reading.serial = Some("invented-unit".into());
    reading.container = Some("invented-container".into());
    reading
}

#[test]
fn repeated_inventory_presence_checks_allocate_nothing() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    engine.apply("test", Ok(vec![reading("invented", 0, 80)]), 0.0, false);
    let (_, allocations) = measure(|| {
        for _ in 0..1000 {
            assert!(std::hint::black_box(&engine).has_readings());
        }
    });
    assert_eq!(allocations.calls, 0);
    assert_eq!(allocations.bytes, 0);
}

#[test]
fn thirty_day_insights_consume_owned_rows_without_per_row_clones() {
    // Input ownership is established before measuring builder work.
    let observations: Vec<_> = (0..43_200)
        .map(|index| UsageObservation {
            reading: reading("invented", index * 60, (100 - index % 100) as u8),
            polling_rate: Some(PollingRate::try_from(1000).unwrap()),
            session: Some(1),
        })
        .collect();
    let mut builder = InsightsBuilder::default();
    let (_, allocations) = measure(|| {
        for observation in observations {
            builder.push(observation);
        }
    });
    println!("43,200 owned Insights pushes: {allocations:?}");
    assert!(
        allocations.calls <= 4,
        "only bounded cycle deque growth is needed: {allocations:?}"
    );
    assert!(allocations.bytes < 4096);
    let summary = builder.finish();
    assert_eq!(summary.cycles.len(), 10);
    assert_eq!(summary.rates.len(), 1);
    assert_eq!(summary.rates[0].hz, 1000);
    assert!(summary.rates[0].awake_seconds > 2_000_000);
    assert!(summary.rates[0].remaining_hours.is_some());
}

#[test]
fn empty_held_queue_does_not_rebuild_readings() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    let readings = (0..8)
        .map(|index| reading(&format!("invented-{index}"), 0, 80))
        .collect();
    engine.apply("test", Ok(readings), 0.0, false);
    assert!(!engine.has_held_notifications());
    let (_, allocations) = measure(|| {
        for _ in 0..1000 {
            assert!(engine.flush_held().is_empty());
        }
    });
    assert_eq!(
        allocations.calls, 0,
        "empty notification maintenance: {allocations:?}"
    );
    assert_eq!(allocations.bytes, 0);
}

#[test]
fn empty_brand_polls_do_not_copy_other_devices_or_trigger_alerts_from_old_readings() {
    let mut engine = Engine::new(
        Settings {
            low: 10,
            ..Settings::default()
        },
        Estimator::default(),
    );
    engine.apply("test", Ok(vec![reading("invented", 1, 19)]), 1.0, false);
    let before = serde_json::to_value(engine.snapshot(1)).unwrap();
    let (_, allocations) = measure(|| {
        for _ in 0..1000 {
            assert!(
                engine
                    .apply("unused", Ok(Vec::new()), 2.0, false)
                    .is_empty()
            );
        }
    });
    assert_eq!(
        allocations.calls, 0,
        "empty provider polls: {allocations:?}"
    );
    assert_eq!(serde_json::to_value(engine.snapshot(1)).unwrap(), before);
    engine.update_settings(Settings {
        low: 20,
        ..Settings::default()
    });
    assert!(
        engine
            .apply("unused", Ok(Vec::new()), 3.0, false)
            .is_empty()
    );
    // The device's next fresh poll triggers the revised threshold.
    assert_eq!(
        engine
            .apply("test", Ok(vec![reading("invented", 4, 19)]), 4.0, false)
            .len(),
        1
    );
    engine.apply(
        "unused",
        Err(ProviderError::new("synthetic unavailable provider")),
        5.0,
        false,
    );
    assert!(engine.provider_error("unused").is_some());
    engine.apply("unused", Ok(Vec::new()), 6.0, false);
    assert!(engine.provider_error("unused").is_none());
    // Empty brand results still release valid notifications held during a game.
    engine.apply("other", Ok(vec![reading("other", 7, 5)]), 7.0, true);
    assert_eq!(engine.apply("unused", Ok(Vec::new()), 8.0, false).len(), 1);
}

#[test]
fn provider_snapshot_state_tracks_cache_errors_and_successful_clear() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    assert!(!engine.provider_has_snapshot_data("test"));
    engine.apply("test", Ok(Vec::new()), 0.0, false);
    assert!(!engine.provider_has_snapshot_data("test"));
    engine.apply("test", Ok(vec![reading("invented", 1, 80)]), 1.0, false);
    assert!(engine.provider_has_snapshot_data("test"));
    engine.apply(
        "test",
        Err(ProviderError::new("invented failure")),
        2.0,
        false,
    );
    assert!(engine.provider_has_snapshot_data("test"));
    assert_eq!(engine.readings()[0].connection, Connection::Stale);
    engine.apply("test", Ok(Vec::new()), 3.0, false);
    assert!(engine.provider_has_snapshot_data("test")); // first miss retains stale reading
    engine.apply("test", Ok(Vec::new()), 4.0, false);
    assert!(!engine.provider_has_snapshot_data("test"));
    engine.apply(
        "other",
        Err(ProviderError::new("invented failure")),
        5.0,
        false,
    );
    assert!(engine.provider_has_snapshot_data("other")); // error without any reading
    engine.apply("other", Ok(Vec::new()), 6.0, false);
    assert!(!engine.provider_has_snapshot_data("other"));
}

#[test]
fn held_flag_survives_unavailable_retry_and_clears_after_recovery() {
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    engine.apply("test", Ok(vec![reading("invented", 1, 10)]), 1.0, true);
    assert!(engine.has_held_notifications());
    engine.apply(
        "test",
        Err(ProviderError::new("invented failure")),
        2.0,
        false,
    );
    assert!(engine.flush_held().is_empty());
    assert!(engine.has_held_notifications());
    let delivered = engine.apply("test", Ok(vec![reading("invented", 3, 10)]), 3.0, false);
    assert_eq!(delivered.len(), 1);
    assert!(!engine.has_held_notifications());
    engine.notification_failed(delivered.into_iter().next().unwrap());
    assert!(engine.has_held_notifications());
    engine.apply("test", Ok(vec![reading("invented", 4, 80)]), 4.0, false);
    assert!(!engine.has_held_notifications());
}
