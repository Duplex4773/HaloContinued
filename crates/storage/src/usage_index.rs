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
    pub revision: (u64, i64),
    pub total: i64,
    points: Vec<Checkpoint>,
    stride: usize,
    count: usize,
}
impl UsageIndex {
    pub fn new(key: &str, bounds: Option<(i64, i64, i64, i64)>, revision: (u64, i64)) -> Self {
        Self {
            key: key.into(),
            bounds,
            revision,
            total: 0,
            points: Vec::new(),
            stride: 256,
            count: 0,
        }
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
        let mut index = UsageIndex::new("mouse", None, (0, 0));
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
