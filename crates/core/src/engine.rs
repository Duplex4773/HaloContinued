use crate::{Connection, Estimator, PollResult, Precision, Reading, Settings};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceView {
    pub reading: Reading,
    pub name: String,
    pub icon: String,
    pub low_alert_at: u8,
    pub seconds_left: Option<u64>,
    pub text: String,
    pub hidden: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub devices: Vec<DeviceView>,
    pub errors: BTreeMap<String, String>,
    pub timestamp: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationKind {
    Low,
    Full,
}
#[derive(Clone, Debug)]
pub struct Notification {
    pub key: String,
    pub kind: NotificationKind,
    pub title: String,
    pub text: String,
}
#[derive(Default)]
struct AlertState {
    low: bool,
    full: u8,
    threshold: Option<u8>,
}
pub struct Engine {
    pub settings: Settings,
    pub estimator: Estimator,
    by_provider: BTreeMap<String, Vec<Reading>>,
    misses: BTreeMap<String, u8>,
    alerts: BTreeMap<String, AlertState>,
    held: BTreeMap<(String, NotificationKind), Notification>,
    errors: BTreeMap<String, String>,
    sound_at: BTreeMap<String, f64>,
    pending_sound: Option<u8>,
    suspended: bool,
}
impl Engine {
    pub fn new(settings: Settings, mut estimator: Estimator) -> Self {
        estimator.pause_all();
        Self {
            settings,
            estimator,
            by_provider: BTreeMap::new(),
            misses: BTreeMap::new(),
            alerts: BTreeMap::new(),
            held: BTreeMap::new(),
            errors: BTreeMap::new(),
            sound_at: BTreeMap::new(),
            pending_sound: None,
            suspended: false,
        }
    }
    pub fn suspend(&mut self) {
        self.suspended = true;
        self.pending_sound = None;
        self.estimator.pause_all();
        for r in self.by_provider.values_mut().flatten() {
            r.connection = Connection::Sleeping;
        }
    }
    /// Hardware rate changes invalidate the learned discharge slope, never history.
    /// Clear only proven aliases, never another unit sharing a vendor serial.
    pub fn reset_estimate(&mut self, key: &str) {
        let all = self.by_provider.values().flatten();
        let selected = all.clone().find(|r| r.key == key);
        let serial = selected
            .and_then(|r| r.serial.as_deref())
            .and_then(trusted_identity);
        let keys = all
            .clone()
            .filter(|r| {
                r.key == key
                    || serial.as_ref().is_some_and(|s| {
                        r.serial
                            .as_deref()
                            .and_then(trusted_identity)
                            .is_some_and(|other| other.eq_ignore_ascii_case(s))
                            && (self.settings.preference_key(&r.key)
                                == self.settings.preference_key(key)
                                || selected.is_some_and(|target| {
                                    preferred_duplicate(r, std::iter::once(target))
                                        && preferred_duplicate(
                                            r,
                                            all.clone().filter(|candidate| {
                                                candidate.source == target.source
                                                    && candidate
                                                        .serial
                                                        .as_deref()
                                                        .and_then(trusted_identity)
                                                        .is_some_and(|unit| {
                                                            unit.eq_ignore_ascii_case(s)
                                                        })
                                            }),
                                        )
                                }))
                    })
            })
            .map(|r| r.key.clone())
            .collect::<Vec<_>>();
        self.estimator.devices.remove(key);
        for key in keys {
            self.estimator.devices.remove(&key);
        }
    }
    pub fn resume(&mut self) {
        self.suspended = false;
        self.estimator.pause_all();
        for r in self.by_provider.values_mut().flatten() {
            r.connection = Connection::Stale;
        }
    }
    /// Delivery belongs to the UI. A failed delivery is retried only while relevant.
    pub fn notification_failed(&mut self, notification: Notification) {
        self.held
            .insert((notification.key.clone(), notification.kind), notification);
    }
    /// An unchanged empty provider cannot alter a published device/error view.
    pub fn provider_has_snapshot_data(&self, provider: &str) -> bool {
        self.by_provider
            .get(provider)
            .is_some_and(|readings| !readings.is_empty())
            || self.errors.contains_key(provider)
    }
    pub fn provider_error(&self, provider: &str) -> Option<&str> {
        self.errors.get(provider).map(String::as_str)
    }
    /// Provider-local cache for recording availability transitions independently
    /// of the combined UI inventory.
    pub fn provider_readings(&self, provider: &str) -> &[Reading] {
        self.by_provider.get(provider).map_or(&[], Vec::as_slice)
    }
    pub fn has_held_notifications(&self) -> bool {
        !self.held.is_empty()
    }
    pub fn update_settings(&mut self, settings: Settings) {
        self.pending_sound = None;
        for r in self.by_provider.values().flatten() {
            if self.settings.low_for(&r.key) != settings.low_for(&r.key) {
                for identity in related_sound_identities(r, &self.by_provider) {
                    self.sound_at.remove(&identity);
                }
            }
        }
        if !settings.low_sound {
            self.sound_at.clear();
            self.pending_sound = None;
        }
        self.settings = settings;
        self.by_provider.retain(|provider, readings| {
            let enabled = self.settings.enabled(provider);
            for r in readings {
                if !enabled || self.settings.device(&r.key).is_some_and(|d| d.hidden) {
                    self.estimator.pause(&r.key);
                }
            }
            enabled
        });
        self.errors
            .retain(|provider, _| self.settings.enabled(provider));
        self.clean_states();
    }
    fn clean_states(&mut self) {
        let readings = self.readings();
        self.clean_states_with(&readings);
    }
    fn clean_states_with(&mut self, readings: &[Reading]) {
        let connected: BTreeSet<_> = readings
            .iter()
            .filter(|r| !self.settings.device(&r.key).is_some_and(|d| d.hidden))
            .map(|r| r.key.as_str())
            .collect();
        self.alerts
            .retain(|key, _| connected.contains(key.as_str()));
        let known: BTreeSet<_> = self
            .by_provider
            .values()
            .flatten()
            .map(|r| r.key.as_str())
            .collect();
        self.misses.retain(|key, _| known.contains(key.as_str()));
        self.held
            .retain(|(key, _), _| connected.contains(key.as_str()));
        if !self.sound_at.is_empty() {
            let sound_keys: BTreeSet<_> = self
                .by_provider
                .values()
                .flatten()
                .filter(|r| !self.settings.device(&r.key).is_some_and(|d| d.hidden))
                .flat_map(|r| sound_identities(r).into_iter().flatten())
                .collect();
            self.sound_at.retain(|key, _| sound_keys.contains(key));
        }
    }
    /// One coalesced sound per completed poll, separate from toast quiet mode.
    pub fn take_low_battery_sound(&mut self) -> Option<u8> {
        self.pending_sound.take()
    }
    pub fn apply(
        &mut self,
        provider: &str,
        result: PollResult,
        monotonic: f64,
        quiet: bool,
    ) -> Vec<Notification> {
        if self.suspended {
            return Vec::new();
        }
        let quiet = quiet && self.settings.quiet_fullscreen;
        if !self.settings.enabled(provider) {
            if let Some(previous) = self.by_provider.remove(provider) {
                for r in previous {
                    self.estimator.pause(&r.key);
                }
            }
            self.errors.remove(provider);
            self.clean_states();
            return Vec::new();
        }
        let mut fresh = match result {
            Ok(r) => {
                self.errors.remove(provider);
                r
            }
            Err(e) => {
                self.errors.insert(provider.into(), e.to_string());
                if let Some(previous) = self.by_provider.get_mut(provider) {
                    for r in previous {
                        r.connection = Connection::Stale;
                        self.estimator.record(r, monotonic);
                    }
                }
                return Vec::new();
            }
        };
        // Most enabled brands have no connected devices. Their successful empty
        // polls change no battery evidence: avoid cloning every other device and
        // rebuilding notification/identity sets. Error recovery above and held
        // alert delivery below still run; actual removals take the normal path.
        if fresh.is_empty() && self.provider_readings(provider).is_empty() {
            self.by_provider.remove(provider);
            return if quiet { Vec::new() } else { self.flush_held() };
        }
        for r in &mut fresh {
            self.settings
                .link_device(&crate::ConfigurationDevice::from_reading(r));
            if r.level.is_some_and(|l| l > 100) {
                r.level = None;
            }
        }
        let seen: BTreeSet<_> = fresh.iter().map(|r| r.key.clone()).collect();
        if let Some(previous) = self.by_provider.get(provider) {
            for r in previous {
                if seen.contains(&r.key) {
                    self.misses.remove(&r.key);
                    continue;
                }
                self.estimator.pause(&r.key);
                let misses = self.misses.entry(r.key.clone()).or_default();
                *misses = misses.saturating_add(1);
                let limit = if ["xinput", "playstation", "steam"].contains(&provider) {
                    1
                } else {
                    2
                };
                if *misses < limit || self.settings.keep_disconnected && fresh.len() < 512 {
                    let mut cached = r.clone();
                    cached.connection = Connection::Stale;
                    cached.charging = Some(false);
                    fresh.push(cached);
                }
            }
        }
        self.by_provider.insert(provider.into(), fresh);
        let readings = self.readings();
        self.clean_states_with(&readings);
        let mut notifications = Vec::new();
        for r in readings {
            if self.settings.device(&r.key).is_some_and(|d| d.hidden) {
                self.estimator.pause(&r.key);
                continue;
            }
            let fresh_here = seen.contains(&r.key)
                && self
                    .by_provider
                    .get(provider)
                    .is_some_and(|own| own.iter().any(|o| o.key == r.key && o.source == r.source));
            if fresh_here {
                self.estimator.record(&r, monotonic);
            }
            let low = self.settings.low_for(&r.key);
            // Only this provider's fresh, selected reading can trigger a sound.
            // Cached readings of other providers must not restart a low alarm.
            if self.settings.low_sound && fresh_here && r.online() {
                let identities = sound_identities(&r);
                if let Some(level) = r.level {
                    let last = identities
                        .iter()
                        .flatten()
                        .filter_map(|id| self.sound_at.get(id))
                        .copied()
                        .max_by(f64::total_cmp);
                    if low == 0 || level > low || r.charging == Some(true) {
                        for identity in related_sound_identities(&r, &self.by_provider) {
                            self.sound_at.remove(&identity);
                        }
                    } else if monotonic.is_finite()
                        && last.is_none_or(|last| monotonic >= last + 300.0)
                    {
                        for identity in identities.into_iter().flatten() {
                            self.sound_at.insert(identity, monotonic);
                        }
                        self.pending_sound =
                            Some(self.pending_sound.map_or(level, |old| old.min(level)));
                    } else if monotonic.is_finite() && last.is_some_and(|last| monotonic < last) {
                        // A reset clock starts a new cooldown without replaying immediately.
                        for identity in identities.into_iter().flatten() {
                            self.sound_at.insert(identity, monotonic);
                        }
                    } else if let Some(last) = last {
                        // Carry a cooldown across a newly observed connection alias.
                        for identity in identities.into_iter().flatten() {
                            self.sound_at.insert(identity, last);
                        }
                    }
                }
            }
            let name = self
                .settings
                .device(&r.key)
                .and_then(|d| d.name.as_deref())
                .unwrap_or(&r.name);
            let alert = self.alerts.entry(r.key.clone()).or_default();
            if alert.threshold != Some(low) {
                alert.low = false;
                alert.threshold = Some(low);
            }
            if let Some(level) = r.level.filter(|_| r.online()) {
                if r.charging == Some(true) || level > low.saturating_add(5) {
                    alert.low = false;
                } else if low > 0 && level <= low && !alert.low && self.settings.notify {
                    alert.low = true;
                    notifications.push(Notification {
                        key: r.key.clone(),
                        kind: NotificationKind::Low,
                        title: "Low battery".into(),
                        text: if r.approx.is_some() || r.precision == Precision::Coarse {
                            format!("{name}: battery is low. Time to charge.")
                        } else {
                            format!("{name}: {level}% left. Time to charge.")
                        },
                    });
                }
                if level == 100
                    && alert.full == 1
                    && self.settings.full_alert
                    && self.settings.notify
                {
                    notifications.push(Notification {
                        key: r.key.clone(),
                        kind: NotificationKind::Full,
                        title: "Fully charged".into(),
                        text: format!("{name} is fully charged."),
                    });
                }
                if level == 100 {
                    alert.full = 2;
                } else if r.charging != Some(true) {
                    alert.full = 0;
                } else if !(alert.full == 2 && level >= 95) {
                    alert.full = 1;
                }
            }
        }
        for n in notifications {
            self.held.insert((n.key.clone(), n.kind), n);
        }
        if quiet { Vec::new() } else { self.flush_held() }
    }
    pub fn flush_held(&mut self) -> Vec<Notification> {
        if self.suspended || self.held.is_empty() {
            return Vec::new();
        }
        let readings = self.readings();
        let mut ready = Vec::new();
        for mut n in std::mem::take(&mut self.held).into_values() {
            if !self.settings.notify
                || (n.kind == NotificationKind::Full && !self.settings.full_alert)
                || self.settings.device(&n.key).is_some_and(|d| d.hidden)
            {
                continue;
            }
            let Some(r) = readings.iter().find(|r| r.key == n.key) else {
                continue;
            };
            if n.kind == NotificationKind::Low
                && (self.settings.low_for(&r.key) == 0
                    || r.charging == Some(true)
                    || r.level.is_some_and(|v| v > self.settings.low_for(&r.key)))
            {
                continue;
            }
            if n.kind == NotificationKind::Full && r.level.is_some_and(|level| level < 100) {
                continue;
            }
            // An unavailable reading cannot confirm recovery or disconnect. Keep the
            // pending alert until fresh data permits delivery or invalidates it.
            if !r.online() || r.level.is_none() {
                self.held.insert((n.key.clone(), n.kind), n);
                continue;
            }
            let name = self
                .settings
                .device(&r.key)
                .and_then(|d| d.name.as_deref())
                .unwrap_or(&r.name);
            n.text = if n.kind == NotificationKind::Full {
                format!("{name} is fully charged.")
            } else if r.precision == Precision::Coarse || r.approx.is_some() {
                format!("{name}: battery is low. Time to charge.")
            } else {
                format!("{name}: {}% left. Time to charge.", r.level.unwrap())
            };
            ready.push(n);
        }
        ready
    }
    /// Identity precedence always retains at least one reading from a nonempty
    /// enabled inventory. Checking presence needs no cloned, resolved snapshot.
    pub fn has_readings(&self) -> bool {
        self.by_provider
            .iter()
            .any(|(provider, readings)| self.settings.enabled(provider) && !readings.is_empty())
    }
    pub fn readings(&self) -> Vec<Reading> {
        let all = self
            .by_provider
            .iter()
            .filter(|(p, _)| self.settings.enabled(p))
            .flat_map(|(_, r)| r.iter());
        let mut kept: Vec<&Reading> = Vec::new();
        for r in all.clone() {
            if r.source == "bluetooth"
                && preferred_duplicate(
                    r,
                    all.clone().filter(|o| {
                        o.source != "bluetooth"
                            && o.source != "xinput"
                            && o.online()
                            && o.level.is_some()
                    }),
                )
            {
                continue;
            }
            if r.source == "xinput"
                && r.via == "bluetooth"
                && preferred_duplicate(
                    r,
                    all.clone().filter(|o| {
                        o.source == "bluetooth"
                            && o.kind == "gamepad"
                            && o.online()
                            && o.level.is_some()
                    }),
                )
            {
                continue;
            }
            if let Some(i) = kept.iter().position(|o| o.key == r.key) {
                if r.online()
                    && (!kept[i].online()
                        || (r.level.is_some() && (kept[i].level.is_none() || r.via == "usb")))
                {
                    kept[i] = r;
                }
            } else {
                kept.push(r);
            }
        }
        kept.sort_by(|a, b| a.key.cmp(&b.key));
        kept.into_iter().cloned().collect()
    }
    pub fn snapshot(&self, timestamp: i64) -> Snapshot {
        Snapshot {
            timestamp,
            errors: self.errors.clone(),
            devices: self
                .readings()
                .into_iter()
                .map(|r| {
                    let preferences = self.settings.device(&r.key).cloned().unwrap_or_default();
                    let name = preferences.name.unwrap_or_else(|| r.name.clone());
                    let icon = preferences.icon.unwrap_or_else(|| {
                        if r.kind.is_empty() {
                            "bluetooth".into()
                        } else {
                            r.kind.clone()
                        }
                    });
                    let seconds_left = if self.settings.time_left
                        && r.online()
                        && r.charging == Some(false)
                        && !r.charging_inferred
                        && r.precision == Precision::Exact
                        && !preferences.hidden
                    {
                        self.estimator
                            .seconds_left(&r.key, r.level)
                            .map(|s| s as u64)
                    } else {
                        None
                    };
                    let mut text = String::with_capacity(
                        name.len() + r.approx.as_ref().map_or(0, String::len) + 48,
                    );
                    text.push_str(&name);
                    text.push_str(": ");
                    if let Some(approx) = &r.approx {
                        text.push_str(approx);
                    } else if let Some(level) = r.level {
                        let _ = write!(text, "{level}%");
                    } else {
                        text.push_str("battery level unavailable");
                    }
                    if !r.online() && (r.level.is_some() || r.approx.is_some()) {
                        text.push_str(" (last known)");
                    }
                    if r.charging == Some(true) {
                        text.push_str(" · charging");
                    }
                    if !r.online() {
                        text.push_str(if r.connection == Connection::Sleeping {
                            " · asleep"
                        } else {
                            " · reading unavailable"
                        });
                    }
                    if let Some(s) = seconds_left {
                        text.push_str(" · ");
                        text.push_str(&crate::history::format_left(s as f64));
                    }
                    DeviceView {
                        low_alert_at: self.settings.low_for(&r.key),
                        reading: r,
                        name,
                        icon,
                        seconds_left,
                        text,
                        hidden: preferences.hidden,
                    }
                })
                .collect(),
        }
    }
}

fn sound_identities(r: &Reading) -> [Option<String>; 3] {
    [
        Some(format!("key:{}", r.key)),
        r.serial
            .as_deref()
            .and_then(trusted_identity)
            .map(|serial| format!("serial:{}", serial.to_ascii_lowercase())),
        r.container
            .as_deref()
            .and_then(trusted_identity)
            .map(|container| format!("container:{}", container.to_ascii_lowercase())),
    ]
}

fn related_sound_identities(
    r: &Reading,
    providers: &BTreeMap<String, Vec<Reading>>,
) -> Vec<String> {
    let identities = sound_identities(r);
    providers
        .values()
        .flatten()
        .filter(|other| {
            sound_identities(other)
                .iter()
                .flatten()
                .any(|id| identities.iter().flatten().any(|own| own == id))
        })
        .flat_map(|other| sound_identities(other).into_iter().flatten())
        .collect()
}

/// Containers are stronger than vendor serials. Conflicting containers cannot
/// be resolved by a serial alone, and a shared serial needs one unique target.
/// Borrow the candidates so resolving ambiguity adds no allocation or cache.
fn preferred_duplicate<'a>(
    reading: &Reading,
    candidates: impl Iterator<Item = &'a Reading>,
) -> bool {
    let container = reading.container.as_deref().and_then(trusted_identity);
    let serial = reading.serial.as_deref().and_then(trusted_identity);
    let mut serial_key: Option<&str> = None;
    let mut ambiguous = false;
    for other in candidates {
        if !reading.kind.is_empty()
            && reading.kind != "device"
            && !other.kind.is_empty()
            && other.kind != "device"
            && reading.kind != other.kind
        {
            continue;
        }
        if let (Some(a), Some(b)) = (
            container,
            other.container.as_deref().and_then(trusted_identity),
        ) {
            if a.eq_ignore_ascii_case(b) {
                return true;
            }
            continue;
        }
        if serial.is_some_and(|a| {
            other
                .serial
                .as_deref()
                .and_then(trusted_identity)
                .is_some_and(|b| a.eq_ignore_ascii_case(b))
        }) {
            ambiguous |= serial_key.is_some_and(|key| key != other.key);
            serial_key = Some(&other.key);
        }
    }
    serial_key.is_some() && !ambiguous
}
fn trusted_identity(value: &str) -> Option<&str> {
    let normalized = value.trim();
    if normalized.is_empty()
        || ["UNKNOWN", "NONE", "N/A", "NULL"]
            .iter()
            .any(|placeholder| normalized.eq_ignore_ascii_case(placeholder))
        || normalized
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .all(|c| c == '0')
    {
        None
    } else {
        Some(normalized)
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn unavailable_rows_label_retained_battery_without_changing_reading() {
        for connection in [Connection::Online, Connection::Sleeping, Connection::Stale] {
            for approximate in [false, true] {
                let mut engine = Engine::new(Settings::default(), Estimator::default());
                let mut reading = Reading::new("mouse", "Mouse", "test", 42);
                reading.level = Some(70);
                reading.connection = connection.clone();
                if approximate {
                    reading.approx = Some("medium".into());
                    reading.precision = Precision::Coarse;
                }
                engine
                    .by_provider
                    .insert("test".into(), vec![reading.clone()]);
                let snapshot = engine.snapshot(42);
                let row = &snapshot.devices[0];
                assert_eq!(row.reading, reading);
                assert_eq!(row.text.contains("(last known)"), !reading.online());
                assert!(row.text.starts_with(if approximate {
                    "Mouse: medium"
                } else {
                    "Mouse: 70%"
                }));
                if !reading.online() {
                    assert_eq!(row.seconds_left, None);
                }
            }
        }
    }

    #[test]
    fn missing_battery_does_not_claim_a_last_known_value() {
        for connection in [Connection::Online, Connection::Sleeping, Connection::Stale] {
            let mut engine = Engine::new(Settings::default(), Estimator::default());
            let mut reading = Reading::new("mouse", "Mouse", "test", 42);
            reading.connection = connection;
            engine.by_provider.insert("test".into(), vec![reading]);
            let snapshot = engine.snapshot(42);
            let row = &snapshot.devices[0];
            assert!(row.text.starts_with("Mouse: battery level unavailable"));
            assert!(!row.text.contains("last known"));
            assert_eq!(row.seconds_left, None);
        }
    }
}
