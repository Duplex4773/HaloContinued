//! Synthetic Insights benchmark. An explicit new database path is mandatory.
//! Never opens user history or hardware. Seeding/warmup are outside query timings.
use hb_core::{HistoryStore, PollingRate, Reading, UsageObservation};
use hb_storage::Store;
use std::{hint::black_box, path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("provide a new fixture database path")?,
    );
    if path.exists() {
        return Err("fixture path already exists; use a fresh path".into());
    }
    let mut store = Store::open(&path)?;
    const ROWS: i64 = 43_200;
    for minute in 0..ROWS {
        let mut reading = Reading::new(
            "isa:synthetic",
            "Synthetic mouse",
            "simulation",
            minute * 60,
        );
        reading.kind = "mouse".into();
        reading.level = Some((100 - (minute % 3000) / 30) as u8);
        reading.charging = Some(false);
        let hz = if minute % 300 < 60 { 4000 } else { 1000 };
        store.record_usage(&UsageObservation {
            reading,
            polling_rate: Some(PollingRate::try_from(hz)?),
            session: Some(1),
        })?;
        if minute % 1000 == 999 {
            store.flush()?;
        }
    }
    store.flush()?;
    let until = (ROWS - 1) * 60;
    let expected = store.query_insights("isa:synthetic", until)?;
    let fingerprint = format!("{expected:?}");
    assert_eq!(expected.rates.len(), 2);
    black_box(store.query_insights("isa:synthetic", until)?);
    let mut timings = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        let result = black_box(store.query_insights("isa:synthetic", until)?);
        timings.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(format!("{result:?}"), fingerprint);
    }
    let mut sorted = timings.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({
            "rows": ROWS, "samples_ms": timings, "median_ms": sorted[4],
            "min_ms": sorted[0], "max_ms": sorted[8],
            "result_fingerprint": fingerprint,
            "workload": "Warm 30-day SQLite Insights query with two polling rates; synthetic rows; seeding excluded"
        })
    );
    Ok(())
}
