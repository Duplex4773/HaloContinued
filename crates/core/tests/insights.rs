use hb_core::*;

#[test]
fn changed_identity_under_a_reused_key_starts_a_new_partial_cycle() {
    for field in 0..4 {
        let mut next = point(60, 50);
        match field {
            0 => next.reading.source = "replacement-source".into(),
            1 => next.reading.kind = "replacement-kind".into(),
            2 => next.reading.serial = Some("replacement-unit".into()),
            _ => next.reading.container = Some("replacement-container".into()),
        }
        let summary = summarize([point(0, 90), next]);
        assert_eq!(summary.coverage.awake_seconds, 0, "identity field {field}");
        assert!(summary.rates.is_empty());
        assert_eq!(summary.cycles.len(), 2);
        assert_eq!(summary.cycles[1].evidence, CycleEvidence::Partial);
    }
    let mut renamed = point(60, 89);
    renamed.reading.name = "New display name".into();
    assert_eq!(
        summarize([point(0, 90), renamed]).coverage.awake_seconds,
        60
    );
}

#[test]
fn last_rate_evidence_is_not_inherited_from_older_readings() {
    let first = point(0, 80);
    let mut last = point(60, 79);
    last.polling_rate = None;
    let summary = summarize([first, last]);
    assert!(summary.coverage.latest_polling_rate.is_none());
    let mut builder = InsightsBuilder::default();
    builder.push(point(0, 80));
    builder.break_continuity();
    assert!(builder.finish().coverage.latest_polling_rate.is_none());
    assert_eq!(
        summarize([point(0, 80), point(60, 79)])
            .coverage
            .latest_polling_rate
            .unwrap()
            .hz(),
        1000
    );
}

#[test]
fn mixed_projection_weights_drain_and_requires_evidence_for_every_used_rate() {
    let rate = |hz, seconds, hours| RateInsight {
        hz,
        awake_seconds: seconds,
        projection_seconds: 1800,
        projection_consumed_percent: 3,
        projected_full_charge_hours: Some(hours),
        confidence: InsightConfidence::Low,
        ..Default::default()
    };
    let mut data = BatteryInsights {
        rates: vec![rate(1000, 7200, 100.0), rate(2000, 3600, 50.0)],
        ..Default::default()
    };
    assert!((data.mixed_full_charge_hours().unwrap() - 75.0).abs() < 0.001);
    for invalid in [None, Some(0.0), Some(f64::NAN), Some(f64::INFINITY)] {
        data.rates[1].projected_full_charge_hours = invalid;
        assert!(data.mixed_full_charge_hours().is_none());
    }
    data.rates[1] = rate(2000, 3600, 50.0);
    data.rates[1].projection_seconds = 1799;
    assert!(data.mixed_full_charge_hours().is_none());
    data.rates.pop();
    assert!(data.mixed_full_charge_hours().is_none());
    data.rates.clear();
    assert!(data.mixed_full_charge_hours().is_none());
}

fn point(timestamp: i64, level: u8) -> UsageObservation {
    let mut reading = Reading::new("device", "Device", "test", timestamp);
    reading.level = Some(level);
    reading.charging = Some(false);
    reading.via = "receiver".into();
    UsageObservation {
        reading,
        polling_rate: Some(PollingRate::try_from(1000).unwrap()),
        session: Some(1),
    }
}

fn summarize(points: impl IntoIterator<Item = UsageObservation>) -> BatteryInsights {
    let mut builder = InsightsBuilder::default();
    for point in points {
        builder.push(point);
    }
    builder.finish()
}
#[test]
fn a_replacement_unit_never_inherits_old_rate_learning_even_after_a_corrupt_row() {
    for corrupt_boundary in [false, true] {
        let mut builder = InsightsBuilder::default().recent_since(-10000);
        for index in 0..=10 {
            builder.push(point(index * 600, 90 - index as u8));
        }
        if corrupt_boundary {
            builder.break_continuity();
        }
        for (timestamp, level) in [(6600, 80), (7200, 79)] {
            let mut replacement = point(timestamp, level);
            replacement.reading.serial = Some("REPLACEMENT_TEST_UNIT".into());
            replacement.session = Some(2);
            builder.push(replacement);
        }
        let data = builder.finish_at(7200);
        assert_eq!(
            data.coverage.awake_seconds, 6600,
            "Historical recorded use stays available"
        );
        assert_eq!(data.recent.total_seconds, 600);
        assert_eq!(data.recent.since, 6600);
        assert!(
            data.rates
                .iter()
                .all(|r| r.projected_full_charge_hours.is_none())
        );
        assert_eq!(data.cycles.len(), 2);
        assert!(!data.cycles[0].current);
        assert_eq!(data.cycles[1].start_timestamp, 6600);
    }
}

#[test]
fn projections_require_counted_time_and_measured_drop() {
    let summary = summarize((0..=4).map(|i| point(i * 600, 80 - i as u8)));
    let rate = &summary.rates[0];
    assert_eq!(rate.awake_seconds, 2400);
    assert_eq!(rate.consumed_percent, 4);
    assert_eq!(rate.sample_count, 5);
    assert_eq!(rate.drop_count, 4);
    assert_eq!(rate.projection_seconds, 1800);
    assert_eq!(rate.projection_consumed_percent, 3);
    assert_eq!(rate.projection_drop_count, 3);
    assert_eq!(rate.confidence, InsightConfidence::Low);
    assert!((rate.projected_full_charge_hours.unwrap() - 100.0 / 6.0).abs() < 0.001);
    assert!((rate.remaining_hours.unwrap() - 76.0 / 6.0).abs() < 0.001);
    assert!(
        summarize([point(0, 80), point(600, 70)]).rates[0]
            .projected_full_charge_hours
            .is_none()
    );
    assert!(
        summarize((0..=3).map(|i| point(i * 600, 80))).rates[0]
            .projected_full_charge_hours
            .is_none()
    );
}

#[test]
fn pauses_unknown_coarse_malformed_and_long_gaps_do_not_bridge() {
    for kind in 0..6 {
        let mut middle = point(300, 75);
        match kind {
            0 => middle.reading.connection = Connection::Sleeping,
            1 => middle.reading.connection = Connection::Stale,
            2 => middle.reading.precision = Precision::Coarse,
            3 => middle.reading.charging = None,
            4 => middle.reading.level = Some(101),
            _ => middle.reading.charging_inferred = true,
        }
        let summary = summarize([point(0, 80), middle, point(600, 70), point(900, 69)]);
        assert_eq!(summary.rates[0].awake_seconds, 300);
        assert_eq!(summary.rates[0].consumed_percent, 1);
        assert_eq!(summary.cycles[0].consumed_percent, 1);
    }
    let summary = summarize([point(0, 80), point(601, 70), point(901, 69)]);
    assert_eq!(summary.rates[0].consumed_percent, 1);
}

#[test]
fn switches_and_restart_exclude_boundary_interval() {
    for kind in 0..3 {
        let mut switched = point(300, 75);
        match kind {
            0 => switched.polling_rate = Some(PollingRate::try_from(500).unwrap()),
            1 => switched.session = Some(2),
            _ => switched.reading.via = "bluetooth".into(),
        }
        let mut last = switched.clone();
        last.reading.timestamp = 600;
        last.reading.level = Some(74);
        let summary = summarize([point(0, 80), switched, last]);
        assert_eq!(summary.rates[0].consumed_percent, 1);
        assert_eq!(
            summary
                .cycles
                .iter()
                .map(|c| c.consumed_percent)
                .sum::<u64>(),
            1
        );
    }
}

#[test]
fn old_history_summarizes_cycles_without_learning_rates() {
    let summary = summarize((0..3).map(|i| {
        let mut value = point(i * 300, 80 - i as u8);
        value.polling_rate = None;
        value.session = None;
        value
    }));
    assert!(summary.rates.is_empty());
    assert_eq!(summary.cycles[0].consumed_percent, 2);
    assert_eq!(summary.cycles[0].evidence, CycleEvidence::Partial);
}

#[test]
fn observed_charge_and_level_increase_split_discharge() {
    let mut charging = point(300, 85);
    charging.reading.charging = Some(true);
    let summary = summarize([
        point(0, 80),
        charging,
        point(600, 90),
        point(900, 89),
        point(1200, 95),
        point(1500, 94),
    ]);
    assert_eq!(summary.cycles.len(), 3);
    assert_eq!(summary.cycles[1].evidence, CycleEvidence::ObservedCharge);
    assert_eq!(summary.cycles[2].evidence, CycleEvidence::InferredCharge);
    assert_eq!(summary.cycles[1].consumed_percent, 1);
    assert!(!summary.cycles[1].current);
    assert!(summary.cycles[2].current);
}

#[test]
fn backward_duplicate_corrupt_and_identity_breaks_are_conservative() {
    for timestamp in [0, -1, i64::MIN] {
        let summary = summarize([point(0, 80), point(timestamp, 70), point(300, 69)]);
        assert!(summary.rates.iter().all(|rate| rate.consumed_percent <= 1));
    }
    let mut builder = InsightsBuilder::default();
    builder.push(point(0, 80));
    builder.break_continuity();
    builder.push(point(300, 70));
    assert!(builder.finish().rates.is_empty());
    let mut other = point(300, 70);
    other.reading.key = "other".into();
    assert!(summarize([point(0, 80), other]).rates.is_empty());
}

#[test]
fn cycles_and_rate_buckets_remain_bounded() {
    let summary = summarize((0..100).map(|i| point(i * 300, if i % 2 == 0 { 80 } else { 75 })));
    assert_eq!(summary.cycles.len(), 10);
    assert!(summary.rates.len() <= 7);
    assert_eq!(
        summary.cycles.iter().filter(|cycle| cycle.current).count(),
        1
    );
}

#[test]
fn observed_charge_survives_sleep_unknown_state_and_session_changes() {
    let mut charging = point(0, 80);
    charging.reading.charging = Some(true);
    let mut sleeping = point(300, 90);
    sleeping.reading.connection = Connection::Sleeping;
    sleeping.reading.charging = None;
    let mut awake = point(600, 90);
    awake.session = Some(2);
    let mut last = awake.clone();
    last.reading.timestamp = 900;
    last.reading.level = Some(89);
    let summary = summarize([charging, sleeping, awake, last]);
    assert_eq!(summary.cycles.len(), 1);
    assert_eq!(summary.cycles[0].evidence, CycleEvidence::ObservedCharge);
    assert_eq!(summary.cycles[0].awake_seconds, 300);
    assert_eq!(summary.cycles[0].consumed_percent, 1);
}

#[test]
fn physical_cycle_survives_session_and_rate_changes_without_boundary_drain() {
    let mut charging = point(0, 80);
    charging.reading.charging = Some(true);
    let mut switched = point(900, 70);
    switched.session = Some(2);
    switched.polling_rate = Some(PollingRate::try_from(500).unwrap());
    let mut last = switched.clone();
    last.reading.timestamp = 1200;
    last.reading.level = Some(69);
    let summary = summarize([charging, point(300, 90), point(600, 89), switched, last]);
    assert_eq!(summary.cycles.len(), 1);
    assert_eq!(summary.cycles[0].evidence, CycleEvidence::ObservedCharge);
    assert_eq!(summary.cycles[0].awake_seconds, 600);
    assert_eq!(summary.cycles[0].consumed_percent, 2);
    assert_eq!(
        summary
            .rates
            .iter()
            .map(|rate| rate.consumed_percent)
            .sum::<u64>(),
        2
    );
}

#[test]
fn one_or_two_point_oscillation_does_not_manufacture_charge_or_drain() {
    for rebound in [1, 2] {
        let summary =
            summarize((0..100).map(|i| point(i * 300, if i % 2 == 0 { 80 } else { 80 - rebound })));
        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.cycles[0].evidence, CycleEvidence::Partial);
        assert_eq!(summary.cycles[0].consumed_percent, u64::from(rebound));
        assert_eq!(summary.rates[0].consumed_percent, u64::from(rebound));
        assert_eq!(summary.rates[0].drop_count, 1);
        assert!(summary.rates[0].projected_full_charge_hours.is_none());
    }
}

#[test]
fn rebounds_across_pauses_and_rate_boundaries_cannot_recount_consumption() {
    for boundary in 0..4 {
        let mut points = vec![point(0, 80), point(60, 79)];
        for i in 1..=12 {
            let mut wake = point(i * 300, 80);
            match boundary {
                0 => {
                    let mut sleeping = point(i * 300 - 60, 79);
                    sleeping.reading.connection = Connection::Sleeping;
                    points.push(sleeping);
                }
                1 => wake.session = Some(i as u64 + 1),
                2 => {
                    wake.polling_rate =
                        Some(PollingRate::try_from(if i % 2 == 0 { 1000 } else { 500 }).unwrap())
                }
                _ => wake.reading.via = format!("transport-{}", i % 2),
            }
            let mut drop = wake.clone();
            drop.reading.timestamp += 60;
            drop.reading.level = Some(79);
            points.extend([wake, drop]);
        }
        let mut new_low = points.last().unwrap().clone();
        new_low.reading.timestamp += 60;
        new_low.reading.level = Some(78);
        points.push(new_low);
        let data = summarize(points);
        assert_eq!(data.cycles.len(), 1, "boundary {boundary}");
        assert_eq!(data.cycles[0].consumed_percent, 2, "boundary {boundary}");
        assert_eq!(
            data.rates.iter().map(|r| r.consumed_percent).sum::<u64>(),
            2,
            "boundary {boundary}"
        );
        assert!(
            data.rates
                .iter()
                .all(|r| r.projected_full_charge_hours.is_none())
        );
    }
}

#[test]
fn unobserved_charge_across_sleep_is_inferred_without_counting_gap_drain() {
    for awake_level in [100, 69] {
        let mut sleeping = point(600, 70);
        sleeping.reading.connection = Connection::Sleeping;
        sleeping.reading.charging = None;
        let summary = summarize([
            point(0, 71),
            point(300, 70),
            sleeping,
            point(3600, awake_level),
        ]);
        assert_eq!(summary.rates[0].consumed_percent, 1);
        assert_eq!(summary.rates[0].awake_seconds, 300);
        if awake_level == 100 {
            assert_eq!(summary.cycles.len(), 2);
            assert_eq!(summary.cycles[1].evidence, CycleEvidence::InferredCharge);
            assert_eq!(summary.cycles[1].consumed_percent, 0);
            assert_eq!(summary.cycles[1].awake_seconds, 0);
        } else {
            assert_eq!(summary.cycles.len(), 1);
            assert_eq!(summary.cycles[0].consumed_percent, 1);
            assert_eq!(summary.cycles[0].awake_seconds, 300);
        }
    }
}

#[test]
fn direct_charge_evidence_is_not_downgraded_by_inferred_charging() {
    let mut charging = point(0, 80);
    charging.reading.charging = Some(true);
    let mut inferred = point(300, 85);
    inferred.reading.charging = Some(true);
    inferred.reading.charging_inferred = true;
    let summary = summarize([charging, inferred, point(600, 90), point(900, 89)]);
    assert_eq!(summary.cycles[0].evidence, CycleEvidence::ObservedCharge);
}

#[test]
fn cumulative_rise_across_pauses_infers_charge() {
    let mut sleeping = point(600, 70);
    sleeping.reading.connection = Connection::Sleeping;
    sleeping.reading.charging = None;
    let mut second_pause = sleeping.clone();
    second_pause.reading.timestamp = 1200;
    let summary = summarize([
        point(0, 71),
        point(300, 70),
        sleeping,
        point(900, 72),
        second_pause,
        point(1500, 73),
    ]);
    assert_eq!(summary.cycles.len(), 2);
    assert_eq!(summary.cycles[1].evidence, CycleEvidence::InferredCharge);
    assert_eq!(summary.rates[0].awake_seconds, 300);
    assert_eq!(summary.rates[0].consumed_percent, 1);
}

#[test]
fn remaining_projection_waits_for_usage_after_a_boundary() {
    for kind in 0..3 {
        let mut points: Vec<_> = (0..=4).map(|i| point(i * 600, 80 - i as u8)).collect();
        let mut last = point(2700, 75);
        match kind {
            0 => last.session = Some(2),
            1 => last.reading.via = "bluetooth".into(),
            _ => last.reading.timestamp = 3300,
        }
        points.push(last.clone());
        let summary = summarize(points.clone());
        assert!(summary.rates[0].projected_full_charge_hours.is_some());
        assert!(summary.rates[0].remaining_hours.is_none());
        last.reading.timestamp += 300;
        points.push(last);
        assert!(summarize(points).rates[0].remaining_hours.is_some());
    }
}

#[test]
fn query_clock_expires_remaining_without_erasing_observed_projection() {
    for (timestamp, fresh) in [
        (2400, true),
        (3000, true),
        (3001, false),
        (2399, false),
        (i64::MIN, false),
    ] {
        let mut builder = InsightsBuilder::default();
        for i in 0..=4 {
            builder.push(point(i * 600, 80 - i as u8));
        }
        let summary = builder.finish_at(timestamp);
        assert!(summary.rates[0].projected_full_charge_hours.is_some());
        assert_eq!(summary.rates[0].remaining_hours.is_some(), fresh);
        assert_eq!(summary.coverage.last_reading_timestamp, Some(2400));
    }
}

#[test]
fn coverage_counts_unknown_rate_usage_and_excluded_intervals_once() {
    let mut builder = InsightsBuilder::default();
    for i in 0..=2 {
        let mut value = point(i * 300, 80 - i as u8);
        value.polling_rate = None;
        value.session = None;
        builder.push(value);
    }
    let mut sleeping = point(900, 78);
    sleeping.reading.connection = Connection::Sleeping;
    sleeping.reading.charging = None;
    builder.push(sleeping);
    // Different transport, rate, session and long pause still exclude one pair.
    let mut next = point(3000, 70);
    next.reading.via = "bluetooth".into();
    next.session = Some(2);
    builder.push(next);
    builder.break_continuity();
    builder.push(point(3300, 69));
    let summary = builder.finish();
    assert!(summary.rates.is_empty());
    assert_eq!(summary.coverage.observation_count, 6);
    assert_eq!(summary.coverage.discharge_sample_count, 5);
    assert_eq!(summary.coverage.awake_seconds, 600);
    assert_eq!(summary.coverage.excluded_interval_count, 3);
    assert_eq!(summary.coverage.unreadable_row_count, 1);
    assert_eq!(summary.coverage.last_reading_timestamp, Some(3300));
}

#[test]
fn coverage_totals_survive_bounded_cycle_eviction() {
    let summary = summarize((0..100).map(|i| point(i * 300, if i % 2 == 0 { 80 } else { 75 })));
    assert_eq!(summary.cycles.len(), 10);
    assert_eq!(summary.coverage.observation_count, 100);
    assert_eq!(summary.coverage.awake_seconds, 50 * 300);
    assert_eq!(summary.coverage.excluded_interval_count, 49);
}

#[test]
fn flat_tails_wait_for_a_new_low_before_affecting_projection() {
    let mut points: Vec<_> = (0..=4).map(|i| point(i * 600, 80 - i as u8)).collect();
    points.extend([point(3000, 76), point(3600, 77), point(4200, 76)]);
    let summary = summarize(points.clone());
    let rate = &summary.rates[0];
    assert_eq!(rate.awake_seconds, 4200);
    assert_eq!(rate.projection_seconds, 1800);
    assert_eq!(rate.consumed_percent, 4);
    assert_eq!(rate.projection_consumed_percent, 3);
    assert!((rate.projected_full_charge_hours.unwrap() - 100.0 / 6.0).abs() < 0.001);
    points.push(point(4800, 75));
    let summary = summarize(points);
    assert_eq!(summary.rates[0].projection_seconds, 4200);
    assert_eq!(summary.rates[0].consumed_percent, 5);
    assert_eq!(summary.rates[0].projection_consumed_percent, 4);
}

#[test]
fn incomplete_flat_time_cannot_supply_projection_or_moderate_confidence() {
    let summary = summarize((0..=15).map(|i| point(i * 600, if i == 0 { 80 } else { 70 })));
    let rate = &summary.rates[0];
    assert_eq!(rate.awake_seconds, 9000);
    assert_eq!(rate.projection_seconds, 0);
    assert_eq!(rate.projection_consumed_percent, 0);
    assert_eq!(rate.confidence, InsightConfidence::Insufficient);
    assert!(rate.projected_full_charge_hours.is_none());

    let summary = summarize((0..=15).map(|i| {
        let level = match i {
            0 => 80,
            1 => 76,
            2 => 73,
            3 => 70,
            _ => 66,
        };
        point(i * 600, level)
    }));
    let rate = &summary.rates[0];
    assert_eq!(rate.projection_seconds, 1800);
    assert_eq!(rate.consumed_percent, 14);
    assert_eq!(rate.drop_count, 4);
    assert_eq!(rate.projection_consumed_percent, 10);
    assert_eq!(rate.projection_drop_count, 3);
    assert_eq!(rate.confidence, InsightConfidence::Low);
}

#[test]
fn flat_tails_are_discarded_at_all_usage_boundaries() {
    for kind in 0..8 {
        let mut builder = InsightsBuilder::default();
        for i in 0..=4 {
            builder.push(point(i * 600, 80 - i as u8));
        }
        builder.push(point(2700, 76));
        builder.push(point(3000, 76));
        let mut boundary = point(3300, 76);
        match kind {
            0 => boundary.reading.timestamp = 3900,
            1 => boundary.reading.charging = Some(true),
            2 => boundary.session = Some(2),
            3 => boundary.polling_rate = Some(PollingRate::try_from(500).unwrap()),
            4 => boundary.reading.via = "bluetooth".into(),
            5 => boundary.reading.connection = Connection::Sleeping,
            6 => boundary.reading.precision = Precision::Coarse,
            _ => builder.break_continuity(),
        }
        builder.push(boundary.clone());
        let mut resumed = boundary.clone();
        resumed.reading.timestamp += 300;
        resumed.reading.charging = Some(false);
        resumed.reading.connection = Connection::Online;
        resumed.reading.precision = Precision::Exact;
        builder.push(resumed.clone());
        resumed.reading.timestamp += 300;
        resumed.reading.level = Some(75);
        builder.push(resumed.clone());
        resumed.reading.timestamp += 300;
        resumed.reading.level = Some(74);
        builder.push(resumed);
        let summary = builder.finish();
        let original = summary.rates.iter().find(|rate| rate.hz == 1000).unwrap();
        let contribution = match kind {
            3 => 0,   // The later drop belongs to 500 Hz.
            _ => 300, // First drop anchors; only the subsequent window counts.
        };
        assert_eq!(
            original.projection_seconds,
            1800 + contribution,
            "boundary {kind}"
        );
    }
}

#[test]
fn a_new_flat_charge_cycle_does_not_inflate_historical_projection() {
    let mut points: Vec<_> = (0..=4).map(|i| point(i * 600, 80 - i as u8)).collect();
    let mut charging = point(2700, 100);
    charging.reading.charging = Some(true);
    points.push(charging);
    points.extend((5..=12).map(|i| point(i * 600, 100)));
    let summary = summarize(points);
    assert_eq!(summary.cycles.len(), 2);
    assert_eq!(summary.rates[0].projection_seconds, 1800);
    assert_eq!(summary.rates[0].consumed_percent, 4);
    assert_eq!(summary.rates[0].projection_consumed_percent, 3);
    assert!(summary.rates[0].awake_seconds > 1800);
}

#[test]
fn many_short_single_drop_segments_do_not_manufacture_a_projection() {
    let summary = summarize((0..50).flat_map(|segment| {
        [
            point(segment * 1800, 80),
            point(segment * 1800 + 600, 80),
            point(segment * 1800 + 1200, 79),
            point(segment * 1800 + 1500, 79),
        ]
        .map(|mut value| {
            value.session = Some(segment as u64 + 1);
            value
        })
    }));
    let rate = &summary.rates[0];
    assert_eq!(rate.awake_seconds, 50 * 1500);
    // Repeated one-point rebounds across sessions are not fresh consumption.
    assert_eq!(rate.consumed_percent, 1);
    assert_eq!(rate.drop_count, 1);
    assert_eq!(rate.projection_seconds, 0);
    assert_eq!(rate.projection_consumed_percent, 0);
    assert_eq!(rate.projection_drop_count, 0);
    assert_eq!(rate.confidence, InsightConfidence::Insufficient);
    assert!(rate.projected_full_charge_hours.is_none());
}

#[test]
fn complete_continuous_windows_can_support_moderate_evidence() {
    // Battery falls three points every forty minutes, sampled every ten.
    // First observed drop anchors the model; four subsequent windows qualify.
    let summary = summarize((0..=20).map(|i| point(i * 600, 80 - (i / 4) as u8 * 3)));
    let rate = &summary.rates[0];
    assert_eq!(rate.awake_seconds, 12000);
    assert_eq!(rate.consumed_percent, 15);
    assert_eq!(rate.projection_seconds, 9600);
    assert_eq!(rate.projection_consumed_percent, 12);
    assert_eq!(rate.projection_drop_count, 4);
    assert_eq!(rate.confidence, InsightConfidence::Moderate);
    assert!((rate.projected_full_charge_hours.unwrap() - 200.0 / 9.0).abs() < 0.001);
}
