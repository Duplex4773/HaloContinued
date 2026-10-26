//! Predictions use recent confirmed-rate time and independently learned drain.
use crate::{BatteryInsights, PollingRate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollingActivityKind {
    Boost,
    Restore,
    Manual,
    Saver,
    SaverRestore,
}
impl PollingActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boost => "boost",
            Self::Restore => "restore",
            Self::Manual => "manual",
            Self::Saver => "saver",
            Self::SaverRestore => "saver_restore",
        }
    }
}
#[derive(Clone, Debug)]
pub struct PollingActivity {
    pub key: String,
    pub timestamp: i64,
    pub session: u64,
    pub kind: PollingActivityKind,
    pub rate: PollingRate,
    pub previous: Option<PollingRate>,
}
#[derive(Clone, Debug, Default)]
pub struct RecentUse {
    /// Last seven calendar days, clipped at the query boundary.
    pub since: i64,
    pub total_seconds: u64,
    pub rates: [u64; 7],
    pub boost_seconds: u64,
    pub boost_sessions: u64,
    pub restores: u64,
    pub manual_changes: u64,
    pub saver_changes: u64,
    pub saver_restores: u64,
    pub saver_seconds: u64,
    pub normal_rate: Option<PollingRate>,
    pub current_level: Option<u8>,
    pub last_confirmed_timestamp: Option<i64>,
}
pub const INSIGHT_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
#[derive(Clone, Debug, Default)]
pub struct AdaptivePrediction {
    pub full_hours: Option<f64>,
    pub remaining_hours: Option<f64>,
    pub normal_full_hours: Option<f64>,
    pub normal_rate: Option<PollingRate>,
    pub missing_rates: Vec<u32>,
    pub coverage_percent: f64,
}
impl BatteryInsights {
    pub fn adaptive_prediction(
        &self,
        configured_boost: Option<PollingRate>,
        normal: Option<PollingRate>,
    ) -> AdaptivePrediction {
        let recent = &self.recent;
        let confirmed: u64 = recent.rates.iter().sum();
        let mut result = AdaptivePrediction {
            coverage_percent: if recent.total_seconds > 0 {
                confirmed as f64 / recent.total_seconds as f64 * 100.0
            } else {
                0.0
            },
            normal_rate: recent.normal_rate.or(normal),
            ..Default::default()
        };
        let learned = |hz| {
            self.rates
                .iter()
                .find(|r| r.hz == hz)
                .filter(|r| r.projection_seconds >= 1800 && r.projection_consumed_percent >= 3)
                .and_then(|r| r.projected_full_charge_hours)
                .filter(|h| h.is_finite() && *h > 0.0)
        };
        let mut drain = 0.0;
        for (hz, seconds) in INSIGHT_RATES.into_iter().zip(recent.rates) {
            if seconds == 0 {
                continue;
            }
            if let Some(hours) = learned(hz) {
                drain += seconds as f64 / hours;
            } else {
                result.missing_rates.push(hz);
            }
        }
        if let Some(boost) = configured_boost {
            let index = INSIGHT_RATES
                .iter()
                .position(|hz| *hz == boost.hz())
                .unwrap();
            if (recent.rates[index] == 0 || learned(boost.hz()).is_none())
                && !result.missing_rates.contains(&boost.hz())
            {
                result.missing_rates.push(boost.hz());
            }
        }
        // An unobserved majority cannot establish the user's usual rate mix.
        if confirmed >= 1800
            && result.coverage_percent >= 80.0
            && result.missing_rates.is_empty()
            && drain > 0.0
        {
            let hours = confirmed as f64 / drain;
            if hours.is_finite() && hours > 0.0 {
                result.full_hours = Some(hours);
                result.remaining_hours = recent
                    .current_level
                    .map(|level| hours * f64::from(level) / 100.0);
            }
        }
        result.normal_full_hours = result.normal_rate.and_then(|rate| learned(rate.hz()));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RateInsight;
    fn fixture() -> BatteryInsights {
        BatteryInsights {
            rates: vec![
                RateInsight {
                    hz: 1000,
                    projection_seconds: 7200,
                    projection_consumed_percent: 10,
                    projected_full_charge_hours: Some(100.0),
                    ..Default::default()
                },
                RateInsight {
                    hz: 4000,
                    projection_seconds: 7200,
                    projection_consumed_percent: 10,
                    projected_full_charge_hours: Some(50.0),
                    ..Default::default()
                },
            ],
            recent: RecentUse {
                total_seconds: 7200,
                rates: [0, 0, 0, 5400, 0, 1800, 0],
                current_level: Some(50),
                normal_rate: PollingRate::try_from(1000).ok(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    #[test]
    fn mix_weights_drain_not_battery_lifetimes() {
        let result = fixture().adaptive_prediction(PollingRate::try_from(4000).ok(), None);
        assert_eq!(result.full_hours, Some(80.0));
        assert_eq!(result.remaining_hours, Some(40.0));
        assert_eq!(result.normal_full_hours, Some(100.0));
    }
    #[test]
    fn unknown_use_and_unlearned_or_new_boost_prevent_overconfident_predictions() {
        let mut data = fixture();
        data.recent.total_seconds = 20000;
        assert!(data.adaptive_prediction(None, None).full_hours.is_none());
        data = fixture();
        data.rates[1].projection_consumed_percent = 2;
        assert_eq!(
            data.adaptive_prediction(None, None).missing_rates,
            vec![4000]
        );
        data = fixture();
        assert!(
            data.adaptive_prediction(PollingRate::try_from(8000).ok(), None)
                .full_hours
                .is_none()
        );
    }
    #[test]
    fn old_unused_rates_do_not_distort_recent_mix_and_stale_level_has_no_remaining() {
        let mut data = fixture();
        data.rates.push(RateInsight {
            hz: 8000,
            awake_seconds: 7200,
            ..Default::default()
        });
        data.recent.current_level = None;
        let result = data.adaptive_prediction(None, None);
        assert_eq!(result.full_hours, Some(80.0));
        assert_eq!(result.remaining_hours, None);
    }
}
