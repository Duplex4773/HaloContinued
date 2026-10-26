use hb_core::ProviderError;
use rusqlite::Connection;

pub fn sequenced(db: &Connection, table: &str) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name='sequence')",
        [table],
        |r| r.get(0),
    )
}
pub fn migrate(db: &mut Connection) -> Result<(), ProviderError> {
    let mut migrate = || -> rusqlite::Result<()> {
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 4 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-512;",
        )?;
        let transaction = db.transaction()?;
        for table in ["readings", "usage_metadata"] {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )?;
            let legacy = exists && !sequenced(&transaction, table)?;
            if legacy {
                transaction
                    .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_legacy_v3;"))?;
            }
            if table == "readings" {
                transaction.execute_batch("CREATE TABLE IF NOT EXISTS readings(device TEXT NOT NULL, ts INTEGER NOT NULL, level INTEGER, payload TEXT NOT NULL, sequence INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(device,ts,sequence));")?;
                if legacy {
                    transaction.execute_batch("INSERT INTO readings(device,ts,level,payload) SELECT device,ts,level,payload FROM readings_legacy_v3; DROP TABLE readings_legacy_v3;")?;
                }
            } else {
                transaction.execute_batch("CREATE TABLE IF NOT EXISTS usage_metadata(device TEXT NOT NULL, ts INTEGER NOT NULL, polling_rate INTEGER, session TEXT, sequence INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(device,ts,sequence));")?;
                if legacy {
                    transaction.execute_batch("INSERT INTO usage_metadata(device,ts,polling_rate,session) SELECT device,ts,polling_rate,session FROM usage_metadata_legacy_v3; DROP TABLE usage_metadata_legacy_v3;")?;
                }
            }
        }
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS readings_time ON readings(ts);
            CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS usage_metadata_time ON usage_metadata(ts);
            CREATE TABLE IF NOT EXISTS polling_events(device TEXT NOT NULL, session TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, rate INTEGER NOT NULL, previous_rate INTEGER, PRIMARY KEY(device,session,ts,kind));
            CREATE INDEX IF NOT EXISTS polling_events_time ON polling_events(ts);
            CREATE INDEX IF NOT EXISTS polling_events_device_time ON polling_events(device,ts);
            CREATE INDEX IF NOT EXISTS polling_events_device_session_time ON polling_events(device,session,ts);
            PRAGMA user_version=4;")?;
        transaction.commit()
    };
    migrate().map_err(|e| ProviderError::new(format!("History schema: {e}")))
}
