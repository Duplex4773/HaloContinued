//! Bounded calendar samples from one ordered scan of battery observations.
use super::{ProviderError, Store, history_reading, row_text, sql_error};
use crate::usage_sampler::{TimingIdentity, TimingReading};
use hb_core::{Connection, Reading};
use rusqlite::params;

#[derive(Clone, Copy)]
struct Point {
    timestamp: i64,
    ordinal: i64,
    level: Option<u8>,
    available: bool,
}

#[derive(Default, Clone)]
struct Bucket {
    low: Option<Point>,
    high: Option<Point>,
    before_gap: Option<Point>,
    gap: Option<Point>,
    wake: Option<Point>,
    before_drop: Option<Point>,
}

impl Bucket {
    fn observe(
        &mut self,
        point: Point,
        previous: Option<Point>,
        extra: &mut Vec<Point>,
        limit: usize,
    ) {
        if point.available
            && previous.is_some_and(|p| p.available && p.level > point.level)
            && self.before_drop.is_none()
        {
            self.before_drop = previous;
        }
        if let Some(level) = point.level {
            if self.low.and_then(|p| p.level).is_none_or(|lo| level < lo) {
                self.low = Some(point);
            }
            if self.high.and_then(|p| p.level).is_none_or(|hi| level > hi) {
                self.high = Some(point);
            }
        }
        if !point.available && self.gap.is_none() {
            self.gap = Some(point);
            self.before_gap = previous.filter(|p| p.available);
        } else if !point.available && previous.is_some_and(|p| p.available) && extra.len() < limit {
            extra.push(point);
        }
        if point.available && previous.is_some_and(|p| !p.available) {
            if self.wake.is_none() {
                self.wake = Some(point);
            } else if extra.len() < limit {
                extra.push(point);
            }
        }
    }
}

struct Sampler {
    limit: usize,
    since: i64,
    until: i64,
    exact: Vec<Point>,
    buckets: Vec<Bucket>,
    extra_transitions: Vec<Point>,
    width: i128,
    first: Option<Point>,
    last: Option<Point>,
}

impl Sampler {
    fn new(limit: usize, since: i64, until: i64) -> Self {
        Self {
            limit,
            since,
            until,
            exact: Vec::new(),
            buckets: Vec::new(),
            extra_transitions: Vec::new(),
            width: 1,
            first: None,
            last: None,
        }
    }

    fn push(&mut self, point: Point) {
        if self.buckets.is_empty() {
            if self.exact.len() < self.limit {
                self.exact.push(point);
                return;
            }
            let count = (self.limit.saturating_sub(2) / 5).max(1);
            let span = i128::from(self.until) - i128::from(self.since) + 1;
            self.width = (span + count as i128 - 1) / count as i128;
            self.buckets.resize(count, Bucket::default());
            for old in std::mem::take(&mut self.exact) {
                self.sample(old);
            }
        }
        self.sample(point);
    }

    fn sample(&mut self, point: Point) {
        let slot = ((i128::from(point.timestamp) - i128::from(self.since)) / self.width) as usize;
        self.buckets[slot].observe(point, self.last, &mut self.extra_transitions, self.limit);
        self.first.get_or_insert(point);
        self.last = Some(point);
    }

    fn finish(self) -> Vec<Point> {
        if self.buckets.is_empty() {
            return self.exact;
        }
        let mut points = Vec::with_capacity(self.limit);
        if self.limit > 1 {
            points.extend(self.first);
        }
        points.extend(self.last);
        let slots = self.limit.saturating_sub(2);
        for bucket in &self.buckets {
            // Very small charts reserve the endpoints and an explicit gap.
            // Normal widths can retain the last live level and the actual wake.
            let candidates = [
                bucket.gap,
                bucket.low,
                bucket.high,
                bucket.wake,
                bucket.before_gap,
            ];
            let count = if slots < 3 {
                slots.min(1)
            } else {
                slots.min(5)
            };
            points.extend(candidates.into_iter().take(count).flatten());
        }
        let order = |p: &Point| (p.timestamp, p.ordinal);
        points.sort_unstable_by_key(order);
        points.dedup_by_key(|p| order(p));
        // Flat stretches and coincident extrema leave slots free. Use them for
        // later sleep/wake transitions in the same bucket, in recording order.
        let primary = points.len();
        for point in self
            .extra_transitions
            .into_iter()
            .chain(self.buckets.into_iter().filter_map(|b| b.before_drop))
        {
            if points.len() == self.limit {
                break;
            }
            if points[..primary]
                .binary_search_by_key(&order(&point), order)
                .is_err()
            {
                points.push(point);
            }
        }
        if points.len() > primary {
            points.sort_unstable_by_key(order);
            points.dedup_by_key(|p| order(p));
        }
        points
    }
}

impl Store {
    pub(super) fn calendar_interval(
        &self,
        key: &str,
        since: i64,
        until: i64,
        limit: usize,
        latest_unit: bool,
    ) -> Result<Vec<Reading>, ProviderError> {
        if since > until {
            return Ok(Vec::new());
        }
        let snapshot = self.db.unchecked_transaction().map_err(sql_error)?;
        // Begin at the exact predecessor. Later rows in that same second may
        // replace it, even when the replacement has no battery percentage.
        let mut baseline = None;
        let mut first_ordinal = i64::MIN;
        let retained = until.saturating_sub(30 * 86400);
        if latest_unit && since > retained {
            let sql = if self.sequenced {
                "SELECT ts,payload,sequence FROM readings WHERE device=?1 AND ts>=?2 AND ts<?3 AND level BETWEEN 0 AND 100 ORDER BY ts DESC,sequence DESC"
            } else {
                "SELECT ts,payload,rowid FROM readings WHERE device=?1 AND ts>=?2 AND ts<?3 AND level BETWEEN 0 AND 100 ORDER BY ts DESC,rowid DESC"
            };
            let mut query = self.db.prepare(sql).map_err(sql_error)?;
            let mut rows = query
                .query(params![key, retained, since.min(until)])
                .map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                if let Some(r) = history_reading(row, key).map_err(sql_error)?
                    && r.level.is_some()
                {
                    first_ordinal = row.get(2).map_err(sql_error)?;
                    baseline = Some(r);
                    break;
                }
            }
        }

        let from = baseline.as_ref().map_or(since, |r| r.timestamp);
        let sql = if self.sequenced {
            "SELECT ts,payload,sequence FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND (ts>?2 OR sequence>=?4) ORDER BY ts,sequence"
        } else {
            "SELECT ts,payload,rowid FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 AND (ts>?2 OR rowid>=?4) ORDER BY ts,rowid"
        };
        // An index-only bounded probe avoids a second lookup and JSON decode
        // per point when the entire interval fits on screen.
        let dense: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM readings WHERE device=?1 AND ts BETWEEN ?2 AND ?3 LIMIT 1 OFFSET ?4)",
            params![key, from, until, limit as i64], |r| r.get(0),
        ).map_err(sql_error)?;
        if !dense {
            let mut out: Vec<_> = baseline.into_iter().collect();
            {
                let mut query = self.db.prepare(sql).map_err(sql_error)?;
                let mut rows = query
                    .query(params![key, from, until, first_ordinal])
                    .map_err(sql_error)?;
                while let Some(row) = rows.next().map_err(sql_error)? {
                    if let Some(reading) = history_reading(row, key).map_err(sql_error)? {
                        if latest_unit
                            && out
                                .last()
                                .is_some_and(|old| !old.same_battery_identity(&reading))
                        {
                            out.clear();
                        }
                        if reading.timestamp >= since {
                            out.push(reading);
                        }
                    }
                }
            }
            snapshot.commit().map_err(sql_error)?;
            return Ok(out);
        }
        let mut identity: Option<TimingIdentity> = None;
        let mut sampler = Sampler::new(limit - usize::from(baseline.is_some()), since, until);
        {
            let mut query = self.db.prepare(sql).map_err(sql_error)?;
            let mut rows = query
                .query(params![key, from, until, first_ordinal])
                .map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let (Ok(ts), Ok(payload)) = (row.get::<_, i64>(0), row_text(row, 1)) else {
                    continue;
                };
                let Ok(r) = serde_json::from_str::<TimingReading<'_>>(payload) else {
                    continue;
                };
                if r.key != key || r.timestamp != ts || r.level.is_some_and(|v| v > 100) {
                    continue;
                }
                if latest_unit && identity.as_ref().is_none_or(|old| !old.same_battery(&r)) {
                    if identity.is_some() {
                        baseline = None;
                        sampler = Sampler::new(limit, since.max(ts), until);
                    }
                    identity = Some(TimingIdentity::from(&r));
                }
                if ts >= since {
                    sampler.push(Point {
                        timestamp: ts,
                        ordinal: row.get(2).map_err(sql_error)?,
                        level: r.level,
                        available: r.connection == Connection::Online && r.level.is_some(),
                    });
                }
            }
        }
        let points = sampler.finish();
        let mut out = Vec::with_capacity(points.len() + usize::from(baseline.is_some()));
        out.extend(baseline);
        {
            let mut fetch = self.db.prepare(self.history_row_sql()).map_err(sql_error)?;
            for point in points {
                if let Some(r) = fetch
                    .query_row(params![key, point.timestamp, point.ordinal], |r| {
                        history_reading(r, key)
                    })
                    .map_err(sql_error)?
                {
                    out.push(r);
                }
            }
        }
        snapshot.commit().map_err(sql_error)?;
        Ok(out)
    }
}
