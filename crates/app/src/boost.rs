//! One verified boost and restoration per stable fullscreen session.
use hb_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const REQUEST_BIT: u64 = 1 << 61;
#[derive(Clone)]
pub struct Guard {
    pub fullscreen: bool,
    pub cancel: Arc<AtomicBool>,
}

impl Guard {
    pub fn active(&self) -> bool {
        !self.cancel.load(Ordering::Acquire)
            && hb_windows::system::fullscreen_polling_state() == Some(self.fullscreen)
    }
}
struct Entry {
    target: ControlTarget,
    boost: PollingRate,
    baseline: Option<PollingRate>,
    active: bool,
}
struct Pending {
    request: ControlRequest,
    guard: Guard,
}
pub struct Boost {
    entries: BTreeMap<String, Entry>,
    attempted: BTreeSet<String>,
    pending: Option<Pending>,
    candidate: Option<(bool, Instant)>,
    stable: Option<bool>,
    next_sample: Instant,
    sequence: u64,
}
impl Boost {
    pub fn new(now: Instant) -> Self {
        Self {
            entries: BTreeMap::new(),
            attempted: BTreeSet::new(),
            pending: None,
            candidate: None,
            stable: None,
            next_sample: now,
            sequence: (1 << 63) | REQUEST_BIT,
        }
    }
    pub fn wanted(&self, settings: &Settings) -> bool {
        settings.polling_controls
            && (self.entries.values().any(|entry| entry.active)
                || self.pending.is_some()
                || settings
                    .devices
                    .values()
                    .any(|d| d.fullscreen_boost_rate.is_some()))
    }
    pub fn due(&self, now: Instant) -> bool {
        now >= self.next_sample
    }
    pub fn observe_generation(&mut self, generation: u64) {
        self.entries.retain(|_, entry| {
            if entry.target.generation != generation {
                if entry.target.connection_stamp.is_none() {
                    return false;
                }
                // Only rebind the cache epoch. The controller must still match
                // the saved native connection stamp before opening/writing.
                entry.target.generation = generation;
            }
            true
        });
        if let Some(pending) = &self.pending
            && pending.request.target.generation != generation
        {
            pending.guard.cancel.store(true, Ordering::Release);
        }
    }
    pub fn observe_provider(&mut self, provider: &str, readings: &[Reading]) {
        self.entries.retain(|_, e| {
            e.target.device.source != provider
                || readings.iter().any(|r| {
                    e.target.device.matches_reading(r) && r.connection != Connection::Stale
                })
        });
        if let Some(p) = &self.pending
            && !self.entries.contains_key(&p.request.target.device.key)
        {
            p.guard.cancel.store(true, Ordering::Release);
        }
    }
    /// Avoid resolving and cloning the device inventory on idle wakes. Once
    /// every configured device has been attempted, a steady fullscreen session
    /// needs only the Shell sample, not another inventory pass.
    pub fn needs_inventory(&self, settings: &Settings) -> bool {
        if self.pending.is_some() || !settings.polling_controls {
            return false;
        }
        match self.stable {
            Some(true) => {
                self.entries
                    .values()
                    .any(|e| !e.active && e.baseline.is_some())
                    || settings.devices.iter().any(|(key, preferences)| {
                        preferences.fullscreen_boost_rate.is_some()
                            && !self.attempted.iter().any(|attempt| {
                                settings.preference_key(attempt) == settings.preference_key(key)
                            })
                    })
            }
            Some(false) => self.entries.values().any(|e| e.active),
            None => false,
        }
    }
    pub fn sample(&mut self, now: Instant, state: Option<bool>) {
        self.next_sample = now + Duration::from_secs(5);
        if let Some(pending) = &self.pending
            && state != Some(pending.guard.fullscreen)
        {
            pending.guard.cancel.store(true, Ordering::Release);
        }
        let Some(state) = state else {
            self.candidate = None;
            self.stable = None;
            return;
        };
        match self.candidate {
            Some((last, since)) if last == state => {
                if now.saturating_duration_since(since)
                    >= Duration::from_secs(if state { 10 } else { 15 })
                {
                    if !state && self.stable != Some(false) {
                        self.attempted.clear();
                        // A cancelled preflight owns no hardware state. Do not let it
                        // block battery saver or reuse its baseline in a later game.
                        self.entries.retain(|_, entry| entry.active);
                    }
                    self.stable = Some(state);
                }
            }
            _ => {
                self.candidate = Some((state, now));
                self.stable = None;
            }
        }
    }
    pub fn owns(&self, key: &str) -> bool {
        self.entries.contains_key(key)
            || self
                .pending
                .as_ref()
                .is_some_and(|p| p.request.target.device.key == key)
    }
    pub fn manual(&mut self, key: &str) {
        if let Some(pending) = &self.pending
            && pending.request.target.device.key == key
        {
            pending.guard.cancel.store(true, Ordering::Release);
        }
        self.entries.remove(key);
        // A manual choice overrides the current game, not the next one. The
        // desktop may already be stable, so waiting for another desktop
        // transition would suppress the whole following fullscreen session.
        if self.stable != Some(false) && self.attempted.len() < 512 {
            self.attempted.insert(key.into());
        }
    }
    pub fn clear(&mut self) {
        if let Some(p) = self.pending.take() {
            p.guard.cancel.store(true, Ordering::Release);
        }
        self.entries.clear();
        self.attempted.clear();
        self.candidate = None;
        self.stable = None;
    }
    pub fn guard(&self, request: u64) -> Option<Guard> {
        self.pending
            .as_ref()
            .filter(|p| p.request.request == request)
            .map(|p| p.guard.clone())
    }
    pub fn next(
        &mut self,
        settings: &Settings,
        generation: u64,
        readings: &[Reading],
    ) -> Option<ControlRequest> {
        self.observe_generation(generation);
        if !self.needs_inventory(settings) {
            return None;
        }
        self.entries.retain(|key, e| {
            e.target.generation == generation
                && readings
                    .iter()
                    .any(|r| &r.key == key && e.target.device.matches_reading(r))
        });
        let fullscreen = self.stable?;
        let existing = self
            .entries
            .iter()
            .find(|(_, e)| {
                let online = readings
                    .iter()
                    .any(|r| e.target.device.matches_reading(r) && r.online());
                online
                    && if fullscreen {
                        !e.active && e.baseline.is_some()
                    } else {
                        e.active
                    }
            })
            .map(|(key, _)| key.clone());
        let key = if let Some(key) = existing {
            key
        } else if fullscreen {
            if self.attempted.len() >= 512 {
                return None;
            }
            let r = readings.iter().find(|r| {
                r.online()
                    && r.kind == "mouse"
                    && !self.attempted.contains(&r.key)
                    && settings
                        .device(&r.key)
                        .and_then(|p| p.fullscreen_boost_rate)
                        .is_some()
                    && hb_providers::controls::polling_menu_candidate(
                        &ConfigurationDevice::from_reading(r),
                    )
            })?;
            let boost = settings.device(&r.key)?.fullscreen_boost_rate?;
            self.attempted.insert(r.key.clone());
            self.entries.insert(
                r.key.clone(),
                Entry {
                    target: ControlTarget {
                        device: ConfigurationDevice::from_reading(r),
                        generation,
                        connection_stamp: None,
                    },
                    boost,
                    baseline: None,
                    active: false,
                },
            );
            r.key.clone()
        } else {
            return None;
        };
        let entry = self.entries.get(&key)?;
        let action = match (fullscreen, entry.baseline) {
            (true, None) => ControlAction::Read,
            (true, Some(expected)) => {
                if settings.device(&key).and_then(|p| p.fullscreen_boost_rate) != Some(entry.boost)
                {
                    self.entries.remove(&key);
                    return None;
                }
                ControlAction::ApplyIf {
                    rate: entry.boost,
                    expected,
                }
            }
            (false, Some(rate)) => ControlAction::ApplyIf {
                rate,
                expected: entry.boost,
            },
            _ => return None,
        };
        self.sequence += 1;
        let request = ControlRequest {
            request: self.sequence,
            target: entry.target.clone(),
            action,
        };
        self.pending = Some(Pending {
            request: request.clone(),
            guard: Guard {
                fullscreen,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        Some(request)
    }
    pub fn complete(&mut self, outcome: &ControlOutcome) {
        let Some(p) = self.pending.take() else {
            return;
        };
        if p.request.request != outcome.request {
            self.pending = Some(p);
            return;
        }
        let key = &outcome.key;
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if entry.target.generation != p.request.target.generation
            && entry.target.connection_stamp.is_some()
            && !outcome.may_have_changed
        {
            // Refresh cancelled before any SET: retain the verified original,
            // then let the next transaction revalidate this connection.
            return;
        }
        let observation = outcome.observation.as_ref();
        if outcome.failure.is_some()
            || observation.is_none()
            || observation.is_some_and(|o| {
                o.target.generation != entry.target.generation
                    || o.target.device != entry.target.device
                    || (entry.target.connection_stamp.is_some()
                        && o.target.connection_stamp != entry.target.connection_stamp)
            })
        {
            // Never retry an uncertain automatic SET, including restore.
            self.entries.remove(key);
            return;
        }
        let o = observation.unwrap();
        match p.request.action {
            ControlAction::Read => {
                if let Some(rate) = o.rate
                    && rate.hz() < entry.boost.hz()
                    && o.supported.contains(&entry.boost)
                    && o.supported.contains(&rate)
                {
                    entry.baseline = Some(rate);
                    entry.target.connection_stamp = o.target.connection_stamp;
                } else {
                    self.entries.remove(key);
                }
            }
            _ if p.guard.fullscreen && o.rate == Some(entry.boost) => entry.active = true,
            _ => {
                self.entries.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rate(hz: u32) -> PollingRate {
        PollingRate::try_from(hz).unwrap()
    }
    fn fixture() -> (Settings, Vec<Reading>) {
        let mut r = Reading::new("simulated:mouse", "Test mouse", "simulation", 0);
        r.kind = "mouse".into();
        r.connection = Connection::Online;
        let mut s = Settings {
            polling_controls: true,
            ..Default::default()
        };
        s.devices
            .entry(r.key.clone())
            .or_default()
            .fullscreen_boost_rate = Some(rate(2000));
        (s, vec![r])
    }
    #[test]
    fn interrupted_preflight_releases_device_after_desktop_settles() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        assert!(boost.owns(&readings[0].key));
        settle(&mut boost, now + Duration::from_secs(30), false);
        assert!(!boost.owns(&readings[0].key));
        settle(&mut boost, now + Duration::from_secs(60), true);
        assert_eq!(
            boost.next(&settings, 1, &readings).unwrap().action,
            ControlAction::Read
        );
    }
    #[test]
    fn desktop_rate_change_does_not_suppress_the_next_game() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, false);
        boost.manual(&readings[0].key);
        // Starting a game immediately after Apply must still be eligible,
        // even without another desktop sample in between.
        settle(&mut boost, now + Duration::from_secs(30), true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(read.action, ControlAction::Read);
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(apply.action.rate(), Some(rate(2000)));
    }
    #[test]
    fn manual_override_in_game_expires_only_after_stable_desktop() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        boost.manual(&readings[0].key);
        assert!(boost.next(&settings, 1, &readings).is_none());
        boost.sample(now + Duration::from_secs(30), Some(false));
        settle(&mut boost, now + Duration::from_secs(35), true);
        assert!(boost.next(&settings, 1, &readings).is_none());
        settle(&mut boost, now + Duration::from_secs(60), false);
        settle(&mut boost, now + Duration::from_secs(90), true);
        assert_eq!(
            boost.next(&settings, 1, &readings).unwrap().action,
            ControlAction::Read
        );
    }
    #[test]
    fn desktop_and_settled_sessions_skip_inventory_but_new_opt_ins_are_discovered() {
        let now = Instant::now();
        let (mut settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, false);
        assert!(!boost.needs_inventory(&settings));
        settle(&mut boost, now + Duration::from_secs(30), true);
        assert!(boost.needs_inventory(&settings));
        let request = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &request, 4000); // Already faster: no write.
        assert!(!boost.needs_inventory(&settings));
        settings
            .devices
            .entry("another-mouse".into())
            .or_default()
            .fullscreen_boost_rate = Some(rate(2000));
        assert!(boost.needs_inventory(&settings));
    }
    #[test]
    fn opted_out_preflight_stops_shell_sampling_without_losing_active_restoration() {
        let now = Instant::now();
        let (mut settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let request = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &request, 1000);
        settings
            .devices
            .get_mut(&readings[0].key)
            .unwrap()
            .fullscreen_boost_rate = None;
        assert!(!boost.wanted(&settings));
        assert!(boost.next(&settings, 1, &readings).is_none());
        assert!(!boost.needs_inventory(&settings));
    }
    fn settle(b: &mut Boost, now: Instant, state: bool) {
        b.sample(now, Some(state));
        b.sample(now + Duration::from_secs(15), Some(state));
    }
    fn complete(b: &mut Boost, request: &ControlRequest, hz: u32) {
        b.complete(&ControlOutcome {
            request: request.request,
            key: request.target.device.key.clone(),
            observation: Some(PollingObservation {
                target: ControlTarget {
                    connection_stamp: ConnectionStamp::new(1),
                    ..request.target.clone()
                },
                supported: vec![rate(1000), rate(2000), rate(4000)],
                rate: Some(rate(hz)),
                timestamp: 0,
                evidence: "test".into(),
            }),
            previous: Some(rate(1000)),
            may_have_changed: request.action.rate().is_some(),
            failure: None,
        });
    }
    #[test]
    fn opt_in_debounce_verified_boost_and_conditional_restore() {
        let now = Instant::now();
        let mut b = Boost::new(now);
        let (mut s, r) = fixture();
        s.polling_controls = false;
        assert!(!b.wanted(&s));
        assert!(!b.needs_inventory(&s));
        s.polling_controls = true;
        b.sample(now, Some(true));
        assert!(!b.needs_inventory(&s));
        assert!(b.next(&s, 1, &r).is_none());
        b.sample(now + Duration::from_secs(9), Some(true));
        assert!(b.next(&s, 1, &r).is_none());
        b.sample(now + Duration::from_secs(10), Some(true));
        let read = b.next(&s, 1, &r).unwrap();
        assert!(!b.needs_inventory(&s));
        assert_eq!(read.action, ControlAction::Read);
        assert!(b.next(&s, 1, &r).is_none());
        complete(&mut b, &read, 1000);
        assert!(b.needs_inventory(&s));
        let apply = b.next(&s, 1, &r).unwrap();
        assert_eq!(
            apply.action,
            ControlAction::ApplyIf {
                rate: rate(2000),
                expected: rate(1000)
            }
        );
        complete(&mut b, &apply, 2000);
        assert!(!b.needs_inventory(&s));
        assert!(b.next(&s, 1, &r).is_none());
        b.sample(now + Duration::from_secs(20), Some(false));
        b.sample(now + Duration::from_secs(25), Some(true)); // brief Alt-Tab
        assert!(b.next(&s, 1, &r).is_none());
        settle(&mut b, now + Duration::from_secs(30), false);
        assert!(b.needs_inventory(&s));
        let restore = b.next(&s, 1, &r).unwrap();
        assert_eq!(
            restore.action,
            ControlAction::ApplyIf {
                rate: rate(1000),
                expected: rate(2000)
            }
        );
        complete(&mut b, &restore, 1000);
        assert!(!b.needs_inventory(&s));
        assert!(b.next(&s, 1, &r).is_none());
    }
    #[test]
    fn failure_manual_override_and_disconnect_do_not_retry_or_restore_stale_targets() {
        for scenario in 0..4 {
            let now = Instant::now();
            let mut b = Boost::new(now);
            let (s, mut r) = fixture();
            settle(&mut b, now, true);
            let read = b.next(&s, 1, &r).unwrap();
            complete(&mut b, &read, 1000);
            let apply = b.next(&s, 1, &r).unwrap();
            if scenario == 0 {
                let mut outcome = ControlOutcome::failed(&apply, "uncertain SET");
                outcome.may_have_changed = true;
                b.complete(&outcome);
            } else {
                complete(&mut b, &apply, 2000);
                if scenario == 1 {
                    b.manual(&r[0].key);
                }
                if scenario == 2 {
                    r[0].serial = Some("different-device".into());
                }
                if scenario == 3 {
                    r[0].connection = Connection::Stale;
                    b.observe_provider("simulation", &r);
                    r[0].connection = Connection::Online;
                    assert!(b.next(&s, 2, &r).is_none());
                }
            }
            assert!(b.next(&s, 1, &r).is_none());
            settle(&mut b, now + Duration::from_secs(30), false);
            assert!(b.next(&s, 1, &r).is_none());
        }
    }
    #[test]
    fn refresh_and_unrelated_changes_preserve_verified_restoration() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut b = Boost::new(now);
        settle(&mut b, now, true);
        let read = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &read, 1000);
        let apply = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &apply, 2000);
        let mut other = readings[0].clone();
        other.key.push_str(":other-unit");
        // Changes to another mouse from the same provider are unrelated too.
        b.observe_provider("simulation", &[readings[0].clone(), other]);
        b.observe_provider("simulation", &readings);
        for generation in 2..=20 {
            b.observe_provider("logitech", &[]);
            b.observe_generation(generation);
            assert!(b.owns(&readings[0].key));
            assert!(!b.needs_inventory(&settings));
        }
        settle(&mut b, now + Duration::from_secs(30), false);
        let restore = b.next(&settings, 20, &readings).unwrap();
        assert_eq!(restore.target.generation, 20);
        assert_eq!(restore.target.connection_stamp, ConnectionStamp::new(1));
        assert_eq!(
            restore.action,
            ControlAction::ApplyIf {
                rate: rate(1000),
                expected: rate(2000)
            }
        );
        complete(&mut b, &restore, 1000);
        assert!(!b.owns(&readings[0].key));
    }
    #[test]
    fn refresh_before_set_preserves_ownership_but_uncertain_write_does_not() {
        for changed in [false, true] {
            let now = Instant::now();
            let (settings, readings) = fixture();
            let mut b = Boost::new(now);
            settle(&mut b, now, true);
            let read = b.next(&settings, 1, &readings).unwrap();
            complete(&mut b, &read, 1000);
            let apply = b.next(&settings, 1, &readings).unwrap();
            complete(&mut b, &apply, 2000);
            settle(&mut b, now + Duration::from_secs(30), false);
            let restore = b.next(&settings, 1, &readings).unwrap();
            let guard = b.guard(restore.request).unwrap();
            b.observe_generation(2);
            assert!(guard.cancel.load(Ordering::Acquire));
            let mut failure =
                ControlOutcome::failed(&restore, "refresh invalidated queued transaction");
            failure.may_have_changed = changed;
            b.complete(&failure);
            let retry = b.next(&settings, 2, &readings);
            assert_eq!(retry.is_some(), !changed);
            if let Some(retry) = retry {
                // A real reconnect is detected by the controller's fresh stamp.
                b.complete(&ControlOutcome::failed(&retry, "connection stamp changed"));
                assert!(b.next(&settings, 2, &readings).is_none());
            }
        }
    }
    #[test]
    fn missing_proof_disconnect_and_changed_identity_revoke_recovery() {
        for case in 0..4 {
            let now = Instant::now();
            let (settings, mut readings) = fixture();
            let mut b = Boost::new(now);
            settle(&mut b, now, true);
            let read = b.next(&settings, 1, &readings).unwrap();
            complete(&mut b, &read, 1000);
            let apply = b.next(&settings, 1, &readings).unwrap();
            complete(&mut b, &apply, 2000);
            match case {
                0 => {
                    b.entries
                        .get_mut(&readings[0].key)
                        .unwrap()
                        .target
                        .connection_stamp = None;
                    b.observe_generation(2);
                }
                1 => b.observe_provider("simulation", &[]),
                2 => {
                    readings[0].connection = Connection::Stale;
                    b.observe_provider("simulation", &readings);
                    readings[0].connection = Connection::Online;
                }
                _ => {
                    readings[0].container = Some("new receiver".into());
                    b.observe_provider("simulation", &readings);
                }
            }
            settle(&mut b, now + Duration::from_secs(30), false);
            assert!(b.next(&settings, 2, &readings).is_none());
        }
    }
    #[test]
    fn communication_failure_pauses_restore_without_discarding_original() {
        let now = Instant::now();
        let (settings, mut readings) = fixture();
        let mut b = Boost::new(now);
        settle(&mut b, now, true);
        let read = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &read, 1000);
        let apply = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &apply, 2000);
        // Runtime does not report a failed provider poll as confirmed discovery.
        readings[0].connection = Connection::Stale;
        settle(&mut b, now + Duration::from_secs(30), false);
        assert!(b.next(&settings, 2, &readings).is_none());
        assert!(b.owns(&readings[0].key));
        readings[0].connection = Connection::Online;
        assert_eq!(
            b.next(&settings, 2, &readings).unwrap().action.rate(),
            Some(rate(1000))
        );
    }
    #[test]
    fn sleeping_device_keeps_original_until_it_wakes() {
        let now = Instant::now();
        let (settings, mut readings) = fixture();
        let mut b = Boost::new(now);
        settle(&mut b, now, true);
        let read = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &read, 1000);
        let apply = b.next(&settings, 1, &readings).unwrap();
        complete(&mut b, &apply, 2000);
        readings[0].connection = Connection::Sleeping;
        b.observe_provider("simulation", &readings);
        settle(&mut b, now + Duration::from_secs(30), false);
        assert!(b.next(&settings, 2, &readings).is_none());
        assert!(b.owns(&readings[0].key));
        readings[0].connection = Connection::Online;
        assert_eq!(
            b.next(&settings, 2, &readings).unwrap().action.rate(),
            Some(rate(1000))
        );
    }
    #[test]
    fn unknown_state_cancels_queued_work_and_clear_revokes_guard() {
        let now = Instant::now();
        let mut b = Boost::new(now);
        let (s, r) = fixture();
        settle(&mut b, now, true);
        let request = b.next(&s, 1, &r).unwrap();
        let guard = b.guard(request.request).unwrap();
        b.sample(now + Duration::from_secs(20), None);
        assert!(guard.cancel.load(Ordering::Acquire));
        assert!(b.next(&s, 1, &r).is_none());
        b.clear();
        assert!(b.guard(request.request).is_none());
    }
    #[test]
    fn already_high_rate_does_not_reduce_rate_and_opt_out_restores_owned_boost() {
        let now = Instant::now();
        let (mut s, r) = fixture();
        let mut b = Boost::new(now);
        settle(&mut b, now, true);
        let read = b.next(&s, 1, &r).unwrap();
        complete(&mut b, &read, 4000);
        assert!(b.next(&s, 1, &r).is_none());
        b.clear();
        settle(&mut b, now, true);
        let read = b.next(&s, 1, &r).unwrap();
        complete(&mut b, &read, 1000);
        let apply = b.next(&s, 1, &r).unwrap();
        complete(&mut b, &apply, 2000);
        s.devices.get_mut(&r[0].key).unwrap().fullscreen_boost_rate = None;
        settle(&mut b, now + Duration::from_secs(30), false);
        assert_eq!(b.next(&s, 1, &r).unwrap().action.rate(), Some(rate(1000)));
    }
}
