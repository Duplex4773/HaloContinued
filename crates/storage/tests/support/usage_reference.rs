// Straightforward owned-reading sampler used only as a behavioural/performance oracle.
// Keep this independent of the production sampler so representation changes
// cannot silently alter selected extrema, boundaries or usage coordinates.
use hb_core::{ProviderError, Reading};
use rusqlite::{Connection, params};
fn sql_error(e: rusqlite::Error) -> ProviderError {
    ProviderError::new(e.to_string())
}
fn row_text<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a str> {
    row.get_ref(index)?.as_str().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
pub fn query_usage(
    db: &Connection,
    key: &str,
    until: i64,
    seconds: i64,
    max_points: usize,
) -> Result<hb_core::HistorySeries, ProviderError> {
    use hb_core::{HistoryAxis, HistorySample, HistorySeries};
    let limit = max_points.clamp(2, 4096);
    let total = stream_usage(db, key, until, |_, _| {})?;
    let since = total.saturating_sub(seconds.max(1)).max(0);
    let slots = limit.saturating_sub(2);
    let buckets = if slots == 2 { 1 } else { slots / 3 };
    let mut input = Vec::new();
    let mut baseline = None;
    stream_usage(db, key, until, |reading, position| {
        if reading.level.is_some_and(|l| l <= 100) {
            let sample = HistorySample { reading, position };
            if position < since {
                baseline = Some(sample);
            } else {
                input.extend(baseline.take());
                input.push(sample);
            }
        }
    })?;
    let mut edges = Vec::new();
    for run in input.chunk_by(|a, b| {
        a.reading.level == b.reading.level
            && a.reading.online() == b.reading.online()
            && a.reading.charging == b.reading.charging
    }) {
        edges.push((&run[0], false));
        if run.len() > 1 {
            edges.push((run.last().unwrap(), true));
        }
    }
    let samples = if edges.len() <= limit {
        edges.iter().map(|(s, _)| (*s).clone()).collect()
    } else {
        let mut samples = vec![
            input.first().unwrap().clone(),
            input.last().unwrap().clone(),
        ];
        let span = (total - since).max(1);
        for i in 0..buckets {
            let mut points = edges
                .iter()
                .enumerate()
                .filter(|(_, (s, _))| {
                    (((s.position - since).max(0) as i128 * buckets as i128 / span as i128)
                        as usize)
                        .min(buckets - 1)
                        == i
                })
                .peekable();
            if let Some(&(_, (first, _))) = points.peek() {
                let (mut low, mut high, mut anchor) = (*first, *first, None);
                for (n, (s, end)) in points {
                    if s.reading.level < low.reading.level {
                        low = s;
                    }
                    if s.reading.level > high.reading.level {
                        high = s;
                    }
                    if slots >= 3
                        && *end
                        && anchor.is_none()
                        && edges
                            .get(n + 1)
                            .is_some_and(|(next, _)| next.reading.level < s.reading.level)
                    {
                        anchor = Some(*s);
                    }
                }
                samples.extend([low.clone(), high.clone()]);
                samples.extend(anchor.cloned());
            }
        }
        samples.sort_by_key(|s| s.reading.timestamp);
        samples.dedup_by_key(|s| s.reading.timestamp);
        samples
    };
    Ok(HistorySeries {
        samples,
        axis: HistoryAxis::Usage,
        since,
        until: total,
    })
}
fn stream_usage(
    db: &Connection,
    key: &str,
    until: i64,
    mut visit: impl FnMut(Reading, i64),
) -> Result<i64, ProviderError> {
    let sql = "SELECT r.ts,r.payload,m.session FROM readings r LEFT JOIN usage_metadata m ON m.device=r.device AND m.ts=r.ts WHERE r.device=?1 AND r.ts BETWEEN ?2 AND ?3 ORDER BY r.ts";
    let mut query = db.prepare(sql).map_err(sql_error)?;
    let mut rows = query
        .query(params![key, until.saturating_sub(30 * 86400), until])
        .map_err(sql_error)?;
    // Continuity needs only time and awake state, not another owned copy
    // of every reading and its strings during both chart passes.
    let mut previous: Option<(i64, bool, Option<u64>, u16)> = None;
    let mut total = 0i64;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let timestamp: i64 = row.get(0).map_err(sql_error)?;
        let payload = row_text(row, 1).map_err(sql_error)?;
        let Ok(reading) = serde_json::from_str::<Reading>(payload) else {
            previous = None;
            continue;
        };
        if reading.key != key || reading.timestamp != timestamp {
            previous = None;
            continue;
        }
        let awake = |r: &Reading| {
            r.online()
                && r.level.is_some_and(|level| level <= 100)
                && r.charging == Some(false)
                && !r.charging_inferred
        };
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
                    visit(reading, total);
                    continue;
                }
            },
        };
        let reading_awake = awake(&reading);
        if let Some((timestamp, true, previous_session, limit)) = previous
            && reading_awake
            && previous_session == session
            && reading
                .timestamp
                .checked_sub(timestamp)
                .is_some_and(|delta| (1..=i64::from(limit)).contains(&delta))
        {
            total = total.saturating_add(reading.timestamp - timestamp);
        }
        previous = Some((
            reading.timestamp,
            reading_awake,
            session,
            hb_core::usage_gap_seconds(reading.next_poll_seconds),
        ));
        visit(reading, total);
    }
    Ok(total)
}
