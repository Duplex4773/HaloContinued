//! One on-demand, bounded timing index. No raw history or device strings per row.
#[derive(Clone, Copy, Default)]
pub(super) struct Checkpoint {
    pub timestamp: i64,
    pub row: i64,
    pub position: i64,
}
pub(super) struct UsageIndex {
    pub key: String,
    pub bounds: Option<(i64, i64, i64, i64)>,
    pub revision: i64,
    pub total: i64,
    pub segment: Option<Checkpoint>,
    identity: Option<super::usage_sampler::TimingIdentity>,
    // Four row references reproduce both endpoints and extrema at position zero.
    zero: [Option<(i64, i64, u8)>; 4],
    points: Vec<Checkpoint>,
    stride: usize,
    count: usize,
}
impl UsageIndex {
    pub fn new(key: &str, bounds: Option<(i64, i64, i64, i64)>, revision: i64) -> Self {
        Self {
            key: key.into(),
            bounds,
            revision,
            total: 0,
            segment: None,
            identity: None,
            zero: [None; 4],
            points: Vec::new(),
            stride: 256,
            count: 0,
        }
    }
    pub fn observe(
        &mut self,
        r: &super::usage_sampler::TimingReading<'_>,
        row: i64,
        position: i64,
        origin: &mut i64,
    ) {
        if r.level.is_some_and(|level| level > 100) {
            return;
        }
        if self
            .identity
            .as_ref()
            .is_none_or(|old| !old.same_battery(r))
        {
            self.identity = Some(super::usage_sampler::TimingIdentity::from(r));
            self.segment = Some(Checkpoint {
                timestamp: r.timestamp,
                row,
                position: 0,
            });
            *origin = position;
            self.points.clear();
            self.count = 0;
            self.stride = 256;
            self.zero = [None; 4];
        }
        let Some(level) = r.level else {
            return;
        };
        let point = Checkpoint {
            timestamp: r.timestamp,
            row,
            position: position - *origin,
        };
        if point.position == 0 {
            let sample = (r.timestamp, row, level);
            self.zero[0].get_or_insert(sample);
            self.zero[1] = Some(sample);
            if self.zero[2].is_none_or(|p| level < p.2) {
                self.zero[2] = Some(sample);
            }
            if self.zero[3].is_none_or(|p| level > p.2) {
                self.zero[3] = Some(sample);
            }
        }
        self.push(point);
    }

    /// Reuse zero-duration history as its retention window advances. An expired
    /// extreme is still proven if the new first reading has the same level.
    pub fn trim_zero(&mut self, start: Checkpoint, first_level: Option<u8>) -> bool {
        let retained = |(ts, row, _): (i64, i64, u8)| (ts, row) >= (start.timestamp, start.row);
        if self.zero[1].is_none_or(|p| !retained(p)) {
            self.zero = [None; 4];
        } else if self.zero[0].is_some_and(|p| !retained(p)) {
            let Some(level) = first_level else {
                return false;
            };
            if self.zero[2..]
                .iter()
                .flatten()
                .any(|p| !retained(*p) && p.2 != level)
            {
                return false;
            }
            for sample in &mut self.zero {
                if sample.is_some_and(|p| !retained(p)) {
                    *sample = Some((start.timestamp, start.row, level));
                }
            }
        }
        self.segment = Some(start);
        self.rebase(start.timestamp, start.row, 0);
        true
    }

    pub fn zero_rows(&self, extrema: bool) -> Vec<(i64, i64)> {
        let mut rows: Vec<_> = self.zero[..if extrema { 4 } else { 2 }]
            .iter()
            .flatten()
            .map(|&(ts, row, _)| (ts, row))
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    pub fn push(&mut self, point: Checkpoint) {
        self.count += 1;
        if !self.count.is_multiple_of(self.stride) {
            return;
        }
        if self.points.len() == 512 {
            let mut index = 0usize;
            self.points.retain(|_| {
                index += 1;
                index.is_multiple_of(2)
            });
            self.stride *= 2;
            if !self.count.is_multiple_of(self.stride) {
                return;
            }
        }
        self.points.push(point);
    }
    pub fn first_retained(&self, timestamp: i64, row: i64) -> Option<Checkpoint> {
        self.points
            .iter()
            .copied()
            .find(|p| (p.timestamp, p.row) >= (timestamp, row))
    }
    pub fn rebase(&mut self, timestamp: i64, row: i64, offset: i64) {
        self.points.retain_mut(|p| {
            if (p.timestamp, p.row) < (timestamp, row) {
                return false;
            }
            p.position -= offset;
            true
        });
        self.total -= offset;
    }
    #[cfg(test)]
    pub fn points_len_for_test(&self) -> usize {
        self.points.len()
    }
    pub fn before(&self, position: i64) -> Option<Checkpoint> {
        self.points
            .iter()
            .rev()
            .find(|point| point.position < position)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_remains_bounded_after_repeated_compaction() {
        let mut index = UsageIndex::new("mouse", None, 0);
        for n in 0..1_000_000 {
            index.push(Checkpoint {
                timestamp: n,
                row: n,
                position: n,
            });
        }
        assert!(index.points.len() <= 512);
        assert!(
            index
                .points
                .windows(2)
                .all(|p| p[0].timestamp < p[1].timestamp)
        );
        assert!(index.before(999_999).unwrap().position < 999_999);
    }
}
