//! Bounded, batched history storage; configuration never shares upstream's folder.
mod schema;
mod usage_index;
#[cfg(test)]
#[path = "../tests/support/usage_reference.rs"]
mod usage_reference;
mod usage_sampler;
use hb_core::{
    BatteryInsights, Estimator, HistoryStore, InsightsBuilder, PollingActivity,
    PollingActivityKind, PollingRate, ProviderError, Reading, Settings, Snapshot, UsageObservation,
};
use rusqlite::{Connection, params};
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

// Retain the earlier Rust app folder across the Halo Continued rebrand.
// This is separate from the upstream HaloBattery application.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("HaloBatteryNext")
}
/// Explicit diagnostic directories take priority. A marker beside the executable
/// requests portable data in an app-specific subfolder, never the parent's config.
#[derive(Debug)]
pub struct DataDirectory {
    pub path: PathBuf,
    pub portable: bool,
    pub portable_requested: bool,
}
pub fn resolve_data_directory(
    executable: Option<&Path>,
    explicit: Option<PathBuf>,
) -> DataDirectory {
    choose_data_directory(executable, explicit, data_dir(), writable_directory)
}
fn choose_data_directory(
    executable: Option<&Path>,
    explicit: Option<PathBuf>,
    fallback: PathBuf,
    writable: impl FnOnce(&Path) -> bool,
) -> DataDirectory {
    if let Some(path) = explicit {
        return DataDirectory {
            path,
            portable: false,
            portable_requested: false,
        };
    }
    let parent = executable.and_then(Path::parent);
    let requested = parent.is_some_and(|directory| directory.join("portable.txt").is_file());
    if requested {
        let path = parent.unwrap().join("HaloBatteryNext-data");
        if writable(&path) {
            return DataDirectory {
                path,
                portable: true,
                portable_requested: true,
            };
        }
    }
    DataDirectory {
        path: fallback,
        portable: false,
        portable_requested: requested,
    }
}
fn writable_directory(directory: &Path) -> bool {
    if fs::create_dir_all(directory).is_err() {
        return false;
    }
    static PROBE_NUMBER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..8 {
        let number = PROBE_NUMBER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".writable-{}-{number}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                return fs::remove_file(path).is_ok();
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }
    false
}
#[cfg(test)]
mod portable_tests {
    use super::*;
    fn select(exe: &Path, fallback: &Path, writable: bool) -> DataDirectory {
        choose_data_directory(Some(exe), None, fallback.to_owned(), |_| writable)
    }
    #[test]
    fn marker_requires_file_and_writable_app_specific_folder() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("HaloContinued.exe");
        let fallback = temp.path().join("fallback");
        let normal = select(&exe, &fallback, true);
        assert_eq!(normal.path, fallback);
        assert!(!normal.portable && !normal.portable_requested);
        fs::create_dir(temp.path().join("portable.txt")).unwrap();
        assert!(!select(&exe, &fallback, true).portable);
        fs::remove_dir(temp.path().join("portable.txt")).unwrap();
        fs::write(temp.path().join("portable.txt"), []).unwrap();
        let portable = select(&exe, &fallback, true);
        assert!(portable.portable && portable.portable_requested);
        assert_eq!(portable.path, temp.path().join("HaloBatteryNext-data"));
        let failed = select(&exe, &fallback, false);
        assert!(!failed.portable && failed.portable_requested);
        assert_eq!(failed.path, fallback);
    }
    #[test]
    fn explicit_directory_wins_and_missing_executable_uses_fallback() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("portable.txt"), []).unwrap();
        let explicit = temp.path().join("simulation");
        let selected = choose_data_directory(
            Some(&temp.path().join("app.exe")),
            Some(explicit.clone()),
            temp.path().join("fallback"),
            |_| panic!("explicit directory must not probe"),
        );
        assert_eq!(selected.path, explicit);
        assert!(!selected.portable && !selected.portable_requested);
        let fallback = choose_data_directory(None, None, temp.path().join("fallback"), |_| {
            panic!("no executable must not probe")
        });
        assert!(!fallback.portable && !fallback.portable_requested);
        assert_eq!(fallback.path, temp.path().join("fallback"));
    }
    #[test]
    fn writable_probe_cleans_up_and_does_not_import_parent_settings() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("HaloBatteryNext-data");
        fs::write(temp.path().join("config.json"), b"parent settings").unwrap();
        assert!(writable_directory(&data));
        assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
        assert!(!data.join("config.json").exists());
        assert_eq!(
            fs::read(temp.path().join("config.json")).unwrap(),
            b"parent settings"
        );
        let blocked = temp.path().join("a-file");
        fs::write(&blocked, []).unwrap();
        assert!(!writable_directory(&blocked));
    }
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Separate files even for concurrent exports or equal stems (config.json
    // and config.txt). create_new also refuses an existing file or symlink.
    static TEMP_NUMBER: AtomicU64 = AtomicU64::new(0);
    let mut opened = None;
    for _ in 0..32 {
        let number = TEMP_NUMBER.fetch_add(1, Ordering::Relaxed);
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.{number}.tmp", std::process::id()));
        let temp = path.with_file_name(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => {
                opened = Some((temp, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let (temp, mut file) =
        opened.ok_or_else(|| ProviderError::new("Cannot create temporary save file"))?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn load_settings(path: &Path) -> Settings {
    // Bound startup allocations even if a damaged or replaced config is enormous.
    const MAX_SETTINGS_BYTES: u64 = 2 * 1024 * 1024;
    let bytes = fs::File::open(path).and_then(|file| {
        if file.metadata()?.len() > MAX_SETTINGS_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Settings file exceeds size limit",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Settings file exceeds size limit",
            ));
        }
        Ok(bytes)
    });
    match bytes {
        Ok(bytes) => match serde_json::from_slice(&bytes)
            .map_err(|e| e.to_string())
            .and_then(Settings::from_value)
        {
            Ok(s) => s,
            Err(_) => {
                let _ = fs::rename(path, path.with_extension("json.bad"));
                Settings::default()
            }
        },
        Err(error) => {
            if error.kind() == std::io::ErrorKind::InvalidData {
                let _ = fs::rename(path, path.with_extension("json.bad"));
            }
            Settings::default()
        }
    }
}
pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), ProviderError> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(settings).map_err(|e| ProviderError::new(e.to_string()))?,
    )
}

/// Explicit changes only: save and sync recovery data before a worker may write HID.
/// Retain earlier originals rather than overwriting them with subsequent choices.
pub fn save_dpi_backup(folder: &Path, backup: &hb_core::DpiBackup) -> Result<(), ProviderError> {
    static NUMBER: AtomicU64 = AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ProviderError::new("Cannot timestamp the original DPI backup."))?
        .as_nanos();
    let sequence = NUMBER.fetch_add(1, Ordering::Relaxed);
    let path = folder
        .join("dpi-backups")
        .join(format!("{timestamp}-{sequence}.json"));
    let bytes = serde_json::to_vec_pretty(backup).map_err(|e| ProviderError::new(e.to_string()))?;
    atomic_write(&path, &bytes)
}
pub fn write_status(path: &Path, snapshot: &Snapshot, running: bool) -> Result<(), ProviderError> {
    let status = Status {
        app: "Halo Continued",
        version: env!("CARGO_PKG_VERSION"),
        running,
        updated_unix: snapshot.timestamp,
        updated: timestamp(snapshot.timestamp),
        devices: StatusDevices { snapshot, running },
    };
    atomic_write(
        path,
        &serde_json::to_vec(&status).map_err(|e| ProviderError::new(e.to_string()))?,
    )
}
#[derive(Serialize)]
struct Status<'a> {
    app: &'static str,
    version: &'static str,
    running: bool,
    updated_unix: i64,
    updated: String,
    devices: StatusDevices<'a>,
}
struct StatusDevices<'a> {
    snapshot: &'a Snapshot,
    running: bool,
}
#[derive(Serialize)]
struct StatusDevice<'a> {
    key: &'a str,
    name: &'a str,
    level: Option<u8>,
    charging: bool,
    online: bool,
    kind: &'a str,
    approx: &'a Option<String>,
    low_alert_at: u8,
    seconds_left: Option<u64>,
    text: &'a str,
}
impl Serialize for StatusDevices<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for d in self
            .snapshot
            .devices
            .iter()
            .filter(|d| self.running && !d.hidden)
        {
            sequence.serialize_element(&StatusDevice {
                key: &d.reading.key,
                name: &d.name,
                level: d.reading.level,
                charging: d.reading.charging.unwrap_or(false),
                online: d.reading.online(),
                kind: &d.icon,
                approx: &d.reading.approx,
                low_alert_at: d.low_alert_at,
                seconds_left: d.seconds_left,
                text: &d.text,
            })?;
        }
        sequence.end()
    }
}
fn sql_error(e: rusqlite::Error) -> ProviderError {
    ProviderError::new(e.to_string())
}
// The SQLite row owns this text until the next step; parsing does not need a copy.
fn row_text<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a str> {
    let value = row.get_ref(index)?;
    value.as_str().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
    })
}
fn history_reading(row: &rusqlite::Row<'_>, key: &str) -> rusqlite::Result<Option<Reading>> {
    let (Ok(timestamp), Ok(payload)) = (row.get::<_, i64>(0), row_text(row, 1)) else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<Reading>(payload)
        .ok()
        .filter(|reading| {
            reading.key == key
                && reading.timestamp == timestamp
                && reading.level.is_none_or(|level| level <= 100)
        }))
}
/// UTC calendar rendering avoids locale-dependent output. All status fields match
/// upstream; the RFC3339 suffix makes the timestamp's timezone explicit.
fn timestamp(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let seconds = unix.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}
pub struct Store {
    db: Connection,
    pending: Vec<UsageObservation>,
    last: BTreeMap<String, UsageObservation>,
    has_usage_metadata: bool,
    has_polling_events: bool,
    sequenced: bool,
    usage_index: std::cell::RefCell<Option<usage_index::UsageIndex>>,
    #[cfg(test)]
    usage_rows: std::cell::Cell<usize>,
}
const INSIGHTS_WITH_ACTIVITY: &str = "SELECT r.ts,r.payload,m.polling_rate,m.session,(SELECT CASE WHEN e.rate=m.polling_rate THEN e.kind END FROM polling_events e WHERE e.device=r.device AND e.session=m.session AND e.ts<=r.ts ORDER BY e.ts DESC,e.rowid DESC LIMIT 1) FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts";
const INSIGHTS_WITH_ACTIVITY_SEQUENCED: &str = "SELECT r.ts,r.payload,m.polling_rate,m.session,(SELECT CASE WHEN e.rate=m.polling_rate THEN e.kind END FROM polling_events e WHERE e.device=r.device AND e.session=m.session AND e.ts<=r.ts ORDER BY e.ts DESC,e.rowid DESC LIMIT 1) FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts AND m.sequence=r.sequence WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts,r.sequence";
impl Store {
    pub fn open(path: &Path) -> Result<Self, ProviderError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut db = Connection::open(path).map_err(sql_error)?;
        db.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(sql_error)?;
        schema::migrate(&mut db)?;
        Ok(Self {
            db,
            pending: Vec::with_capacity(64),
            last: BTreeMap::new(),
            has_usage_metadata: true,
            has_polling_events: true,
            sequenced: true,
            usage_index: Default::default(),
            #[cfg(test)]
            usage_rows: Default::default(),
        })
    }
    pub fn read_only(path: &Path) -> Result<Self, ProviderError> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql_error)?;
        db.execute_batch("PRAGMA cache_size=-256;")
            .map_err(sql_error)?;
        let has_usage_metadata = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_metadata')",
            [], |row| row.get(0),
        ).map_err(sql_error)?;
        let has_polling_events = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='polling_events')", [], |row| row.get(0)).map_err(sql_error)?;
        let sequenced = schema::sequenced(&db, "readings").map_err(sql_error)?;
        Ok(Self {
            db,
            pending: Vec::new(),
            last: BTreeMap::new(),
            has_usage_metadata,
            has_polling_events,
            sequenced,
            usage_index: Default::default(),
            #[cfg(test)]
            usage_rows: Default::default(),
        })
    }
    pub fn prune(&mut self, now: i64) -> Result<(), ProviderError> {
        self.prune_inner(now, true)
    }
    /// Routine retention uses timestamp indexes. Full orphan repair is reserved
    /// for startup, rather than rescanning 30 days of metadata each minute.
    pub fn prune_expired(&mut self, now: i64) -> Result<(), ProviderError> {
        self.prune_inner(now, false)
    }
    fn prune_inner(&mut self, now: i64, repair: bool) -> Result<(), ProviderError> {
        // Flush first so pending old observations cannot reappear after pruning.
        self.flush()?;
        let cutoff = now.saturating_sub(30 * 86400);
        let changes = self.db.total_changes();
        let transaction = self.db.transaction().map_err(sql_error)?;
        transaction
            .execute("DELETE FROM readings WHERE ts < ?1", [cutoff])
            .map_err(sql_error)?;
        // Ordered EXCEPT merges the existing covering key indexes instead of
        // performing one readings lookup per retained metadata row. Subtract
        // only retained readings so expired keys and arbitrary orphans both go.
        if repair {
            transaction.execute("DELETE FROM usage_metadata WHERE (device,ts,sequence) IN (SELECT device,ts,sequence FROM usage_metadata EXCEPT SELECT device,ts,sequence FROM readings WHERE ts >= ?1 ORDER BY device,ts,sequence)", [cutoff]).map_err(sql_error)?;
        } else {
            transaction
                .execute("DELETE FROM usage_metadata WHERE ts < ?1", [cutoff])
                .map_err(sql_error)?;
        }
        transaction
            .execute("DELETE FROM polling_events WHERE ts < ?1", [cutoff])
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        self.last
            .retain(|_, observation| observation.reading.timestamp >= cutoff);
        if self.db.total_changes() != changes {
            self.usage_index.get_mut().take();
        }
        Ok(())
    }
    /// Retained raw observations, streamed independently of chart sampling.
    /// As with history queries, flush pending observations before querying.
    pub fn query_insights(&self, key: &str, until: i64) -> Result<BatteryInsights, ProviderError> {
        let sql = if self.has_usage_metadata && self.has_polling_events {
            if self.sequenced {
                INSIGHTS_WITH_ACTIVITY_SEQUENCED
            } else {
                INSIGHTS_WITH_ACTIVITY
            }
        } else if self.has_usage_metadata {
            if self.sequenced {
                "SELECT r.ts,r.payload,m.polling_rate,m.session,NULL FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts AND m.sequence=r.sequence WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts,r.sequence"
            } else {
                "SELECT r.ts,r.payload,m.polling_rate,m.session,NULL FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts"
            }
        } else if self.sequenced {
            "SELECT ts,payload,NULL,NULL,NULL FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,sequence"
        } else {
            "SELECT ts,payload,NULL,NULL,NULL FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts"
        };
        let mut query = self.db.prepare(sql).map_err(sql_error)?;
        let mut rows = query
            .query(params![key, until.saturating_sub(30 * 86400), until])
            .map_err(sql_error)?;
        let mut builder = InsightsBuilder::default().recent_since(until.saturating_sub(7 * 86400));
        while let Some(row) = rows.next().map_err(sql_error)? {
            let (Ok(timestamp), Ok(payload)) = (row.get::<_, i64>(0), row_text(row, 1)) else {
                builder.break_continuity();
                continue;
            };
            let Ok(reading) = serde_json::from_str::<Reading>(payload) else {
                builder.break_continuity();
                continue;
            };
            if reading.key != key || reading.timestamp != timestamp {
                builder.break_continuity();
                continue;
            }
            let polling_rate = match row.get::<_, Option<u32>>(2) {
                Ok(None) => None,
                Ok(Some(hz)) => match PollingRate::try_from(hz) {
                    Ok(rate) => Some(rate),
                    Err(_) => {
                        builder.break_continuity();
                        continue;
                    }
                },
                Err(_) => {
                    builder.break_continuity();
                    continue;
                }
            };
            let session = match row.get_ref(3).map_err(sql_error)? {
                rusqlite::types::ValueRef::Null => None,
                value => match value
                    .as_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    Some(session) => Some(session),
                    None => {
                        builder.break_continuity();
                        continue;
                    }
                },
            };
            // Activity classification is optional; damaged event text must not
            // discard independently valid battery/rate evidence.
            let activity_text = row.get_ref(4).map_err(sql_error)?.as_str().ok();
            let activity = match activity_text {
                Some("boost") => Some(PollingActivityKind::Boost),
                Some("restore") => Some(PollingActivityKind::Restore),
                Some("manual") => Some(PollingActivityKind::Manual),
                Some("saver") => Some(PollingActivityKind::Saver),
                Some("saver_restore") => Some(PollingActivityKind::SaverRestore),
                _ => None,
            };
            builder.push_with_activity(
                UsageObservation {
                    reading,
                    polling_rate,
                    session,
                },
                activity,
            );
        }
        let mut result = builder.finish_at(until);
        if self.has_polling_events {
            // A reused identity can cut the interval later than seven days ago.
            // At its first timestamp, both units can have saved an event: require
            // matching session evidence within the new unit's retained period.
            let sql = if self.has_usage_metadata
                && result.recent.since > until.saturating_sub(7 * 86400)
            {
                "SELECT kind,rate,previous_rate FROM polling_events e WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND (ts>?2 OR EXISTS(SELECT 1 FROM usage_metadata m WHERE m.device=e.device AND m.ts BETWEEN ?2 AND ?3 AND m.session=e.session)) ORDER BY ts"
            } else {
                "SELECT kind,rate,previous_rate FROM polling_events WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts"
            };
            let mut query = self.db.prepare(sql).map_err(sql_error)?;
            let mut rows = query
                .query(params![key, result.recent.since, until])
                .map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                // One malformed event must not prevent the rest of the user's
                // valid battery history from loading, or inflate activity counts.
                let Ok(kind) = row_text(row, 0) else {
                    continue;
                };
                let Ok(rate) = row.get::<_, u32>(1) else {
                    continue;
                };
                if PollingRate::try_from(rate).is_err() {
                    continue;
                }
                let Ok(previous) = row.get::<_, Option<u32>>(2) else {
                    continue;
                };
                if previous.is_some_and(|hz| PollingRate::try_from(hz).is_err()) {
                    continue;
                }
                // Same-rate confirmations can transfer ownership from an
                // automatic boost to a manual choice. Keep those events for
                // interval classification, but do not count a hardware change.
                if previous == Some(rate) {
                    continue;
                }
                match kind {
                    "boost" => {
                        result.recent.boost_sessions += 1;
                        result.recent.normal_rate = previous
                            .filter(|previous| *previous < rate)
                            .and_then(|hz| PollingRate::try_from(hz).ok());
                    }
                    "restore" => result.recent.restores += 1,
                    "manual" => result.recent.manual_changes += 1,
                    "saver" => result.recent.saver_changes += 1,
                    "saver_restore" => result.recent.saver_restores += 1,
                    _ => {}
                }
            }
        }
        Ok(result)
    }
    /// Successful readback only; no device access and no inferred historical events.
    pub fn record_polling_activity(
        &mut self,
        event: &PollingActivity,
    ) -> Result<(), ProviderError> {
        self.db.execute("INSERT OR IGNORE INTO polling_events(device,session,ts,kind,rate,previous_rate) VALUES(?1,?2,?3,?4,?5,?6)",
            params![event.key, event.session.to_string(), event.timestamp, event.kind.as_str(), event.rate.hz(), event.previous.map(PollingRate::hz)]).map_err(sql_error)?;
        Ok(())
    }
    /// Preserve confirmed configuration evidence without applying it to hardware.
    pub fn record_usage(&mut self, observation: &UsageObservation) -> Result<(), ProviderError> {
        // A failed transaction retains queued observations for retry. Refuse
        // further growth before updating pacing/identity state at the hard cap.
        if self.pending.len() >= 4096 {
            self.flush()?;
        }
        let reading = &observation.reading;
        let changed = self.last.get(&reading.key).is_none_or(|previous| {
            let r = &previous.reading;
            previous.polling_rate != observation.polling_rate
                || previous.session != observation.session
                || r.level != reading.level
                || r.charging != reading.charging
                || r.charging_inferred != reading.charging_inferred
                || r.connection != reading.connection
                || r.precision != reading.precision
                || r.approx != reading.approx
                || r.name != reading.name
                || r.kind != reading.kind
                || r.source != reading.source
                || r.via != reading.via
                || r.serial != reading.serial
                || r.container != reading.container
                || reading.timestamp < r.timestamp
                || reading.timestamp.saturating_sub(r.timestamp) >= 60
        });
        if changed {
            if let Some(previous) = self.last.get_mut(&reading.key) {
                *previous = observation.clone();
            } else {
                self.last.insert(reading.key.clone(), observation.clone());
            }
            self.pending.push(observation.clone());
        }
        if self.pending.len() >= 4096 {
            self.flush()?;
        }
        Ok(())
    }
    /// Chart history with at most one retained, valid last-known predecessor.
    /// Raw `HistoryStore::query` remains interval-only. No record is fabricated.
    pub fn query_with_baseline(
        &self,
        key: &str,
        since: i64,
        until: i64,
        max_points: usize,
    ) -> Result<Vec<Reading>, ProviderError> {
        let limit = max_points.clamp(2, 4096);
        let retained_since = until.saturating_sub(30 * 86400);
        let sql = if self.sequenced {
            "SELECT ts,payload FROM readings WHERE device=?1 AND ts>=?2 AND ts<?3 AND level BETWEEN 0 AND 100 ORDER BY ts DESC,sequence DESC"
        } else {
            "SELECT ts,payload FROM readings WHERE device=?1 AND ts>=?2 AND ts<?3 AND level BETWEEN 0 AND 100 ORDER BY ts DESC"
        };
        let mut query = self.db.prepare(sql).map_err(sql_error)?;
        let mut baseline = None;
        let mut rows = query
            .query(params![key, retained_since, since.min(until)])
            .map_err(sql_error)?;
        while let Some(row) = rows.next().map_err(sql_error)? {
            if let Some(reading) = history_reading(row, key).map_err(sql_error)?
                && reading.level.is_some_and(|level| level <= 100)
            {
                baseline = Some(reading);
                break;
            }
        }
        let Some(baseline) = baseline else {
            return self.query(key, since, until, limit);
        };
        let mut interval = self.query(key, since, until, limit - 1)?;
        // The raw query reserves at least two endpoints. With a two-point chart,
        // retain the latest interval observation alongside the predecessor.
        if interval.len() > limit - 1 {
            interval = interval.into_iter().rev().take(limit - 1).collect();
            interval.reverse();
        }
        let mut out = Vec::with_capacity(interval.len() + 1);
        out.push(baseline);
        out.extend(interval);
        Ok(out)
    }
    /// Active-use history, summed from raw retained rows before display sampling.
    pub fn query_usage(
        &self,
        key: &str,
        until: i64,
        seconds: i64,
        max_points: usize,
    ) -> Result<hb_core::HistorySeries, ProviderError> {
        use hb_core::{HistoryAxis, HistorySeries};
        use usage_sampler::Sample;
        let limit = max_points.clamp(2, 4096);
        let bounds_sql = if self.sequenced {
            "SELECT (SELECT ts FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,sequence LIMIT 1),(SELECT sequence FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,sequence LIMIT 1),(SELECT ts FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts DESC,sequence DESC LIMIT 1),(SELECT sequence FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts DESC,sequence DESC LIMIT 1)"
        } else {
            "SELECT (SELECT ts FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,rowid LIMIT 1),(SELECT rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,rowid LIMIT 1),(SELECT ts FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts DESC,rowid DESC LIMIT 1),(SELECT rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts DESC,rowid DESC LIMIT 1)"
        };
        let bounds: Option<(i64, i64, i64, i64)> = self
            .db
            .query_row(
                bounds_sql,
                params![key, until.saturating_sub(30 * 86400), until],
                |r| match r.get::<_, Option<i64>>(0)? {
                    Some(first) => Ok(Some((first, r.get(1)?, r.get(2)?, r.get(3)?))),
                    None => Ok(None),
                },
            )
            .map_err(sql_error)?;
        // flush/prune invalidate this cache when local history changes.
        // Check other connections here, but do not invalidate for unrelated
        // estimator checkpoints or polling-activity writes on this connection.
        let revision = self
            .db
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .map_err(sql_error)?;
        let mut index = self.usage_index.borrow_mut();
        if !index
            .as_ref()
            .is_some_and(|i| i.key == key && i.bounds == bounds && i.revision == revision)
        {
            let mut fresh = usage_index::UsageIndex::new(key, bounds, revision);
            fresh.total = self.stream_usage(key, until, None, |_, r, position, row| {
                if r.level.is_some_and(|level| level <= 100) {
                    fresh.push(usage_index::Checkpoint {
                        timestamp: r.timestamp,
                        row,
                        position,
                    });
                }
            })?;
            *index = Some(fresh);
        }
        let total = index.as_ref().map_or(0, |i| i.total);
        let since = total.saturating_sub(seconds.max(1)).max(0);
        let buckets = limit.saturating_sub(2) / 2;
        let mut extremes: Vec<Option<(Sample, Sample)>> = vec![None; buckets];
        let mut baseline: Option<Sample> = None;
        let mut first = None;
        let mut last: Option<Sample> = None;
        let checkpoint = index.as_ref().and_then(|i| i.before(since));
        self.stream_usage(
            key,
            until,
            checkpoint,
            |payload, reading, position, ordinal| {
                if !reading.level.is_some_and(|level| level <= 100) {
                    return;
                }
                if position < since {
                    baseline
                        .get_or_insert_with(Sample::default)
                        .replace(payload, reading, position, ordinal);
                    return;
                }
                let sample = last.get_or_insert_with(Sample::default);
                sample.replace(payload, reading, position, ordinal);
                if first.is_none() {
                    first = Some(baseline.take().unwrap_or_else(|| sample.clone()));
                }
                if buckets > 0 {
                    let span = total.saturating_sub(since).max(1);
                    let bucket = ((position.saturating_sub(since) as i128 * buckets as i128)
                        / span as i128)
                        .min(buckets as i128 - 1) as usize;
                    if let Some((low, high)) = &mut extremes[bucket] {
                        if sample.level < low.level {
                            low.replace(payload, reading, position, ordinal);
                        }
                        if sample.level > high.level {
                            high.replace(payload, reading, position, ordinal);
                        }
                    } else {
                        extremes[bucket] = Some((sample.clone(), sample.clone()));
                    }
                }
            },
        )?;
        let mut samples = Vec::with_capacity(limit);
        if let Some(sample) = first {
            samples.push(sample);
        }
        for (low, high) in extremes.into_iter().flatten() {
            samples.extend([low, high]);
        }
        if let Some(sample) = last {
            samples.push(sample);
        }
        // A timestamp alone does not identify an observation. Keep ordered
        // same-second changes so the newest charge/rate transition stays visible.
        samples.sort_by_key(|sample| (sample.timestamp, sample.ordinal));
        samples.dedup_by_key(|sample| (sample.timestamp, sample.ordinal));
        Ok(HistorySeries {
            samples: samples
                .into_iter()
                .map(Sample::decode)
                .collect::<Result<_, _>>()?,
            axis: HistoryAxis::Usage,
            since,
            until: total,
        })
    }
    fn stream_usage(
        &self,
        key: &str,
        until: i64,
        checkpoint: Option<usage_index::Checkpoint>,
        mut visit: impl FnMut(&str, &usage_sampler::TimingReading<'_>, i64, i64),
    ) -> Result<i64, ProviderError> {
        let sql = if self.has_usage_metadata {
            if self.sequenced {
                "SELECT r.ts,r.payload,m.session,r.sequence FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts AND m.sequence=r.sequence WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 AND (r.ts>?4 OR r.ts=?4 AND r.sequence>=?5) ORDER BY r.ts,r.sequence"
            } else {
                "SELECT r.ts,r.payload,m.session,r.rowid FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 AND (r.ts>?4 OR r.ts=?4 AND r.rowid>=?5) ORDER BY r.ts"
            }
        } else if self.sequenced {
            "SELECT ts,payload,NULL,sequence FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND (ts>?4 OR ts=?4 AND sequence>=?5) ORDER BY ts,sequence"
        } else {
            "SELECT ts,payload,NULL,rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND (ts>?4 OR ts=?4 AND rowid>=?5) ORDER BY ts"
        };
        let mut query = self.db.prepare(sql).map_err(sql_error)?;
        let mut rows = query
            .query(params![
                key,
                checkpoint.map_or(until.saturating_sub(30 * 86400), |c| c.timestamp),
                until,
                checkpoint.map_or(until.saturating_sub(30 * 86400), |c| c.timestamp),
                checkpoint.map_or(i64::MIN, |c| c.row)
            ])
            .map_err(sql_error)?;
        // Keep time, awake state and one bounded identity; never copy every
        // full reading during either chart pass.
        let mut previous: Option<(i64, bool, Option<u64>)> = None;
        let mut identity: Option<usage_sampler::TimingIdentity> = None;
        let mut total = checkpoint.map_or(0, |c| c.position);
        while let Some(row) = rows.next().map_err(sql_error)? {
            #[cfg(test)]
            self.usage_rows.set(self.usage_rows.get() + 1);
            let (Ok(timestamp), Ok(payload)) = (row.get::<_, i64>(0), row_text(row, 1)) else {
                previous = None;
                continue;
            };
            let Ok(reading) = serde_json::from_str::<usage_sampler::TimingReading<'_>>(payload)
            else {
                previous = None;
                continue;
            };
            if reading.key != key || reading.timestamp != timestamp {
                previous = None;
                continue;
            }
            let session = match row.get_ref(2).map_err(sql_error)? {
                rusqlite::types::ValueRef::Null => None,
                value => match value
                    .as_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    Some(session) => Some(session),
                    None => {
                        previous = None;
                        visit(payload, &reading, total, row.get(3).map_err(sql_error)?);
                        continue;
                    }
                },
            };
            let reading_awake = reading.awake();
            let same_identity = identity.as_ref().is_some_and(|old| old.matches(&reading));
            if let Some((timestamp, true, previous_session)) = previous
                && reading_awake
                && same_identity
                && previous_session == session
                && reading
                    .timestamp
                    .checked_sub(timestamp)
                    .is_some_and(|delta| (1..=600).contains(&delta))
            {
                total = total.saturating_add(reading.timestamp - timestamp);
            }
            if !same_identity {
                identity = Some(usage_sampler::TimingIdentity::from(&reading));
            }
            previous = Some((reading.timestamp, reading_awake, session));
            visit(payload, &reading, total, row.get(3).map_err(sql_error)?);
        }
        Ok(total)
    }
    pub fn release_history_cache(&self) {
        self.usage_index.borrow_mut().take();
    }
    pub fn load_estimator(&self) -> Estimator {
        // A damaged checkpoint must not allocate an arbitrarily large JSON
        // tree at startup. Keep the original row for recovery; history is intact.
        const MAX_CHECKPOINT_BYTES: i64 = 4 * 1024 * 1024;
        let now = hb_core::Clock::unix(&hb_core::SystemClock::default());
        self.db
            .query_row(
                "SELECT payload FROM state WHERE key='estimator' AND octet_length(payload) <= ?1",
                [MAX_CHECKPOINT_BYTES],
                |r| {
                    let payload = row_text(r, 0)?;
                    // Normal checkpoints deserialize directly into their final
                    // representation. A malformed entry still permits recovery
                    // of valid neighbours through the tolerant fallback.
                    let mut estimator =
                        serde_json::from_str::<Estimator>(payload).unwrap_or_else(|_| {
                            serde_json::from_str(payload)
                                .map(|value| Estimator::from_value(value, now))
                                .unwrap_or_default()
                        });
                    estimator.validate(now);
                    Ok(estimator)
                },
            )
            .ok()
            .unwrap_or_default()
    }
    pub fn save_estimator(&mut self, estimator: &Estimator) -> Result<(), ProviderError> {
        self.db
            .execute(
                "INSERT OR REPLACE INTO state(key,payload) VALUES('estimator',?1)",
                [serde_json::to_string(estimator)
                    .map_err(|e| ProviderError::new(e.to_string()))?],
            )
            .map_err(sql_error)?;
        Ok(())
    }
}
impl HistoryStore for Store {
    fn record(&mut self, reading: &Reading) -> Result<(), ProviderError> {
        self.record_usage(&UsageObservation {
            reading: reading.clone(),
            polling_rate: None,
            session: None,
        })
    }
    fn query(
        &self,
        key: &str,
        since: i64,
        until: i64,
        max_points: usize,
    ) -> Result<Vec<Reading>, ProviderError> {
        let limit = max_points.clamp(2, 4096);
        let count: i64 = self
            .db
            .query_row(
                "SELECT count(*) FROM (SELECT 1 FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 LIMIT ?4)",
                params![key, since, until, limit as i64 + 1],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        let mut out = Vec::with_capacity(limit.min(count as usize));
        if count > limit as i64 && limit < 5 {
            let mut query = self
                .db
                .prepare("SELECT ts,payload FROM readings WHERE device=?1 AND rowid=?2")
                .map_err(sql_error)?;
            let bounds = self
                .db
                .query_row(
                    "SELECT (SELECT rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,rowid LIMIT 1),(SELECT rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts DESC,rowid DESC LIMIT 1)",
                    params![key, since, until],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .map_err(sql_error)?;
            // With a tiny chart budget, prioritize an observed unknown gap
            // between the endpoints over an extra known-level representative.
            let gap: Option<i64> = if limit >= 3 {
                self.db.query_row(
                    "SELECT (SELECT rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND level IS NULL AND rowid NOT IN (?4,?5) ORDER BY ts,rowid LIMIT 1)",
                    params![key, since, until, bounds.0, bounds.1], |r| r.get(0),
                ).map_err(sql_error)?
            } else {
                None
            };
            for ts in [Some(bounds.0), gap, Some(bounds.1)].into_iter().flatten() {
                let reading = query
                    .query_row(params![key, ts], |r| history_reading(r, key))
                    .map_err(sql_error)?;
                if let Some(r) = reading {
                    out.push(r);
                }
            }
        } else if count <= limit as i64 {
            let mut query=self.db.prepare("SELECT ts,payload FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts,rowid").map_err(sql_error)?;
            for row in query
                .query_map(params![key, since, until], |r| history_reading(r, key))
                .map_err(sql_error)?
            {
                if let Some(r) = row.map_err(sql_error)? {
                    out.push(r);
                }
            }
        } else {
            // Reserve real interval endpoints, then up to low/high/unknown
            // representatives per bucket. Flat plateaus still span their real
            // timestamps; deterministic representatives avoid SQLite's arbitrary
            // GROUP BY payload selection.
            let buckets = ((limit - 2) / 3).max(1) as i64;
            let span = (i128::from(until) - i128::from(since)).max(0) + 1;
            let width = ((span + i128::from(buckets) - 1) / i128::from(buckets))
                .clamp(1, i128::from(i64::MAX)) as i64;
            let mut query=self.db.prepare("WITH selected AS (SELECT rowid AS id,ts,level,min((ts-?2)/?4,(?5-2)/3-1) AS bucket FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3), extremes AS (SELECT bucket,min(level) AS lo,max(level) AS hi FROM selected GROUP BY bucket), representatives AS (SELECT s.bucket,s.level,min(s.ts) AS ts FROM selected s JOIN extremes e ON s.bucket=e.bucket WHERE s.level=e.lo OR s.level=e.hi OR s.level IS NULL GROUP BY s.bucket,s.level), ids AS (SELECT min(s.id) AS id FROM selected s JOIN representatives p ON s.bucket=p.bucket AND s.level IS p.level AND s.ts=p.ts GROUP BY s.bucket,s.level UNION SELECT id FROM (SELECT id FROM selected ORDER BY ts,id LIMIT 1) UNION SELECT id FROM (SELECT id FROM selected ORDER BY ts DESC,id DESC LIMIT 1)) SELECT ts,payload FROM readings WHERE device=?1 AND rowid IN (SELECT id FROM ids) ORDER BY ts,rowid LIMIT ?5").map_err(sql_error)?;
            for row in query
                .query_map(params![key, since, until, width, limit as i64], |r| {
                    history_reading(r, key)
                })
                .map_err(sql_error)?
            {
                if let Some(r) = row.map_err(sql_error)? {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }
    fn flush(&mut self) -> Result<(), ProviderError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let transaction = self.db.transaction().map_err(sql_error)?;
        {
            let mut readings = transaction
                .prepare(
                    "INSERT INTO readings(device,ts,level,payload,sequence) SELECT ?1,?2,?3,?4,coalesce(max(sequence)+1,0) FROM readings WHERE device=?1 AND ts=?2 RETURNING sequence",
                )
                .map_err(sql_error)?;
            let mut metadata = transaction.prepare("INSERT INTO usage_metadata(device,ts,polling_rate,session,sequence) VALUES(?1,?2,?3,?4,?5)").map_err(sql_error)?;
            // Reuse serialization storage for the whole batch, rather than
            // allocating a new JSON string and session string for every row.
            let mut payload = Vec::with_capacity(512);
            let mut session = String::with_capacity(20);
            for observation in &self.pending {
                let r = &observation.reading;
                payload.clear();
                serde_json::to_writer(&mut payload, r)
                    .map_err(|e| ProviderError::new(e.to_string()))?;
                let text =
                    std::str::from_utf8(&payload).map_err(|e| ProviderError::new(e.to_string()))?;
                session.clear();
                if let Some(value) = observation.session {
                    use std::fmt::Write;
                    write!(&mut session, "{value}").expect("writing to a String");
                }
                let sequence: i64 = readings
                    .query_row(params![r.key, r.timestamp, r.level, text], |r| r.get(0))
                    .map_err(sql_error)?;
                metadata
                    .execute(params![
                        r.key,
                        r.timestamp,
                        observation.polling_rate.map(PollingRate::hz),
                        observation.session.map(|_| session.as_str()),
                        sequence
                    ])
                    .map_err(sql_error)?;
            }
        }
        transaction.commit().map_err(sql_error)?;
        self.usage_index.get_mut().take();
        self.pending.clear();
        // Large recovery bursts are exceptional. Do not retain their empty
        // backing allocation throughout subsequent quiet monitoring.
        if self.pending.capacity() > 1024 {
            self.pending.shrink_to(64);
        }
        Ok(())
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dpi_backups_retain_original_axes_and_profile_bytes_and_report_save_failure() {
        use hb_core::{DpiBackup, DpiState, DpiValue};
        let directory = tempfile::tempdir().unwrap();
        let mut backup = DpiBackup {
            device: "synthetic-mouse".into(),
            timestamp: 1000,
            original: DpiState {
                value: DpiValue { x: 800, y: 1200 },
                profile: Some(2),
                stage: Some(3),
            },
            requested: DpiValue::uniform(1600),
            profile_bytes: vec![0x45; 256],
            profile_format: Some(7),
        };
        save_dpi_backup(directory.path(), &backup).unwrap();
        backup.requested = DpiValue::uniform(2400);
        save_dpi_backup(directory.path(), &backup).unwrap();
        let files: Vec<_> = fs::read_dir(directory.path().join("dpi-backups"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 2);
        let mut requested = vec![];
        for file in files {
            let saved: DpiBackup =
                serde_json::from_slice(&fs::read(file.unwrap().path()).unwrap()).unwrap();
            assert_eq!(saved.original, backup.original);
            assert_eq!(saved.profile_bytes, backup.profile_bytes);
            requested.push(saved.requested.x);
        }
        requested.sort();
        assert_eq!(requested, [1600, 2400]);
        let blocked = directory.path().join("not-a-directory");
        fs::write(&blocked, b"retain me").unwrap();
        assert!(save_dpi_backup(&blocked, &backup).is_err());
        assert_eq!(fs::read(blocked).unwrap(), b"retain me");
    }
    #[test]
    fn oversized_settings_are_bounded_and_preserved_for_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(8 * 1024 * 1024)
            .unwrap();
        assert_eq!(load_settings(&path).interval, Settings::default().interval);
        assert!(!path.exists());
        assert_eq!(
            fs::metadata(path.with_extension("json.bad")).unwrap().len(),
            8 * 1024 * 1024
        );
    }

    use super::*;
    #[test]
    fn insights_activity_lookup_does_not_scan_unrelated_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        for index in 0..256 {
            store
                .record_polling_activity(&hb_core::PollingActivity {
                    key: "synthetic".into(),
                    timestamp: index,
                    session: 1,
                    kind: hb_core::PollingActivityKind::Manual,
                    rate: PollingRate::try_from(1000).unwrap(),
                    previous: None,
                })
                .unwrap();
            let mut reading = Reading::new("synthetic", "Synthetic mouse", "test", 1000 + index);
            reading.level = Some(80 - (index % 2) as u8);
            reading.charging = Some(false);
            store
                .record_usage(&UsageObservation {
                    reading,
                    polling_rate: PollingRate::try_from(1000).ok(),
                    session: Some(2),
                })
                .unwrap();
        }
        store.flush().unwrap();
        let mut statement = store.db.prepare(INSIGHTS_WITH_ACTIVITY).unwrap();
        {
            let mut rows = statement.query(params!["synthetic", 0, 2000]).unwrap();
            let mut count = 0;
            while let Some(row) = rows.next().unwrap() {
                assert_eq!(row.get::<_, Option<String>>(4).unwrap(), None);
                count += 1;
            }
            assert_eq!(count, 256);
        }
        let steps = statement.get_status(rusqlite::StatementStatus::VmStep);
        println!("256 readings with 256 unrelated polling events: {steps} SQL VM steps");
        assert!(
            steps < 256 * 100,
            "Session lookup scanned unrelated events: {steps} SQL VM steps"
        );
    }
    #[test]
    fn persistent_flush_failure_bounds_pending_and_recovers_without_losing_queued_rows() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        store.db.execute_batch("PRAGMA query_only=ON").unwrap();
        for timestamp in 0..4096 {
            let mut reading = Reading::new("invented", "Invented", "test", timestamp);
            reading.level = Some((timestamp % 100) as u8);
            let result = store.record(&reading);
            assert_eq!(result.is_err(), timestamp == 4095);
        }
        for timestamp in 4096..4196 {
            let mut reading = Reading::new(
                format!("invented-{timestamp}"),
                "Invented",
                "test",
                timestamp,
            );
            reading.level = Some(80);
            assert!(store.record(&reading).is_err());
            assert_eq!(store.pending.len(), 4096);
            assert_eq!(store.last.len(), 1);
        }
        store.db.execute_batch("PRAGMA query_only=OFF").unwrap();
        let mut latest = Reading::new("invented", "Invented", "test", 4200);
        latest.level = Some(80);
        store.record(&latest).unwrap();
        assert_eq!(store.pending.len(), 1);
        store.flush().unwrap();
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4097);
        assert_eq!(
            store.query("invented", 4200, 4200, 2).unwrap(),
            vec![latest]
        );
    }

    #[test]
    fn sampled_flat_history_preserves_endpoints_and_extreme_interval_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        for timestamp in 0..100 {
            let mut reading = Reading::new("invented", "Invented", "test", timestamp * 60);
            reading.level = Some(80);
            store.record(&reading).unwrap();
            reading.key = "varied".into();
            reading.level = Some((timestamp % 100) as u8);
            store.record(&reading).unwrap();
        }
        store.flush().unwrap();
        for cap in 2..=20 {
            for (since, until) in [(0, 5940), (i64::MIN, i64::MAX)] {
                for key in ["invented", "varied"] {
                    let readings = store.query(key, since, until, cap).unwrap();
                    assert!(readings.len() <= cap);
                    assert_eq!(readings.first().unwrap().timestamp, 0);
                    assert_eq!(readings.last().unwrap().timestamp, 5940);
                    assert!(
                        readings
                            .windows(2)
                            .all(|pair| pair[0].timestamp < pair[1].timestamp)
                    );
                }
            }
        }
    }

    #[test]
    fn tiny_history_budget_keeps_observed_unknown_gap_between_endpoints() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        for timestamp in 0..10 {
            let mut reading = Reading::new("invented", "Invented", "test", timestamp * 60);
            reading.level = (timestamp != 4).then_some(80);
            store.record(&reading).unwrap();
        }
        store.flush().unwrap();
        for cap in [3, 4] {
            let readings = store.query("invented", 0, 540, cap).unwrap();
            assert_eq!(
                readings.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
                [0, 240, 540]
            );
            assert_eq!(readings[1].level, None);
        }
    }

    #[test]
    fn calendar_queries_reject_mismatched_payloads_at_every_sampling_budget() {
        let (_directory, mut store) = baseline_store();
        for ts in 0..10 {
            let mut reading = Reading::new("test", "Test mouse", "test", ts);
            reading.level = Some(80);
            store.record(&reading).unwrap();
            store.flush().unwrap();
            // Seed each SQL timestamp explicitly; unchanged pacing is unrelated.
            if ts == 0 {
                reading.key = "other-device".into();
            }
            if ts == 1 {
                reading.timestamp = 999;
            }
            if ts == 2 {
                reading.level = Some(255);
            }
            store
                .db
                .execute(
                    "INSERT OR REPLACE INTO readings(device,ts,level,payload) VALUES('test',?1,80,?2)",
                    params![ts, serde_json::to_string(&reading).unwrap()],
                )
                .unwrap();
        }
        for budget in [2, 4, 8, 32] {
            let result = store.query("test", 0, 9, budget).unwrap();
            assert!(!result.is_empty());
            assert!(
                result.iter().all(|r| r.key == "test"
                    && (3..=9).contains(&r.timestamp)
                    && r.level == Some(80))
            );
        }
    }

    #[test]
    fn corruption_is_preserved_and_atomic_writes_replace() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        fs::write(&p, b"broken").unwrap();
        assert_eq!(load_settings(&p).interval, 60);
        assert!(p.with_extension("json.bad").exists());
        let s = Settings {
            low: 15,
            ..Default::default()
        };
        save_settings(&p, &s).unwrap();
        save_settings(&p, &Settings::default()).unwrap();
        assert_eq!(load_settings(&p).low, 20);
    }
    #[test]
    #[cfg(windows)]
    fn failed_atomic_save_preserves_previous_configuration() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        save_settings(&p, &Settings::default()).unwrap();
        let previous = fs::read(&p).unwrap();
        // A reader denying delete access forces a genuine replacement failure.
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&p)
            .unwrap();
        let changed = Settings {
            low: 10,
            ..Default::default()
        };
        assert!(save_settings(&p, &changed).is_err());
        assert_eq!(fs::read(&p).unwrap(), previous);
        assert_eq!(load_settings(&p).low, 20);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }
    #[test]
    fn concurrent_atomic_writers_never_share_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.json");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for value in 0..8u8 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    atomic_write(path, &vec![value; 16384]).unwrap();
                });
            }
        });
        let bytes = fs::read(path).unwrap();
        assert_eq!(bytes.len(), 16384);
        assert!(bytes.iter().all(|v| *v == bytes[0]));
        // File-system filters may create their own sibling files. Assert cleanup
        // only for names generated by atomic_write; never remove foreign files.
        let prefix = format!("export.json.{}.", std::process::id());
        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| {
                name.to_string_lossy()
                    .to_ascii_lowercase()
                    .strip_prefix(&prefix)
                    .and_then(|suffix| suffix.strip_suffix(".tmp"))
                    .is_some_and(|number| number.parse::<u64>().is_ok())
            })
            .collect();
        assert!(entries.is_empty(), "Unreleased app save files: {entries:?}");
    }
    #[test]
    fn configuration_roundtrip_preserves_unicode_and_device_preferences() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        let mut settings = Settings {
            interval: 120,
            animation: false,
            ..Default::default()
        };
        settings.devices.insert(
            "hardware-id".into(),
            hb_core::DevicePreferences {
                name: Some("Souris préférée 🖱".into()),
                hidden: true,
                low: Some(12),
                icon: Some("mouse".into()),
                ..Default::default()
            },
        );
        save_settings(&p, &settings).unwrap();
        let actual = load_settings(&p);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(settings).unwrap()
        );
    }
    #[test]
    fn samples_are_bounded_persistent_and_pruned() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("history.db");
        let mut store = Store::open(&p).unwrap();
        for ts in 0..1000 {
            let mut r = Reading::new("one", "Mouse", "razer", ts);
            r.level = Some((ts % 100) as u8);
            store.record(&r).unwrap();
        }
        store.flush().unwrap();
        assert!(store.query("one", 0, 999, 20).unwrap().len() <= 20);
        store.prune(31 * 86400).unwrap();
        assert!(store.query("one", 0, 999, 20).unwrap().is_empty());
    }

    #[test]
    fn successful_recovery_releases_oversized_history_batch_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("history.db")).unwrap();
        for ts in 0..2048 {
            let mut reading = Reading::new("device", "Device", "test", ts * 60);
            reading.level = Some(50);
            store.record(&reading).unwrap();
        }
        let before = store.pending.capacity();
        assert!(before >= 2048);
        store.db.execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(store.flush().is_err());
        assert_eq!(store.pending.len(), 2048);
        assert_eq!(store.pending.capacity(), before);
        store.db.execute_batch("PRAGMA query_only=OFF").unwrap();
        store.flush().unwrap();
        assert!(store.pending.is_empty());
        assert!(store.pending.capacity() <= 64);
        assert_eq!(
            store.query("device", 0, 2047 * 60, 4096).unwrap().len(),
            2048
        );
        println!(
            "Recovered batch capacity: {} -> {} bytes",
            before * std::mem::size_of::<UsageObservation>(),
            store.pending.capacity() * std::mem::size_of::<UsageObservation>()
        );
    }
    #[test]
    fn unchanged_readings_are_not_recorded_more_than_once_a_minute() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(&d.path().join("history.db")).unwrap();
        for ts in 0..120 {
            let mut r = Reading::new("one", "Mouse", "razer", ts);
            r.level = Some(80);
            s.record(&r).unwrap();
        }
        s.flush().unwrap();
        assert_eq!(s.query("one", 0, 120, 100).unwrap().len(), 2);
    }
    #[test]
    fn precision_and_charging_evidence_changes_are_recorded_without_waiting_a_minute() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(&d.path().join("history.db")).unwrap();
        let mut r = Reading::new("one", "Mouse", "razer", 100);
        r.level = Some(50);
        r.charging = Some(true);
        s.record(&r).unwrap();
        r.timestamp += 1;
        r.precision = hb_core::Precision::Coarse;
        r.approx = Some("about half".into());
        s.record(&r).unwrap();
        r.timestamp += 1;
        r.charging_inferred = true;
        s.record(&r).unwrap();
        r.timestamp += 1;
        r.name = "Updated device model".into();
        s.record(&r).unwrap();
        s.flush().unwrap();
        let rows = s.query("one", 100, 103, 20).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].precision, hb_core::Precision::Exact);
        assert_eq!(rows[1].approx.as_deref(), Some("about half"));
        assert!(!rows[1].charging_inferred);
        assert!(rows[2].charging_inferred);
        assert_eq!(rows[3].name, "Updated device model");
    }
    #[test]
    fn null_gaps_do_not_truncate_the_requested_interval() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(&d.path().join("history.db")).unwrap();
        for ts in 0..3000 {
            let mut r = Reading::new("one", "Mouse", "razer", ts);
            r.level = if ts % 3 == 0 {
                None
            } else {
                Some((ts % 100) as u8)
            };
            s.record(&r).unwrap();
        }
        s.flush().unwrap();
        let points = s.query("one", 0, 2999, 60).unwrap();
        assert!(points.len() <= 60);
        assert!(points.last().unwrap().timestamp >= 2850);
        let endpoints = s.query("one", 0, 2999, 2).unwrap();
        assert_eq!(endpoints[0].timestamp, 0);
        assert_eq!(endpoints[1].timestamp, 2999);
        assert!(s.query("other", 0, 2999, 100).unwrap().is_empty());
    }
    #[test]
    fn shutdown_drop_flushes_and_backward_clock_restarts_pacing() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("history.db");
        {
            let mut s = Store::open(&path).unwrap();
            let mut r = Reading::new("one", "Mouse", "razer", 1000);
            r.level = Some(60);
            s.record(&r).unwrap();
            r.timestamp = 100;
            s.record(&r).unwrap();
        }
        let s = Store::read_only(&path).unwrap();
        assert_eq!(s.query("one", 0, 1000, 100).unwrap().len(), 2);
    }
    #[test]
    fn status_schema_filters_hidden_and_preserves_nullable_percentage() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("status.json");
        let mut engine = hb_core::Engine::new(Settings::default(), Estimator::default());
        let a = Reading::new("unknown", "Controller", "xinput", 0);
        let b = Reading::new("hidden", "Mouse", "razer", 0);
        engine.settings.devices.insert(
            "hidden".into(),
            hb_core::DevicePreferences {
                hidden: true,
                ..Default::default()
            },
        );
        engine.apply("xinput", Ok(vec![a]), 0., false);
        engine.apply("razer", Ok(vec![b]), 0., false);
        write_status(&path, &engine.snapshot(0), true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["updated"], "1970-01-01T00:00:00Z");
        assert_eq!(value["devices"].as_array().unwrap().len(), 1);
        assert!(value["devices"][0]["level"].is_null());
        assert_eq!(value["running"], true);
    }
    #[test]
    fn status_values_include_renames_thresholds_types_and_estimates() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("status.json");
        let mut settings = Settings::default();
        settings.devices.insert(
            "mouse".into(),
            hb_core::DevicePreferences {
                name: Some("My mouse".into()),
                icon: Some("headphones".into()),
                low: Some(13),
                ..Default::default()
            },
        );
        let mut engine = hb_core::Engine::new(settings, Estimator::default());
        let mut reading = Reading::new("mouse", "Factory name", "razer", 1_780_000_000);
        reading.level = Some(42);
        reading.charging = Some(true);
        reading.approx = Some("Medium".into());
        engine.apply("razer", Ok(vec![reading]), 0., false);
        let mut snapshot = engine.snapshot(1_780_000_000);
        snapshot.devices[0].seconds_left = Some(3600);
        snapshot.devices[0].text = "My mouse: Medium · 1h left".into();
        write_status(&path, &snapshot, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value["devices"][0],
            serde_json::json!({
                "key":"mouse", "name":"My mouse", "level":42, "charging":true,
                "online":true, "kind":"headphones", "approx":"Medium",
                "low_alert_at":13, "seconds_left":3600, "text":"My mouse: Medium · 1h left"
            })
        );
        assert_eq!(value["updated_unix"], 1_780_000_000);
        write_status(&path, &Snapshot::default(), false).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["devices"], serde_json::json!([]));
        assert_eq!(value["running"], false);
    }
    fn baseline_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("baseline.db")).unwrap();
        (dir, store)
    }
    fn seed(store: &mut Store, key: &str, timestamp: i64, level: Option<u8>) {
        let mut reading = Reading::new(key, "Mouse", "test", timestamp);
        reading.level = level;
        reading.connection = hb_core::Connection::Sleeping;
        store.record(&reading).unwrap();
        store.flush().unwrap();
    }
    #[test]
    fn sleeping_interval_receives_original_last_known_baseline_only() {
        let (_dir, mut store) = baseline_store();
        seed(&mut store, "a", 90, Some(73));
        let result = store.query_with_baseline("a", 100, 200, 100).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp, 90);
        assert_eq!(result[0].level, Some(73));
        assert_eq!(result[0].connection, hb_core::Connection::Sleeping);
        assert!(store.query("a", 100, 200, 100).unwrap().is_empty());
    }
    #[test]
    fn baseline_skips_unknown_invalid_and_corrupt_predecessors_and_other_devices() {
        let (_dir, mut store) = baseline_store();
        seed(&mut store, "a", 70, Some(60));
        seed(&mut store, "a", 80, Some(101));
        seed(&mut store, "a", 90, None);
        seed(&mut store, "b", 99, Some(95));
        store
            .db
            .execute(
                "INSERT INTO readings(device,ts,level,payload) VALUES('a',95,50,'broken')",
                [],
            )
            .unwrap();
        let result = store.query_with_baseline("a", 100, 200, 2).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            (result[0].key.as_str(), result[0].timestamp, result[0].level),
            ("a", 70, Some(60))
        );
    }
    #[test]
    fn baseline_retention_is_relative_to_interval_end_and_inclusive() {
        let (_dir, mut store) = baseline_store();
        let until = 30 * 86400 + 100;
        seed(&mut store, "a", 99, Some(50));
        assert!(
            store
                .query_with_baseline("a", until - 20, until, 20)
                .unwrap()
                .is_empty()
        );
        seed(&mut store, "a", 100, Some(40));
        assert_eq!(
            store
                .query_with_baseline("a", until - 20, until, 20)
                .unwrap()[0]
                .timestamp,
            100
        );
    }
    #[test]
    fn baseline_budget_preserves_interval_endpoints_and_never_exceeds_cap() {
        let (_dir, mut store) = baseline_store();
        seed(&mut store, "a", 90, Some(80));
        for ts in 100..120 {
            seed(&mut store, "a", ts, Some((ts - 50) as u8));
        }
        let two = store.query_with_baseline("a", 100, 119, 2).unwrap();
        assert_eq!(
            two.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
            vec![90, 119]
        );
        let three = store.query_with_baseline("a", 100, 119, 3).unwrap();
        assert_eq!(
            three.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
            vec![90, 100, 119]
        );
        let all = store.query_with_baseline("a", 100, 119, 30).unwrap();
        assert_eq!(all.len(), 21);
        assert_eq!(all[1].timestamp, 100);
        assert_eq!(all.last().unwrap().timestamp, 119);
        for cap in [0, 1, 2, 3, 4, 8, 4096, 5000] {
            let result = store.query_with_baseline("a", 100, 119, cap).unwrap();
            assert!(result.len() <= cap.clamp(2, 4096));
            assert_eq!(result[0].timestamp, 90);
        }
    }
    fn usage_seed(
        store: &mut Store,
        ts: i64,
        level: Option<u8>,
        connection: hb_core::Connection,
        charging: Option<bool>,
    ) {
        let mut r = Reading::new("usage", "Mouse", "test", ts);
        r.level = level;
        r.connection = connection;
        r.charging = charging;
        store.record(&r).unwrap();
        store.flush().unwrap();
    }
    #[test]
    fn usage_pauses_sleep_charging_unknown_and_resumes_after_wake() {
        use hb_core::Connection::{Online, Sleeping};
        let (_dir, mut store) = baseline_store();
        for (ts, level, connection, charging) in [
            (0, Some(80), Online, Some(false)),
            (60, Some(79), Online, Some(false)),
            (120, Some(79), Sleeping, None),
            (10000, Some(79), Sleeping, None),
            (10060, Some(79), Online, Some(false)),
            (10120, Some(78), Online, Some(false)),
            (10180, Some(78), Online, Some(true)),
            (10240, Some(80), Online, Some(true)),
            (10300, Some(80), Online, Some(false)),
            (10360, None, Online, None),
            (10420, Some(79), Online, Some(false)),
            (10480, Some(78), Online, Some(false)),
        ] {
            usage_seed(&mut store, ts, level, connection, charging);
        }
        let series = store.query_usage("usage", 10480, 3600, 100).unwrap();
        assert_eq!((series.since, series.until), (0, 180));
        assert_eq!(series.axis, hb_core::HistoryAxis::Usage);
        assert_eq!(series.samples.last().unwrap().reading.timestamp, 10480);
        assert_eq!(series.samples.last().unwrap().position, 180);
    }
    #[test]
    fn usage_excludes_app_gaps_and_sampling_does_not_change_total_or_device_scope() {
        let (_dir, mut store) = baseline_store();
        usage_seed(
            &mut store,
            0,
            Some(100),
            hb_core::Connection::Online,
            Some(false),
        );
        usage_seed(
            &mut store,
            10000,
            Some(90),
            hb_core::Connection::Online,
            Some(false),
        );
        for i in 1..101 {
            usage_seed(
                &mut store,
                10000 + i * 60,
                Some((90 - i % 80) as u8),
                hb_core::Connection::Online,
                Some(false),
            );
        }
        seed(&mut store, "other", 16001, Some(50));
        for cap in [2, 3, 4, 10, 4096] {
            let series = store.query_usage("usage", 16001, 1200, cap).unwrap();
            assert_eq!((series.since, series.until), (4800, 6000));
            assert!(series.samples.len() <= cap);
            assert_eq!(series.samples.last().unwrap().reading.timestamp, 16000);
            assert_eq!(series.samples.last().unwrap().position, 6000);
            assert!(series.samples.iter().all(|s| s.reading.key == "usage"));
            assert!(series.samples.first().unwrap().position <= 4800);
        }
    }
    #[test]
    fn usage_empty_sleep_only_and_retention_are_safe() {
        let (_dir, mut store) = baseline_store();
        assert!(
            store
                .query_usage("usage", 100, 0, 0)
                .unwrap()
                .samples
                .is_empty()
        );
        usage_seed(
            &mut store,
            10,
            Some(50),
            hb_core::Connection::Sleeping,
            None,
        );
        let series = store.query_usage("usage", 100, 0, 2).unwrap();
        assert_eq!((series.since, series.until), (0, 0));
        assert_eq!(series.samples[0].reading.timestamp, 10);
        assert!(
            store
                .query_usage("usage", 30 * 86400 + 11, 100, 10)
                .unwrap()
                .samples
                .is_empty()
        );
    }
    #[test]
    fn usage_stale_invalid_and_corrupt_rows_break_interval_continuity() {
        let (_dir, mut store) = baseline_store();
        for (ts, level, state) in [
            (0, Some(50), hb_core::Connection::Online),
            (60, Some(101), hb_core::Connection::Online),
            (120, Some(50), hb_core::Connection::Online),
            (180, Some(50), hb_core::Connection::Stale),
            (240, Some(50), hb_core::Connection::Online),
            (360, Some(50), hb_core::Connection::Online),
            (420, Some(49), hb_core::Connection::Online),
        ] {
            usage_seed(&mut store, ts, level, state, Some(false));
        }
        store
            .db
            .execute(
                "INSERT INTO readings(device,ts,level,payload) VALUES('usage',300,50,'broken')",
                [],
            )
            .unwrap();
        let series = store.query_usage("usage", 420, 3600, 20).unwrap();
        assert_eq!(series.until, 60);
        assert!(
            series
                .samples
                .iter()
                .all(|s| s.reading.level.is_some_and(|level| level <= 100))
        );
    }
    #[test]
    fn prune_repairs_arbitrary_orphans_and_keeps_inclusive_retention_boundary() {
        let (_dir, mut store) = baseline_store();
        for timestamp in [99, 100, 101] {
            usage_seed(
                &mut store,
                timestamp,
                Some(50 + (timestamp % 3) as u8),
                hb_core::Connection::Online,
                None,
            );
        }
        store.flush().unwrap();
        // An external deletion and metadata-only rows must still be repaired,
        // including an orphan whose timestamp lies after the pruning boundary.
        store
            .db
            .execute("DELETE FROM readings WHERE ts=101", [])
            .unwrap();
        for timestamp in [50, 150] {
            store
                .db
                .execute(
                    "INSERT INTO usage_metadata(device,ts,polling_rate,session) VALUES('orphan',?1,1000,'1')",
                    [timestamp],
                )
                .unwrap();
        }
        store.prune(30 * 86400 + 100).unwrap();
        let keys: Vec<(String, i64)> = store
            .db
            .prepare("SELECT device,ts FROM usage_metadata ORDER BY device,ts")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(keys, vec![("usage".into(), 100)]);
    }
    #[test]
    fn routine_retention_uses_index_and_removes_only_expired_rows() {
        let (_dir, mut store) = baseline_store();
        for timestamp in [99, 100, 101] {
            usage_seed(
                &mut store,
                timestamp,
                Some(50 + (timestamp % 3) as u8),
                hb_core::Connection::Online,
                None,
            );
        }
        store.prune_expired(30 * 86400 + 100).unwrap();
        let timestamps: Vec<i64> = store
            .db
            .prepare("SELECT ts FROM usage_metadata ORDER BY ts")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(timestamps, [100, 101]);
        let plan: String = store
            .db
            .query_row(
                "EXPLAIN QUERY PLAN DELETE FROM usage_metadata WHERE ts < ?1",
                [100],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("usage_metadata_time"), "{plan}");
    }
    #[test]
    #[ignore = "explicit synthetic 30-day prune timing; no hardware"]
    fn prune_thirty_day_sql_timing() {
        let (_dir, mut store) = baseline_store();
        let transaction = store.db.transaction().unwrap();
        {
            let mut reading = transaction
                .prepare("INSERT INTO readings(device,ts,level,payload) VALUES(?1,?2,50,'{}')")
                .unwrap();
            let mut metadata = transaction
                .prepare("INSERT INTO usage_metadata(device,ts,polling_rate,session) VALUES(?1,?2,1000,'123456789')")
                .unwrap();
            for device in 0..10 {
                for sample in 0..43200i64 {
                    reading
                        .execute(params![device.to_string(), sample * 60])
                        .unwrap();
                    metadata
                        .execute(params![device.to_string(), sample * 60])
                        .unwrap();
                }
            }
        }
        transaction.commit().unwrap();
        for (name, sql) in [
            (
                "original",
                "DELETE FROM usage_metadata WHERE ts < ?1 OR NOT EXISTS(SELECT 1 FROM readings r WHERE r.device=usage_metadata.device AND r.ts=usage_metadata.ts)",
            ),
            (
                "merged",
                "DELETE FROM usage_metadata WHERE (device,ts) IN (SELECT device,ts FROM usage_metadata EXCEPT SELECT device,ts FROM readings WHERE ts >= ?1 ORDER BY device,ts)",
            ),
            ("indexed-expiry", "DELETE FROM usage_metadata WHERE ts < ?1"),
        ] {
            let plan: Vec<String> = store
                .db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([-1], |row| row.get(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            println!("{name} plan: {plan:?}");
            let mut timings = vec![];
            for _ in 0..7 {
                let start = std::time::Instant::now();
                assert_eq!(store.db.execute(sql, [-1]).unwrap(), 0);
                timings.push(start.elapsed());
            }
            timings.sort();
            println!(
                "{name}: 432000 retained rows, median {:?}, samples {timings:?}",
                timings[3]
            );
        }
    }
    #[test]
    #[ignore = "explicit synthetic 30-day query timing; no hardware"]
    fn usage_thirty_day_stream_timing() {
        let (_dir, mut store) = baseline_store();
        let transaction = store.db.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare("INSERT INTO readings(device,ts,level,payload) VALUES(?1,?2,?3,?4)")
                .unwrap();
            for i in 0..43200i64 {
                let mut r = Reading::new("usage", "Mouse", "test", i * 60);
                r.level = Some((100 - i % 100) as u8);
                r.charging = Some(false);
                insert
                    .execute(params![
                        r.key,
                        r.timestamp,
                        r.level,
                        serde_json::to_string(&r).unwrap()
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        let series = store
            .query_usage("usage", 43199 * 60, 12 * 3600, 1000)
            .unwrap();
        assert_eq!(series.until, 43199 * 60);
        assert!(series.samples.len() <= 1000);
        let reference =
            usage_reference::query_usage(&store.db, "usage", 43199 * 60, 12 * 3600, 1000).unwrap();
        assert_eq!(format!("{series:?}"), format!("{reference:?}"));
        let mut before = Vec::new();
        let mut after = Vec::new();
        // Use the same SQLite connection, page cache and warmed database for
        // both paths. Alternate query order; seeding is outside the timings.
        for round in 0..9 {
            for optimized in [round % 2 == 0, round % 2 != 0] {
                let start = std::time::Instant::now();
                let result = if optimized {
                    store
                        .query_usage("usage", 43199 * 60, 12 * 3600, 1000)
                        .unwrap()
                } else {
                    usage_reference::query_usage(&store.db, "usage", 43199 * 60, 12 * 3600, 1000)
                        .unwrap()
                };
                std::hint::black_box(result);
                let milliseconds = start.elapsed().as_secs_f64() * 1000.;
                if optimized {
                    after.push(milliseconds)
                } else {
                    before.push(milliseconds)
                }
            }
        }
        before.sort_by(f64::total_cmp);
        after.sort_by(f64::total_cmp);
        println!(
            "43,200-row usage query, shared connection: previous median {:.3} ms, sampled median {:.3} ms; before {before:?}; after {after:?}",
            before[4], after[4]
        );
    }
}

#[cfg(test)]
mod usage_index_tests {
    use super::*;
    #[test]
    fn short_usage_queries_reuse_a_bounded_index_and_release_it_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut store = Store::open(&path).unwrap();
        for minute in 0..4000 {
            let mut r = Reading::new("mouse", "Mouse", "simulation", minute * 60);
            r.charging = Some(false);
            r.level = Some((100 - minute % 100) as u8);
            store.record(&r).unwrap();
        }
        store.flush().unwrap();
        let until = 3999 * 60;
        let first = store.query_usage("mouse", until, 600, 80).unwrap();
        assert_eq!(first.until, until);
        store.usage_rows.set(0);
        let second = store.query_usage("mouse", until + 1, 600, 160).unwrap();
        assert_eq!(second.until, first.until);
        assert!(store.usage_rows.get() < 512);
        store.save_estimator(&Estimator::default()).unwrap();
        store
            .record_polling_activity(&PollingActivity {
                key: "mouse".into(),
                timestamp: until,
                session: 1,
                kind: PollingActivityKind::Boost,
                rate: PollingRate::try_from(4000).unwrap(),
                previous: PollingRate::try_from(1000).ok(),
            })
            .unwrap();
        store.usage_rows.set(0);
        let after_unrelated_writes = store.query_usage("mouse", until + 1, 600, 160).unwrap();
        assert_eq!(after_unrelated_writes.until, first.until);
        assert!(
            store.usage_rows.get() < 512,
            "Unrelated checkpoint/activity writes rescanned {} history rows",
            store.usage_rows.get()
        );
        store.prune_expired(until).unwrap();
        store.usage_rows.set(0);
        store.query_usage("mouse", until + 1, 600, 160).unwrap();
        assert!(
            store.usage_rows.get() < 512,
            "short query examined {} rows",
            store.usage_rows.get()
        );
        assert!(
            store
                .usage_index
                .borrow()
                .as_ref()
                .unwrap()
                .points_len_for_test()
                <= 512
        );
        let other = Connection::open(&path).unwrap();
        let mut r = Reading::new("mouse", "Mouse", "simulation", until);
        r.level = Some(1);
        r.charging = Some(false);
        r.connection = hb_core::Connection::Sleeping;
        other
            .execute(
                "UPDATE readings SET payload=?1 WHERE ts=?2",
                params![serde_json::to_string(&r).unwrap(), until],
            )
            .unwrap();
        store.usage_rows.set(0);
        assert_eq!(
            store.query_usage("mouse", until, 600, 80).unwrap().until,
            until - 60
        );
        assert!(store.usage_rows.get() >= 4000);
        // A local insertion inside the existing time bounds must invalidate
        // the index too, even when the first/last row and data_version agree.
        r.timestamp = until - 120;
        store.record(&r).unwrap();
        store.flush().unwrap();
        store.usage_rows.set(0);
        assert_eq!(
            store.query_usage("mouse", until, 600, 80).unwrap().until,
            until - 120
        );
        assert!(store.usage_rows.get() >= 4000);
        store.release_history_cache();
        assert!(store.usage_index.borrow().is_none());
    }
}
