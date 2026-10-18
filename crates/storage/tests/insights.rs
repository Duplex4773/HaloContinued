use hb_core::{
    Connection as DeviceConnection, HistoryStore, InsightsBuilder, PollingRate, Reading,
    UsageObservation,
};
use hb_storage::Store;
use rusqlite::{Connection, params};

fn observation(ts: i64, level: u8, hz: Option<u32>, session: Option<u64>) -> UsageObservation {
    let mut reading = Reading::new("synthetic-a", "Test mouse", "test", ts);
    reading.connection = DeviceConnection::Online;
    reading.level = Some(level);
    reading.charging = Some(false);
    UsageObservation {
        reading,
        polling_rate: hz.map(|rate| PollingRate::try_from(rate).unwrap()),
        session,
    }
}

#[test]
fn usage_and_insights_do_not_bridge_reused_keys_after_identity_changes() {
    for field in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        let first = observation(0, 90, Some(1000), Some(1));
        let mut second = observation(60, 50, Some(1000), Some(1));
        match field {
            0 => second.reading.source = "replacement-source".into(),
            1 => second.reading.kind = "replacement-kind".into(),
            2 => second.reading.serial = Some("replacement-unit".into()),
            3 => second.reading.container = Some("replacement-container".into()),
            _ => second.reading.via = "replacement-connection".into(),
        }
        store.record_usage(&first).unwrap();
        store.record_usage(&second).unwrap();
        store.flush().unwrap();
        assert_eq!(
            store
                .query_usage("synthetic-a", 60, 3600, 100)
                .unwrap()
                .until,
            0,
            "identity field {field}"
        );
        assert_eq!(
            store
                .query_insights("synthetic-a", 60)
                .unwrap()
                .coverage
                .awake_seconds,
            0
        );
    }
}

#[test]
fn linked_activity_uses_insertion_order_when_event_timestamps_match() {
    use hb_core::{PollingActivity, PollingActivityKind};
    for last_boost in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        let events = if last_boost {
            [PollingActivityKind::Restore, PollingActivityKind::Boost]
        } else {
            [PollingActivityKind::Boost, PollingActivityKind::Restore]
        };
        for kind in events {
            store
                .record_polling_activity(&PollingActivity {
                    key: "synthetic-a".into(),
                    timestamp: 0,
                    session: 1,
                    kind,
                    rate: PollingRate::try_from(1000).unwrap(),
                    previous: PollingRate::try_from(500).ok(),
                })
                .unwrap();
        }
        for ts in [0, 60, 120] {
            store
                .record_usage(&observation(ts, 90, Some(1000), Some(1)))
                .unwrap();
        }
        store.flush().unwrap();
        let data = store.query_insights("synthetic-a", 120).unwrap();
        assert_eq!(data.recent.boost_seconds, if last_boost { 120 } else { 0 });
        assert_eq!(data.coverage.awake_seconds, 120);
    }
}

#[test]
fn malformed_linked_activity_does_not_hide_valid_battery_readings() {
    for kind in ["X'FF'", "X'626F6F7374'"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let mut store = Store::open(&path).unwrap();
        for ts in [0, 60, 120] {
            store
                .record_usage(&observation(ts, 90, Some(1000), Some(1)))
                .unwrap();
        }
        store.flush().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                &format!(
                    "INSERT INTO polling_events VALUES ('synthetic-a','1',0,{kind},1000,NULL)"
                ),
                [],
            )
            .unwrap();
        let data = store.query_insights("synthetic-a", 120).unwrap();
        assert_eq!(data.coverage.observation_count, 3);
        assert_eq!(data.coverage.awake_seconds, 120);
        assert_eq!(data.recent.boost_sessions, 0);
        assert_eq!(data.recent.manual_changes, 0);
    }
}
#[test]
fn malformed_payload_types_break_usage_continuity_without_breaking_queries() {
    for payload in ["X'FF'", "X'7B7D'", "CAST(X'FF' AS TEXT)"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let mut store = Store::open(&path).unwrap();
        for ts in [0, 60, 120] {
            store
                .record_usage(&observation(ts, 90, Some(1000), Some(1)))
                .unwrap();
        }
        store.flush().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                &format!("UPDATE readings SET payload={payload} WHERE ts=60"),
                [],
            )
            .unwrap();
        let data = store.query_insights("synthetic-a", 120).unwrap();
        assert_eq!(data.coverage.unreadable_row_count, 1);
        assert_eq!(data.coverage.awake_seconds, 0);
        assert_eq!(
            store
                .query_usage("synthetic-a", 120, 120, 500)
                .unwrap()
                .until,
            0
        );
        assert_eq!(store.query("synthetic-a", 0, 120, 500).unwrap().len(), 2);
        assert_eq!(
            store
                .query_with_baseline("synthetic-a", 90, 120, 500)
                .unwrap()[0]
                .timestamp,
            0
        );
    }
}
#[test]
fn malformed_polling_events_do_not_hide_valid_history_or_inflate_activity_counts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for ts in [0, 60, 120] {
        store
            .record_usage(&observation(ts, 90, Some(1000), Some(1)))
            .unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO polling_events VALUES
        ('synthetic-a','1',1,'manual',1000,NULL),
        ('synthetic-a','2',2,'boost',2000,1000),
        ('synthetic-a','3',3,'restore',1000,2000),
        ('synthetic-a','4',4,'boost',-1,1000),
        ('synthetic-a','5',5,'boost','damaged',1000),
        ('synthetic-a','6',6,'boost',1234,1000),
        ('synthetic-a','7',7,'boost',4000,'damaged'),
        ('synthetic-a','8',8,X'FF',4000,1000),
        ('synthetic-a','9',9,'restore',-1,2000),
        ('synthetic-a','10',10,'manual',1234,NULL),
        ('synthetic-a','11',11,'boost',4000,1234),
        ('synthetic-a','12',12,'boost',4000,0);",
    )
    .unwrap();
    let insights = store.query_insights("synthetic-a", 120).unwrap();
    assert_eq!(insights.recent.total_seconds, 120);
    assert_eq!(insights.recent.manual_changes, 1);
    assert_eq!(insights.recent.boost_sessions, 1);
    assert_eq!(insights.recent.restores, 1);
    assert_eq!(
        insights.recent.normal_rate,
        PollingRate::try_from(1000).ok()
    );
}

#[test]
fn pending_metadata_survives_drop_restart_and_rate_session_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let samples = [
        observation(0, 90, Some(1000), Some(u64::MAX)),
        observation(1, 90, Some(500), Some(u64::MAX)),
        observation(2, 90, Some(500), Some(7)),
        observation(62, 89, Some(500), Some(7)),
        observation(122, 88, Some(500), Some(7)),
    ];
    {
        let mut store = Store::open(&path).unwrap();
        for sample in &samples {
            store.record_usage(sample).unwrap();
        }
        // Repeated polls in the same state do not create additional rows.
        store
            .record_usage(&observation(123, 88, Some(500), Some(7)))
            .unwrap();
    }
    let db = Connection::open(&path).unwrap();
    let stored: (i64, String) = db
        .query_row(
            "SELECT polling_rate,session FROM usage_metadata WHERE ts=0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, (1000, u64::MAX.to_string()));
    let store = Store::read_only(&path).unwrap();
    assert_eq!(
        store.query("synthetic-a", 0, 200, 100).unwrap().len(),
        samples.len()
    );
    let mut expected = InsightsBuilder::default().recent_since(200 - 7 * 86400);
    for sample in samples {
        expected.push(sample);
    }
    let expected = expected.finish();
    let actual = store.query_insights("synthetic-a", 200).unwrap();
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    assert!(
        store
            .query_insights("synthetic-other", 200)
            .unwrap()
            .rates
            .is_empty()
    );
}

#[test]
fn same_second_metadata_replacement_and_legacy_record_clear_evidence_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    store
        .record_usage(&observation(100, 80, Some(1000), Some(1)))
        .unwrap();
    store
        .record_usage(&observation(100, 80, Some(500), Some(2)))
        .unwrap();
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    let metadata: (i64, String) = db
        .query_row(
            "SELECT polling_rate,session FROM usage_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metadata, (500, "2".into()));
    store
        .record(&observation(100, 80, None, None).reading)
        .unwrap();
    store.flush().unwrap();
    let metadata: (Option<i64>, Option<String>) = db
        .query_row(
            "SELECT polling_rate,session FROM usage_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metadata, (None, None));
    assert_eq!(store.query("synthetic-a", 0, 200, 100).unwrap().len(), 1);
}

fn legacy_database(path: &std::path::Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE readings(device TEXT NOT NULL, ts INTEGER NOT NULL, level INTEGER, payload TEXT NOT NULL, PRIMARY KEY(device,ts)); CREATE TABLE state(key TEXT PRIMARY KEY,payload TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    for sample in [
        observation(0, 90, None, None),
        observation(60, 89, None, None),
    ] {
        db.execute(
            "INSERT INTO readings VALUES(?1,?2,?3,?4)",
            params![
                sample.reading.key,
                sample.reading.timestamp,
                sample.reading.level,
                serde_json::to_string(&sample.reading).unwrap()
            ],
        )
        .unwrap();
    }
}

#[test]
fn old_database_is_readable_without_upgrade_and_upgrades_preserving_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    legacy_database(&path);
    {
        let store = Store::read_only(&path).unwrap();
        assert_eq!(store.query("synthetic-a", 0, 60, 100).unwrap().len(), 2);
        assert!(
            store
                .query_insights("synthetic-a", 60)
                .unwrap()
                .rates
                .is_empty()
        );
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.query("synthetic-a", 0, 60, 100).unwrap().len(), 2);
    store
        .record_usage(&observation(120, 88, Some(1000), Some(1)))
        .unwrap();
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM usage_metadata", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn prune_flushes_pending_and_removes_metadata_at_retention_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for (ts, level) in [(99, 80), (100, 81), (160, 82)] {
        store
            .record_usage(&observation(ts, level, Some(1000), Some(1)))
            .unwrap();
    }
    store.prune(30 * 86400 + 100).unwrap();
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT min(ts) FROM readings", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        100
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM usage_metadata", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let actual = store
        .query_insights("synthetic-a", 30 * 86400 + 101)
        .unwrap();
    let mut expected = InsightsBuilder::default().recent_since(30 * 86400 + 101 - 7 * 86400);
    expected.push(observation(160, 82, Some(1000), Some(1)));
    assert_eq!(format!("{actual:?}"), format!("{:?}", expected.finish()));
}

#[test]
fn retained_raw_rows_reach_aggregation_without_chart_sampling() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    let mut expected = InsightsBuilder::default().recent_since(43_199 * 60 - 7 * 86400);
    for index in 0..43_200i64 {
        // Full month, monotonically repeated discharge segments, bounded builder output.
        let sample = observation(index * 60, (100 - index % 100) as u8, Some(1000), Some(1));
        store.record_usage(&sample).unwrap();
        expected.push(sample);
    }
    store.flush().unwrap();
    let actual = store.query_insights("synthetic-a", 43_199 * 60).unwrap();
    assert_eq!(format!("{actual:?}"), format!("{:?}", expected.finish()));
    assert!(
        store
            .query("synthetic-a", 0, 43_199 * 60, 20)
            .unwrap()
            .len()
            <= 20
    );
}

#[test]
fn metadata_failure_rolls_back_reading_and_preserves_pending_for_retry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_metadata BEFORE INSERT ON usage_metadata BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
    store
        .record_usage(&observation(100, 80, Some(1000), Some(1)))
        .unwrap();
    assert!(store.flush().is_err());
    assert!(store.query("synthetic-a", 0, 200, 100).unwrap().is_empty());
    db.execute_batch("DROP TRIGGER reject_metadata;").unwrap();
    store.flush().unwrap();
    assert_eq!(store.query("synthetic-a", 0, 200, 100).unwrap().len(), 1);
    assert_eq!(
        db.query_row("SELECT count(*) FROM usage_metadata", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn corrupt_raw_rows_break_insight_continuity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    let first = observation(0, 90, Some(1000), Some(1));
    let last = observation(120, 88, Some(1000), Some(1));
    store.record_usage(&first).unwrap();
    store.record_usage(&last).unwrap();
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO readings VALUES('synthetic-a',60,89,'corrupt')",
        [],
    )
    .unwrap();
    let mut expected = InsightsBuilder::default().recent_since(120 - 7 * 86400);
    expected.push(first);
    expected.break_continuity();
    expected.push(last);
    let actual = store.query_insights("synthetic-a", 120).unwrap();
    assert_eq!(format!("{actual:?}"), format!("{:?}", expected.finish()));
}

#[test]
fn malformed_session_metadata_cannot_become_a_continuous_unknown_session() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for ts in [0, 60, 120] {
        store
            .record_usage(&observation(ts, 90 - (ts / 60) as u8, Some(1000), Some(1)))
            .unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE usage_metadata SET session='damaged' WHERE ts IN (0,60)",
        [],
    )
    .unwrap();
    let result = store.query_insights("synthetic-a", 120).unwrap();
    assert_eq!(result.coverage.unreadable_row_count, 2);
    assert_eq!(result.coverage.awake_seconds, 0);
    assert_eq!(result.coverage.observation_count, 1);
    assert!(result.rates.is_empty());
}

#[test]
fn malformed_polling_metadata_breaks_continuity_instead_of_becoming_legacy_usage() {
    for value in ["-1", "3000", "'broken'", "1000.5", "X'FF'"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let mut store = Store::open(&path).unwrap();
        for ts in [0, 60, 120] {
            store
                .record_usage(&observation(ts, 90 - (ts / 60) as u8, Some(1000), Some(1)))
                .unwrap();
        }
        store.flush().unwrap();
        let db = Connection::open(path).unwrap();
        db.execute(
            &format!("UPDATE usage_metadata SET polling_rate={value} WHERE ts IN (0,60)"),
            [],
        )
        .unwrap();
        let data = store.query_insights("synthetic-a", 120).unwrap();
        assert_eq!(data.coverage.unreadable_row_count, 2, "{value}");
        assert_eq!(data.coverage.awake_seconds, 0, "{value}");
        assert!(data.rates.is_empty());
    }
}

#[test]
fn old_rate_evidence_is_historical_not_current_remaining_use() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for i in 0..=4 {
        store
            .record_usage(&observation(i * 600, 80 - i as u8, Some(1000), Some(1)))
            .unwrap();
    }
    store.flush().unwrap();
    let fresh = store.query_insights("synthetic-a", 2400).unwrap();
    let old = store.query_insights("synthetic-a", 3100).unwrap();
    assert!(fresh.rates[0].remaining_hours.is_some());
    assert!(old.rates[0].remaining_hours.is_none());
    assert_eq!(
        fresh.rates[0].projected_full_charge_hours,
        old.rates[0].projected_full_charge_hours
    );
    assert_eq!(old.coverage.observation_count, 5);
    assert_eq!(old.coverage.last_reading_timestamp, Some(2400));
}

#[test]
fn confirmed_events_are_scoped_idempotent_and_not_inferred_for_old_data() {
    use hb_core::{PollingActivity, PollingActivityKind as Kind};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    let event = PollingActivity {
        key: "synthetic-a".into(),
        timestamp: 0,
        session: 1,
        kind: Kind::Boost,
        rate: PollingRate::try_from(2000).unwrap(),
        previous: PollingRate::try_from(1000).ok(),
    };
    store.record_polling_activity(&event).unwrap();
    store.record_polling_activity(&event).unwrap();
    for (ts, session) in [(0, 1), (60, 1), (120, 1), (180, 1), (240, 2), (300, 2)] {
        store
            .record_usage(&observation(ts, 90, Some(2000), Some(session)))
            .unwrap();
    }
    store
        .record_polling_activity(&PollingActivity {
            timestamp: 120,
            kind: Kind::Manual,
            ..event.clone()
        })
        .unwrap();
    store
        .record_polling_activity(&PollingActivity {
            timestamp: 1000,
            kind: Kind::Restore,
            ..event.clone()
        })
        .unwrap();
    store.flush().unwrap();
    let data = store.query_insights("synthetic-a", 300).unwrap();
    assert_eq!(data.recent.boost_sessions, 1);
    assert_eq!(data.recent.manual_changes, 1);
    assert_eq!(data.recent.restores, 0);
    assert_eq!(data.recent.boost_seconds, 60);
    assert_eq!(data.recent.total_seconds, 240);
    assert_eq!(data.recent.normal_rate, PollingRate::try_from(1000).ok());
    assert_eq!(
        store
            .query_insights("other", 300)
            .unwrap()
            .recent
            .boost_sessions,
        0
    );
    store.prune(30 * 86400 + 1001).unwrap();
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM polling_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn recent_window_clips_intervals_without_changing_long_term_learning() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(&directory.path().join("history.db")).unwrap();
    for ts in [0, 60, 120, 180] {
        store
            .record_usage(&observation(ts, 90, Some(1000), Some(1)))
            .unwrap();
    }
    store.flush().unwrap();
    let data = store.query_insights("synthetic-a", 7 * 86400 + 90).unwrap();
    assert_eq!(data.recent.total_seconds, 90);
    assert_eq!(data.coverage.awake_seconds, 180);
    assert_eq!(data.recent.current_level, None);
    assert_eq!(data.recent.boost_sessions, 0);
}

#[test]
fn same_rate_manual_choice_ends_boost_ownership_without_inflating_change_counts() {
    use hb_core::{PollingActivity, PollingActivityKind};
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(&directory.path().join("history.db")).unwrap();
    let rate = PollingRate::try_from(2000).unwrap();
    for timestamp in [0, 60, 120, 180, 240] {
        store
            .record_usage(&observation(timestamp, 90, Some(2000), Some(1)))
            .unwrap();
    }
    let event = PollingActivity {
        key: "synthetic-a".into(),
        timestamp: 0,
        session: 1,
        kind: PollingActivityKind::Boost,
        rate,
        previous: PollingRate::try_from(1000).ok(),
    };
    store.record_polling_activity(&event).unwrap();
    store
        .record_polling_activity(&PollingActivity {
            timestamp: 120,
            kind: PollingActivityKind::Manual,
            previous: Some(rate),
            ..event.clone()
        })
        .unwrap();
    store.flush().unwrap();
    let data = store.query_insights("synthetic-a", 240).unwrap();
    assert_eq!(data.recent.boost_sessions, 1);
    assert_eq!(data.recent.manual_changes, 0);
    assert_eq!(data.recent.boost_seconds, 60);
    assert_eq!(data.recent.total_seconds, 240);
}
