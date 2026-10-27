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
    charging_wait: Option<ChargingWait>,
    restore_retry: u8,
    restore_failures: u8,
    recovered: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChargingWait {
    Saver,
    Awaiting(i64),
    Seen,
}
struct Pending {
    request: ControlRequest,
    guard: Guard,
    settings_cancelled: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChargingHandoff {
    pub baseline: PollingRate,
    pub applied: PollingRate,
    pub recovered: bool,
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
    /// Adopt only ownership recorded for the same connection. The normal
    /// desktop transition and compare-before-write path performs restoration.
    pub fn recover(
        &mut self,
        record: &crate::recovery::Record,
        reading: &Reading,
        settings: &Settings,
        generation: u64,
    ) -> bool {
        if record.layer != crate::recovery::Layer::BoostPolling
            || !settings.polling_controls
            || !record.device.matches_reading(reading)
            || !reading.online()
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.request.target.device.key == reading.key)
        {
            return false;
        }
        let (Ok(baseline), Ok(boost)) = (
            PollingRate::try_from(record.before),
            PollingRate::try_from(record.applied),
        ) else {
            return false;
        };
        if let Some(entry) = self.entries.get(&reading.key).filter(|entry| entry.active) {
            return entry.target.device == record.device
                && entry.baseline == Some(baseline)
                && entry.boost == boost
                && entry
                    .target
                    .connection_stamp
                    .is_some_and(|stamp| record.matches_stamp(stamp));
        }
        if self.entries.len() >= 512 && !self.entries.contains_key(&reading.key)
            || self.attempted.len() >= 512 && !self.attempted.contains(&reading.key)
        {
            return false;
        }
        self.attempted.insert(reading.key.clone());
        self.entries.insert(
            reading.key.clone(),
            Entry {
                target: ControlTarget {
                    device: record.device.clone(),
                    generation,
                    connection_stamp: Some(record.stamp),
                },
                boost,
                baseline: Some(baseline),
                active: true,
                charging_wait: None,
                restore_retry: 0,
                restore_failures: 0,
                recovered: true,
            },
        );
        true
    }

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
    pub fn settings_changed(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.settings_cancelled = true;
            pending.guard.cancel.store(true, Ordering::Release);
        }
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
    pub fn observe_provider(&mut self, provider: &str, readings: &[Reading], now: i64) {
        let pending_key = self
            .pending
            .as_ref()
            .map(|p| p.request.target.device.key.as_str());
        let mut cancel_pending = false;
        self.entries.retain(|_, e| {
            if e.target.device.source != provider || e.charging_wait == Some(ChargingWait::Saver) {
                return true;
            }
            let (mut present, mut online, mut conflicting, mut possible_wired) =
                (false, false, false, false);
            for reading in readings {
                if reading.key == e.target.device.key {
                    if e.target.device.matches_reading(reading) {
                        present |= reading.connection != Connection::Stale;
                        online |= reading.online()
                            && reading.timestamp <= now
                            && now.saturating_sub(reading.timestamp) <= 120;
                    } else {
                        conflicting = true;
                    }
                }
                possible_wired |= reading.online() && reading.wired_mouse();
            }
            let has_pending = pending_key == Some(e.target.device.key.as_str());
            let alias = ((e.active || has_pending)
                && e.baseline.is_some()
                && e.target.connection_stamp.is_some()
                && !e.target.device.wired_mouse()
                && (!present || possible_wired || e.charging_wait.is_some()))
            .then(|| e.target.device.shared_preference_key())
            .flatten();
            let wired = alias.as_ref().map_or(0, |alias| {
                readings
                    .iter()
                    .filter(|reading| {
                        reading.online()
                            && reading.wired_mouse()
                            && reading.timestamp <= now
                            && now.saturating_sub(reading.timestamp) <= 120
                            && ConfigurationDevice::from_reading(reading)
                                .shared_preference_key()
                                .as_ref()
                                == Some(alias)
                    })
                    .take(2)
                    .count()
            });
            let expired = matches!(e.charging_wait, Some(ChargingWait::Awaiting(since))
                if now < since || now.saturating_sub(since) > 120);
            if conflicting || expired || (wired > 1 && (!present || e.charging_wait.is_some())) {
                return false;
            }
            if wired == 1 {
                if has_pending {
                    // A confirmed original remains owned until the pending
                    // restore reports whether a SET began. Charging cancels
                    // the request; an uncertain completion still discards it.
                    cancel_pending = true;
                }
                e.charging_wait = Some(ChargingWait::Seen);
            } else if online {
                match e.charging_wait {
                    Some(ChargingWait::Awaiting(_)) => return false,
                    Some(ChargingWait::Seen) => e.charging_wait = None,
                    _ => {}
                }
            } else if !present {
                match e.charging_wait {
                    Some(ChargingWait::Seen) => {}
                    Some(ChargingWait::Awaiting(since))
                        if now >= since && now.saturating_sub(since) <= 120 => {}
                    None if alias.is_some() => {
                        e.charging_wait = Some(ChargingWait::Awaiting(now));
                        cancel_pending |= has_pending;
                    }
                    _ => return false,
                }
            }
            true
        });
        if let Some(p) = &self.pending
            && (cancel_pending || !self.entries.contains_key(&p.request.target.device.key))
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
                    .any(|e| e.charging_wait.is_none() && !e.active && e.baseline.is_some())
                    || self.entries.len() < 512
                        && self.attempted.len() < 512
                        && settings.devices.iter().any(|(key, preferences)| {
                            !self.entries.contains_key(key)
                                && preferences.fullscreen_boost_rate.is_some()
                                && !self.attempted.iter().any(|attempt| {
                                    settings.preference_key(attempt) == settings.preference_key(key)
                                })
                        })
            }
            Some(false) => self
                .entries
                .values()
                .any(|e| e.active && e.charging_wait.is_none() && e.restore_retry == 0),
            None => false,
        }
    }
    pub fn sample(&mut self, now: Instant, state: Option<bool>) {
        if now >= self.next_sample {
            for entry in self.entries.values_mut() {
                entry.restore_retry = entry.restore_retry.saturating_sub(1);
            }
        }
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
                        // A cancelled preflight owns no hardware state. A SET
                        // can already have completed while its readback is in
                        // flight, so retain its proven original until completion.
                        let pending_key = self
                            .pending
                            .as_ref()
                            .map(|p| p.request.target.device.key.as_str());
                        self.entries.retain(|key, entry| {
                            entry.active
                                || pending_key == Some(key.as_str())
                                    && entry.baseline.is_some()
                                    && entry.target.connection_stamp.is_some()
                        });
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
    /// Sleeping and disconnected devices can remain unavailable for hours.
    /// Check borrowed provider caches before resolving the combined inventory.
    pub fn ready<'a>(&self, settings: &Settings, readings: impl Fn(&str) -> &'a [Reading]) -> bool {
        if !self.needs_inventory(settings) {
            return false;
        }
        let fullscreen = self.stable == Some(true);
        let existing = self.entries.values().any(|entry| {
            if entry.charging_wait.is_some() {
                return false;
            }
            let needs_action = if fullscreen {
                !entry.active && entry.baseline.is_some()
            } else {
                entry.active && entry.restore_retry == 0
            };
            if !needs_action {
                return false;
            }
            let cached = readings(&entry.target.device.source);
            // Filtered/disabled caches are not authoritative disconnect evidence.
            // Confirmed provider observations revoke missing/replaced hardware;
            // until then keep ownership without resolving an empty inventory.
            cached
                .iter()
                .any(|r| r.online() && entry.target.device.matches_reading(r))
        });
        if existing || !fullscreen {
            return existing;
        }
        hb_providers::controls::POLLING_PROVIDERS
            .iter()
            .copied()
            .chain(["simulation"])
            .any(|provider| {
                readings(provider).iter().any(|reading| {
                    reading.online()
                        && reading.kind == "mouse"
                        && !self.entries.contains_key(&reading.key)
                        && settings
                            .device(&reading.key)
                            .and_then(|p| p.fullscreen_boost_rate)
                            .is_some()
                        && !self.attempted.iter().any(|attempt| {
                            settings.preference_key(attempt)
                                == settings.preference_key(&reading.key)
                        })
                })
            })
    }
    pub fn owns(&self, key: &str) -> bool {
        self.entries.contains_key(key)
            || self
                .pending
                .as_ref()
                .is_some_and(|p| p.request.target.device.key == key)
    }
    fn charging_handoff_matches(
        &self,
        device: &ConfigurationDevice,
        stamp: ConnectionStamp,
        saver_rate: Option<PollingRate>,
    ) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.request.target.device.key == device.key)
        {
            return false;
        }
        self.entries.get(&device.key).is_some_and(|entry| {
            entry.active
                && entry.baseline.is_some()
                && saver_rate.is_none_or(|rate| entry.baseline == Some(rate))
                && entry.target.device == *device
                && entry.target.connection_stamp == Some(stamp)
        })
    }
    /// Defer an owned boost while a possible USB charging transition is being
    /// confirmed. No polling or writes run while the wireless unit is absent.
    pub fn hold_for_charging(
        &mut self,
        device: &ConfigurationDevice,
        stamp: ConnectionStamp,
        saver_rate: Option<PollingRate>,
    ) {
        if self.charging_handoff_matches(device, stamp, saver_rate)
            && let Some(entry) = self.entries.get_mut(&device.key)
        {
            entry.charging_wait = Some(ChargingWait::Saver);
        }
    }
    pub fn discard_unclaimed_charging(&mut self, provider: &str, keep: impl Fn(&str) -> bool) {
        self.entries.retain(|key, entry| {
            entry.charging_wait != Some(ChargingWait::Saver)
                || entry.target.device.source != provider
                || keep(key)
        });
    }
    /// Transfer a confirmed boost to Battery saver when the same mouse starts
    /// charging. No write is issued; the eventual restore still compares the
    /// current rate and validates the original native connection.
    pub fn handoff_for_charging(
        &mut self,
        device: &ConfigurationDevice,
        stamp: ConnectionStamp,
        saver_rate: Option<PollingRate>,
    ) -> Option<ChargingHandoff> {
        if !self.charging_handoff_matches(device, stamp, saver_rate) {
            return None;
        }
        let entry = self.entries.remove(&device.key)?;
        Some(ChargingHandoff {
            baseline: entry.baseline?,
            applied: entry.boost,
            recovered: entry.recovered,
        })
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
    /// A manual selection on a validated wired alias also cancels any boost
    /// owned by its wireless connection. Aliases never authorize writes.
    pub fn manual_device(&mut self, device: &ConfigurationDevice) {
        self.manual(&device.key);
        if let Some(alias) = device.shared_preference_key()
            && alias != device.key
        {
            self.manual(&alias);
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
    /// Keep only confirmed boosts tied to their original native connection.
    /// An operation interrupted by suspend is uncertain and is not retried.
    pub fn suspend(&mut self) {
        if let Some(p) = self.pending.take() {
            p.guard.cancel.store(true, Ordering::Release);
            self.entries.remove(&p.request.target.device.key);
        }
        self.entries
            .retain(|_, entry| entry.active && entry.target.connection_stamp.is_some());
        // Preserve manual overrides and failed attempts until a stable desktop
        // transition; waking in the same game must not start another boost.
        self.candidate = None;
        self.stable = None;
    }
    pub fn guard(&self, request: u64) -> Option<Guard> {
        self.pending
            .as_ref()
            .filter(|p| p.request.request == request)
            .map(|p| p.guard.clone())
    }
    /// Read-only evidence that the device no longer has our boosted value.
    /// This can also supersede an underlying Battery saver polling layer.
    pub fn external_change<'a>(
        &self,
        outcome: &'a ControlOutcome,
    ) -> Option<&'a PollingObservation> {
        let pending = self.pending.as_ref()?;
        let expected = pending.request.action.expected()?;
        let observed = outcome.observation.as_ref()?;
        (pending.request.request == outcome.request
            && pending.request.target.device.key == outcome.key
            && outcome.failure.is_some()
            && !outcome.may_have_changed
            && observed.target.device == pending.request.target.device
            && observed.target.generation == pending.request.target.generation
            && observed.rate.is_some_and(|rate| rate != expected))
        .then_some(observed)
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
        let fullscreen = self.stable?;
        let existing = self
            .entries
            .iter()
            .find(|(_, e)| {
                if e.charging_wait.is_some() {
                    return false;
                }
                let online = readings
                    .iter()
                    .any(|r| e.target.device.matches_reading(r) && r.online());
                online
                    && if fullscreen {
                        !e.active && e.baseline.is_some()
                    } else {
                        e.active && e.restore_retry == 0
                    }
            })
            .map(|(key, _)| key.clone());
        let key = if let Some(key) = existing {
            key
        } else if fullscreen {
            if self.entries.len() >= 512 || self.attempted.len() >= 512 {
                return None;
            }
            let r = readings.iter().find(|r| {
                r.online()
                    && r.kind == "mouse"
                    // A prior session can still own a delayed/sleeping restore.
                    // Never replace its original baseline with a new preflight.
                    && !self.entries.contains_key(&r.key)
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
                    charging_wait: None,
                    restore_retry: 0,
                    restore_failures: 0,
                    recovered: false,
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
            settings_cancelled: false,
            guard: Guard {
                fullscreen,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        Some(request)
    }
    pub fn complete(&mut self, outcome: &ControlOutcome) -> bool {
        let Some(p) = self.pending.take() else {
            return false;
        };
        if p.request.request != outcome.request {
            self.pending = Some(p);
            return false;
        }
        let key = &outcome.key;
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        let observation = outcome.observation.as_ref();
        let original_still_owned = observation.is_none_or(|observed| {
            observed.target.device == entry.target.device
                && observed.target.generation == p.request.target.generation
                && (observed.target.connection_stamp == entry.target.connection_stamp
                    || entry.recovered
                        && crate::recovery::strong_identity(&entry.target.device)
                        && observed.target.connection_stamp.is_some())
                && observed.rate.is_none_or(|rate| rate == entry.boost)
        });
        let interrupted_restore =
            entry.active && !p.guard.fullscreen && p.guard.cancel.load(Ordering::Acquire);
        if (entry.target.generation != p.request.target.generation
            || p.settings_cancelled
            || interrupted_restore)
            && entry.target.connection_stamp.is_some()
            && p.request.target.connection_stamp.is_some()
            && !outcome.may_have_changed
            && (!entry.active || original_still_owned)
        {
            // Refresh/settings or a fullscreen transition cancelled before SET.
            // Retain the verified original; a later stable desktop must still
            // revalidate the connection and expected rate before restoring it.
            if !entry.active && self.stable == Some(false) {
                // The desktop already settled while this SET was pending.
                // Its unwritten completion owns no hardware; do not leave a
                // preflight latch blocking saver or the next game indefinitely.
                self.entries.remove(key);
            }
            return true;
        }
        if entry.active
            && !p.guard.fullscreen
            && outcome.failure.is_some()
            && !outcome.may_have_changed
            && entry.target.connection_stamp.is_some()
            && entry.target.connection_stamp == p.request.target.connection_stamp
            && original_still_owned
        {
            // No SET began: keep the verified original and the exact connection
            // proof. Existing five-second Shell samples pace retries at 30, 60,
            // 120, then 300 seconds; idle monitoring gains no timer or worker.
            // A startup worker may already have persisted a verified rebind.
            // Keep the original journal stamp: a retry must repeat that probe,
            // never adopt a new connection from this failure alone.
            entry.restore_failures = entry.restore_failures.saturating_add(1).min(4);
            entry.restore_retry = match entry.restore_failures {
                1 => 6,
                2 => 12,
                3 => 24,
                _ => 60,
            };
            return true;
        }
        if outcome.failure.is_some()
            || observation.is_none()
            || observation.is_some_and(|o| {
                // The cache may have refreshed after a successful native SET.
                // This completion must match its own request and the saved
                // native connection, rather than the newer cache generation.
                o.target.generation != p.request.target.generation
                    || o.target.device != entry.target.device
                    || (entry.target.connection_stamp.is_some()
                        && o.target.connection_stamp != entry.target.connection_stamp)
            })
        {
            // Never retry an uncertain automatic SET, including restore.
            self.entries.remove(key);
            return true;
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
        true
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
    fn charging_handoff_requires_confirmed_rate_identity_and_connection() {
        for case in 0..6 {
            let now = Instant::now();
            let (settings, readings) = fixture();
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &read, 1000);
            if case != 0 {
                let apply = boost.next(&settings, 1, &readings).unwrap();
                complete(&mut boost, &apply, 2000);
            }
            if case == 4 {
                settle(&mut boost, now + Duration::from_secs(30), false);
                let _pending_restore = boost.next(&settings, 1, &readings).unwrap();
            }
            let mut device = ConfigurationDevice::from_reading(&readings[0]);
            if case == 3 {
                device.serial = Some("another-device".into());
            }
            let stamp = ConnectionStamp::new(if case == 2 { 2 } else { 1 }).unwrap();
            let saver_rate = rate(if case == 1 { 500 } else { 1000 });
            let result = boost.handoff_for_charging(&device, stamp, Some(saver_rate));
            if case == 5 {
                assert_eq!(
                    result,
                    Some(ChargingHandoff {
                        baseline: rate(1000),
                        applied: rate(2000),
                        recovered: false
                    })
                );
                assert!(!boost.owns(&readings[0].key));
            } else {
                assert_eq!(result, None, "unsafe handoff case {case}");
                assert!(boost.owns(&readings[0].key));
            }
        }
    }
    #[test]
    fn manual_wired_polling_selection_cancels_only_the_matching_wireless_boost() {
        for matching in [false, true] {
            let now = Instant::now();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            let mut settings = Settings {
                polling_controls: true,
                ..Default::default()
            };
            settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(2000));
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &apply, 2000);
            let mut wired = reading.clone();
            wired.key = if matching {
                "razer:00be:TEST"
            } else {
                "razer:00be:OTHER"
            }
            .into();
            wired.serial = Some(if matching { "TEST" } else { "OTHER" }.into());
            boost.manual_device(&ConfigurationDevice::from_reading(&wired));
            assert_eq!(boost.owns(&reading.key), !matching);
        }
    }
    #[test]
    fn standalone_boost_restores_after_confirmed_usb_charging() {
        for gap in [false, true] {
            let now = Instant::now();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            let mut settings = Settings {
                polling_controls: true,
                ..Default::default()
            };
            settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(2000));
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &apply, 2000);
            if gap {
                boost.observe_provider("razer", &[], 100);
            }
            let mut wired = reading.clone();
            wired.key = "razer:00be:TEST".into();
            wired.container = Some("wired-collection".into());
            wired.timestamp = 101;
            boost.observe_provider("razer", std::slice::from_ref(&wired), 101);
            assert!(
                boost.owns(&reading.key),
                "Boost baseline must survive charging"
            );
            settle(&mut boost, now + Duration::from_secs(30), false);
            assert!(!boost.ready(&settings, |_| std::slice::from_ref(&wired)));
            assert!(
                boost
                    .next(&settings, 2, std::slice::from_ref(&wired))
                    .is_none()
            );
            reading.timestamp = 102;
            boost.observe_provider("razer", std::slice::from_ref(&reading), 102);
            let restore = boost
                .next(&settings, 2, std::slice::from_ref(&reading))
                .unwrap();
            assert_eq!(restore.target.connection_stamp, ConnectionStamp::new(1));
            assert_eq!(
                restore.action,
                ControlAction::ApplyIf {
                    rate: rate(1000),
                    expected: rate(2000)
                }
            );
            complete(&mut boost, &restore, 1000);
            assert!(!boost.owns(&reading.key));
        }
    }
    #[test]
    fn standalone_boost_never_restores_after_unproven_or_ambiguous_reconnect() {
        for case in 0..5 {
            let now = Instant::now();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            let mut settings = Settings {
                polling_controls: true,
                ..Default::default()
            };
            settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(2000));
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            complete(&mut boost, &apply, 2000);
            boost.observe_provider("razer", &[], 100);
            let mut wired = reading.clone();
            wired.key = "razer:00be:TEST".into();
            wired.timestamp = 101;
            match case {
                0 => {} // No evidence of a wired charging transition.
                1 => {
                    wired.key = "razer:00be:OTHER".into();
                    wired.serial = Some("OTHER".into());
                    boost.observe_provider("razer", &[wired], 101);
                }
                2 => {
                    let mut duplicate = wired.clone();
                    duplicate.container = Some("other-collection".into());
                    boost.observe_provider("razer", &[wired, duplicate], 101);
                }
                3 => {
                    wired.timestamp = 221;
                    boost.observe_provider("razer", &[wired], 221);
                }
                _ => {
                    wired.timestamp = 99;
                    boost.observe_provider("razer", &[wired], 99);
                }
            }
            let timestamp = if case == 3 { 222 } else { 102 };
            reading.timestamp = timestamp;
            boost.observe_provider("razer", std::slice::from_ref(&reading), timestamp);
            settle(&mut boost, now + Duration::from_secs(30), false);
            assert!(!boost.owns(&reading.key), "unproven connection case {case}");
            assert!(
                boost
                    .next(&settings, 2, std::slice::from_ref(&reading))
                    .is_none()
            );
        }
    }
    #[test]
    fn suspend_preserves_only_confirmed_boosts_with_connection_proof() {
        for case in 0..3 {
            let now = Instant::now();
            let (settings, readings) = fixture();
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &apply, 2000);
            if case == 1 {
                boost
                    .entries
                    .get_mut(&readings[0].key)
                    .unwrap()
                    .target
                    .connection_stamp = None;
            } else if case == 2 {
                settle(&mut boost, now + Duration::from_secs(30), false);
                let pending = boost.next(&settings, 1, &readings).unwrap();
                let guard = boost.guard(pending.request).unwrap();
                boost.suspend();
                assert!(guard.cancel.load(Ordering::Acquire));
            }
            boost.suspend();
            assert_eq!(boost.owns(&readings[0].key), case == 0);
            assert!(!boost.needs_inventory(&settings));
        }
    }
    #[test]
    fn suspend_does_not_reapply_a_manually_overridden_boost_in_the_same_game() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        boost.manual(&readings[0].key);
        boost.suspend();
        settle(&mut boost, now + Duration::from_secs(30), true);
        assert!(boost.next(&settings, 1, &readings).is_none());
        settle(&mut boost, now + Duration::from_secs(60), false);
        settle(&mut boost, now + Duration::from_secs(90), true);
        assert_eq!(
            boost.next(&settings, 1, &readings).unwrap().action,
            ControlAction::Read
        );
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
                    b.observe_provider("simulation", &r, 0);
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
        b.observe_provider("simulation", &[readings[0].clone(), other], 0);
        b.observe_provider("simulation", &readings, 0);
        for generation in 2..=20 {
            b.observe_provider("logitech", &[], 0);
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
    fn settings_cancellation_without_connection_proof_cannot_resume_restore() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &apply, 2000);
        boost
            .entries
            .get_mut(&readings[0].key)
            .unwrap()
            .target
            .connection_stamp = None;
        settle(&mut boost, now + Duration::from_secs(30), false);
        let restore = boost.next(&settings, 1, &readings).unwrap();
        let guard = boost.guard(restore.request).unwrap();
        boost.settings_changed();
        assert!(guard.cancel.load(Ordering::Acquire));
        boost.complete(&ControlOutcome::failed(
            &restore,
            "Settings revoked permission",
        ));
        assert!(boost.next(&settings, 1, &readings).is_none());
    }
    #[test]
    fn unrelated_settings_preserve_verified_restore_only_before_any_write() {
        for may_have_changed in [false, true] {
            let now = Instant::now();
            let (mut settings, readings) = fixture();
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &apply, 2000);
            settle(&mut boost, now + Duration::from_secs(30), false);
            let restore = boost.next(&settings, 1, &readings).unwrap();
            settings.animation = !settings.animation;
            boost.settings_changed();
            let mut cancelled =
                ControlOutcome::failed(&restore, "Settings revoked the old permission epoch");
            cancelled.may_have_changed = may_have_changed;
            boost.complete(&cancelled);
            let retry = boost.next(&settings, 1, &readings);
            assert_eq!(retry.is_some(), !may_have_changed);
            if let Some(retry) = retry {
                assert_eq!(
                    retry.target.connection_stamp,
                    restore.target.connection_stamp
                );
                assert_eq!(retry.action, restore.action);
                boost.complete(&ControlOutcome::failed(&retry, "Connection stamp changed"));
                assert!(boost.next(&settings, 1, &readings).is_none());
            }
        }
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
                1 => b.observe_provider("simulation", &[], 0),
                2 => {
                    readings[0].connection = Connection::Stale;
                    b.observe_provider("simulation", &readings, 0);
                    readings[0].connection = Connection::Online;
                }
                _ => {
                    readings[0].container = Some("new receiver".into());
                    b.observe_provider("simulation", &readings, 0);
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
    fn unwritten_restore_failure_keeps_original_until_a_paced_retry() {
        let now = Instant::now();
        let (mut settings, readings) = fixture();
        settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(4000));
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &apply, 4000);
        settle(&mut boost, now + Duration::from_secs(30), false);
        let restore = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(
            restore.action,
            ControlAction::ApplyIf {
                rate: rate(1000),
                expected: rate(4000)
            }
        );
        assert!(boost.complete(&ControlOutcome::failed(
            &restore,
            "Recovery storage is busy."
        )));
        assert!(
            boost.owns(&readings[0].key),
            "A failed preflight must not forget the original desktop rate"
        );
        assert!(!boost.needs_inventory(&settings));
        assert!(boost.next(&settings, 1, &readings).is_none());
        for tick in 1..=6 {
            boost.sample(now + Duration::from_secs(45 + tick * 5), Some(false));
            if tick < 6 {
                assert!(!boost.ready(&settings, |_| &readings));
            }
        }
        let retry = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(retry.action, restore.action);
        assert_eq!(
            retry.target.connection_stamp,
            restore.target.connection_stamp
        );
        complete(&mut boost, &retry, 1000);
        assert!(!boost.owns(&readings[0].key));
    }
    #[test]
    fn usb_charging_during_an_unwritten_restore_retains_the_desktop_original() {
        for gap in [false, true] {
            for uncertain in [false, true] {
                let now = Instant::now();
                let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
                reading.kind = "mouse".into();
                reading.via = "usb".into();
                reading.serial = Some("TEST".into());
                let mut settings = Settings {
                    polling_controls: true,
                    ..Default::default()
                };
                settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(4000));
                let mut boost = Boost::new(now);
                settle(&mut boost, now, true);
                let read = boost
                    .next(&settings, 1, std::slice::from_ref(&reading))
                    .unwrap();
                complete(&mut boost, &read, 1000);
                let apply = boost
                    .next(&settings, 1, std::slice::from_ref(&reading))
                    .unwrap();
                complete(&mut boost, &apply, 4000);
                settle(&mut boost, now + Duration::from_secs(30), false);
                let restore = boost
                    .next(&settings, 1, std::slice::from_ref(&reading))
                    .unwrap();
                let guard = boost.guard(restore.request).unwrap();
                if gap {
                    boost.observe_provider("razer", &[], 100);
                }
                let mut wired = reading.clone();
                wired.key = "razer:00be:TEST".into();
                wired.timestamp = 101;
                wired.charging = Some(true);
                boost.observe_provider("razer", &[wired], 101);
                assert!(guard.cancel.load(Ordering::Acquire));
                let mut outcome =
                    ControlOutcome::failed(&restore, "Charging cancelled restore before SET");
                outcome.may_have_changed = uncertain;
                boost.complete(&outcome);
                assert_eq!(boost.owns(&reading.key), !uncertain);
                assert!(!boost.needs_inventory(&settings));
                reading.timestamp = 102;
                boost.observe_provider("razer", std::slice::from_ref(&reading), 102);
                let retry = boost.next(&settings, 2, std::slice::from_ref(&reading));
                if uncertain {
                    assert!(retry.is_none());
                } else {
                    let retry = retry.unwrap();
                    assert_eq!(retry.action, restore.action);
                    assert_eq!(
                        retry.target.connection_stamp,
                        restore.target.connection_stamp
                    );
                }
            }
        }
    }
    #[test]
    fn cancellation_does_not_hide_a_readback_that_revokes_ownership() {
        for actual in [1000, 2000] {
            let now = Instant::now();
            let (mut boost, settings, readings, restore) = pending_restore_fixture(now);
            boost.settings_changed();
            let mut outcome =
                ControlOutcome::failed(&restore, "Setting changed before cancellation");
            outcome.observation = Some(PollingObservation {
                target: restore.target.clone(),
                supported: vec![rate(1000), rate(2000), rate(4000)],
                rate: Some(rate(actual)),
                timestamp: 100,
                evidence: "test".into(),
            });
            assert!(boost.complete(&outcome));
            assert!(!boost.owns(&readings[0].key));
            assert!(boost.next(&settings, 1, &readings).is_none());
        }
    }
    #[test]
    fn filtered_inventory_does_not_revoke_an_owned_restore() {
        let now = Instant::now();
        let (mut boost, settings, readings, restore) = pending_restore_fixture(now);
        boost.settings_changed();
        assert!(boost.complete(&ControlOutcome::failed(&restore, "Refresh before SET")));
        boost.observe_generation(2);
        assert!(boost.next(&settings, 2, &[]).is_none());
        assert!(
            boost.owns(&readings[0].key),
            "Filtered inventory is not proof of a physical disconnect"
        );
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!boost.ready(&settings, |_| &[]));
            }
        });
        assert_eq!(allocations.calls, 0);
        let retry = boost.next(&settings, 2, &readings).unwrap();
        assert_eq!(retry.action, restore.action);
        assert_eq!(
            retry.target.connection_stamp,
            restore.target.connection_stamp
        );
        // A confirmed provider disappearance still revokes the old unit.
        boost.observe_provider("simulation", &[], 100);
        assert!(
            boost
                .guard(retry.request)
                .unwrap()
                .cancel
                .load(Ordering::Acquire)
        );
        assert!(!boost.complete(&ControlOutcome::failed(&retry, "Device disconnected")));
        assert!(!boost.owns(&readings[0].key));
    }
    #[test]
    fn retained_restore_owners_remain_bounded_without_idle_allocations() {
        let now = Instant::now();
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        let mut boost = Boost::new(now);
        for index in 0..512 {
            let mut reading = Reading::new(
                format!("simulation:owned-{index}"),
                "Mouse",
                "simulation",
                100,
            );
            reading.kind = "mouse".into();
            let record = crate::recovery::Record {
                device: ConfigurationDevice::from_reading(&reading),
                stamp: ConnectionStamp::new(1).unwrap(),
                rebound_stamp: None,
                before: 1000,
                applied: 4000,
                layer: crate::recovery::Layer::BoostPolling,
                confirmed: true,
                manual_pending: false,
            };
            assert!(boost.recover(&record, &reading, &settings, 1));
        }
        settle(&mut boost, now, false);
        settle(&mut boost, now + Duration::from_secs(30), true);
        let mut new_device = Reading::new("simulation:new-device", "Mouse", "simulation", 100);
        new_device.kind = "mouse".into();
        settings.device_mut(&new_device.key).fullscreen_boost_rate = Some(rate(4000));
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!boost.needs_inventory(&settings));
                assert!(!boost.ready(&settings, |_| panic!(
                    "Full ownership cache must not query inventory"
                )));
            }
        });
        assert_eq!(allocations.calls, 0);
        assert!(boost.next(&settings, 1, &[new_device]).is_none());
        assert_eq!(boost.entries.len(), 512);
    }
    #[test]
    fn verified_boost_apply_retains_its_original_across_refresh() {
        let now = Instant::now();
        let (settings, readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        boost.observe_generation(2);
        complete(&mut boost, &apply, 2000);
        assert!(
            boost.owns(&readings[0].key),
            "A verified late SET must retain its desktop original"
        );
        settle(&mut boost, now + Duration::from_secs(30), false);
        let restore = boost.next(&settings, 2, &readings).unwrap();
        assert_eq!(
            restore.action,
            ControlAction::ApplyIf {
                rate: rate(1000),
                expected: rate(2000)
            }
        );
        assert_eq!(restore.target.connection_stamp, ConnectionStamp::new(1));
    }
    #[test]
    fn charging_during_boost_apply_keeps_only_a_verified_original() {
        for gap in [false, true] {
            for case in 0..7 {
                let now = Instant::now();
                let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
                reading.kind = "mouse".into();
                reading.via = "usb".into();
                reading.serial = Some("TEST".into());
                let mut settings = Settings {
                    polling_controls: true,
                    ..Default::default()
                };
                settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(4000));
                let mut boost = Boost::new(now);
                settle(&mut boost, now, true);
                let read = boost
                    .next(&settings, 1, std::slice::from_ref(&reading))
                    .unwrap();
                complete(&mut boost, &read, 1000);
                let apply = boost
                    .next(&settings, 1, std::slice::from_ref(&reading))
                    .unwrap();
                if gap || case == 6 {
                    boost.observe_provider("razer", &[], 100);
                }
                let mut wired = reading.clone();
                wired.key = "razer:00be:TEST".into();
                wired.charging = Some(true);
                wired.timestamp = 101;
                if case == 5 {
                    let mut duplicate = wired.clone();
                    duplicate.container = Some("DUPLICATE-COLLECTION".into());
                    boost.observe_provider("razer", &[wired.clone(), duplicate], 101);
                } else if case != 6 {
                    boost.observe_provider("razer", std::slice::from_ref(&wired), 101);
                }
                boost.observe_generation(2);
                if case == 3 {
                    boost.manual(&reading.key);
                }
                let mut outcome = ControlOutcome::failed(&apply, "Apply interrupted by charging");
                outcome.may_have_changed = case != 2;
                if !matches!(case, 1 | 2) {
                    outcome.failure = None;
                    outcome.observation = Some(PollingObservation {
                        target: ControlTarget {
                            connection_stamp: ConnectionStamp::new(if case == 4 { 2 } else { 1 }),
                            ..apply.target.clone()
                        },
                        supported: vec![rate(1000), rate(4000)],
                        rate: Some(rate(4000)),
                        timestamp: 101,
                        evidence: "test".into(),
                    });
                }
                boost.complete(&outcome);
                settle(&mut boost, now + Duration::from_secs(30), false);
                assert_eq!(
                    boost.owns(&reading.key),
                    case == 0 || case == 6,
                    "Charging must retain a confirmed apply, but not uncertain, manual or changed-connection state"
                );
                assert!(boost.next(&settings, 2, &[wired]).is_none());
                reading.timestamp = 102;
                boost.observe_provider("razer", std::slice::from_ref(&reading), 102);
                let restore = boost.next(&settings, 2, std::slice::from_ref(&reading));
                if case == 0 {
                    let restore = restore.unwrap();
                    assert_eq!(
                        restore.action,
                        ControlAction::ApplyIf {
                            rate: rate(1000),
                            expected: rate(4000)
                        }
                    );
                    assert_eq!(restore.target.connection_stamp, ConnectionStamp::new(1));
                } else {
                    assert!(restore.is_none());
                }
            }
        }
    }
    #[test]
    fn desktop_transition_does_not_drop_an_inflight_boost_original() {
        for case in 0..3 {
            let verified = case == 0;
            let now = Instant::now();
            let (settings, readings) = fixture();
            let mut boost = Boost::new(now);
            settle(&mut boost, now, true);
            let read = boost.next(&settings, 1, &readings).unwrap();
            complete(&mut boost, &read, 1000);
            let apply = boost.next(&settings, 1, &readings).unwrap();
            settle(&mut boost, now + Duration::from_secs(30), false);
            if verified {
                complete(&mut boost, &apply, 2000);
            } else {
                if case == 2 {
                    boost.settings_changed();
                }
                let mut outcome = ControlOutcome::failed(&apply, "SET acknowledgement is unknown");
                outcome.may_have_changed = case == 1;
                boost.complete(&outcome);
            }
            assert_eq!(
                boost.owns(&readings[0].key),
                verified,
                "A desktop transition must wait for the pending SET's verified outcome"
            );
            let restore = boost.next(&settings, 1, &readings);
            if verified {
                assert_eq!(
                    restore.unwrap().action,
                    ControlAction::ApplyIf {
                        rate: rate(1000),
                        expected: rate(2000)
                    }
                );
            } else {
                assert!(restore.is_none());
            }
        }
    }
    #[test]
    fn another_game_cannot_replace_an_owned_boost_waiting_for_the_mouse_to_wake() {
        let now = Instant::now();
        let (mut settings, mut readings) = fixture();
        settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(4000));
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &apply, 4000);
        readings[0].connection = Connection::Sleeping;
        settle(&mut boost, now + Duration::from_secs(30), false);
        assert!(!boost.ready(&settings, |_| &readings));
        settle(&mut boost, now + Duration::from_secs(60), true);
        readings[0].connection = Connection::Online;
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!boost.ready(&settings, |_| &readings));
                assert!(
                    boost.next(&settings, 1, &readings).is_none(),
                    "An owned 4000 Hz boost must not become a new preflight"
                );
            }
        });
        assert_eq!(allocations.calls, 0);
        settle(&mut boost, now + Duration::from_secs(90), false);
        let restore = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(
            restore.action,
            ControlAction::ApplyIf {
                rate: rate(1000),
                expected: rate(4000)
            }
        );
        complete(&mut boost, &restore, 1000);
        // Completing the restore must not suppress the next game's boost.
        settle(&mut boost, now + Duration::from_secs(120), true);
        assert_eq!(
            boost.next(&settings, 1, &readings).unwrap().action,
            ControlAction::Read
        );
    }
    fn pending_restore_fixture(now: Instant) -> (Boost, Settings, Vec<Reading>, ControlRequest) {
        let (mut settings, readings) = fixture();
        settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(4000));
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &apply, 4000);
        settle(&mut boost, now + Duration::from_secs(30), false);
        let restore = boost.next(&settings, 1, &readings).unwrap();
        (boost, settings, readings, restore)
    }
    #[test]
    fn failed_restore_backoff_is_bounded_and_waiting_does_not_allocate() {
        let now = Instant::now();
        let (mut boost, settings, readings, mut request) = pending_restore_fixture(now);
        let mut seconds = 45;
        for ticks in [6, 12, 24, 60, 60] {
            assert!(boost.complete(&ControlOutcome::failed(
                &request,
                "GET timed out before SET"
            )));
            let (_, allocations) = crate::allocation_counter::measure(|| {
                for _ in 0..1000 {
                    assert!(!boost.ready(&settings, |_| &readings));
                    assert!(boost.next(&settings, 1, &readings).is_none());
                }
            });
            assert_eq!(allocations.calls, 0);
            for tick in 1..=ticks {
                seconds += 5;
                boost.sample(now + Duration::from_secs(seconds), Some(false));
                if tick < ticks {
                    assert!(!boost.needs_inventory(&settings));
                }
            }
            request = boost.next(&settings, 1, &readings).unwrap();
            assert_eq!(
                request.action,
                ControlAction::ApplyIf {
                    rate: rate(1000),
                    expected: rate(4000)
                }
            );
            assert_eq!(request.target.connection_stamp, ConnectionStamp::new(1));
        }
        complete(&mut boost, &request, 1000);
    }
    #[test]
    fn uncertain_or_changed_restore_evidence_never_authorizes_retry() {
        for case in 0..5 {
            let now = Instant::now();
            let (mut boost, settings, readings, request) = pending_restore_fixture(now);
            let mut outcome = ControlOutcome::failed(&request, "Restore failed");
            if case == 0 {
                outcome.may_have_changed = true;
            } else if case == 4 {
                boost
                    .entries
                    .get_mut(&readings[0].key)
                    .unwrap()
                    .target
                    .connection_stamp = None;
            } else {
                let mut target = request.target.clone();
                if case == 1 {
                    target.connection_stamp = ConnectionStamp::new(2);
                }
                if case == 2 {
                    target.device.serial = Some("different-unit".into());
                }
                outcome.observation = Some(PollingObservation {
                    target,
                    supported: vec![rate(1000), rate(4000)],
                    rate: Some(rate(if case == 3 { 1000 } else { 4000 })),
                    timestamp: 0,
                    evidence: "test".into(),
                });
            }
            assert!(boost.complete(&outcome));
            assert!(!boost.owns(&readings[0].key), "unsafe retry case {case}");
            for tick in 1..=60 {
                boost.sample(now + Duration::from_secs(45 + tick * 5), Some(false));
            }
            assert!(boost.next(&settings, 1, &readings).is_none());
        }
    }
    #[test]
    fn a_new_game_or_manual_choice_cannot_be_overridden_by_a_delayed_restore() {
        for manual in [false, true] {
            let now = Instant::now();
            let (mut boost, settings, readings, restore) = pending_restore_fixture(now);
            boost.complete(&ControlOutcome::failed(&restore, "GET failed before SET"));
            if manual {
                boost.manual(&readings[0].key);
            }
            for tick in 1..=6 {
                boost.sample(now + Duration::from_secs(45 + tick * 5), Some(!manual));
            }
            assert!(boost.next(&settings, 1, &readings).is_none());
            if !manual {
                settle(&mut boost, now + Duration::from_secs(90), false);
                assert_eq!(
                    boost.next(&settings, 1, &readings).unwrap().action,
                    restore.action
                );
            }
        }
    }
    #[test]
    fn waiting_for_a_sleeping_restore_skips_inventory_allocations() {
        let now = Instant::now();
        let (settings, mut readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        let apply = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &apply, 2000);
        settle(&mut boost, now + Duration::from_secs(30), false);
        for connection in [Connection::Sleeping, Connection::Stale] {
            readings[0].connection = connection;
            let (_, before) = crate::allocation_counter::measure(|| {
                for _ in 0..1000 {
                    if boost.needs_inventory(&settings) {
                        assert!(boost.next(&settings, 1, &readings.clone()).is_none());
                    }
                }
            });
            let (_, after) = crate::allocation_counter::measure(|| {
                for _ in 0..1000 {
                    assert!(!boost.ready(&settings, |_| &readings));
                }
            });
            println!("1,000 unavailable restore checks: before={before:?}, after={after:?}");
            assert!(before.calls > 0);
            assert_eq!(after.calls, 0);
            assert_eq!(after.bytes, 0);
        }
        assert!(!boost.ready(&settings, |_| &[]));
        readings[0].connection = Connection::Online;
        assert!(boost.ready(&settings, |_| &readings));
        readings[0].serial = Some("SYNTHETIC-REPLACEMENT".into());
        assert!(!boost.ready(&settings, |_| &readings));
        readings[0].serial = None;
        let restore = boost.next(&settings, 1, &readings).unwrap();
        assert_eq!(restore.action.rate(), Some(rate(1000)));
        assert!(!boost.ready(&settings, |_| &readings));
        complete(&mut boost, &restore, 1000);
        assert!(!boost.ready(&settings, |_| &readings));
    }
    #[test]
    fn fullscreen_preflight_waits_without_cloning_until_the_opted_device_is_available() {
        let now = Instant::now();
        let (settings, mut readings) = fixture();
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);

        assert!(!boost.ready(&settings, |_| &[]));
        let mut unrelated = readings[0].clone();
        unrelated.key = "simulated:other".into();
        assert!(!boost.ready(&settings, |_| std::slice::from_ref(&unrelated)));
        readings[0].connection = Connection::Sleeping;
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!boost.ready(&settings, |_| &readings));
            }
        });
        assert_eq!(allocations.calls, 0);
        readings[0].connection = Connection::Online;
        assert!(boost.ready(&settings, |_| &readings));

        let read = boost.next(&settings, 1, &readings).unwrap();
        complete(&mut boost, &read, 1000);
        readings[0].connection = Connection::Stale;
        assert!(!boost.ready(&settings, |_| &readings));
        assert!(!boost.ready(&settings, |_| &[]));
        readings[0].connection = Connection::Online;
        assert!(boost.ready(&settings, |_| &readings));
    }
    #[test]
    fn fullscreen_preflight_honours_linked_wired_and_wireless_preferences() {
        let now = Instant::now();
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        settings
            .device_mut("razer:00bf:TESTUNIT1234")
            .fullscreen_boost_rate = Some(rate(2000));
        let mut wired = Reading::new("razer:00be:TESTUNIT1234", "Mouse", "razer", 0);
        wired.kind = "mouse".into();
        wired.via = "usb".into();
        wired.serial = Some("TESTUNIT1234".into());
        settings.link_reading(&wired);
        let mut boost = Boost::new(now);
        settle(&mut boost, now, true);
        assert!(boost.ready(&settings, |source| {
            if source == "razer" {
                std::slice::from_ref(&wired)
            } else {
                &[]
            }
        }));
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
        b.observe_provider("simulation", &readings, 0);
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
    fn interrupted_restore_keeps_original_only_with_connection_proof_and_no_write() {
        for state in [Some(true), None] {
            for proof in [true, false] {
                for may_have_changed in [false, true] {
                    let now = Instant::now();
                    let (settings, readings) = fixture();
                    let mut boost = Boost::new(now);
                    settle(&mut boost, now, true);
                    let read = boost.next(&settings, 1, &readings).unwrap();
                    complete(&mut boost, &read, 1000);
                    let apply = boost.next(&settings, 1, &readings).unwrap();
                    complete(&mut boost, &apply, 2000);
                    if !proof {
                        boost
                            .entries
                            .get_mut(&readings[0].key)
                            .unwrap()
                            .target
                            .connection_stamp = None;
                    }
                    settle(&mut boost, now + Duration::from_secs(30), false);
                    let restore = boost.next(&settings, 1, &readings).unwrap();
                    let guard = boost.guard(restore.request).unwrap();
                    boost.sample(now + Duration::from_secs(60), state);
                    assert!(guard.cancel.load(Ordering::Acquire));
                    let mut outcome = ControlOutcome::failed(&restore, "Fullscreen state changed");
                    outcome.may_have_changed = may_have_changed;
                    boost.complete(&outcome);
                    assert!(boost.next(&settings, 1, &readings).is_none());
                    settle(&mut boost, now + Duration::from_secs(90), false);
                    let retry = boost.next(&settings, 1, &readings);
                    assert_eq!(retry.is_some(), proof && !may_have_changed);
                    if let Some(retry) = retry {
                        assert_eq!(retry.target.connection_stamp, ConnectionStamp::new(1));
                        assert_eq!(
                            retry.action,
                            ControlAction::ApplyIf {
                                rate: rate(1000),
                                expected: rate(2000)
                            }
                        );
                        complete(&mut boost, &retry, 1000);
                        assert!(!boost.owns(&readings[0].key));
                    }
                }
            }
        }
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
