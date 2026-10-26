//! Bounded, conservative summaries of a single device's retained raw history.
use crate::{PollingRate, Precision, Reading};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct UsageObservation {
    pub reading: Reading,
    pub polling_rate: Option<PollingRate>,
    pub session: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InsightConfidence {
    #[default]
    Insufficient,
    Low,
    Moderate,
}

#[derive(Clone, Debug, Default)]
pub struct RateInsight {
    pub hz: u32,
    pub awake_seconds: u64,
    /// Accepted time in complete drop-to-drop windows. The initial plateau and
    /// unfinished flat tail of each uninterrupted segment remain coverage only.
    pub projection_seconds: u64,
    pub projection_consumed_percent: u64,
    /// Complete drop-to-drop windows, excluding each segment's first drop.
    pub projection_drop_count: u64,
    pub consumed_percent: u64,
    /// Distinct endpoints of accepted intervals.
    pub sample_count: u64,
    /// Accepted intervals reaching a new low within their uninterrupted segment.
    pub drop_count: u64,
    pub confidence: InsightConfidence,
    pub projected_full_charge_hours: Option<f64>,
    pub remaining_hours: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleEvidence {
    /// Discharge started after an explicitly observed charging reading.
    /// This does not imply that the battery reached 100%.
    ObservedCharge,
    InferredCharge,
    /// Only part of an unknown charge cycle was observed.
    Partial,
}

#[derive(Clone, Debug)]
pub struct ChargeCycle {
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub awake_seconds: u64,
    pub start_percent: u8,
    pub end_percent: u8,
    pub consumed_percent: u64,
    pub evidence: CycleEvidence,
    pub current: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InsightCoverage {
    pub observation_count: u64,
    /// Exact online readings explicitly known to be discharging.
    pub discharge_sample_count: u64,
    /// Accepted adjacent discharge time, including unknown polling rates.
    pub awake_seconds: u64,
    /// Adjacent pairs excluded from usage, counted once regardless of reason.
    pub excluded_interval_count: u64,
    /// Raw rows the caller could not decode; not included in observation_count.
    pub unreadable_row_count: u64,
    pub last_reading_timestamp: Option<i64>,
    /// Rate evidence attached to the latest stored reading, not an old tooltip.
    pub latest_polling_rate: Option<PollingRate>,
}

#[derive(Clone, Debug, Default)]
pub struct BatteryInsights {
    pub recent: crate::RecentUse,
    /// Only rates with accepted observed usage, in ascending Hz order.
    pub rates: Vec<RateInsight>,
    /// At most ten recent observed discharge periods, oldest first.
    pub cycles: Vec<ChargeCycle>,
    pub coverage: InsightCoverage,
}

impl BatteryInsights {
    /// Project the observed time mix, weighting drain (not runtime) by usage.
    /// Every represented rate must independently have enough measured evidence.
    /// This is historical confirmed-rate use, not a prediction of future gaming.
    pub fn mixed_full_charge_hours(&self) -> Option<f64> {
        let mut count = 0;
        let mut seconds = 0.0;
        let mut weighted_drain = 0.0;
        for rate in self.rates.iter().filter(|r| r.awake_seconds > 0) {
            let hours = rate.projected_full_charge_hours?;
            if !hours.is_finite()
                || hours <= 0.0
                || rate.projection_seconds < 1800
                || rate.projection_consumed_percent < 3
                || rate.confidence == InsightConfidence::Insufficient
            {
                return None;
            }
            count += 1;
            seconds += rate.awake_seconds as f64;
            weighted_drain += rate.awake_seconds as f64 / hours;
        }
        let hours = seconds / weighted_drain;
        (count >= 2 && hours.is_finite() && hours > 0.0).then_some(hours)
    }
}

pub struct InsightsBuilder {
    recent: crate::RecentUse,
    previous_activity: Option<crate::PollingActivityKind>,
    previous: Option<UsageObservation>,
    contiguous: bool,
    rates: [RateInsight; 7],
    previous_counted_rate: Option<u32>,
    pending_projection_seconds: u64,
    projection_anchored: bool,
    cycles: VecDeque<ChargeCycle>,
    pending_charge: Option<CycleEvidence>,
    segment_low: Option<u8>,
    cycle_low: Option<u8>,
    coverage: InsightCoverage,
    has_input: bool,
}

impl Default for InsightsBuilder {
    fn default() -> Self {
        Self {
            recent: crate::RecentUse::default(),
            previous_activity: None,
            previous: None,
            contiguous: false,
            rates: [125, 250, 500, 1000, 2000, 4000, 8000].map(|hz| RateInsight {
                hz,
                ..RateInsight::default()
            }),
            previous_counted_rate: None,
            pending_projection_seconds: 0,
            projection_anchored: false,
            cycles: VecDeque::new(),
            pending_charge: None,
            segment_low: None,
            cycle_low: None,
            coverage: InsightCoverage::default(),
            has_input: false,
        }
    }
}

fn valid_level(reading: &Reading) -> Option<u8> {
    reading.level.filter(|level| *level <= 100).filter(|_| {
        reading.online() && reading.precision == Precision::Exact && reading.timestamp >= 0
    })
}

fn discharging(reading: &Reading) -> bool {
    valid_level(reading).is_some() && reading.charging == Some(false) && !reading.charging_inferred
}

impl InsightsBuilder {
    pub fn recent_since(mut self, since: i64) -> Self {
        self.recent.since = since;
        self
    }
    /// Use when a corrupt or missing raw row prevents proving adjacency.
    pub fn break_continuity(&mut self) {
        self.coverage.latest_polling_rate = None;
        self.coverage.unreadable_row_count = self.coverage.unreadable_row_count.saturating_add(1);
        self.has_input = true;
        self.reset_continuity();
    }

    fn reset_continuity(&mut self) {
        // Retain the previous identity across damaged rows without cloning its
        // strings. It must not supply an interval or a remaining-use estimate.
        self.contiguous = false;
        self.previous_counted_rate = None;
        self.pending_projection_seconds = 0;
        self.projection_anchored = false;
        self.pending_charge = None;
        self.segment_low = None;
        self.cycle_low = None;
        self.close_cycle();
    }

    fn close_cycle(&mut self) {
        if let Some(cycle) = self.cycles.back_mut() {
            cycle.current = false;
        }
    }

    fn start_cycle(&mut self, observation: &UsageObservation) {
        if self.cycles.len() == 10 {
            self.cycles.pop_front();
        }
        let reading = &observation.reading;
        let level = valid_level(reading).expect("validated discharge");
        self.cycle_low = Some(level);
        self.cycles.push_back(ChargeCycle {
            start_timestamp: reading.timestamp,
            end_timestamp: reading.timestamp,
            awake_seconds: 0,
            start_percent: level,
            end_percent: level,
            consumed_percent: 0,
            evidence: self.pending_charge.take().unwrap_or(CycleEvidence::Partial),
            current: true,
        });
    }

    /// Feed chronological raw observations for one device. All memory is bounded.
    /// Equal/backwards timestamps start a partial period; a different physical
    /// unit starts fresh learning even when it reuses the same storage key.
    /// Session/rate changes and pauses exclude their boundary interval only.
    pub fn push(&mut self, observation: UsageObservation) {
        self.push_with_activity(observation, None);
    }
    pub fn push_with_activity(
        &mut self,
        observation: UsageObservation,
        activity: Option<crate::PollingActivityKind>,
    ) {
        let reading = &observation.reading;
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| !reading.same_battery_identity(&previous.reading))
        {
            // Preserve historical sessions/totals, but do not lend their drain
            // model or recent-rate mix to a different physical battery.
            let since = self.recent.since.max(reading.timestamp);
            self.reset_continuity();
            self.previous = None;
            self.previous_activity = None;
            self.recent = crate::RecentUse {
                since,
                ..Default::default()
            };
            self.rates = crate::INSIGHT_RATES.map(|hz| RateInsight {
                hz,
                ..Default::default()
            });
        }
        if observation.polling_rate.is_some()
            && observation.session.is_some()
            && valid_level(reading).is_some()
        {
            self.recent.last_confirmed_timestamp = Some(reading.timestamp);
        }
        let had_input = self.has_input;
        self.has_input = true;
        self.coverage.observation_count = self.coverage.observation_count.saturating_add(1);
        self.coverage.last_reading_timestamp = Some(reading.timestamp);
        self.coverage.latest_polling_rate = observation
            .polling_rate
            .filter(|_| observation.session.is_some() && valid_level(reading).is_some());
        if discharging(reading) {
            self.coverage.discharge_sample_count =
                self.coverage.discharge_sample_count.saturating_add(1);
        }
        let discontinuity = self.previous.as_ref().is_some_and(|previous| {
            let old = &previous.reading;
            reading.timestamp <= old.timestamp
        });
        if discontinuity {
            self.reset_continuity();
        }

        let mut accepted = false;

        if valid_level(reading).is_some() && reading.charging == Some(true) {
            self.close_cycle();
            self.segment_low = None;
            self.pending_charge = Some(
                if reading.charging_inferred
                    && self.pending_charge != Some(CycleEvidence::ObservedCharge)
                {
                    CycleEvidence::InferredCharge
                } else {
                    CycleEvidence::ObservedCharge
                },
            );
        } else if discharging(reading) {
            let previous_observation = self.previous.take();
            let previous = previous_observation.as_ref();
            let delta = previous.and_then(|p| reading.timestamp.checked_sub(p.reading.timestamp));
            let continuous = self.contiguous
                && previous.is_some_and(|p| {
                    discharging(&p.reading)
                        && delta.is_some_and(|seconds| (1..=600).contains(&seconds))
                        && reading.via == p.reading.via
                        && observation.polling_rate == p.polling_rate
                        && observation.session == p.session
                });
            let level = reading.level.unwrap();
            let increase = self.pending_charge.is_none()
                && ((continuous
                    && self
                        .segment_low
                        .is_some_and(|low| level.saturating_sub(low) >= 3))
                    || (self.cycles.back().is_some_and(|cycle| cycle.current)
                        && self
                            .cycle_low
                            .is_some_and(|low| level.saturating_sub(low) >= 3)));
            if increase {
                self.close_cycle();
                self.pending_charge = Some(CycleEvidence::InferredCharge);
            }
            if !self.cycles.back().is_some_and(|cycle| cycle.current) {
                self.start_cycle(&observation);
            }
            // Pauses exclude drain accounting, but cumulative rises still provide
            // evidence of an unobserved charge within the physical cycle.
            let cycle_low = self.cycle_low.unwrap_or(level);
            self.cycle_low = Some(cycle_low.min(level));
            if continuous && !increase {
                accepted = true;
                let previous = previous.expect("accepted interval has previous");
                let seconds = delta.expect("accepted positive duration") as u64;
                let recent_seconds = reading
                    .timestamp
                    .saturating_sub(previous.reading.timestamp.max(self.recent.since))
                    .max(0) as u64;
                self.recent.total_seconds =
                    self.recent.total_seconds.saturating_add(recent_seconds);
                if let Some(rate) = observation
                    .polling_rate
                    .filter(|_| observation.session.is_some())
                {
                    let index = crate::INSIGHT_RATES
                        .iter()
                        .position(|hz| *hz == rate.hz())
                        .unwrap();
                    self.recent.rates[index] =
                        self.recent.rates[index].saturating_add(recent_seconds);
                    if activity == Some(crate::PollingActivityKind::Boost)
                        && activity == self.previous_activity
                    {
                        self.recent.boost_seconds =
                            self.recent.boost_seconds.saturating_add(recent_seconds);
                    }
                }
                self.coverage.awake_seconds = self.coverage.awake_seconds.saturating_add(seconds);
                // Repeated small rebounds cannot manufacture additional drain.
                // A pause/rate change must not let a one-point rebound count
                // the same battery percentage twice within a physical cycle.
                let low = self
                    .segment_low
                    .unwrap_or(previous.reading.level.unwrap())
                    .min(cycle_low);
                let drop = u64::from(low.saturating_sub(level));
                self.segment_low = Some(low.min(level));
                let cycle = self.cycles.back_mut().expect("discharge cycle");
                cycle.awake_seconds = cycle.awake_seconds.saturating_add(seconds);
                cycle.consumed_percent = cycle.consumed_percent.saturating_add(drop);
                cycle.end_timestamp = reading.timestamp;
                cycle.end_percent = reading.level.unwrap();
                if let Some(polling_rate) = observation.polling_rate.filter(|_| {
                    observation.session.is_some()
                        && observation.polling_rate == previous.polling_rate
                }) {
                    let hz = polling_rate.hz();
                    if self.previous_counted_rate != Some(hz) {
                        self.pending_projection_seconds = 0;
                        self.projection_anchored = false;
                    }
                    self.pending_projection_seconds =
                        self.pending_projection_seconds.saturating_add(seconds);
                    let rate = self.rates.iter_mut().find(|rate| rate.hz == hz).unwrap();
                    rate.awake_seconds = rate.awake_seconds.saturating_add(seconds);
                    rate.consumed_percent = rate.consumed_percent.saturating_add(drop);
                    if drop > 0 {
                        if self.projection_anchored {
                            rate.projection_seconds = rate
                                .projection_seconds
                                .saturating_add(self.pending_projection_seconds);
                            rate.projection_consumed_percent =
                                rate.projection_consumed_percent.saturating_add(drop);
                            rate.projection_drop_count =
                                rate.projection_drop_count.saturating_add(1);
                        }
                        // The first drop only anchors a segment: its preceding
                        // plateau began at an unknown point in the percentage bin.
                        self.projection_anchored = true;
                        self.pending_projection_seconds = 0;
                    }
                    rate.sample_count = rate.sample_count.saturating_add(
                        if self.previous_counted_rate == Some(hz) {
                            1
                        } else {
                            2
                        },
                    );
                    rate.drop_count = rate.drop_count.saturating_add(u64::from(drop > 0));
                    self.previous_counted_rate = Some(hz);
                } else {
                    self.previous_counted_rate = None;
                }
            } else {
                self.segment_low = Some(level);
                self.previous_counted_rate = None;
                let cycle = self.cycles.back_mut().unwrap();
                cycle.end_timestamp = reading.timestamp;
                cycle.end_percent = reading.level.unwrap();
            }
        } else {
            // Unknown charging, coarse data or a pause never bridges a drain interval.
            self.segment_low = None;
        }
        if !discharging(reading) {
            self.previous_counted_rate = None;
        }
        if !accepted || self.previous_counted_rate.is_none() {
            self.pending_projection_seconds = 0;
            self.projection_anchored = false;
        }
        if had_input && !accepted {
            self.coverage.excluded_interval_count =
                self.coverage.excluded_interval_count.saturating_add(1);
        }
        self.previous = Some(observation);
        self.contiguous = true;
        self.previous_activity = activity;
    }

    /// Suppress remaining-use estimates when the latest observation is stale or
    /// lies in the future relative to the query clock. Historical totals remain.
    pub fn finish_at(mut self, timestamp: i64) -> BatteryInsights {
        let fresh = self.previous.as_ref().is_some_and(|previous| {
            timestamp
                .checked_sub(previous.reading.timestamp)
                .is_some_and(|age| (0..=600).contains(&age))
        });
        if !fresh {
            self.previous_counted_rate = None;
        }
        self.finish()
    }

    pub fn finish(mut self) -> BatteryInsights {
        self.recent.current_level = self
            .previous
            .as_ref()
            .filter(|o| {
                discharging(&o.reading)
                    && self.previous_counted_rate.is_some()
                    && o.polling_rate.is_some()
            })
            .and_then(|o| o.reading.level);
        for rate in &mut self.rates {
            if rate.projection_seconds >= 1800 && rate.projection_consumed_percent >= 3 {
                rate.confidence = if rate.projection_seconds >= 7200
                    && rate.projection_consumed_percent >= 10
                    && rate.projection_drop_count >= 3
                {
                    InsightConfidence::Moderate
                } else {
                    InsightConfidence::Low
                };
                let hours =
                    rate.projection_seconds as f64 / 36.0 / rate.projection_consumed_percent as f64;
                rate.projected_full_charge_hours = Some(hours);
                if let Some(current) = self.previous.as_ref().filter(|current| {
                    discharging(&current.reading)
                        && self.previous_counted_rate == Some(rate.hz)
                        && current.session.is_some()
                        && current.polling_rate.is_some_and(|r| r.hz() == rate.hz)
                }) {
                    rate.remaining_hours =
                        Some(hours * f64::from(current.reading.level.unwrap()) / 100.0);
                }
            }
        }
        BatteryInsights {
            recent: self.recent,
            rates: self
                .rates
                .into_iter()
                .filter(|rate| rate.awake_seconds > 0)
                .collect(),
            cycles: self.cycles.into_iter().collect(),
            coverage: self.coverage,
        }
    }
}
