//! Recent awake-use least-squares estimator, isolated from wall-clock storage.
use crate::{Precision, Reading};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Discharge {
    pub usage: f64,
    /// Relative discharge coordinates. After an unobserved boundary the levels
    /// are rebased, preserving measured drops without counting boundary loss.
    pub samples: Vec<(f64, u8)>,
    pub seen: i64,
    #[serde(skip)]
    last: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Estimator {
    pub devices: BTreeMap<String, Discharge>,
}
impl Estimator {
    /// Reset only the awake-time baseline, preserving the learned discharge rate.
    pub fn pause(&mut self, key: &str) {
        if let Some(d) = self.devices.get_mut(key) {
            d.last = None;
        }
    }
    pub fn pause_all(&mut self) {
        for d in self.devices.values_mut() {
            d.last = None;
        }
    }
    /// The state owner detected a backward wall-clock change. Keep the learned
    /// monotonic-time fit, but allow fresh readings in the new clock epoch.
    pub fn clock_changed(&mut self, now: i64) {
        for discharge in self.devices.values_mut() {
            discharge.last = None;
            discharge.seen = discharge.seen.min(now.max(0));
        }
    }
    /// Recover valid entries independently; loading never counts time while closed.
    pub fn from_value(value: serde_json::Value, now: i64) -> Self {
        let mut estimator = Self::default();
        if let serde_json::Value::Object(mut root) = value
            && let Some(serde_json::Value::Object(devices)) = root.remove("devices")
        {
            for (key, value) in devices {
                if let Ok(d) = serde_json::from_value::<Discharge>(value) {
                    estimator.devices.insert(key, d);
                }
            }
        }
        estimator.validate(now);
        estimator
    }
    pub fn validate(&mut self, now: i64) {
        self.devices.retain(|_, d| {
            d.last = None;
            if !d.usage.is_finite()
                || d.usage < 0.0
                || d.seen < 0
                || d.seen > now.saturating_add(300)
                || (d.seen > 0 && now.saturating_sub(d.seen) >= 60 * 86400)
            {
                return false;
            }
            if d.samples.iter().any(|(time, level)| {
                !time.is_finite() || *time < 0.0 || *time > d.usage || *level > 100
            }) || d
                .samples
                .windows(2)
                .any(|pair| pair[0].0 >= pair[1].0 || pair[0].1 <= pair[1].1)
            {
                return false;
            }
            if d.samples.len() > 400 {
                d.samples.drain(..d.samples.len() - 400);
            }
            true
        });
    }
    pub fn record(&mut self, r: &Reading, monotonic: f64) {
        let Some(level) = r.level.filter(|l| *l <= 100) else {
            self.pause(&r.key);
            return;
        };
        if r.precision != Precision::Exact
            || !monotonic.is_finite()
            || monotonic < 0.0
            || r.timestamp < 0
        {
            self.pause(&r.key);
            return;
        }
        // Existing devices are the steady-state path. Borrow the key instead
        // of allocating a replacement identity on every battery observation.
        if let Some(d) = self.devices.get_mut(&r.key) {
            Self::record_sample(d, r, monotonic, level);
        } else {
            Self::record_sample(
                self.devices.entry(r.key.clone()).or_default(),
                r,
                monotonic,
                level,
            );
        }
    }
    fn record_sample(d: &mut Discharge, r: &Reading, monotonic: f64, level: u8) {
        // Cached and inferred charging are not proof of a new charge cycle.
        // They must not clear a valid fit or advance its fresh-reading clock.
        if !r.online() || r.charging_inferred {
            d.last = None;
            return;
        }
        if r.timestamp < d.seen {
            d.last = None;
            return;
        }
        d.seen = r.timestamp;
        if r.charging == Some(true) {
            *d = Discharge {
                seen: r.timestamp,
                ..Default::default()
            };
            return;
        }
        if r.charging != Some(false) {
            d.last = None;
            return;
        }
        let elapsed = d.last.map(|last| monotonic - last);
        let continuous = elapsed.is_some_and(|seconds| seconds > 0.0 && seconds <= 600.0);
        d.last = Some(monotonic);
        let restarted = d
            .samples
            .last()
            .is_some_and(|(_, previous)| level.saturating_sub(*previous) >= 3);
        if restarted {
            d.usage = 0.0;
            d.samples.clear();
        }
        if continuous && !restarted {
            d.usage += elapsed.unwrap();
        } else if let Some((_, previous)) = d.samples.last() {
            // A sleeping/disconnected/restarted device can lose charge while we
            // cannot observe its awake time. Shift the fit's level origin instead
            // of manufacturing a drop at zero (or capped) elapsed time.
            let loss = previous.saturating_sub(level);
            for (_, sample_level) in &mut d.samples {
                *sample_level -= loss;
            }
        }
        if d.samples
            .last()
            .is_none_or(|(_, previous)| level < *previous)
        {
            d.samples.push((d.usage, level));
            if d.samples.len() > 400 {
                d.samples.remove(0);
            }
        }
    }
    pub fn seconds_left(&self, key: &str, level: Option<u8>) -> Option<f64> {
        let level = level.filter(|l| *l <= 100)?;
        let d = self.devices.get(key)?;
        let &(last_time, last_level) = d.samples.last()?;
        // Older usage conditions must not dominate today's drain. Select a
        // recent ten-point window, widening it until it spans thirty minutes.
        let start = d
            .samples
            .iter()
            .rposition(|(time, sample_level)| {
                sample_level.saturating_sub(last_level) >= 10 && d.usage - time >= 1800.0
            })
            .unwrap_or(0);
        let recent = &d.samples[start..];
        let &(first_time, first_level) = recent.first()?;
        let endpoint = (d.usage > last_time).then_some((d.usage, last_level));
        let count = recent.len() + usize::from(endpoint.is_some());
        if recent.len() < 4
            || endpoint.map_or(last_time, |sample| sample.0) - first_time < 1800.0
            || first_level.saturating_sub(last_level) < 3
        {
            return None;
        }
        // Keep the synthetic stalled-level endpoint without copying the history.
        let samples = recent.iter().copied().chain(endpoint);
        let n = count as f64;
        let mt = samples.clone().map(|s| s.0).sum::<f64>() / n;
        let ml = samples.clone().map(|s| f64::from(s.1)).sum::<f64>() / n;
        let numerator = samples
            .clone()
            .map(|s| (s.0 - mt) * (f64::from(s.1) - ml))
            .sum::<f64>();
        let denominator = samples.map(|s| (s.0 - mt).powi(2)).sum::<f64>();
        if denominator <= 0.0 {
            return None;
        }
        let rate = -numerator / denominator;
        let endpoint_rate = f64::from(first_level - last_level) / (d.usage - first_time);
        // A highly inconsistent curve does not support a useful extrapolation.
        (rate.is_finite()
            && rate > 0.0
            && rate >= endpoint_rate * 0.5
            && rate <= endpoint_rate * 2.0)
            .then(|| f64::from(level) / rate)
            .filter(|s| s.is_finite())
    }
}

pub fn format_left(seconds: f64) -> String {
    let hours = seconds / 3600.0;
    if hours < 1.0 {
        "less than 1 h of use left".into()
    } else if hours < 48.0 {
        format!("about {:.0} h of use left", hours.round())
    } else {
        format!("about {:.0} days of use left", (hours / 24.0).round())
    }
}
