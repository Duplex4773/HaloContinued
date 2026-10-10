//! Read-only, identifier-free local audit. No hardware access or history repair.
//! cargo run -p hb-storage --example audit_insights -- [path/to/history.db]
use hb_core::{Clock, Reading, SystemClock};
use hb_storage::Store;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| hb_storage::data_dir().join("history.db"));
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let store = Store::read_only(&path)?;
    let now = SystemClock::default().unix();
    let integrity: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    let has_metadata: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_metadata')",
        [],
        |row| row.get(0),
    )?;
    let orphaned_metadata: i64 = if has_metadata {
        db.query_row("SELECT count(*) FROM usage_metadata m LEFT JOIN readings r ON r.device=m.device AND r.ts=m.ts WHERE r.device IS NULL", [], |row| row.get(0))?
    } else {
        0
    };
    let mut invalid_rate_metadata = 0u64;
    if has_metadata {
        let mut metadata = db.prepare("SELECT polling_rate,session FROM usage_metadata")?;
        let mut rows = metadata.query([])?;
        while let Some(row) = rows.next()? {
            let rate: Option<i64> = row.get(0)?;
            let session: Option<String> = row.get(1)?;
            if rate.is_some_and(|hz| ![125, 250, 500, 1000, 2000, 4000, 8000].contains(&hz))
                || rate.is_some() && session.as_ref().is_none_or(|s| s.parse::<u64>().is_err())
            {
                invalid_rate_metadata += 1;
            }
        }
    }
    let mut keys = db.prepare("SELECT DISTINCT device FROM readings ORDER BY device")?;
    let mut devices = Vec::new();
    for (index, key) in keys
        .query_map([], |row| row.get::<_, String>(0))?
        .enumerate()
    {
        let key = key?;
        let mut query =
            db.prepare("SELECT ts,level,payload FROM readings WHERE device=?1 ORDER BY ts")?;
        let mut rows = query.query([&key])?;
        let mut states = BTreeMap::<String, u64>::new();
        let mut charging = BTreeMap::<String, u64>::new();
        let (mut count, mut malformed, mut mismatched, mut invalid_levels, mut future) =
            (0, 0, 0, 0, 0);
        let (mut first, mut last, mut level) = (None, None, None);
        while let Some(row) = rows.next()? {
            count += 1;
            let timestamp: i64 = row.get(0)?;
            first.get_or_insert(timestamp);
            last = Some(timestamp);
            future += u64::from(timestamp > now);
            let stored_level: Option<i64> = row.get(1)?;
            let payload: String = row.get(2)?;
            let Ok(reading) = serde_json::from_str::<Reading>(&payload) else {
                malformed += 1;
                continue;
            };
            mismatched += u64::from(
                reading.key != key
                    || reading.timestamp != timestamp
                    || reading.level.map(i64::from) != stored_level,
            );
            invalid_levels += u64::from(reading.level.is_some_and(|level| level > 100));
            level = reading.level;
            *states
                .entry(format!("{:?}", reading.connection))
                .or_default() += 1;
            *charging
                .entry(format!("{:?}", reading.charging))
                .or_default() += 1;
        }
        let summary = store.query_insights(&key, now)?;
        let coverage = summary.coverage;
        let rates: Vec<_> = summary.rates.iter().map(|rate| json!({
            "hz":rate.hz, "awake_seconds":rate.awake_seconds, "projection_seconds":rate.projection_seconds, "consumed_points":rate.consumed_percent,
            "projection_consumed_points":rate.projection_consumed_percent, "projection_drop_count":rate.projection_drop_count,
            "samples":rate.sample_count, "drops":rate.drop_count, "confidence":format!("{:?}",rate.confidence),
            "full_charge_hours":rate.projected_full_charge_hours, "remaining_hours":rate.remaining_hours
        })).collect();
        let cycles: Vec<_> = summary
            .cycles
            .iter()
            .map(|cycle| {
                json!({
                    "start_percent":cycle.start_percent, "end_percent":cycle.end_percent,
                    "awake_seconds":cycle.awake_seconds, "consumed_points":cycle.consumed_percent,
                    "evidence":format!("{:?}",cycle.evidence)
                })
            })
            .collect();
        devices.push(json!({"device":index+1,"rows":count,"malformed":malformed,
            "mismatched":mismatched,"invalid_levels":invalid_levels,"future_rows":future,
            "first_timestamp":first,"last_timestamp":last,"last_level":level,
            "states":states,"charging":charging,"rates":rates,"cycles":cycles,
            "coverage":{"readings":coverage.observation_count,"discharge_readings":coverage.discharge_sample_count,
                "awake_seconds":coverage.awake_seconds,"excluded_intervals":coverage.excluded_interval_count,
                "unreadable_rows":coverage.unreadable_row_count}}));
    }
    // Labels are ordinal only. Never emit keys, names, paths, serials or containers.
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"as_of":now,"integrity":integrity,"orphaned_metadata":orphaned_metadata,"invalid_rate_metadata":invalid_rate_metadata,"devices":devices})
        )?
    );
    Ok(())
}
