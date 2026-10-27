//! Event-fed hardware reductions. No timer, input tracking, or periodic rate reads.
use hb_core::settings::LowBatteryMode;
use hb_core::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WiredTransition {
    Awaiting(i64),
    Seen,
}
#[derive(Clone, Copy, Default)]
struct RestoreRetry {
    after: Option<Instant>,
    failures: u8,
    recovered: bool,
}
impl RestoreRetry {
    fn ready(&self) -> bool {
        self.after.is_none_or(|after| Instant::now() >= after)
    }
    fn defer(&mut self) {
        self.failures = self.failures.saturating_add(1).min(4);
        let seconds = match self.failures {
            1 => 30,
            2 => 60,
            3 => 120,
            _ => 300,
        };
        self.after = Some(Instant::now() + Duration::from_secs(seconds));
    }
}
struct Entry {
    target: ControlTarget,
    choices: LowBatteryMode,
    low: bool,
    rate: Setting,
    sleep: Setting,
    rate_stamp: Option<ConnectionStamp>,
    sleep_stamp: Option<ConnectionStamp>,
    wired_transition: Option<WiredTransition>,
    restore_retry: [RestoreRetry; 2],
}
impl Entry {
    fn needs_action(&self) -> bool {
        self.wired_transition.is_none()
            && [self.rate, self.sleep]
                .into_iter()
                .enumerate()
                .any(|(index, s)| match s {
                    Setting::New | Setting::Ready(_) => true,
                    Setting::Active { .. } => !self.low && self.restore_retry[index].ready(),
                    Setting::Done => false,
                })
    }
    fn usable(&self, r: &Reading, now: i64) -> bool {
        self.wired_transition.is_none()
            && self.target.device.matches_reading(r)
            && r.online()
            && (!self.low || (r.level.is_some() && r.charging == Some(false)))
            && r.timestamp <= now
            && now.saturating_sub(r.timestamp) <= 120
    }
    fn has_proven_original(&self) -> bool {
        (matches!(self.rate, Setting::Active { .. }) && self.rate_stamp.is_some())
            || (matches!(self.sleep, Setting::Active { .. }) && self.sleep_stamp.is_some())
    }

    fn boost_charging_proof(&self) -> Option<(ConnectionStamp, Option<PollingRate>, Option<u32>)> {
        if let Setting::Active { before, applied } = self.rate {
            return Some((
                self.rate_stamp?,
                Some(PollingRate::try_from(applied).ok()?),
                Some(before),
            ));
        }
        // A sleep-only saver can still own a simultaneous confirmed game boost.
        // A configured/manual polling override must never be silently replaced.
        if self.choices.polling_rate.is_none() && matches!(self.sleep, Setting::Active { .. }) {
            return Some((self.sleep_stamp?, None, None));
        }
        None
    }

    fn discard_originals(&mut self) {
        self.rate = Setting::Done;
        self.sleep = Setting::Done;
        self.wired_transition = None;
        self.restore_retry = [RestoreRetry::default(); 2];
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
struct Completion {
    id: u64,
    value: Option<u32>,
    failed: bool,
    may_have_changed: bool,
    connection_stamp: Option<ConnectionStamp>,
    matches_target: bool,
}
#[derive(Default)]
pub struct BatterySaver {
    entries: BTreeMap<String, Entry>,
    pending: Option<Pending>,
    sequence: u64,
}
impl BatterySaver {
    /// Restore ownership, not hardware state, from a durable pre-write record.
    /// The eventual guarded transaction still checks the native connection and
    /// the value currently on the device before writing.
    pub fn recover(
        &mut self,
        record: &crate::recovery::Record,
        reading: &Reading,
        settings: &Settings,
        generation: u64,
    ) -> bool {
        let (choices, sleep) = match record.layer {
            crate::recovery::Layer::SaverPolling(mode) => (mode, false),
            crate::recovery::Layer::SaverSleep(mode) => (mode, true),
            crate::recovery::Layer::BoostPolling => return false,
        };
        if !settings.polling_controls
            || !record.device.matches_reading(reading)
            || !reading.online()
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.action.key() == reading.key)
            || self.entries.len() >= 512 && !self.entries.contains_key(&reading.key)
        {
            return false;
        }
        let low = record.confirmed
            && settings
                .device(&reading.key)
                .and_then(|p| p.low_battery_mode)
                == Some(choices)
            && reading.charging != Some(true)
            && !reading.wired_mouse()
            && reading
                .level
                .is_none_or(|level| level < choices.threshold.saturating_add(5));
        let entry = self
            .entries
            .entry(reading.key.clone())
            .or_insert_with(|| Entry {
                target: ControlTarget {
                    device: record.device.clone(),
                    generation,
                    connection_stamp: None,
                },
                choices,
                low,
                rate: Setting::Done,
                sleep: Setting::Done,
                rate_stamp: None,
                sleep_stamp: None,
                wired_transition: None,
                restore_retry: [RestoreRetry::default(); 2],
            });
        if entry.target.device != record.device || entry.choices != choices {
            if !entry.target.device.matches_reading(reading)
                || matches!(entry.rate, Setting::Active { .. })
                || matches!(entry.sleep, Setting::Active { .. })
            {
                return false;
            }
            // A provider event can prepare the newly configured saver while
            // storage is still loading/rearming the old episode. Retire that
            // unwritten preparation and recover the durable originals first.
            // Never replace another identity or an already owned change.
            entry.target.device = record.device.clone();
            entry.target.generation = generation;
            entry.target.connection_stamp = None;
            entry.choices = choices;
            entry.low = low;
            entry.rate = Setting::Done;
            entry.sleep = Setting::Done;
            entry.rate_stamp = None;
            entry.sleep_stamp = None;
            entry.wired_transition = None;
            entry.restore_retry = [RestoreRetry::default(); 2];
        }
        let (state, stamp) = if sleep {
            (&mut entry.sleep, &mut entry.sleep_stamp)
        } else {
            (&mut entry.rate, &mut entry.rate_stamp)
        };
        // A deferred startup claim can replace a preflight that has not sent
        // its SET. It must never replace an operation already in flight.
        if matches!(*state, Setting::Active { .. }) {
            // Re-enabling controls can rearm an already-owned journal slot.
            // Acknowledge the exact ownership without changing its episode or
            // repeatedly cloning/reconsidering this record on every poll.
            return *state
                == (Setting::Active {
                    before: record.before,
                    applied: record.applied,
                })
                && stamp.is_some_and(|stamp| record.matches_stamp(stamp));
        }
        entry.low &= low;
        *state = Setting::Active {
            before: record.before,
            applied: record.applied,
        };
        *stamp = Some(record.stamp);
        entry.restore_retry[usize::from(sleep)] = RestoreRetry {
            recovered: true,
            ..Default::default()
        };
        true
    }

    pub fn recovery_choices(&self, id: u64) -> Option<LowBatteryMode> {
        let pending = self
            .pending
            .as_ref()
            .filter(|pending| pending.action.id() == id)?;
        self.entries
            .get(pending.action.key())
            .map(|entry| entry.choices)
    }

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
    pub fn restoring(&self, id: u64) -> Option<bool> {
        self.pending
            .as_ref()
            .filter(|p| p.action.id() == id)
            .map(|p| p.restoring)
    }
    pub fn cancel(&self, id: u64) -> Option<Arc<AtomicBool>> {
        self.pending
            .as_ref()
            .filter(|p| p.action.id() == id)
            .map(|p| p.cancel.clone())
    }
    /// Respect an external rate without discarding independent sleep ownership.
    pub fn observe_external_rate(&mut self, device: &ConfigurationDevice, rate: PollingRate) {
        let superseded = self.entries.get(&device.key).is_some_and(|entry| {
            entry.target.device == *device
                && matches!(entry.rate, Setting::Active { applied, .. } if applied != rate.hz())
        });
        if superseded {
            self.manual(device, false);
        }
    }
    /// Manual changes take ownership of the affected setting for this low-battery episode.
    pub fn manual(&mut self, device: &ConfigurationDevice, sleep: bool) {
        // The alias can only cancel automation. It never authorizes a write to
        // the other connection, and requires the validated model/serial pair.
        let alias = device
            .wired_mouse()
            .then(|| device.shared_preference_key())
            .flatten();
        let mut cancel = false;
        for entry in self.entries.values_mut() {
            if entry.target.device.key == device.key
                || alias.as_ref().is_some_and(|key| {
                    entry.target.device.shared_preference_key().as_ref() == Some(key)
                })
            {
                if sleep {
                    entry.sleep = Setting::Done;
                } else {
                    entry.rate = Setting::Done;
                }
                entry.restore_retry[usize::from(sleep)] = RestoreRetry::default();
                cancel |= self.pending.as_ref().is_some_and(|p| {
                    p.action.key() == entry.target.device.key
                        && matches!(p.action, Action::Sleep(_)) == sleep
                });
            }
        }
        if cancel {
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
            e.restore_retry = [RestoreRetry::default(); 2];
        }
    }
    /// Suspend performs no I/O. Retain only completed changes tied to their
    /// original native connection; an in-flight operation remains uncertain.
    pub fn suspend(&mut self) {
        if let Some(p) = self.pending.take() {
            p.cancel.store(true, Ordering::Release);
            if let Some(entry) = self.entries.get_mut(p.action.key()) {
                if matches!(p.action, Action::Sleep(_)) {
                    entry.sleep = Setting::Done;
                    entry.sleep_stamp = None;
                } else {
                    entry.rate = Setting::Done;
                    entry.rate_stamp = None;
                }
            }
        }
        for entry in self.entries.values_mut() {
            if !matches!(entry.rate, Setting::Active { .. }) || entry.rate_stamp.is_none() {
                entry.rate = Setting::Done;
                entry.rate_stamp = None;
            }
            if !matches!(entry.sleep, Setting::Active { .. }) || entry.sleep_stamp.is_none() {
                entry.sleep = Setting::Done;
                entry.sleep_stamp = None;
            }
        }
    }
    pub fn observe_provider(&mut self, provider: &str, readings: &[Reading], now: i64) {
        for entry in self.entries.values_mut() {
            if entry.target.device.source != provider {
                continue;
            }
            let (mut present, mut online, mut conflicting, mut possible_wired) =
                (false, false, false, false);
            for r in readings {
                if r.key == entry.target.device.key {
                    if entry.target.device.matches_reading(r) {
                        present |= r.connection != Connection::Stale;
                        online |= r.online()
                            && r.timestamp <= now
                            && now.saturating_sub(r.timestamp) <= 120;
                    } else {
                        conflicting = true;
                    }
                }
                possible_wired |= r.online() && r.wired_mouse();
            }
            // A SET may have completed before its acknowledgement arrives.
            // Preserve its already-read original only during a bounded,
            // serial-proven charging transition; completion decides ownership.
            let pending_original = self.pending.as_ref().is_some_and(|pending| {
                pending.action.key() == entry.target.device.key
                    && pending.action.target().connection_stamp.is_some()
                    && matches!(pending.old, Setting::Ready(_))
                    && if matches!(pending.action, Action::Sleep(_)) {
                        entry.sleep == pending.old
                            && entry.sleep_stamp == pending.action.target().connection_stamp
                    } else {
                        entry.rate == pending.old
                            && entry.rate_stamp == pending.action.target().connection_stamp
                    }
            });
            let alias = (!entry.target.device.wired_mouse()
                && (entry.has_proven_original() || pending_original)
                && (!present || possible_wired || entry.wired_transition.is_some()))
            .then(|| entry.target.device.shared_preference_key())
            .flatten();
            let wired = alias.as_ref().map_or(0, |alias| {
                readings
                    .iter()
                    .filter(|r| {
                        r.online()
                            && r.wired_mouse()
                            && r.timestamp <= now
                            && now.saturating_sub(r.timestamp) <= 120
                            && ConfigurationDevice::from_reading(r)
                                .shared_preference_key()
                                .as_ref()
                                == Some(alias)
                    })
                    .take(2)
                    .count()
            });
            let mut cancel = false;
            let expired = matches!(entry.wired_transition, Some(WiredTransition::Awaiting(since))
                if now < since || now.saturating_sub(since) > 120);
            if conflicting
                || expired
                || (wired > 1 && (!present || entry.wired_transition.is_some()))
            {
                entry.discard_originals();
                cancel = true;
            } else if wired == 1 {
                // This proves only that charging occurred. A later restoration
                // still targets the original wireless identity and connection
                // stamp, and compares its current value before writing.
                cancel = entry.wired_transition != Some(WiredTransition::Seen);
                entry.wired_transition = Some(WiredTransition::Seen);
                entry.low = false;
            } else if online {
                match entry.wired_transition {
                    Some(WiredTransition::Awaiting(_)) => entry.discard_originals(),
                    Some(WiredTransition::Seen) => entry.wired_transition = None,
                    None => {}
                }
            } else if !present {
                match entry.wired_transition {
                    Some(WiredTransition::Seen) => {}
                    Some(WiredTransition::Awaiting(since))
                        if now >= since && now.saturating_sub(since) <= 120 => {}
                    None if alias.is_some() => {
                        entry.wired_transition = Some(WiredTransition::Awaiting(now));
                    }
                    _ => entry.discard_originals(),
                }
                cancel = true;
            }
            if cancel
                && let Some(p) = &self.pending
                && p.action.key() == entry.target.device.key
            {
                p.cancel.store(true, Ordering::Release);
            }
        }
    }
    /// Preserve the normal desktop rate if a confirmed game boost was active
    /// during a possible or confirmed wired charging transition.
    pub fn coordinate_charging_boost(&mut self, provider: &str, boost: &mut crate::boost::Boost) {
        for entry in self.entries.values_mut() {
            if entry.target.device.source != provider || entry.wired_transition.is_none() {
                continue;
            }
            if let Some((stamp, saver_rate, original)) = entry.boost_charging_proof() {
                match entry.wired_transition {
                    Some(WiredTransition::Awaiting(_)) => {
                        boost.hold_for_charging(&entry.target.device, stamp, saver_rate);
                    }
                    Some(WiredTransition::Seen) => {
                        if let Some(handoff) =
                            boost.handoff_for_charging(&entry.target.device, stamp, saver_rate)
                        {
                            // A new polling owner inherits the boost's startup
                            // proof. A flattened saver+boost chain requires both
                            // originals to come from restart recovery. Keep the
                            // old stamp: every retry must still verify the journal.
                            let recovered = handoff.recovered
                                && (!matches!(entry.rate, Setting::Active { .. })
                                    || entry.restore_retry[0].recovered);
                            entry.rate = Setting::Active {
                                before: original.unwrap_or(handoff.baseline.hz()),
                                applied: handoff.applied.hz(),
                            };
                            entry.rate_stamp = Some(stamp);
                            entry.restore_retry[0] = RestoreRetry {
                                recovered,
                                ..Default::default()
                            };
                        }
                    }
                    None => {}
                }
            }
        }
        boost.discard_unclaimed_charging(provider, |key| {
            self.entries.get(key).is_some_and(|entry| {
                matches!(entry.wired_transition, Some(WiredTransition::Awaiting(_)))
                    && entry.boost_charging_proof().is_some()
            })
        });
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
                    || r.level
                        .is_some_and(|level| level >= e.choices.threshold.saturating_add(5))
                {
                    e.low = false;
                }
            } else if let Some(level) = r.level
                && let Some(choices) = configured
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
                        rate: if choices.polling_rate.is_some() {
                            Setting::New
                        } else {
                            Setting::Done
                        },
                        sleep: if choices.sleep_seconds.is_some() {
                            Setting::New
                        } else {
                            Setting::Done
                        },
                        rate_stamp: None,
                        sleep_stamp: None,
                        wired_transition: None,
                        restore_retry: [RestoreRetry::default(); 2],
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
                ) || !e.restore_retry[usize::from(sleep)].ready()
                {
                    continue;
                }
                let (value, expected, restoring) = match (*state, e.low) {
                    (Setting::Active { before, applied }, false) => {
                        (Some(before), Some(applied), true)
                    }
                    (Setting::New | Setting::Ready(_), true) => {
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
                        match *state {
                            Setting::New => (None, None, false),
                            Setting::Ready(before) => (Some(desired), Some(before), false),
                            _ => unreachable!(),
                        }
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
    fn finish(&mut self, completion: Completion) -> bool {
        let Completion {
            id,
            value,
            failed,
            may_have_changed,
            connection_stamp,
            matches_target,
        } = completion;
        if !self.pending.as_ref().is_some_and(|p| p.action.id() == id) {
            return false;
        }
        let p = self.pending.take().unwrap();
        let Some(e) = self.entries.get_mut(p.action.key()) else {
            return false;
        };
        let sleep = matches!(p.action, Action::Sleep(_));
        let retry = &mut e.restore_retry[usize::from(sleep)];
        let original_still_owned = matches_target
            && connection_stamp.is_none_or(|stamp| {
                Some(stamp) == p.action.target().connection_stamp
                    || retry.recovered && crate::recovery::strong_identity(&e.target.device)
            })
            && matches!(p.old, Setting::Active { applied, .. } if value.is_none_or(|value| value == applied));
        if (p.action.target().generation != e.target.generation || p.settings_cancelled)
            && p.action.target().connection_stamp.is_some()
            && !may_have_changed
            && (!p.restoring || original_still_owned)
        {
            // Refresh/settings before SET cannot have changed the owned setting.
            // Keep the original; the next request still checks its connection.
            if !e.low && matches!(p.old, Setting::Ready(_)) {
                // The episode ended without this SET. Retire its preflight
                // now, instead of resolving inventory/Shell state just to do so.
                if sleep {
                    e.sleep = Setting::Done;
                    e.sleep_stamp = None;
                } else {
                    e.rate = Setting::Done;
                    e.rate_stamp = None;
                }
                *retry = RestoreRetry::default();
            }
            return true;
        }
        let state = if sleep { &mut e.sleep } else { &mut e.rate };
        if p.restoring
            && *state == p.old
            && matches!(p.old, Setting::Active { .. })
            && failed
            && !may_have_changed
            && p.action.target().connection_stamp.is_some()
            && original_still_owned
        {
            // Only a definitely unwritten restore can retry. Keep the original
            // journal connection proof; startup retries must repeat its probe.
            // Check deadlines on existing engine wakes, without another timer.
            // Polling and sleep pace independently so one cannot block the other.
            retry.defer();
            return true;
        }
        let desired = if sleep {
            e.choices.sleep_seconds.map(u32::from)
        } else {
            e.choices.polling_rate.map(PollingRate::hz)
        };
        let failed = failed
            || !matches_target
            || (p.action.target().connection_stamp.is_some()
                && connection_stamp != p.action.target().connection_stamp)
            || (sleep
                && value.is_some_and(|v| {
                    !hb_providers::sleep_controls::model(&e.target.device)
                        .is_some_and(|m| m.supports(v as u16))
                }));
        // Cancellation can arrive after SET and its successful readback. Keep
        // that verified original only while the same pending setting is still
        // owned; manual/disconnect revocation must never be resurrected.
        let verified_apply = !failed
            && matches!(p.old, Setting::Ready(_))
            && *state == p.old
            && p.action.target().connection_stamp.is_some()
            && connection_stamp == p.action.target().connection_stamp
            && desired.is_some()
            && value == desired;
        *state = if failed || p.cancel.load(Ordering::Acquire) && !verified_apply || p.restoring {
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
        *retry = RestoreRetry::default();
        true
    }
    pub fn polling_complete(&mut self, o: &ControlOutcome) -> bool {
        let matches_target = self.pending.as_ref().is_some_and(|p| {
            o.key == p.action.key()
                && o.observation.as_ref().is_none_or(|observed| {
                    observed.target.device == p.action.target().device
                        && observed.target.generation == p.action.target().generation
                })
        });
        self.finish(Completion {
            id: o.request,
            value: o
                .observation
                .as_ref()
                .and_then(|v| v.rate)
                .map(PollingRate::hz),
            failed: o.failure.is_some()
                || self.entries.get(&o.key).is_some_and(|e| {
                    e.choices.polling_rate.is_some_and(|wanted| {
                        o.observation
                            .as_ref()
                            .is_some_and(|observed| !observed.supported.contains(&wanted))
                    })
                }),
            may_have_changed: o.may_have_changed,
            connection_stamp: o
                .observation
                .as_ref()
                .and_then(|o| o.target.connection_stamp),
            matches_target,
        })
    }
    pub fn sleep_complete(&mut self, o: &SleepOutcome) -> bool {
        let matches_target = self.pending.as_ref().is_some_and(|p| {
            o.key == p.action.key() && o.generation == p.action.target().generation
        });
        self.finish(Completion {
            id: o.request,
            value: o.seconds.map(u32::from),
            failed: o.failure.is_some(),
            may_have_changed: o.may_have_changed,
            connection_stamp: o.connection_stamp,
            matches_target,
        })
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
    fn pending_restore(
        sleep: bool,
    ) -> (
        BatterySaver,
        Settings,
        Vec<Reading>,
        crate::boost::Boost,
        Action,
    ) {
        let (mut saver, settings, mut readings, boost) = fixture();
        activate(&mut saver, &settings, &readings, &boost);
        readings[0].level = Some(25);
        if sleep {
            let polling = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            complete(&mut saver, &polling, 1000);
        } else {
            saver.manual(&ConfigurationDevice::from_reading(&readings[0]), true);
        }
        let action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
        (saver, settings, readings, boost, action)
    }
    fn fail_restore(
        saver: &mut BatterySaver,
        action: &Action,
        changed: bool,
        value: Option<u32>,
        stamp: Option<ConnectionStamp>,
        valid_target: bool,
    ) {
        let owned = saver.pending.is_some();
        match action {
            Action::Polling(request) => {
                let mut outcome = ControlOutcome::failed(request, "Restore stopped before SET");
                outcome.may_have_changed = changed;
                if value.is_some() || stamp.is_some() || !valid_target {
                    let mut target = request.target.clone();
                    target.connection_stamp = stamp;
                    if !valid_target {
                        target.device.serial = Some("different-unit".into());
                    }
                    outcome.observation = Some(PollingObservation {
                        target,
                        supported: vec![rate(125), rate(1000)],
                        rate: value.map(rate),
                        timestamp: 100,
                        evidence: "test".into(),
                    });
                }
                assert_eq!(saver.polling_complete(&outcome), owned);
            }
            Action::Sleep(request) => {
                let mut outcome = SleepOutcome::failed(request, "Restore stopped before SET");
                outcome.may_have_changed = changed;
                outcome.seconds = value.map(|v| v as u16);
                outcome.connection_stamp = stamp;
                if !valid_target {
                    outcome.key.push_str(":different-unit");
                }
                assert_eq!(saver.sleep_complete(&outcome), owned);
            }
        }
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
        b.manual(&ConfigurationDevice::from_reading(&r[0]), false);
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
    fn charging_or_opt_out_restores_owned_settings_even_without_a_percentage() {
        for charging in [false, true] {
            let (mut saver, mut settings, mut readings, boost) = fixture();
            activate(&mut saver, &settings, &readings, &boost);
            readings[0].level = None;
            if charging {
                readings[0].charging = Some(true);
            } else {
                settings.device_mut(&readings[0].key).low_battery_mode = None;
            }
            let action = saver
                .next(&settings, 1, &readings, 100, &boost)
                .expect("Unknown percentage must not prevent proven charging/opt-out restoration");
            let Action::Polling(request) = &action else {
                panic!("Expected rate restoration")
            };
            assert_eq!(
                request.action,
                ControlAction::ApplyIf {
                    rate: rate(1000),
                    expected: rate(125)
                }
            );
            assert!(request.target.connection_stamp.is_some());
            complete(&mut saver, &action, 1000);
            let action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            let Action::Sleep(request) = &action else {
                panic!("Expected sleep restoration")
            };
            assert_eq!((request.seconds, request.expected), (Some(300), Some(60)));
            assert!(request.target.connection_stamp.is_some());
            complete(&mut saver, &action, 300);
            assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
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
                b.manual(&ConfigurationDevice::from_reading(&r[0]), false);
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
                b.observe_provider("razer", &r, 100);
                r[0].connection = Connection::Online;
            }
            r[0].charging = Some(true);
            assert!(b.next(&s, generation, &r, 100, &boost).is_none());
        }
    }
    fn paired_charging_fixture() -> (BatterySaver, Settings, Vec<Reading>, crate::boost::Boost) {
        let (mut saver, settings, mut readings, boost) = fixture();
        readings[0].serial = Some("TEST".into());
        activate(&mut saver, &settings, &readings, &boost);
        (saver, settings, readings, boost)
    }
    fn wired_reading(wireless: &Reading, timestamp: i64) -> Reading {
        let mut wired = wireless.clone();
        wired.key = "razer:00be:TEST".into();
        wired.container = Some("WIRED-COLLECTION".into());
        wired.charging = Some(true);
        wired.timestamp = timestamp;
        wired
    }
    #[test]
    fn charging_on_usb_restores_only_through_the_proven_wireless_connection() {
        for gap in [false, true] {
            let (mut saver, settings, mut readings, boost) = paired_charging_fixture();
            if gap {
                saver.observe_provider("razer", &[], 100);
                assert!(
                    !saver.needs_action(),
                    "Waiting must not trigger device work"
                );
            }
            let wired = wired_reading(&readings[0], 101);
            let inventory = if gap {
                vec![wired]
            } else {
                vec![readings[0].clone(), wired]
            };
            saver.observe_provider("razer", &inventory, 101);
            assert!(!saver.needs_action());
            assert!(!saver.ready(&boost, 101, |_| &inventory));
            assert!(saver.next(&settings, 2, &inventory, 101, &boost).is_none());

            readings[0].level = Some(100);
            readings[0].timestamp = 102;
            saver.observe_provider("razer", &readings, 102);
            let rate_action = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
            assert!(matches!(&rate_action, Action::Polling(q)
                if q.target.device.key == readings[0].key
                    && q.target.connection_stamp == ConnectionStamp::new(1)
                    && q.action == ControlAction::ApplyIf { rate: rate(1000), expected: rate(125) }));
            complete(&mut saver, &rate_action, 1000);
            let sleep = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
            assert!(matches!(&sleep, Action::Sleep(q)
                if q.target.device.key == readings[0].key
                    && q.target.connection_stamp == ConnectionStamp::new(1)
                    && q.seconds == Some(300) && q.expected == Some(60)));
            complete(&mut saver, &sleep, 300);
            assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
        }
    }
    #[test]
    fn polling_only_saver_restores_after_charging() {
        let (mut saver, mut settings, mut readings, boost) = fixture();
        readings[0].serial = Some("TEST".into());
        settings
            .device_mut(&readings[0].key)
            .low_battery_mode
            .as_mut()
            .unwrap()
            .sleep_seconds = None;
        for value in [1000, 125] {
            let action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            complete(&mut saver, &action, value);
        }
        let wired = wired_reading(&readings[0], 101);
        saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
        assert!(!saver.needs_action());
        readings[0].level = Some(100);
        readings[0].timestamp = 102;
        saver.observe_provider("razer", &readings, 102);
        let action = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
        assert!(matches!(&action, Action::Polling(q)
            if q.target.connection_stamp == ConnectionStamp::new(1)
                && q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(125)}));
        complete(&mut saver, &action, 1000);
        assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
    }
    #[test]
    fn disconnect_without_timely_unambiguous_charging_evidence_never_restores() {
        for case in 0..4 {
            let (mut saver, settings, mut readings, boost) = paired_charging_fixture();
            saver.observe_provider("razer", &[], 100);
            let mut wired = wired_reading(&readings[0], 101);
            match case {
                0 => {} // A normal wireless reconnect is not evidence of USB charging.
                1 => {
                    wired.key = "razer:00be:OTHER".into();
                    wired.serial = Some("OTHER".into());
                    saver.observe_provider("razer", &[wired], 101);
                }
                2 => {
                    let mut duplicate = wired.clone();
                    duplicate.container = Some("OTHER-COLLECTION".into());
                    saver.observe_provider("razer", &[wired, duplicate], 101);
                }
                _ => {
                    wired.timestamp = 221;
                    saver.observe_provider("razer", &[wired], 221);
                }
            }
            let now = if case == 3 { 222 } else { 102 };
            readings[0].level = Some(100);
            readings[0].timestamp = now;
            saver.observe_provider("razer", &readings, now);
            assert!(
                saver.next(&settings, 2, &readings, now, &boost).is_none(),
                "case {case}"
            );
        }
    }
    #[test]
    fn manual_wired_sleep_change_wins_over_the_pending_wireless_restore() {
        let (mut saver, settings, mut readings, boost) = paired_charging_fixture();
        let wired = wired_reading(&readings[0], 101);
        saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
        saver.manual(&ConfigurationDevice::from_reading(&wired), true);
        readings[0].level = Some(100);
        readings[0].timestamp = 102;
        saver.observe_provider("razer", &readings, 102);
        let rate = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
        assert!(matches!(rate, Action::Polling(_)));
        complete(&mut saver, &rate, 1000);
        assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
    }
    #[test]
    fn manual_wired_polling_change_wins_over_the_pending_wireless_restore() {
        let (mut saver, settings, mut readings, boost) = paired_charging_fixture();
        let wired = wired_reading(&readings[0], 101);
        saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
        saver.manual(&ConfigurationDevice::from_reading(&wired), false);
        readings[0].level = Some(100);
        readings[0].timestamp = 102;
        saver.observe_provider("razer", &readings, 102);
        let sleep = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
        assert!(
            matches!(&sleep, Action::Sleep(q) if q.seconds == Some(300) && q.expected == Some(60))
        );
        complete(&mut saver, &sleep, 300);
        assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
    }
    #[test]
    fn charging_during_game_boost_restores_the_original_desktop_rate() {
        for scenario in 0..3 {
            let (mut saver, mut settings, mut readings, mut boost) = paired_charging_fixture();
            settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(2000));
            let now = Instant::now();
            boost.sample(now, Some(true));
            boost.sample(now + Duration::from_secs(10), Some(true));
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
            let read = boost.next(&settings, 1, &readings).unwrap();
            boost.complete(&outcome(&read, 125));
            let apply = boost.next(&settings, 1, &readings).unwrap();
            boost.complete(&outcome(&apply, 2000));

            if scenario != 0 {
                saver.observe_provider("razer", &[], 100);
                saver.coordinate_charging_boost("razer", &mut boost);
                boost.observe_provider("razer", &[], 100);
                boost.sample(now + Duration::from_secs(20), Some(false));
                boost.sample(now + Duration::from_secs(35), Some(false));
                assert!(boost.owns(&readings[0].key));
                assert!(!boost.ready(&settings, |_| &[]));
                assert!(boost.next(&settings, 2, &[]).is_none());
            }
            if scenario == 2 {
                readings[0].level = Some(100);
                readings[0].timestamp = 101;
                saver.observe_provider("razer", &readings, 101);
                saver.coordinate_charging_boost("razer", &mut boost);
                boost.observe_provider("razer", &readings, 101);
                assert!(!boost.owns(&readings[0].key));
                assert!(saver.next(&settings, 2, &readings, 101, &boost).is_none());
                continue;
            }
            let wired = wired_reading(&readings[0], 101);
            saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
            saver.coordinate_charging_boost("razer", &mut boost);
            assert!(!boost.owns(&readings[0].key));
            boost.observe_provider("razer", std::slice::from_ref(&wired), 101);
            assert!(!saver.needs_action());
            readings[0].level = Some(100);
            readings[0].timestamp = 102;
            saver.observe_provider("razer", &readings, 102);
            boost.sample(now + Duration::from_secs(20), Some(false));
            boost.sample(now + Duration::from_secs(35), Some(false));
            let rate_action = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
            assert!(matches!(&rate_action, Action::Polling(q)
            if q.target.connection_stamp == ConnectionStamp::new(1)
                && q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(2000)}));
            complete(&mut saver, &rate_action, 1000);
            let sleep = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
            assert!(
                matches!(&sleep, Action::Sleep(q) if q.seconds == Some(300) && q.expected == Some(60))
            );
            complete(&mut saver, &sleep, 300);
            assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
        }
    }
    #[test]
    fn verified_saver_apply_retains_its_original_after_cancellation() {
        for sleep in [false, true] {
            for refresh in [false, true] {
                let (mut saver, mut settings, mut readings, boost) = fixture();
                settings.device_mut(&readings[0].key).low_battery_mode = Some(LowBatteryMode {
                    threshold: 20,
                    polling_rate: (!sleep).then(|| rate(125)),
                    sleep_seconds: sleep.then_some(60),
                });
                let read = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                complete(&mut saver, &read, if sleep { 300 } else { 1000 });
                let apply = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                if refresh {
                    saver.observe_generation(2);
                } else {
                    saver.settings_changed();
                }
                complete(&mut saver, &apply, if sleep { 60 } else { 125 });
                let entry = &saver.entries[&readings[0].key];
                assert!(
                    matches!(
                        if sleep { entry.sleep } else { entry.rate },
                        Setting::Active { .. }
                    ),
                    "A verified SET must retain its original even if its completion was cancelled"
                );
                readings[0].level = Some(100);
                let restore = saver
                    .next(
                        &settings,
                        if refresh { 2 } else { 1 },
                        &readings,
                        100,
                        &boost,
                    )
                    .unwrap();
                assert_eq!(restore.target().connection_stamp, ConnectionStamp::new(1));
                match restore {
                    Action::Polling(request) => assert_eq!(
                        request.action,
                        ControlAction::ApplyIf {
                            rate: rate(1000),
                            expected: rate(125)
                        }
                    ),
                    Action::Sleep(request) => {
                        assert_eq!((request.seconds, request.expected), (Some(300), Some(60)))
                    }
                }
            }
        }
    }
    #[test]
    fn charging_during_saver_apply_keeps_only_a_verified_original() {
        for sleep in [false, true] {
            for gap in [false, true] {
                for case in 0..7 {
                    let (mut saver, mut settings, mut readings, boost) = fixture();
                    readings[0].serial = Some("TEST".into());
                    settings.device_mut(&readings[0].key).low_battery_mode = Some(LowBatteryMode {
                        threshold: 20,
                        polling_rate: (!sleep).then(|| rate(125)),
                        sleep_seconds: sleep.then_some(60),
                    });
                    let read = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                    complete(&mut saver, &read, if sleep { 300 } else { 1000 });
                    let apply = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                    if gap || case == 6 {
                        saver.observe_provider("razer", &[], 100);
                    }
                    let wired = wired_reading(&readings[0], 101);
                    if case == 5 {
                        let mut duplicate = wired.clone();
                        duplicate.container = Some("DUPLICATE-COLLECTION".into());
                        saver.observe_provider("razer", &[wired.clone(), duplicate], 101);
                    } else if case != 6 {
                        saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
                    }
                    saver.observe_generation(2);
                    if case == 3 {
                        saver.manual(&ConfigurationDevice::from_reading(&readings[0]), sleep);
                    }
                    if case == 4 {
                        match &apply {
                            Action::Polling(request) => {
                                saver.polling_complete(&ControlOutcome {
                                    request: request.request,
                                    key: request.target.device.key.clone(),
                                    previous: Some(rate(1000)),
                                    may_have_changed: true,
                                    failure: None,
                                    observation: Some(PollingObservation {
                                        target: ControlTarget {
                                            connection_stamp: ConnectionStamp::new(2),
                                            ..request.target.clone()
                                        },
                                        rate: Some(rate(125)),
                                        supported: vec![rate(125), rate(1000)],
                                        timestamp: 101,
                                        evidence: "test".into(),
                                    }),
                                });
                            }
                            Action::Sleep(request) => {
                                saver.sleep_complete(&SleepOutcome {
                                    request: request.request,
                                    key: request.target.device.key.clone(),
                                    generation: 1,
                                    connection_stamp: ConnectionStamp::new(2),
                                    seconds: Some(60),
                                    may_have_changed: true,
                                    failure: None,
                                });
                            }
                        }
                    } else if !matches!(case, 1 | 2) {
                        complete(&mut saver, &apply, if sleep { 60 } else { 125 });
                    } else {
                        fail_restore(&mut saver, &apply, case == 1, None, None, true);
                    }
                    assert!(saver.next(&settings, 2, &[wired], 101, &boost).is_none());
                    readings[0].timestamp = 102;
                    readings[0].level = Some(100);
                    saver.observe_provider("razer", &readings, 102);
                    let restore = saver.next(&settings, 2, &readings, 102, &boost);
                    if case == 0 {
                        let restore =
                            restore.expect("A confirmed saver SET must survive USB charging");
                        assert_eq!(restore.target().connection_stamp, ConnectionStamp::new(1));
                        match restore {
                            Action::Polling(request) => assert_eq!(
                                request.action,
                                ControlAction::ApplyIf {
                                    rate: rate(1000),
                                    expected: rate(125)
                                }
                            ),
                            Action::Sleep(request) => assert_eq!(
                                (request.seconds, request.expected),
                                (Some(300), Some(60))
                            ),
                        }
                    } else {
                        assert!(restore.is_none());
                    }
                }
            }
        }
    }
    #[test]
    fn ended_saver_episode_retires_a_cancelled_unwritten_apply_without_idle_work() {
        for sleep in [false, true] {
            let (mut saver, mut settings, mut readings, boost) = fixture();
            settings.device_mut(&readings[0].key).low_battery_mode = Some(LowBatteryMode {
                threshold: 20,
                polling_rate: (!sleep).then(|| rate(125)),
                sleep_seconds: sleep.then_some(60),
            });
            let read = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            complete(&mut saver, &read, if sleep { 300 } else { 1000 });
            let apply = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            readings[0].level = Some(100);
            saver.observe(&settings, 1, &readings, 100);
            saver.settings_changed();
            fail_restore(&mut saver, &apply, false, None, None, true);
            assert!(!saver.ready(&boost, 100, |_| panic!(
                "An unwritten ended episode needs no inventory"
            )));
        }
    }
    #[test]
    fn recovered_boost_handoff_preserves_restart_proof_for_sleep_only_saver() {
        let (mut saver, mut settings, mut readings, mut boost) = fixture();
        let reading = &mut readings[0];
        reading.serial = Some("TEST".into());
        reading.container = Some("00112233445566778899aabbccddeeff".into());
        let choices = LowBatteryMode {
            polling_rate: None,
            threshold: 20,
            sleep_seconds: Some(60),
        };
        settings.device_mut(&reading.key).low_battery_mode = Some(choices);
        settings.device_mut(&reading.key).fullscreen_boost_rate = Some(rate(4000));
        let sleep = crate::recovery::Record {
            device: ConfigurationDevice::from_reading(reading),
            stamp: ConnectionStamp::new(1).unwrap(),
            rebound_stamp: None,
            before: 300,
            applied: 60,
            layer: crate::recovery::Layer::SaverSleep(choices),
            confirmed: true,
            manual_pending: false,
        };
        let polling = crate::recovery::Record {
            before: 1000,
            applied: 4000,
            layer: crate::recovery::Layer::BoostPolling,
            ..sleep.clone()
        };
        assert!(saver.recover(&sleep, reading, &settings, 1));
        assert!(boost.recover(&polling, reading, &settings, 1));
        let wired = wired_reading(reading, 101);
        saver.observe_provider("razer", &[wired], 101);
        saver.coordinate_charging_boost("razer", &mut boost);
        assert!(!boost.owns(&reading.key));
        reading.timestamp = 102;
        reading.level = Some(100);
        saver.observe_provider("razer", &readings, 102);
        let restore = saver.next(&settings, 1, &readings, 102, &boost).unwrap();
        fail_restore(
            &mut saver,
            &restore,
            false,
            Some(4000),
            ConnectionStamp::new(2),
            true,
        );
        let entry = saver.entries.get_mut(&readings[0].key).unwrap();
        assert_eq!(
            entry.rate,
            Setting::Active {
                before: 1000,
                applied: 4000
            },
            "A verified restart rebind must not lose the handed-off desktop original"
        );
        assert!(entry.restore_retry[0].after.is_some());
        entry.restore_retry[0].after = Some(Instant::now() - Duration::from_secs(1));
        let retry = saver.next(&settings, 1, &readings, 102, &boost).unwrap();
        assert_eq!(retry.target().connection_stamp, ConnectionStamp::new(1));
    }
    #[test]
    fn flattened_charging_recovery_requires_restart_proof_from_both_polling_layers() {
        for saver_recovered in [false, true] {
            for boost_recovered in [false, true] {
                let (mut saver, mut settings, mut readings, mut boost) = fixture();
                readings[0].serial = Some("TEST".into());
                readings[0].container = Some("00112233445566778899aabbccddeeff".into());
                let choices = settings
                    .device(&readings[0].key)
                    .unwrap()
                    .low_battery_mode
                    .unwrap();
                settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(4000));
                let saver_record = crate::recovery::Record {
                    device: ConfigurationDevice::from_reading(&readings[0]),
                    stamp: ConnectionStamp::new(1).unwrap(),
                    rebound_stamp: None,
                    before: 1000,
                    applied: 125,
                    layer: crate::recovery::Layer::SaverPolling(choices),
                    confirmed: true,
                    manual_pending: false,
                };
                if saver_recovered {
                    assert!(saver.recover(&saver_record, &readings[0], &settings, 1));
                } else {
                    activate(&mut saver, &settings, &readings, &boost);
                }
                if boost_recovered {
                    let record = crate::recovery::Record {
                        before: 125,
                        applied: 4000,
                        layer: crate::recovery::Layer::BoostPolling,
                        ..saver_record
                    };
                    assert!(boost.recover(&record, &readings[0], &settings, 1));
                } else {
                    let now = Instant::now();
                    boost.sample(now, Some(true));
                    boost.sample(now + Duration::from_secs(10), Some(true));
                    for value in [125, 4000] {
                        let request = boost.next(&settings, 1, &readings).unwrap();
                        assert!(boost.complete(&ControlOutcome {
                            request: request.request,
                            key: request.target.device.key.clone(),
                            previous: Some(rate(125)),
                            may_have_changed: request.action.rate().is_some(),
                            failure: None,
                            observation: Some(PollingObservation {
                                target: ControlTarget {
                                    connection_stamp: ConnectionStamp::new(1),
                                    ..request.target
                                },
                                supported: vec![rate(125), rate(1000), rate(4000)],
                                rate: Some(rate(value)),
                                timestamp: 100,
                                evidence: "test".into(),
                            }),
                        }));
                    }
                }
                let wired = wired_reading(&readings[0], 101);
                saver.observe_provider("razer", &[wired], 101);
                saver.coordinate_charging_boost("razer", &mut boost);
                readings[0].timestamp = 102;
                readings[0].level = Some(100);
                saver.observe_provider("razer", &readings, 102);
                let restore = saver.next(&settings, 1, &readings, 102, &boost).unwrap();
                fail_restore(
                    &mut saver,
                    &restore,
                    false,
                    Some(4000),
                    ConnectionStamp::new(2),
                    true,
                );
                let entry = &saver.entries[&readings[0].key];
                assert_eq!(
                    matches!(
                        entry.rate,
                        Setting::Active {
                            before: 1000,
                            applied: 4000
                        }
                    ),
                    saver_recovered && boost_recovered
                );
                assert_eq!(
                    entry.restore_retry[0].after.is_some(),
                    saver_recovered && boost_recovered
                );
            }
        }
    }
    #[test]
    fn sleep_only_saver_preserves_game_boost_restoration_during_usb_charging() {
        for scenario in 0..4 {
            let (mut saver, mut settings, mut readings, mut boost) = fixture();
            readings[0].serial = Some("TEST".into());
            let preference = settings.device_mut(&readings[0].key);
            preference.low_battery_mode.as_mut().unwrap().polling_rate = None;
            preference.fullscreen_boost_rate = Some(rate(2000));
            for value in [300, 60] {
                let action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                assert!(matches!(action, Action::Sleep(_)));
                complete(&mut saver, &action, value);
            }
            let now = Instant::now();
            boost.sample(now, Some(true));
            boost.sample(now + Duration::from_secs(10), Some(true));
            let outcome = |q: &ControlRequest, hz| ControlOutcome {
                request: q.request,
                key: q.target.device.key.clone(),
                previous: Some(rate(1000)),
                may_have_changed: true,
                failure: None,
                observation: Some(PollingObservation {
                    target: ControlTarget {
                        connection_stamp: ConnectionStamp::new(1),
                        ..q.target.clone()
                    },
                    supported: vec![rate(1000), rate(2000)],
                    rate: Some(rate(hz)),
                    timestamp: 100,
                    evidence: "test".into(),
                }),
            };
            let read = boost.next(&settings, 1, &readings).unwrap();
            boost.complete(&outcome(&read, 1000));
            let apply = boost.next(&settings, 1, &readings).unwrap();
            boost.complete(&outcome(&apply, 2000));
            if scenario == 1 || scenario == 3 {
                saver.observe_provider("razer", &[], 100);
                saver.coordinate_charging_boost("razer", &mut boost);
                boost.observe_provider("razer", &[], 100);
            }
            let wired = wired_reading(&readings[0], 101);
            if scenario == 3 {
                let manual = ConfigurationDevice::from_reading(&wired);
                saver.manual(&manual, false);
                boost.manual_device(&manual);
            }
            saver.observe_provider("razer", std::slice::from_ref(&wired), 101);
            saver.coordinate_charging_boost("razer", &mut boost);
            boost.observe_provider("razer", std::slice::from_ref(&wired), 101);
            if scenario == 2 {
                let manual = ConfigurationDevice::from_reading(&wired);
                saver.manual(&manual, false);
                boost.manual_device(&manual);
            }
            assert!(!saver.needs_action());
            readings[0].level = Some(100);
            readings[0].timestamp = 102;
            saver.observe_provider("razer", &readings, 102);
            boost.sample(now + Duration::from_secs(20), Some(false));
            boost.sample(now + Duration::from_secs(35), Some(false));
            if scenario < 2 {
                let rate_action = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
                assert!(matches!(&rate_action, Action::Polling(q)
                    if q.target.connection_stamp == ConnectionStamp::new(1)
                        && q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(2000)}));
                complete(&mut saver, &rate_action, 1000);
            }
            let sleep = saver.next(&settings, 2, &readings, 102, &boost).unwrap();
            assert!(
                matches!(&sleep, Action::Sleep(q) if q.seconds == Some(300) && q.expected == Some(60))
            );
            complete(&mut saver, &sleep, 300);
            assert!(saver.next(&settings, 2, &readings, 102, &boost).is_none());
        }
    }
    #[test]
    fn suspend_preserves_confirmed_originals_but_not_uncertain_operations() {
        for pending in [false, true] {
            let (mut saver, settings, mut readings, boost) = paired_charging_fixture();
            readings[0].level = Some(25);
            let inflight =
                pending.then(|| saver.next(&settings, 1, &readings, 100, &boost).unwrap());
            let guard = inflight.as_ref().and_then(|a| saver.cancel(a.id()));
            saver.suspend();
            saver.suspend();
            if let Some(guard) = guard {
                assert!(guard.load(Ordering::Acquire));
            }
            if !pending {
                let rate_action = saver.next(&settings, 2, &readings, 100, &boost).unwrap();
                assert!(matches!(&rate_action, Action::Polling(q)
                    if q.target.connection_stamp == ConnectionStamp::new(1)
                        && q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(125)}));
                complete(&mut saver, &rate_action, 1000);
            }
            let sleep = saver.next(&settings, 2, &readings, 100, &boost).unwrap();
            assert!(matches!(&sleep, Action::Sleep(q)
                if q.target.connection_stamp == ConnectionStamp::new(1)
                    && q.seconds == Some(300) && q.expected == Some(60)));
            complete(&mut saver, &sleep, 300);
            assert!(saver.next(&settings, 2, &readings, 100, &boost).is_none());
        }
    }
    #[test]
    fn resumed_game_boost_restores_saver_rate_before_normal_polling_rate() {
        let (mut saver, mut settings, mut readings, mut boost) = paired_charging_fixture();
        settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(2000));
        let now = Instant::now();
        boost.sample(now, Some(true));
        boost.sample(now + Duration::from_secs(10), Some(true));
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
        let read = boost.next(&settings, 1, &readings).unwrap();
        boost.complete(&outcome(&read, 125));
        let apply = boost.next(&settings, 1, &readings).unwrap();
        boost.complete(&outcome(&apply, 2000));
        boost.suspend();
        saver.suspend();
        assert!(boost.owns(&readings[0].key));
        readings[0].level = Some(100);
        boost.sample(now + Duration::from_secs(30), Some(false));
        boost.sample(now + Duration::from_secs(45), Some(false));
        let boost_restore = boost.next(&settings, 2, &readings).unwrap();
        assert_eq!(
            boost_restore.action,
            ControlAction::ApplyIf {
                rate: rate(125),
                expected: rate(2000)
            }
        );
        boost.complete(&outcome(&boost_restore, 125));
        let saver_restore = saver.next(&settings, 2, &readings, 100, &boost).unwrap();
        assert!(matches!(&saver_restore, Action::Polling(q)
            if q.action == ControlAction::ApplyIf {rate: rate(1000), expected: rate(125)}));
        complete(&mut saver, &saver_restore, 1000);
        let sleep = saver.next(&settings, 2, &readings, 100, &boost).unwrap();
        complete(&mut saver, &sleep, 300);
        assert!(saver.next(&settings, 2, &readings, 100, &boost).is_none());
    }
    #[test]
    fn unrelated_refreshes_preserve_both_originals_and_connection_proofs() {
        let (mut b, s, mut r, boost) = fixture();
        activate(&mut b, &s, &r, &boost);
        let mut other = r[0].clone();
        other.key.push_str(":other-unit");
        b.observe_provider("razer", &[r[0].clone(), other], 100);
        b.observe_provider("razer", &r, 100);
        for generation in 2..=20 {
            b.observe_provider("logitech", &[], 100);
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
    fn unwritten_restore_failures_keep_the_original_polling_rate_and_sleep_time() {
        for sleep in [false, true] {
            let (mut saver, settings, mut readings, boost) = fixture();
            activate(&mut saver, &settings, &readings, &boost);
            readings[0].level = Some(25);
            if sleep {
                let polling = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                complete(&mut saver, &polling, 1000);
            }
            let restore = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            match &restore {
                Action::Polling(request) => {
                    saver.polling_complete(&ControlOutcome::failed(
                        request,
                        "GET failed before SET",
                    ));
                }
                Action::Sleep(request) => {
                    saver
                        .sleep_complete(&SleepOutcome::failed(request, "Recovery storage is busy"));
                }
            }
            let entry = &saver.entries[&readings[0].key];
            let (state, original, applied) = if sleep {
                (entry.sleep, 300, 60)
            } else {
                (entry.rate, 1000, 125)
            };
            assert_eq!(
                state,
                Setting::Active {
                    before: original,
                    applied
                },
                "A pre-SET failure must not discard the original setting"
            );
            // A polling failure must still allow the independent sleep restore.
            let next = saver.next(&settings, 1, &readings, 100, &boost);
            if sleep {
                assert!(next.is_none());
            } else {
                let next = next.unwrap();
                assert!(matches!(next, Action::Sleep(_)));
                complete(&mut saver, &next, 300);
            }
            assert!(!saver.ready(&boost, 100, |_| &readings));
        }
    }
    #[test]
    fn cancellation_does_not_hide_a_readback_that_revokes_ownership() {
        for sleep in [false, true] {
            let (mut saver, settings, readings, boost, action) = pending_restore(sleep);
            saver.settings_changed();
            fail_restore(
                &mut saver,
                &action,
                false,
                Some(if sleep { 300 } else { 1000 }),
                ConnectionStamp::new(1),
                true,
            );
            assert!(
                !saver.needs_action(),
                "A cancelled restore must respect a setting already restored elsewhere"
            );
            assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
        }
    }
    #[test]
    fn restoration_backoff_is_bounded_monotonic_and_allocation_free_while_waiting() {
        for sleep in [false, true] {
            let (mut saver, settings, readings, boost, mut action) = pending_restore(sleep);
            for (attempt, seconds) in [30, 60, 120, 300, 300].into_iter().enumerate() {
                let started = Instant::now();
                fail_restore(&mut saver, &action, false, None, None, true);
                let retry = &saver.entries[&readings[0].key].restore_retry[usize::from(sleep)];
                assert_eq!(retry.failures, (attempt as u8 + 1).min(4));
                let deadline = retry.after.unwrap();
                assert!(deadline >= started + Duration::from_secs(seconds));
                assert!(deadline <= Instant::now() + Duration::from_secs(seconds));
                let (_, allocations) = crate::allocation_counter::measure(|| {
                    for now in [0, 100, 100_000, i64::MAX] {
                        for _ in 0..250 {
                            assert!(!saver.ready(&boost, now, |_| panic!(
                                "Waiting must not resolve inventory"
                            )));
                        }
                    }
                });
                assert_eq!(allocations.calls, 0);
                assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
                saver
                    .entries
                    .get_mut(&readings[0].key)
                    .unwrap()
                    .restore_retry[usize::from(sleep)]
                .after = Some(Instant::now() - Duration::from_secs(1));
                action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
                assert_eq!(action.target().connection_stamp, ConnectionStamp::new(1));
                match &action {
                    Action::Polling(request) => assert_eq!(
                        request.action,
                        ControlAction::ApplyIf {
                            rate: rate(1000),
                            expected: rate(125)
                        }
                    ),
                    Action::Sleep(request) => {
                        assert_eq!((request.seconds, request.expected), (Some(300), Some(60)))
                    }
                }
            }
            complete(&mut saver, &action, if sleep { 300 } else { 1000 });
            assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
        }
    }
    #[test]
    fn unsafe_restore_evidence_and_manual_changes_never_rearm_a_retry() {
        for sleep in [false, true] {
            for case in 0..6 {
                let (mut saver, settings, readings, boost, action) = pending_restore(sleep);
                if case == 3 {
                    saver.manual(&ConfigurationDevice::from_reading(&readings[0]), sleep);
                }
                if case == 4 {
                    saver.pending.as_mut().unwrap().action = match action.clone() {
                        Action::Polling(mut request) => {
                            request.target.connection_stamp = None;
                            Action::Polling(request)
                        }
                        Action::Sleep(mut request) => {
                            request.target.connection_stamp = None;
                            Action::Sleep(request)
                        }
                    };
                }
                fail_restore(
                    &mut saver,
                    &action,
                    case == 0,
                    (case == 2).then_some(if sleep { 120 } else { 2000 }),
                    ConnectionStamp::new(if case == 1 { 2 } else { 1 }),
                    case != 5,
                );
                let entry = &saver.entries[&readings[0].key];
                assert_eq!(
                    if sleep { entry.sleep } else { entry.rate },
                    Setting::Done,
                    "unsafe restore case {case}"
                );
                assert!(!saver.needs_action());
                assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
            }
        }
    }
    #[test]
    fn recovered_retry_keeps_the_original_connection_proof_after_a_verified_rebind() {
        for sleep in [false, true] {
            let (mut saver, settings, mut readings, boost) = fixture();
            readings[0].serial = Some("TEST".into());
            readings[0].container = Some("00112233445566778899aabbccddeeff".into());
            readings[0].level = Some(100);
            let choices = settings
                .device(&readings[0].key)
                .unwrap()
                .low_battery_mode
                .unwrap();
            let record = crate::recovery::Record {
                device: ConfigurationDevice::from_reading(&readings[0]),
                stamp: ConnectionStamp::new(1).unwrap(),
                rebound_stamp: None,
                before: if sleep { 300 } else { 1000 },
                applied: if sleep { 60 } else { 125 },
                layer: if sleep {
                    crate::recovery::Layer::SaverSleep(choices)
                } else {
                    crate::recovery::Layer::SaverPolling(choices)
                },
                confirmed: true,
                manual_pending: false,
            };
            assert!(crate::recovery::strong_identity(&record.device));
            assert!(saver.recover(&record, &readings[0], &settings, 1));
            let action = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            fail_restore(
                &mut saver,
                &action,
                false,
                Some(if sleep { 60 } else { 125 }),
                ConnectionStamp::new(2),
                true,
            );
            let entry = saver.entries.get_mut(&readings[0].key).unwrap();
            assert!(entry.restore_retry[usize::from(sleep)].after.is_some());
            entry.restore_retry[usize::from(sleep)].after =
                Some(Instant::now() - Duration::from_secs(1));
            let retry = saver.next(&settings, 1, &readings, 100, &boost).unwrap();
            assert_eq!(retry.target().connection_stamp, ConnectionStamp::new(1));
        }
    }
    #[test]
    fn waiting_restore_respects_new_games_reconnects_and_manual_choices() {
        for case in 0..4 {
            let (mut saver, mut settings, mut readings, mut boost, action) = pending_restore(false);
            fail_restore(&mut saver, &action, false, None, None, true);
            match case {
                0 => saver.manual(&ConfigurationDevice::from_reading(&readings[0]), false),
                1 => {
                    readings[0].serial = Some("different-unit".into());
                    saver.observe_provider("razer", &readings, 100);
                }
                2 => saver.observe_provider("razer", &[], 100),
                _ => {
                    settings.device_mut(&readings[0].key).fullscreen_boost_rate = Some(rate(2000));
                    let now = Instant::now();
                    boost.sample(now, Some(true));
                    boost.sample(now + Duration::from_secs(10), Some(true));
                    assert!(boost.next(&settings, 1, &readings).is_some());
                }
            }
            saver
                .entries
                .get_mut(&readings[0].key)
                .unwrap()
                .restore_retry[0]
                .after = Some(Instant::now() - Duration::from_secs(1));
            assert!(!saver.ready(&boost, 100, |_| &readings));
            assert!(saver.next(&settings, 1, &readings, 100, &boost).is_none());
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
                b.observe_provider("razer", &[], 100);
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
