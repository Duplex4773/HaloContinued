//! Display coordinates stay separate from real observation timestamps.
use crate::Reading;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryAxis {
    #[default]
    Usage,
    Calendar,
}

#[derive(Clone, Debug)]
pub struct HistorySample {
    pub reading: Reading,
    pub position: i64,
}

#[derive(Clone, Debug, Default)]
pub struct HistorySeries {
    pub samples: Vec<HistorySample>,
    pub axis: HistoryAxis,
    pub since: i64,
    pub until: i64,
}

impl HistorySeries {
    pub fn calendar(readings: Vec<Reading>, since: i64, until: i64) -> Self {
        Self {
            samples: readings
                .into_iter()
                .map(|reading| HistorySample {
                    position: reading.timestamp,
                    reading,
                })
                .collect(),
            axis: HistoryAxis::Calendar,
            since,
            until,
        }
    }
}
