use hb_core::{Connection, HistoryStore, Precision, Reading, UsageObservation};
use hb_storage::Store;

fn sample(timestamp: i64, session: Option<u64>) -> UsageObservation {
    let mut reading = Reading::new("synthetic", "Synthetic", "test", timestamp);
    reading.level = Some(80);
    reading.charging = Some(false);
    UsageObservation {
        reading,
        polling_rate: None,
        session,
    }
}

#[test]
fn session_boundary_survives_same_second_availability_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(&directory.path().join("history.db")).unwrap();
    store.record_usage(&sample(0, Some(1))).unwrap();
    let mut pause = sample(60, Some(2));
    pause.reading.connection = Connection::Sleeping;
    store.record_usage(&pause).unwrap();
    store.record_usage(&sample(60, Some(2))).unwrap();
    store.record_usage(&sample(120, Some(2))).unwrap();
    store.flush().unwrap();
    let usage = store.query_usage("synthetic", 120, 3600, 100).unwrap();
    assert_eq!(usage.until, 60);
    assert_eq!(usage.samples.last().unwrap().position, 60);
}

#[test]
fn usage_requires_known_discharge_and_observed_short_intervals() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(&directory.path().join("history.db")).unwrap();
    let mut observations = vec![
        sample(0, None),
        sample(60, None),
        sample(120, None),
        sample(180, None),
        sample(240, None),
        sample(300, None),
        sample(360, None),
        sample(10000, None),
        sample(10060, None),
    ];
    observations[1].reading.charging = None;
    observations[3].reading.charging_inferred = true;
    observations[5].reading.precision = Precision::Coarse;
    for observation in observations {
        store.record_usage(&observation).unwrap();
    }
    store.flush().unwrap();
    // Unknown/inferred charging excludes both neighbors; coarse confirmed
    // availability remains usable; the unobserved long gap contributes zero.
    assert_eq!(
        store
            .query_usage("synthetic", 10060, 3600, 100)
            .unwrap()
            .until,
        180
    );
}

#[test]
fn malformed_session_breaks_adjacency_without_losing_valid_display_samples() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for timestamp in [0, 60, 120, 180] {
        let mut observation = sample(timestamp, Some(1));
        if timestamp == 60 {
            observation.reading.level = Some(70);
        }
        store.record_usage(&observation).unwrap();
    }
    store.flush().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE usage_metadata SET session='invalid' WHERE ts=60",
        [],
    )
    .unwrap();
    let usage = store.query_usage("synthetic", 180, 3600, 100).unwrap();
    assert_eq!(usage.until, 60);
    assert!(
        usage
            .samples
            .iter()
            .any(|sample| sample.reading.timestamp == 60)
    );
}

#[test]
fn legacy_database_without_metadata_retains_explicitly_observed_usage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE readings(device TEXT NOT NULL,ts INTEGER NOT NULL,level INTEGER,payload TEXT NOT NULL,PRIMARY KEY(device,ts));").unwrap();
    for timestamp in [0, 60] {
        let reading = sample(timestamp, None).reading;
        db.execute(
            "INSERT INTO readings VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                reading.key,
                reading.timestamp,
                reading.level,
                serde_json::to_string(&reading).unwrap()
            ],
        )
        .unwrap();
    }
    let store = Store::read_only(&path).unwrap();
    assert_eq!(
        store.query_usage("synthetic", 60, 3600, 100).unwrap().until,
        60
    );
}
