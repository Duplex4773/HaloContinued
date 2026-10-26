use hb_core::*;
use hb_storage::Store;
use rusqlite::Connection;
fn reading(timestamp: i64, level: u8) -> Reading {
    let mut r = Reading::new(
        "synthetic:mouse",
        "Synthetic mouse",
        "simulation",
        timestamp,
    );
    r.level = Some(level);
    r.charging = Some(false);
    r.kind = "mouse".into();
    r
}
#[test]
fn same_second_transitions_keep_their_own_rate_and_session_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let mut store = Store::open(&path).unwrap();
    for (r, hz, session) in [
        (reading(100, 50), 1000, 1),
        (reading(160, 49), 1000, 1),
        (
            {
                let mut r = reading(160, 49);
                r.connection = hb_core::Connection::Sleeping;
                r
            },
            125,
            2,
        ),
    ] {
        store
            .record_usage(&UsageObservation {
                reading: r,
                polling_rate: PollingRate::try_from(hz).ok(),
                session: Some(session),
            })
            .unwrap();
    }
    store.flush().unwrap();
    let data = store.query("synthetic:mouse", 100, 160, 20).unwrap();
    assert_eq!(data.len(), 3);
    assert!(data[1].online());
    assert_eq!(data[2].connection, hb_core::Connection::Sleeping);
    let insights = store.query_insights("synthetic:mouse", 160).unwrap();
    assert_eq!(insights.coverage.observation_count, 3);
    assert_eq!(insights.coverage.awake_seconds, 60);
    assert_eq!(insights.rates.len(), 1);
    assert_eq!(insights.rates[0].hz, 1000);
    assert_eq!(
        store
            .query_usage("synthetic:mouse", 160, 3600, 20)
            .unwrap()
            .until,
        60
    );
    drop(store);
    let db = Connection::open(&path).unwrap();
    let metadata: Vec<(i64, String)> = db
        .prepare("SELECT polling_rate,session FROM usage_metadata ORDER BY ts,sequence")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        metadata,
        [(1000, "1".into()), (1000, "1".into()), (125, "2".into())]
    );
}
#[test]
fn version_three_history_migrates_atomically_and_read_only_legacy_queries_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE readings(device TEXT,ts INTEGER,level INTEGER,payload TEXT,PRIMARY KEY(device,ts)); CREATE INDEX readings_time ON readings(ts); CREATE TABLE usage_metadata(device TEXT,ts INTEGER,polling_rate INTEGER,session TEXT,PRIMARY KEY(device,ts)); CREATE TABLE state(key TEXT PRIMARY KEY,payload TEXT); PRAGMA user_version=3;").unwrap();
    let r = reading(100, 50);
    db.execute(
        "INSERT INTO readings(device,ts,level,payload) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            r.key,
            r.timestamp,
            r.level,
            serde_json::to_string(&r).unwrap()
        ],
    )
    .unwrap();
    db.execute("INSERT INTO usage_metadata(device,ts,polling_rate,session) VALUES('synthetic:mouse',100,1000,'1')",[]).unwrap();
    db.execute("INSERT INTO state VALUES('retained','original')", [])
        .unwrap();
    drop(db);
    assert_eq!(
        Store::read_only(&path)
            .unwrap()
            .query_insights(&r.key, 100)
            .unwrap()
            .coverage
            .observation_count,
        1
    );
    let mut store = Store::open(&path).unwrap();
    let mut next = r.clone();
    next.level = Some(49);
    store.record(&next).unwrap();
    store.flush().unwrap();
    assert_eq!(store.query(&r.key, 100, 100, 20).unwrap().len(), 2);
    drop(store);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row("SELECT payload FROM state WHERE key='retained'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "original"
    );
    assert_eq!(
        db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn saver_changes_and_restorations_have_distinct_insights_activity_and_duration() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("history.db")).unwrap();
    for (ts, hz, session, kind, previous) in [
        (100, 125, 1, PollingActivityKind::Saver, 1000),
        (220, 1000, 2, PollingActivityKind::SaverRestore, 125),
    ] {
        store
            .record_polling_activity(&PollingActivity {
                key: "synthetic:mouse".into(),
                timestamp: ts,
                session,
                kind,
                rate: PollingRate::try_from(hz).unwrap(),
                previous: PollingRate::try_from(previous).ok(),
            })
            .unwrap();
        for offset in [0, 60] {
            store
                .record_usage(&UsageObservation {
                    reading: reading(ts + offset, 50 - (ts / 60) as u8),
                    polling_rate: PollingRate::try_from(hz).ok(),
                    session: Some(session),
                })
                .unwrap();
        }
    }
    store.flush().unwrap();
    let insights = store.query_insights("synthetic:mouse", 280).unwrap();
    assert_eq!(insights.recent.saver_changes, 1);
    assert_eq!(insights.recent.saver_restores, 1);
    assert_eq!(insights.recent.saver_seconds, 60);
    assert_eq!(insights.recent.manual_changes, 0);
    assert_eq!(insights.recent.boost_sessions, 0);
    assert_eq!(insights.rates.len(), 2);
}

#[test]
fn usage_chart_keeps_the_latest_of_multiple_changes_in_one_second() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("history.db")).unwrap();
    for mut r in [reading(100, 50), reading(160, 40), reading(160, 60)] {
        r.charging = Some(r.level == Some(60));
        store.record(&r).unwrap();
    }
    store.flush().unwrap();
    for budget in [2, 3, 8, 800] {
        let series = store
            .query_usage("synthetic:mouse", 160, 3600, budget)
            .unwrap();
        assert!(series.samples.len() <= budget);
        assert_eq!(
            series.samples.last().unwrap().reading.level,
            Some(60),
            "budget {budget}"
        );
        if budget >= 4 {
            assert!(series.samples.iter().any(|s| s.reading.level == Some(40)));
        }
    }
}
