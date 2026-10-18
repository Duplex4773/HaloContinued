//! Event-fed hardware reductions. No timer, input tracking, or periodic rate reads.
use hb_core::settings::LowBatteryMode;
use hb_core::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub const REQUEST_BIT: u64 = 1 << 60;
pub const BACKGROUND: u64 = u64::MAX;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    New,
    Ready(u32),
    Active { before: u32, applied: u32 },
    Done,
}
struct Entry {
    target: ControlTarget,
    choices: LowBatteryMode,
    low: bool,
    rate: Setting,
    sleep: Setting,
    rate_stamp: Option<ConnectionStamp>,
    sleep_stamp: Option<ConnectionStamp>,
}
impl Entry {
    fn needs_action(&self) -> bool {
        [self.rate, self.sleep].into_iter().any(|s| match s {
            Setting::New | Setting::Ready(_) => true,
            Setting::Active { .. } => !self.low,
            Setting::Done => false,
        })
    }
    fn usable(&self, r: &Reading, now: i64) -> bool {
        self.target.device.matches_reading(r)
            && r.online()
            && (!self.low || (r.level.is_some() && r.charging == Some(false)))
            && r.timestamp <= now
            && now.saturating_sub(r.timestamp) <= 120
    }
}
#[derive(Clone)]
pub enum Action {
    Polling(ControlRequest),
    Sleep(SleepRequest),
}
impl Action {
    pub fn id(&self) -> u64 {
        match self {
            Self::Polling(r) => r.request,
            Self::Sleep(r) => r.request,
        }
    }
    fn key(&self) -> &str {
        match self {
            Self::Polling(r) => &r.target.device.key,
            Self::Sleep(r) => &r.target.device.key,
        }
    }
    fn target(&self) -> &ControlTarget {
        match self {
            Self::Polling(r) => &r.target,
            Self::Sleep(r) => &r.target,
        }
    }
}
struct Pending {
    action: Action,
    old: Setting,
    restoring: bool,
    cancel: Arc<AtomicBool>,
    settings_cancelled: bool,
}
#[derive(Default)]
pub struct BatterySaver {
    entries: BTreeMap<String, Entry>,
    pending: Option<Pending>,
    sequence: u64,
}
impl BatterySaver {
    pub fn settings_changed(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.settings_cancelled = true;
            pending.cancel.store(true, Ordering::Release);
        }
    }
    pub fn wanted(&self, settings: &Settings) -> bool {
        !self.entries.is_empty()
            || settings
                .devices
                .values()
                .any(|p| p.low_battery_mode.is_some())
    }
    /// Cheap preflight: settled low-battery episodes need no inventory clones or Shell calls.
    #[cfg(test)]
    pub fn needs_action(&self) -> bool {
        self.pending.is_none() && self.entries.values().any(Entry::needs_action)
    }
    /// Check borrowed provider slices before resolving the inventory or asking
    /// Windows about gaming. Sleeping, stale and boost-owned units need no work.
    pub fn ready<'a>(
        &self,
        boost: &crate::boost::Boost,
        now: i64,
        readings: impl Fn(&str) -> &'a [Reading],
    ) -> bool {
        self.pending.is_none()
            && self.entries.iter().any(|(key, e)| {
                e.needs_action()
                    && !boost.owns(key)
                    && readings(&e.target.device.source)
                        .iter()
                        .any(|r| e.usable(r, now))
            })
    }
    pub fn cancel(&self, id: u64) -> Option<Arc<AtomicBool>> {
        self.pending
            .as_ref()
            .filter(|p| p.action.id() == id)
            .map(|p| p.cancel.clone())
    }
    /// Manual changes take ownership of the affected setting for this low-battery episode.
    pub fn manual(&mut self, key: &str, sleep: bool) {
        if let Some(e) = self.entries.get_mut(key) {
            if sleep {
                e.sleep = Setting::Done;
            } else {
                e.rate = Setting::Done;
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.action.key() == key && matches!(p.action, Action::Sleep(_)) == sleep)
        {
            self.pending
                .take()
                .unwrap()
                .cancel
                .store(true, Ordering::Release);
        }
    }
    pub fn pause(&mut self) {
        if let Some(p) = self.pending.take() {
            p.cancel.store(true, Ordering::Release);
        }
        // Retain episode latches: neither retry uncertain writes nor adopt a stale baseline.
        for e in self.entries.values_mut() {
            e.rate = Setting::Done;
            e.sleep = Setting::Done;
        }
    }
    pub fn observe_provider(&mut self, provider: &str, readings: &[Reading]) {
        for entry in self.entries.values_mut() {
            if entry.target.device.source == provider
                && !readings.iter().any(|r| {
                    entry.target.device.matches_reading(r) && r.connection != Connection::Stale
                })
            {
                entry.rate = Setting::Done;
                entry.sleep = Setting::Done;
                if let Some(p) = &self.pending
                    && p.action.key() == entry.target.device.key
                {
                    p.cancel.store(true, Ordering::Release);
                }
            }
        }
    }
    pub fn observe(
        &mut self,
        settings: &Settings,
        generation: u64,
        readings: &[Reading],
        now: i64,
    ) {
        if !settings.polling_controls {
            return;
        }
        self.observe_generation(generation);
        for r in readings {
            if !r.online()
                || r.kind != "mouse"
                || r.timestamp > now
                || now.saturating_sub(r.timestamp) > 120
            {
                continue;
            }
            let Some(level) = r.level else {
                continue;
            };
            let configured = settings
                .device(&r.key)
                .and_then(|p| p.low_battery_mode)
                .filter(LowBatteryMode::valid);
            if let Some(e) = self.entries.get_mut(&r.key) {
                if !e.target.device.matches_reading(r) {
                    e.rate = Setting::Done;
                    e.sleep = Setting::Done;
                }
                if configured != Some(e.choices)
                    || r.charging == Some(true)
                    || r.wired_mouse()
                    || level >= e.choices.threshold.saturating_add(5)
                {
                    e.low = false;
                }
            } else if let Some(choices) = configured
                && level <= choices.threshold
                && r.charging == Some(false)
                && !r.wired_mouse()
                && self.entries.len() < 512
            {
                self.entries.insert(
                    r.key.clone(),
                    Entry {
                        target: ControlTarget {
                            device: ConfigurationDevice::from_reading(r),
                            generation,
                            connection_stamp: None,
                        },
                        choices,
                        low: true,
                        rate: Setting::New,
                        sleep: Setting::New,
                        rate_stamp: None,
                        sleep_stamp: None,
                    },
                );
            }
        }
        self.entries
            .retain(|_, e| e.low || e.rate != Setting::Done || e.sleep != Setting::Done);
    }
    /// Cheap epoch rebinding must run even while a worker is in flight, before
    /// its cancellation outcome is processed. No inventory or native queries.
    pub fn observe_generation(&mut self, generation: u64) {
        for entry in self.entries.values_mut() {
            if entry.target.generation != generation {
                if entry.rate_stamp.is_none() {
                    entry.rate = Setting::Done;
                }
                if entry.sleep_stamp.is_none() {
                    entry.sleep = Setting::Done;
                }
                entry.target.generation = generation;
                if let Some(p) = &self.pending
                    && p.action.key() == entry.target.device.key
                {
                    p.cancel.store(true, Ordering::Release);
                }
            }
        }
    }
    pub fn next(
        &mut self,
        settings: &Settings,
        generation: u64,
        readings: &[Reading],
        now: i64,
        boost: &crate::boost::Boost,
    ) -> Option<Action> {
        if self.pending.is_some() || !settings.polling_controls {
            return None;
        }
        self.observe(settings, generation, readings, now);
        let mut action = None;
        for (key, e) in &mut self.entries {
            if boost.owns(key) || !readings.iter().any(|r| e.usable(r, now)) {
                continue;
            }
            if e.target.generation != generation {
                continue;
            }
            for sleep in [false, true] {
                let state = if sleep { &mut e.sleep } else { &mut e.rate };
                if matches!(
                    (*state, e.low),
                    (Setting::Done, _) | (Setting::Active { .. }, true)
                ) {
                    continue;
                }
                let desired = if sleep {
                    e.choices
                        .sleep_seconds
                        .filter(|s| {
                            hb_providers::sleep_controls::model(&e.target.device)
                                .is_some_and(|m| m.readable() && m.supports(*s))
                        })
                        .map(u32::from)
                } else {
                    e.choices
                        .polling_rate
                        .filter(|_| {
                            hb_providers::controls::polling_menu_candidate(&e.target.device)
                        })
                        .map(PollingRate::hz)
                };
                let Some(desired) = desired else {
                    *state = Setting::Done;
                    continue;
                };
                let (value, expected, restoring) = match (*state, e.low) {
                    (Setting::New, true) => (None, None, false),
                    (Setting::Ready(before), true) => (Some(desired), Some(before), false),
                    (Setting::Active { before, applied }, false) => {
                        (Some(before), Some(applied), true)
                    }
                    (Setting::New | Setting::Ready(_), false) => {
                        *state = Setting::Done;
                        continue;
                    }
                    _ => continue,
                };
                self.sequence = self.sequence.wrapping_add(1);
                let id = (1 << 63) | REQUEST_BIT | self.sequence;
                let request = if sleep {
                    Action::Sleep(SleepRequest {
                        request: id,
                        target: ControlTarget {
                            connection_stamp: e.sleep_stamp,
                            ..e.target.clone()
                        },
                        seconds: value.map(|v| v as u16),
                        expected: expected.map(|v| v as u16),
                    })
                } else {
                    Action::Polling(ControlRequest {
                        request: id,
                        target: ControlTarget {
                            connection_stamp: e.rate_stamp,
                            ..e.target.clone()
                        },
                        action: match (value, expected) {
                            (Some(v), Some(x)) => ControlAction::ApplyIf {
                                rate: PollingRate::try_from(v).unwrap(),
                                expected: PollingRate::try_from(x).unwrap(),
                            },
                            _ => ControlAction::Read,
                        },
                    })
                };
                self.pending = Some(Pending {
                    action: request.clone(),
                    old: *state,
                    restoring,
                    cancel: Arc::new(AtomicBool::new(false)),
                    settings_cancelled: false,
                });
                action = Some(request);
                break;
            }
            if action.is_some() {
                break;
            }
        }
        self.entries
            .retain(|_, e| e.low || e.rate != Setting::Done || e.sleep != Setting::Done);
        action
    }
    fn finish(
        &mut self,
        id: u64,
        value: Option<u32>,
        failed: bool,
        may_have_changed: bool,
        connection_stamp: Option<ConnectionStamp>,
    ) -> bool {
        if !self.pending.as_ref().is_some_and(|p| p.action.id() == id) {
            return false;
        }
        let p = self.pending.take().unwrap();
        let Some(e) = self.entries.get_mut(p.action.key()) else {
            return false;
        };
        let sleep = matches!(p.action, Action::Sleep(_));
        if (p.action.target().generation != e.target.generation || p.settings_cancelled)
            && p.action.target().connection_stamp.is_some()
            && !may_have_changed
        {
            // Refresh/settings before SET cannot have changed the owned setting.
            // Keep the original; the next request still checks its connection.
            return true;
        }
        let desired = if sleep {
            e.choices.sleep_seconds.map(u32::from)
        } else {
            e.choices.polling_rate.map(PollingRate::hz)
        };
        let failed = failed
            || (p.action.target().connection_stamp.is_some()
                && connection_stamp != p.action.target().connection_stamp)
            || (sleep
                && value.is_some_and(|v| {
                    !hb_providers::sleep_controls::model(&e.target.device)
                        .is_some_and(|m| m.supports(v as u16))
                }));
        let state = if sleep { &mut e.sleep } else { &mut e.rate };
        *state = if failed || p.cancel.load(Ordering::Acquire) || p.restoring {
            Setting::Done
        } else {
            match (p.old, value, desired) {
                // Zero sleep means never: any finite timer is shorter.
                (Setting::New, Some(before), Some(want))
                    if before > want || (sleep && before == 0) =>
                {
                    Setting::Ready(before)
                }
                (Setting::Ready(before), Some(applied), Some(want)) if applied == want => {
                    Setting::Active { before, applied }
                }
                _ => Setting::Done,
            }
        };
        let stamp = if sleep {
            &mut e.sleep_stamp
        } else {
            &mut e.rate_stamp
        };
        *stamp = if *state == Setting::Done {
            None
        } else {
            connection_stamp
        };
        true
    }
    pub fn polling_complete(&mut self, o: &ControlOutcome) -> bool {
        self.finish(
            o.request,
            o.observation
                .as_ref()
                .and_then(|v| v.rate)
                .map(PollingRate::hz),
            o.failure.is_some()
                || self.entries.get(&o.key).is_some_and(|e| {
                    e.choices.polling_rate.is_some_and(|wanted| {
                        o.observation
                            .as_ref()
                            .is_some_and(|observed| !observed.supported.contains(&wanted))
                    })
                }),
            o.may_have_changed,
            o.observation
                .as_ref()
                .and_then(|o| o.target.connection_stamp),
        )
    }
    pub fn sleep_complete(&mut self, o: &SleepOutcome) -> bool {
        self.finish(
            o.request,
            o.seconds.map(u32::from),
            o.failure.is_some(),
            o.may_have_changed,
            o.connection_stamp,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn rate(hz: u32) -> PollingRate {
        PollingRate::try_from(hz).unwrap()
    }
    fn fixture() -> (BatterySaver, Settings, Vec<Reading>, crate::boost::Boost) {
        let mut r = Reading::new("razer:00bf:TEST", "Test mouse", "razer", 100);
        r.kind = "mouse".into();
        r.via = "usb".into();
        r.level = Some(20);
        r.charging = Some(false);
        let mut s = Settings {
            polling_controls: true,
            ..Default::default()
        };
        s.devices.entry(r.key.clone()).or_default().low_battery_mode = Some(LowBatteryMode {
            threshold: 20,
            polling_rate: Some(rate(125)),
            sleep_seconds: Some(60),
        });
        (
            BatterySaver::default(),
            s,
            vec![r],
            crate::boost::Boost::new(Instant::now()),
        )
    }
    fn complete(b: &mut BatterySaver, a: &Action, value: u32) {
        match a {
            Action::Polling(r) => b.polling_complete(&ControlOutcome {
                request: r.request,
                key: r.target.device.key.clone(),
                previous: None,
                may_have_changed: r.action.rate().is_some(),
                failure: None,
                observation: Some(PollingObservation {
                    target: ControlTarget {
                        connection_stamp: ConnectionStamp::new(1),
                        ..r.target.clone()
                    },
                    supported: vec![rate(125), rate(1000), rate(2000)],
                    rate: Some(rate(value)),
                    timestamp: 100,
                    evidence: "test".into(),
                }),
            }),
            Action::Sleep(r) => b.sleep_complete(&SleepOutcome {
                request: r.request,
                key: r.target.device.key.clone(),
                generation: 1,
                connection_stamp: ConnectionStamp::new(1),
                seconds: Some(value as u16),
                may_have_changed: r.seconds.is_some(),
                failure: None,
            }),
        };
    }
    fn activate(b: &mut BatterySaver, s: &Settings, r: &[Reading], boost: &crate::boost::Boost) {
        for value in [1000, 125, 300, 60] {
            let a = b.next(s, 1, r, 100, boost).unwrap();
            complete(b, &a, value);
        }
        assert!(b.next(s, 1, r, 100, boost).is_none());
    }
    #[test]
    fn borrowed_preflight_defers_unusable_readings_without_resolving_inventory() {
        let (mut b, s, mut r, mut boost) = fixture();
        b.observe(&s, 1, &r, 100);
        assert!(b.ready(&boost, 100, |_| &r));
        for condition in 0..7 {
            let mut unavailable = r.clone();
            match condition {
                0 => unavailable[0].connection = Connection::Sleeping,
                1 => unavailable[0].connection = Connection::Stale,
                2 => unavailable[0].charging = None,
                3 => unavailable[0].timestamp = 101,
                4 => unavailable[0].timestamp = -21,
                5 => unavailable[0].serial = Some("replacement".into()),
                _ => unavailable[0].level = None,
            }
            assert!(!b.ready(&boost, 100, |_| &unavailable));
        }
        assert!(!b.ready(&boost, 100, |_| &[]));
        let now = std::time::Instant::now();
        let mut s = s;
        s.devices.get_mut(&r[0].key).unwrap().fullscreen_boost_rate = Some(rate(2000));
        boost.sample(now, Some(true));
        boost.sample(now + std::time::Duration::from_secs(10), Some(true));
        let boost_request = boost.next(&s, 1, &r).unwrap();
        assert!(!b.ready(&boost, 100, |_| &r));
        boost.manual(&r[0].key);
        assert!(!b.ready(&boost, 100, |_| &r));
        assert!(!boost.complete(&ControlOutcome::failed(&boost_request, "cancelled")));
        assert!(b.ready(&boost, 100, |_| &r));
        let action = b.next(&s, 1, &r, 100, &boost).unwrap();
        assert!(!b.ready(&boost, 100, |_| panic!(
            "pending work must not query inventory"
        )));
        complete(&mut b, &action, 1000);
        r[0].charging = None;
        assert!(!b.ready(&boost, 100, |_| &r));
    }
    #[test]
    fn direct_usb_never_starts_reductions_even_when_full_charge_is_not_charging() {
        for charging in [Some(false), Some(true), None] {
            let (mut b, mut s, mut readings, boost) = fixture();
            readings[0].serial = Some("TEST".into());
            s.link_device(&ConfigurationDevice::from_reading(&readings[0]));
            readings[0].key = "razer:00be:TEST".into();
            readings[0].charging = charging;
            assert!(
                s.device(&readings[0].key)
                    .unwrap()
                    .low_battery_mode
                    .is_some()
            );
            assert!(b.next(&s, 1, &readings, 100, &boost).is_none());
            assert!(!b.needs_action());
            readings[0].key = "razer:00bf:TEST".into();
            readings[0].charging = Some(false);
            assert!(matches!(
                b.next(&s, 2, &readings, 100, &boost),
                Some(Action::Polling(_))
            ));
        }
    }
    #[test]
    fn idle_preflight_skips_steady_inventory_and_rearms_only_on_new_evidence() {
        let (mut b, s, mut r, boost) = fixture();
        r[0].level = Some(80);
        b.observe(&s, 1, &r, 100);
        assert!(!b.needs_action());
        r[0].level = Some(20);
        b.observe(&s, 1, &r, 100);
        assert!(b.needs_action());
        activate(&mut b, &s, &r, &boost);
        for _ in 0..1000 {
            assert!(!b.needs_action());
        }
        r[0].charging = Some(true);
        b.observe(&s, 1, &r, 100);
        assert!(b.needs_action());
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        complete(&mut b, &a, 1000);
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        complete(&mut b, &a, 300);
        b.observe(&s, 1, &r, 100);
        assert!(!b.needs_action());
        assert!(b.entries.is_empty());
    }
    #[test]
    fn unadvertised_rate_stops_at_read_without_an_automatic_set() {
        let (mut b, mut s, r, boost) = fixture();
        s.devices
            .get_mut(&r[0].key)
            .unwrap()
            .low_battery_mode
            .as_mut()
            .unwrap()
            .sleep_seconds = None;
        let Action::Polling(request) = b.next(&s, 1, &r, 100, &boost).unwrap() else {
            panic!("polling read expected")
        };
        b.polling_complete(&ControlOutcome {
            request: request.request,
            key: request.target.device.key.clone(),
            previous: None,
            may_have_changed: false,
            failure: None,
            observation: Some(PollingObservation {
                target: request.target,
                supported: vec![rate(1000)],
                rate: Some(rate(1000)),
                timestamp: 100,
                evidence: "test".into(),
            }),
        });
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        assert!(!b.needs_action());
    }
    #[test]
    fn sleeping_reconnect_releases_pending_work_without_hardware_queries() {
        let (mut b, s, mut r, _) = fixture();
        b.observe(&s, 1, &r, 100);
        assert!(b.needs_action());
        r[0].connection = Connection::Sleeping;
        b.observe(&s, 2, &r, 100);
        assert!(!b.needs_action());
    }
    #[test]
    fn manual_before_first_automatic_action_wins_and_unknown_charge_defers_pending_apply() {
        let (mut b, mut s, mut r, boost) = fixture();
        s.devices
            .get_mut(&r[0].key)
            .unwrap()
            .low_battery_mode
            .as_mut()
            .unwrap()
            .sleep_seconds = None;
        b.observe(&s, 1, &r, 100);
        b.manual(&r[0].key, false);
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        let mut b = BatterySaver::default();
        let read = b.next(&s, 1, &r, 100, &boost).unwrap();
        complete(&mut b, &read, 1000);
        r[0].charging = None;
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        r[0].charging = Some(false);
        assert!(b.next(&s, 1, &r, 100, &boost).is_some());
    }
    #[test]
    fn reductions_are_once_per_episode_and_restore_conditionally_with_hysteresis() {
        let (mut b, s, mut r, boost) = fixture();
        activate(&mut b, &s, &r, &boost);
        r[0].level = Some(24);
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        r[0].level = Some(25);
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        assert!(
            matches!(&a, Action::Polling(q) if q.action == ControlAction::ApplyIf {rate:rate(1000),expected:rate(125)})
        );
        complete(&mut b, &a, 1000);
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        assert!(matches!(&a, Action::Sleep(q) if q.seconds==Some(300) && q.expected==Some(60)));
        complete(&mut b, &a, 300);
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        assert!(b.entries.is_empty());
    }
    #[test]
    fn charging_and_opt_out_restore_but_permission_revocation_sends_nothing() {
        for condition in 0..3 {
            let (mut b, mut s, mut r, boost) = fixture();
            activate(&mut b, &s, &r, &boost);
            match condition {
                0 => r[0].charging = Some(true),
                1 => s.devices.get_mut(&r[0].key).unwrap().low_battery_mode = None,
                _ => s.polling_controls = false,
            }
            assert_eq!(b.next(&s, 1, &r, 100, &boost).is_some(), condition != 2);
        }
    }
    #[test]
    fn sleeping_unknown_and_stale_batteries_never_start_changes() {
        for condition in 0..5 {
            let (mut b, s, mut r, boost) = fixture();
            match condition {
                0 => r[0].connection = Connection::Sleeping,
                1 => r[0].level = None,
                2 => r[0].charging = None,
                3 => r[0].timestamp = -1000,
                _ => r[0].timestamp = 200,
            }
            assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        }
    }
    #[test]
    fn manual_change_and_uncertain_failure_are_not_retried() {
        for manual in [false, true] {
            let (mut b, mut s, r, boost) = fixture();
            s.devices
                .get_mut(&r[0].key)
                .unwrap()
                .low_battery_mode
                .as_mut()
                .unwrap()
                .sleep_seconds = None;
            let read = b.next(&s, 1, &r, 100, &boost).unwrap();
            complete(&mut b, &read, 1000);
            let apply = b.next(&s, 1, &r, 100, &boost).unwrap();
            let cancel = b.cancel(apply.id()).unwrap();
            if manual {
                b.manual(&r[0].key, false);
                assert!(cancel.load(Ordering::Acquire));
                complete(&mut b, &apply, 125);
            } else if let Action::Polling(q) = apply {
                let mut o = ControlOutcome::failed(&q, "uncertain");
                o.may_have_changed = true;
                b.polling_complete(&o);
            }
            for _ in 0..20 {
                assert!(b.next(&s, 1, &r, 100, &boost).is_none());
            }
        }
    }
    #[test]
    fn reconnect_and_identity_changes_never_restore_old_baselines() {
        for generation in [1, 2] {
            let (mut b, s, mut r, boost) = fixture();
            activate(&mut b, &s, &r, &boost);
            if generation == 1 {
                r[0].serial = Some("another mouse".into());
            } else {
                r[0].connection = Connection::Stale;
                b.observe_provider("razer", &r);
                r[0].connection = Connection::Online;
            }
            r[0].charging = Some(true);
            assert!(b.next(&s, generation, &r, 100, &boost).is_none());
        }
    }
    #[test]
    fn unrelated_refreshes_preserve_both_originals_and_connection_proofs() {
        let (mut b, s, mut r, boost) = fixture();
        activate(&mut b, &s, &r, &boost);
        let mut other = r[0].clone();
        other.key.push_str(":other-unit");
        b.observe_provider("razer", &[r[0].clone(), other]);
        b.observe_provider("razer", &r);
        for generation in 2..=20 {
            b.observe_provider("logitech", &[]);
            b.observe(&s, generation, &r, 100);
            assert!(!b.needs_action());
        }
        r[0].level = Some(25);
        let polling = b.next(&s, 20, &r, 100, &boost).unwrap();
        assert_eq!(polling.target().connection_stamp, ConnectionStamp::new(1));
        assert_eq!(polling.target().generation, 20);
        assert!(
            matches!(&polling, Action::Polling(q) if q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(125)})
        );
        complete(&mut b, &polling, 1000);
        // A second refresh between the independent restores must preserve sleep.
        let sleep = b.next(&s, 21, &r, 100, &boost).unwrap();
        assert_eq!(sleep.target().connection_stamp, ConnectionStamp::new(1));
        assert!(
            matches!(&sleep, Action::Sleep(q) if q.seconds == Some(300) && q.expected == Some(60))
        );
        complete(&mut b, &sleep, 300);
        assert!(b.next(&s, 21, &r, 100, &boost).is_none());
    }
    #[test]
    fn settings_cancellation_without_connection_proof_cannot_resume_sleep_restore() {
        let (mut saver, settings, mut readings, boost) = fixture();
        activate(&mut saver, &settings, &readings, &boost);
        readings[0].level = Some(25);
        let polling = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
        complete(&mut saver, &polling, 1000);
        saver.entries.get_mut(&readings[0].key).unwrap().sleep_stamp = None;
        let Action::Sleep(restore) = saver.next(&settings, 1, &readings, 100, &boost).unwrap()
        else {
            panic!("Expected independent sleep restoration");
        };
        let guard = saver.cancel(restore.request).unwrap();
        saver.settings_changed();
        assert!(guard.load(Ordering::Acquire));
        saver.sleep_complete(&SleepOutcome::failed(
            &restore,
            "Settings revoked permission",
        ));
        assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
    }
    #[test]
    fn unrelated_settings_do_not_lose_original_polling_or_sleep_restore() {
        for sleep in [false, true] {
            for may_have_changed in [false, true] {
                let (mut saver, mut settings, mut readings, boost) = fixture();
                activate(&mut saver, &settings, &readings, &boost);
                readings[0].level = Some(25);
                if sleep {
                    let polling = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                    complete(&mut saver, &polling, 1000);
                }
                let restore = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                settings.animation = !settings.animation;
                saver.settings_changed();
                match &restore {
                    Action::Polling(request) => {
                        let mut failed =
                            ControlOutcome::failed(request, "Settings revoked permission");
                        failed.may_have_changed = may_have_changed;
                        saver.polling_complete(&failed);
                    }
                    Action::Sleep(request) => {
                        let mut failed =
                            SleepOutcome::failed(request, "Settings revoked permission");
                        failed.may_have_changed = may_have_changed;
                        saver.sleep_complete(&failed);
                    }
                }
                let next = saver.next(&settings, 1, &readings, 100, &boost);
                if !may_have_changed {
                    let next = next.unwrap();
                    assert_eq!(matches!(next, Action::Sleep(_)), sleep);
                    assert_eq!(
                        next.target().connection_stamp,
                        restore.target().connection_stamp
                    );
                } else {
                    assert_eq!(next.is_some(), !sleep);
                    if let Some(next) = next {
                        assert!(matches!(next, Action::Sleep(_)));
                    }
                }
            }
        }
    }
    #[test]
    fn safe_refresh_cancellation_preserves_restore_but_uncertain_set_is_not_retried() {
        for sleep in [false, true] {
            for changed in [false, true] {
                let (mut b, s, mut r, boost) = fixture();
                activate(&mut b, &s, &r, &boost);
                r[0].level = Some(25);
                if sleep {
                    let polling = b.next(&s, 1, &r, 100, &boost).unwrap();
                    complete(&mut b, &polling, 1000);
                }
                let restore = b.next(&s, 1, &r, 100, &boost).unwrap();
                let guard = b.cancel(restore.id()).unwrap();
                b.observe_generation(2);
                assert!(guard.load(Ordering::Acquire));
                match &restore {
                    Action::Polling(q) => {
                        let mut o = ControlOutcome::failed(q, "refresh before SET");
                        o.may_have_changed = changed;
                        b.polling_complete(&o);
                    }
                    Action::Sleep(q) => {
                        let mut o = SleepOutcome::failed(q, "refresh before SET");
                        o.may_have_changed = changed;
                        b.sleep_complete(&o);
                    }
                }
                let next = b.next(&s, 2, &r, 100, &boost);
                if !changed {
                    assert_eq!(next.as_ref().unwrap().target().generation, 2);
                    assert_eq!(matches!(next, Some(Action::Sleep(_))), sleep);
                } else {
                    // Polling uncertainty does not discard the independent sleep restore.
                    assert_eq!(next.is_some(), !sleep);
                    if let Some(next) = next {
                        assert!(matches!(next, Action::Sleep(_)));
                    }
                }
            }
        }
    }
    #[test]
    fn missing_connection_evidence_and_observed_disconnect_discard_originals() {
        for disconnected in [false, true] {
            let (mut b, s, mut r, boost) = fixture();
            activate(&mut b, &s, &r, &boost);
            if disconnected {
                b.observe_provider("razer", &[]);
            } else {
                let e = b.entries.get_mut(&r[0].key).unwrap();
                e.rate_stamp = None;
                e.sleep_stamp = None;
            }
            r[0].level = Some(25);
            assert!(b.next(&s, 2, &r, 100, &boost).is_none());
        }
    }
    #[test]
    fn lower_existing_values_and_nonrestorable_timers_are_not_overwritten() {
        let (mut b, s, r, boost) = fixture();
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        complete(&mut b, &a, 125);
        let a = b.next(&s, 1, &r, 100, &boost).unwrap();
        complete(&mut b, &a, 90); // Valid read, unsupported SET baseline.
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
    }
    #[test]
    fn boost_takes_priority_then_returns_to_saver_before_normal_restoration() {
        let (mut b, mut s, mut r, mut boost) = fixture();
        activate(&mut b, &s, &r, &boost);
        s.devices.get_mut(&r[0].key).unwrap().fullscreen_boost_rate = Some(rate(2000));
        let now = Instant::now();
        boost.sample(now, Some(true));
        boost.sample(now + Duration::from_secs(10), Some(true));
        let read = boost.next(&s, 1, &r).unwrap();
        let outcome = |q: &ControlRequest, hz| ControlOutcome {
            request: q.request,
            key: q.target.device.key.clone(),
            previous: Some(rate(125)),
            may_have_changed: true,
            failure: None,
            observation: Some(PollingObservation {
                target: ControlTarget {
                    connection_stamp: ConnectionStamp::new(1),
                    ..q.target.clone()
                },
                supported: vec![rate(125), rate(1000), rate(2000)],
                rate: Some(rate(hz)),
                timestamp: 100,
                evidence: "test".into(),
            }),
        };
        boost.complete(&outcome(&read, 125));
        let apply = boost.next(&s, 1, &r).unwrap();
        assert_eq!(apply.action.rate(), Some(rate(2000)));
        boost.complete(&outcome(&apply, 2000));
        r[0].charging = Some(true);
        assert!(b.next(&s, 1, &r, 100, &boost).is_none());
        boost.sample(now + Duration::from_secs(20), Some(false));
        boost.sample(now + Duration::from_secs(35), Some(false));
        let restore = boost.next(&s, 1, &r).unwrap();
        assert_eq!(restore.action.rate(), Some(rate(125)));
        boost.complete(&outcome(&restore, 125));
        let restore = b.next(&s, 1, &r, 100, &boost).unwrap();
        assert!(matches!(restore,Action::Polling(q) if q.action.rate()==Some(rate(1000))));
    }
}
