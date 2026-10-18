#[path = "../../core/tests/support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/usage_reference.rs"]
mod usage_reference;
use allocation_counter::{TrackingAllocator, measure};
use hb_core::{
    BatteryInsights, Engine, Estimator, HistoryStore, InsightsBuilder, PollingRate, Reading,
    Settings, UsageObservation,
};
use hb_storage::{Store, write_status};
use rusqlite::{Connection, params};

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

#[test]
fn recurring_history_samples_reuse_existing_map_keys() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(&directory.path().join("history.db")).unwrap();
    let observations: Vec<_> = (0..=1000)
        .map(|minute| UsageObservation {
            reading: reading(minute * 60, 80),
            polling_rate: PollingRate::try_from(1000).ok(),
            session: Some(1),
        })
        .collect();
    store.record_usage(&observations[0]).unwrap();
    let (_, copies) = measure(|| {
        for observation in &observations[1..] {
            std::hint::black_box(observation.clone());
            std::hint::black_box(observation.clone());
        }
    });
    let (_, actual) = measure(|| {
        for observation in &observations[1..] {
            store.record_usage(observation).unwrap();
        }
    });
    println!("1,000 history samples: required observation copies={copies:?}, actual={actual:?}");
    // Allow bounded growth of the pending batch, but no repeated key allocation.
    assert!(
        actual.calls <= copies.calls + 16,
        "copies={copies:?}; actual={actual:?}"
    );
    store.flush().unwrap();
    let restored = store.query("invented", 0, 60_000, 2000).unwrap();
    assert_eq!(restored.len(), 1001);
    assert_eq!(restored.last(), Some(&observations[1000].reading));
}

#[test]
fn sampled_usage_matches_previous_output_with_bounded_allocation_traffic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for minute in 0..1440 {
        let mut sample = reading(minute * 60, (100 - minute % 100) as u8);
        if minute % 53 == 0 {
            sample.connection = hb_core::Connection::Sleeping;
        }
        if minute % 61 == 0 {
            sample.charging = Some(true);
        }
        if minute % 83 == 0 {
            sample.level = None;
        }
        store
            .record_usage(&UsageObservation {
                reading: sample,
                polling_rate: PollingRate::try_from(1000).ok(),
                session: Some((minute / 100) as u64),
            })
            .unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute("UPDATE readings SET payload='{}' WHERE ts=4200", [])
        .unwrap();
    db.execute(
        "UPDATE usage_metadata SET session='damaged' WHERE ts=8400",
        [],
    )
    .unwrap();
    let until = 1439 * 60;
    for seconds in [60, 12 * 3600, 30 * 86400] {
        for width in [2, 3, 5, 64, 513] {
            let actual = store
                .query_usage("invented", until, seconds, width)
                .unwrap();
            let expected =
                usage_reference::query_usage(&db, "invented", until, seconds, width).unwrap();
            assert_eq!(
                format!("{actual:?}"),
                format!("{expected:?}"),
                "seconds={seconds}, width={width}"
            );
        }
    }
    let (_, sampled) = measure(|| {
        store
            .query_usage("invented", until, 12 * 3600, 128)
            .unwrap()
    });
    let (_, previous) =
        measure(|| usage_reference::query_usage(&db, "invented", until, 12 * 3600, 128).unwrap());
    assert!(
        sampled.calls * 3 < previous.calls,
        "sampled={sampled:?}, previous={previous:?}"
    );
    assert!(
        sampled.bytes < previous.bytes,
        "sampled={sampled:?}, previous={previous:?}"
    );
    println!("1,440-row usage graph: sampled={sampled:?}, previous={previous:?}");
}

#[test]
#[ignore = "explicit 30-day history allocation measurement; no hardware"]
fn thirty_day_usage_sampling_allocations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for minute in 0..43_200 {
        let mut sample = reading(minute * 60, (100 - (minute % 3000) / 30) as u8);
        sample.name = "Synthetic mouse".into();
        store
            .record_usage(&UsageObservation {
                reading: sample,
                polling_rate: PollingRate::try_from(1000).ok(),
                session: Some(1),
            })
            .unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(path).unwrap();
    db.execute_batch("PRAGMA cache_size=-512;").unwrap();
    let until = 43_199 * 60;
    let expected = usage_reference::query_usage(&db, "invented", until, 12 * 3600, 1000).unwrap();
    let actual = store
        .query_usage("invented", until, 12 * 3600, 1000)
        .unwrap();
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    let (_, sampled) = measure(|| {
        store
            .query_usage("invented", until, 12 * 3600, 1000)
            .unwrap()
    });
    let (_, previous) =
        measure(|| usage_reference::query_usage(&db, "invented", until, 12 * 3600, 1000).unwrap());
    println!("43,200-row usage graph allocations: previous={previous:?}; sampled={sampled:?}");
    assert!(sampled.calls * 10 < previous.calls);
    assert!(sampled.bytes * 5 < previous.bytes);
}

#[test]
fn checkpoint_load_avoids_json_tree_and_recovers_valid_neighbours() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let store = Store::open(&path).unwrap();
    let db = Connection::open(path).unwrap();
    let now = hb_core::Clock::unix(&hb_core::SystemClock::default());
    let devices = (0..32).map(|i| (format!("invented-{i}"), serde_json::json!({
        "seen":now,"usage":60000.0,"samples":(0..100).map(|i| (i as f64*600.0,100-i)).collect::<Vec<_>>()
    }))).collect::<std::collections::BTreeMap<_,_>>();
    let mut value = serde_json::json!({"devices":devices});
    let payload = value.to_string();
    db.execute("INSERT INTO state VALUES('estimator',?1)", [&payload])
        .unwrap();
    store.load_estimator();
    let (loaded, direct) = measure(|| store.load_estimator());
    let (previous, tree) =
        measure(|| Estimator::from_value(serde_json::from_str(&payload).unwrap(), now));
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(previous).unwrap()
    );
    assert!(
        direct.calls * 2 < tree.calls,
        "direct={direct:?}, tree={tree:?}"
    );
    assert!(
        direct.bytes * 2 < tree.bytes,
        "direct={direct:?}, tree={tree:?}"
    );
    println!("32-device checkpoint: direct={direct:?}, tree={tree:?}");
    value["devices"]["broken"] = serde_json::json!({"samples":"damaged"});
    db.execute("UPDATE state SET payload=?1", [value.to_string()])
        .unwrap();
    assert_eq!(
        serde_json::to_value(store.load_estimator()).unwrap(),
        serde_json::to_value(loaded).unwrap()
    );
    // Skip oversized checkpoints without rewriting the original row or history.
    db.execute("UPDATE state SET payload=zeroblob(4194305)", [])
        .unwrap();
    let (empty, skipped) = measure(|| store.load_estimator());
    assert!(empty.devices.is_empty());
    assert!(
        skipped.bytes < 65536,
        "oversized checkpoint was materialised: {skipped:?}"
    );
    assert_eq!(
        db.query_row("SELECT length(payload) FROM state", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        4194305
    );
}

#[test]
fn boost_attribution_does_not_allocate_a_string_per_history_sample() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for i in 0..1440 {
        store
            .record_usage(&UsageObservation {
                reading: reading(i * 60, (100 - i % 100) as u8),
                polling_rate: PollingRate::try_from(4000).ok(),
                session: Some(1),
            })
            .unwrap();
    }
    store.flush().unwrap();
    store.query_insights("invented", 1439 * 60).unwrap();
    let (_, without) = measure(|| store.query_insights("invented", 1439 * 60).unwrap());
    store
        .record_polling_activity(&hb_core::PollingActivity {
            key: "invented".into(),
            timestamp: 0,
            session: 1,
            kind: hb_core::PollingActivityKind::Boost,
            rate: PollingRate::try_from(4000).unwrap(),
            previous: PollingRate::try_from(1000).ok(),
        })
        .unwrap();
    let (result, with) = measure(|| store.query_insights("invented", 1439 * 60).unwrap());
    assert_eq!(result.recent.boost_sessions, 1);
    assert!(result.recent.boost_seconds > 0);
    assert!(
        with.calls <= without.calls + 20,
        "with boost={with:?}, without={without:?}"
    );
    println!("1,440-row Insights allocations: with boost={with:?}, without={without:?}");
}

fn reading(timestamp: i64, level: u8) -> Reading {
    let mut reading = Reading::new("invented", "Invented \"mouse\" 雪\n", "test", timestamp);
    reading.level = Some(level);
    reading.charging = Some(false);
    reading.via = "receiver".into();
    reading.serial = Some("invented-unit".into());
    reading
}

fn owned_payload_reference(db: &Connection, until: i64) -> BatteryInsights {
    let mut query = db.prepare("SELECT r.ts,r.payload,m.polling_rate,m.session FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts").unwrap();
    let mut rows = query
        .query(params!["invented", until - 30 * 86400, until])
        .unwrap();
    let mut builder = InsightsBuilder::default().recent_since(until.saturating_sub(7 * 86400));
    while let Some(row) = rows.next().unwrap() {
        let timestamp: i64 = row.get(0).unwrap();
        let payload: String = row.get(1).unwrap();
        let Ok(reading) = serde_json::from_str::<Reading>(&payload) else {
            builder.break_continuity();
            continue;
        };
        if reading.key != "invented" || reading.timestamp != timestamp {
            builder.break_continuity();
            continue;
        }
        let polling_rate = row
            .get::<_, Option<u32>>(2)
            .ok()
            .flatten()
            .and_then(|hz| PollingRate::try_from(hz).ok());
        let session = match row.get::<_, Option<String>>(3) {
            Ok(None) => None,
            Ok(Some(value)) => match value.parse().ok() {
                Some(session) => Some(session),
                None => {
                    builder.break_continuity();
                    continue;
                }
            },
            Err(_) => {
                builder.break_continuity();
                continue;
            }
        };
        builder.push(UsageObservation {
            reading,
            polling_rate,
            session,
        });
    }
    builder.finish()
}

#[test]
fn borrowed_sqlite_text_removes_payload_and_session_copies_with_identical_insights() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for index in 0..1440 {
        store
            .record_usage(&UsageObservation {
                reading: reading(index * 60, (100 - index % 100) as u8),
                polling_rate: Some(PollingRate::try_from(1000).unwrap()),
                session: Some(u64::MAX),
            })
            .unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    // A malformed unrelated required field must break adjacency too.
    let mut malformed = serde_json::to_value(reading(300 * 60, 100)).unwrap();
    malformed["source"] = serde_json::json!(42);
    db.execute(
        "UPDATE readings SET payload=?1 WHERE ts=?2",
        params![malformed.to_string(), 300 * 60],
    )
    .unwrap();
    db.execute(
        "UPDATE usage_metadata SET session='not-a-session' WHERE ts=24000",
        [],
    )
    .unwrap();
    let until = 1439 * 60;
    // Warm statement/schema work before comparing Rust-owned allocation traffic.
    store.query_insights("invented", until).unwrap();
    owned_payload_reference(&db, until);
    let (actual, borrowed) = measure(|| store.query_insights("invented", until).unwrap());
    let (expected, owned) = measure(|| owned_payload_reference(&db, until));
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    assert!(
        owned.calls >= borrowed.calls + 2 * 1440 - 4,
        "payload and session copies: owned={owned:?}, borrowed={borrowed:?}"
    );
    assert!(
        owned.bytes > borrowed.bytes + 200_000,
        "owned={owned:?}, borrowed={borrowed:?}"
    );
    println!("1,440 Insights rows: owned-text={owned:?}, borrowed-text={borrowed:?}");
}

#[test]
fn borrowed_history_queries_preserve_escaped_strings_and_corrupt_row_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for (timestamp, level) in [(0, 90), (60, 89), (120, 88), (180, 87), (240, 86)] {
        store.record(&reading(timestamp, level)).unwrap();
    }
    store.flush().unwrap();
    let db = Connection::open(&path).unwrap();
    let mut malformed = serde_json::to_value(reading(120, 88)).unwrap();
    malformed["kind"] = serde_json::json!([]);
    db.execute(
        "UPDATE readings SET payload=?1 WHERE ts=120",
        [malformed.to_string()],
    )
    .unwrap();
    let raw = store.query("invented", 0, 240, 100).unwrap();
    assert_eq!(
        raw.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
        [0, 60, 180, 240]
    );
    assert!(raw.iter().all(|r| r.name == "Invented \"mouse\" 雪\n"));
    let usage = store.query_usage("invented", 240, 3600, 100).unwrap();
    assert_eq!(usage.until, 120); // malformed row excludes both adjacent intervals
    let baseline = store
        .query_with_baseline("invented", 121, 240, 100)
        .unwrap();
    assert_eq!(baseline[0].timestamp, 60);
    assert_eq!(baseline[0].name, raw[0].name);
    let endpoints = store.query("invented", 0, 240, 2).unwrap();
    assert_eq!(
        endpoints.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
        [0, 240]
    );
    // A damaged SQLite value also breaks adjacency; valid neighbours remain usable.
    db.execute("UPDATE readings SET payload=X'FF' WHERE ts=60", [])
        .unwrap();
    let insights = store.query_insights("invented", 240).unwrap();
    assert_eq!(insights.coverage.unreadable_row_count, 2);
    assert_eq!(insights.coverage.awake_seconds, 60);
    assert_eq!(
        store.query_usage("invented", 240, 3600, 100).unwrap().until,
        60
    );
    assert_eq!(
        store.query_with_baseline("invented", 61, 240, 100).unwrap()[0].timestamp,
        0
    );
}

#[test]
fn batched_history_flush_preserves_rows_with_bounded_serialization_allocations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for i in 0..512 {
        store
            .record_usage(&UsageObservation {
                reading: reading(i * 60, (100 - i % 100) as u8),
                polling_rate: Some(PollingRate::try_from(1000).unwrap()),
                session: Some(u64::MAX),
            })
            .unwrap();
    }
    let (_, allocations) = measure(|| store.flush().unwrap());
    println!("512-row flush: {allocations:?}");
    assert!(
        allocations.calls <= 16 && allocations.bytes <= 4096,
        "Batch serialization must reuse its buffers: {allocations:?}"
    );
    let db = Connection::open(path).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM readings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 512);
    let restored = store.query("invented", 0, 511 * 60, 1024).unwrap();
    assert_eq!(restored.len(), 512);
    assert_eq!(restored[511], reading(511 * 60, 89));
    let session: String = db
        .query_row(
            "SELECT session FROM usage_metadata WHERE ts=30660",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session, u64::MAX.to_string());
}

#[test]
fn borrowed_status_serialization_preserves_schema_without_a_value_tree() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    let readings = (0..64)
        .map(|index| {
            let mut r = reading(60, 80);
            r.key = format!("invented-{index}");
            r.kind = "mouse".into();
            r
        })
        .collect();
    engine.apply("test", Ok(readings), 1.0, false);
    let snapshot = engine.snapshot(60);
    let old_status = || {
        let devices: Vec<_> = snapshot.devices.iter().filter(|d| !d.hidden).map(|d| serde_json::json!({
            "key":d.reading.key,"name":d.name,"level":d.reading.level,"charging":d.reading.charging.unwrap_or(false),
            "online":d.reading.online(),"kind":d.icon,"approx":d.reading.approx,"low_alert_at":d.low_alert_at,
            "seconds_left":d.seconds_left,"text":d.text
        })).collect();
        serde_json::to_vec(&serde_json::json!({"app":"Halo Continued","version":env!("CARGO_PKG_VERSION"),"running":true,
            "updated_unix":60,"updated":"1970-01-01T00:01:00Z","devices":devices})).unwrap()
    };
    let path = directory.path().join("status.json");
    write_status(&path, &snapshot, true).unwrap();
    let (_, optimized) = measure(|| write_status(&path, &snapshot, true).unwrap());
    let actual: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let (expected, original) = measure(|| {
        let bytes = old_status();
        hb_storage::atomic_write(&path, &bytes).unwrap();
        bytes
    });
    assert_eq!(
        actual,
        serde_json::from_slice::<serde_json::Value>(&expected).unwrap()
    );
    // Both counts include the same atomic filesystem export, including fsync.
    assert!(
        optimized.calls * 2 < original.calls,
        "typed={optimized:?}, value-tree={original:?}"
    );
    assert!(
        optimized.bytes < original.bytes,
        "typed={optimized:?}, value-tree={original:?}"
    );
    println!("64-device status: typed={optimized:?}, value-tree={original:?}");
}
