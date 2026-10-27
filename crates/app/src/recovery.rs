//! Durable ownership of temporary hardware settings. No polling or background work.
use hb_core::{settings::LowBatteryMode, *};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const VERSION: u8 = 1;
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 1536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    SaverPolling(LowBatteryMode),
    SaverSleep(LowBatteryMode),
    BoostPolling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Polling,
    Sleep,
}
impl Layer {
    fn kind(self) -> SettingKind {
        match self {
            Self::SaverSleep(_) => SettingKind::Sleep,
            _ => SettingKind::Polling,
        }
    }
    fn slot(self) -> u8 {
        match self {
            Self::SaverPolling(_) => 0,
            Self::SaverSleep(_) => 1,
            Self::BoostPolling => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub device: ConfigurationDevice,
    pub stamp: ConnectionStamp,
    /// Keep the original journal handle valid if storage finishes a verified
    /// rebind after its caller times out. HID still checks the current proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebound_stamp: Option<ConnectionStamp>,
    pub before: u32,
    pub applied: u32,
    pub layer: Layer,
    /// A crash can leave the pre-write record unconfirmed; reconcile it first.
    #[serde(default)]
    pub confirmed: bool,
    /// A newer manual choice began but did not complete conclusively.
    #[serde(default)]
    pub manual_pending: bool,
}
impl Record {
    pub fn matches_stamp(&self, stamp: ConnectionStamp) -> bool {
        self.stamp == stamp || self.rebound_stamp == Some(stamp)
    }
    fn valid(&self) -> bool {
        let d = &self.device;
        if !d.online()
            || d.kind != "mouse"
            || d.key.is_empty()
            || d.key.len() > 512
            || d.name.len() > 512
            || d.source.len() > 64
            || d.via.len() > 64
            || d.serial.as_ref().is_some_and(|s| s.len() > 512)
            || d.container.as_ref().is_some_and(|s| s.len() > 512)
            || self.rebound_stamp.is_some() && !strong_identity(d)
            || self.before == self.applied
        {
            return false;
        }
        match self.layer {
            Layer::SaverPolling(mode) | Layer::SaverSleep(mode) if !mode.valid() => false,
            Layer::SaverSleep(mode) => {
                mode.sleep_seconds.map(u32::from) == Some(self.applied)
                    && (self.before == 0 || self.before > self.applied)
                    && hb_providers::sleep_controls::model(d).is_some_and(|model| {
                        model.readable()
                            && u16::try_from(self.before).is_ok_and(|v| model.supports(v))
                            && u16::try_from(self.applied).is_ok_and(|v| model.supports(v))
                    })
            }
            Layer::SaverPolling(mode) => {
                mode.polling_rate.map(PollingRate::hz) == Some(self.applied)
                    && self.before > self.applied
                    && self.valid_rates()
            }
            Layer::BoostPolling => {
                self.applied > self.before && self.applied > 1000 && self.valid_rates()
            }
        }
    }
    fn valid_rates(&self) -> bool {
        let rates = hb_providers::controls::polling_menu_rates(&self.device);
        rates.contains(&self.before) && rates.contains(&self.applied)
    }
    fn same_slot(&self, other: &Self) -> bool {
        self.device.key == other.device.key && self.layer.slot() == other.layer.slot()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    version: u8,
    records: Vec<Record>,
}
struct Slot {
    record: Record,
    pending: bool,
    rollback_manual: bool,
    startup: bool,
}

/// The in-memory copy is shared with the existing storage worker. Only the
/// storage worker writes the file, and only when a hardware action is requested.
pub struct Journal {
    path: PathBuf,
    slots: Vec<Slot>,
    error: Option<String>,
}

#[derive(Clone)]
pub enum Operation {
    Prepare(Record),
    Confirm(Record),
    FailedPrepare(Record),
    ValidateRestore {
        device: ConfigurationDevice,
        stamp: ConnectionStamp,
        kind: SettingKind,
        expected: u32,
        desired: u32,
        startup: Arc<AtomicBool>,
    },
    Rebind {
        device: ConfigurationDevice,
        old_stamp: ConnectionStamp,
        new_stamp: ConnectionStamp,
        kind: SettingKind,
        expected: u32,
        desired: u32,
    },
    FinishRestore {
        device: ConfigurationDevice,
        stamp: ConnectionStamp,
        kind: SettingKind,
        expected: u32,
        desired: u32,
        actual: u32,
    },
    ManualBegin {
        device: ConfigurationDevice,
        kind: SettingKind,
        desired: u32,
    },
    ManualFinish {
        device: ConfigurationDevice,
        kind: SettingKind,
        completed: bool,
    },
    Blocked,
}

impl Journal {
    pub fn open(folder: &Path) -> Self {
        let path = folder.join("automatic-recovery.json");
        let result = (|| -> Result<Vec<Record>, String> {
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.to_string()),
            };
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("Recovery data is too large".into());
            }
            let data: Disk = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if data.version != VERSION || data.records.len() > MAX_RECORDS {
                return Err("Recovery data has an unsupported format or size".into());
            }
            for (index, record) in data.records.iter().enumerate() {
                if !record.valid()
                    || data.records[..index]
                        .iter()
                        .any(|other| other.same_slot(record))
                {
                    return Err(
                        "Recovery data contains an invalid or duplicate device setting".into(),
                    );
                }
            }
            Ok(data.records)
        })();
        match result {
            Ok(records) => Self {
                path,
                slots: records
                    .into_iter()
                    .map(|record| Slot {
                        pending: !record.manual_pending,
                        record,
                        rollback_manual: false,
                        startup: true,
                    })
                    .collect(),
                error: None,
            },
            Err(error) => Self {
                path,
                slots: Vec::new(),
                error: Some(error),
            },
        }
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn has_pending(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending)
    }
    pub fn has_uncertain_manual(&self) -> bool {
        self.slots.iter().any(|slot| slot.record.manual_pending)
    }
    pub fn devices(&self) -> impl Iterator<Item = &ConfigurationDevice> {
        self.slots.iter().map(|s| &s.record.device)
    }
    pub fn pending_records(&self, provider: &str, readings: &[Reading], now: i64) -> Vec<Record> {
        self.slots
            .iter()
            .filter_map(|slot| {
                let record = &slot.record;
                if !slot.pending || record.device.source != provider {
                    return None;
                }
                let mut matching = readings.iter().filter(|r| r.key == record.device.key);
                let first = matching.next();
                (first.is_some_and(|r| {
                    record.device.matches_reading(r)
                        && r.online()
                        && r.timestamp <= now
                        && now.saturating_sub(r.timestamp) <= 120
                }) && matching.next().is_none())
                .then(|| record.clone())
            })
            .collect()
    }
    pub fn claimed(&mut self, record: &Record) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.record == *record) {
            slot.pending = false;
        }
    }
    /// A manual choice suppresses in-memory recovery immediately. The worker
    /// durably suspends the same setting before it may issue the user's SET.
    pub fn suppress(&mut self, device: &ConfigurationDevice, kind: SettingKind) {
        for slot in &mut self.slots {
            if slot.record.layer.kind() == kind && same_unit(&slot.record.device, device) {
                slot.pending = false;
            }
        }
    }
    pub fn rearm(&mut self, device: &ConfigurationDevice, kind: SettingKind) {
        for slot in &mut self.slots {
            if !slot.record.manual_pending
                && slot.record.layer.kind() == kind
                && same_unit(&slot.record.device, device)
            {
                slot.pending = true;
            }
        }
    }
    pub fn rearm_confirmed(&mut self) {
        for slot in &mut self.slots {
            if slot.record.confirmed && !slot.record.manual_pending {
                slot.pending = true;
            }
        }
    }
    fn persist(&self, records: Vec<Record>) -> Result<(), ProviderError> {
        let bytes = serde_json::to_vec(&Disk {
            version: VERSION,
            records,
        })
        .map_err(|e| ProviderError::new(e.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(ProviderError::new("Automatic recovery data is too large."));
        }
        hb_storage::atomic_write(&self.path, &bytes)
    }
    fn replace(&mut self, mut records: Vec<Slot>) -> Result<(), ProviderError> {
        self.persist(records.iter().map(|slot| slot.record.clone()).collect())?;
        if records.is_empty() {
            records = Vec::new();
        }
        self.slots = records;
        Ok(())
    }
    fn copy_slots(&self) -> Vec<Slot> {
        self.slots
            .iter()
            .map(|slot| Slot {
                record: slot.record.clone(),
                pending: slot.pending,
                rollback_manual: slot.rollback_manual,
                startup: slot.startup,
            })
            .collect()
    }
    fn chain(
        &self,
        device: &ConfigurationDevice,
        stamp: ConnectionStamp,
        kind: SettingKind,
        expected: u32,
        desired: u32,
    ) -> Option<Vec<usize>> {
        let mut current = expected;
        let mut chain = Vec::new();
        while current != desired {
            let (index, slot) = self.slots.iter().enumerate().find(|(index, slot)| {
                !chain.contains(index)
                    && !slot.record.manual_pending
                    && slot.record.device == *device
                    && slot.record.matches_stamp(stamp)
                    && slot.record.layer.kind() == kind
                    && slot.record.applied == current
            })?;
            current = slot.record.before;
            chain.push(index);
            if chain.len() > 2 {
                return None;
            }
        }
        (!chain.is_empty()).then_some(chain)
    }
    pub fn execute(&mut self, operation: Operation) -> Result<(), ProviderError> {
        if self.error.is_some() {
            return Err(ProviderError::new(
                "Automatic recovery data could not be read. No device change was made.",
            ));
        }
        match operation {
            Operation::Blocked => Err(ProviderError::new(
                "The device connection could not be verified for safe recovery.",
            )),
            Operation::Prepare(record) => {
                if record.confirmed
                    || record.manual_pending
                    || record.rebound_stamp.is_some()
                    || !record.valid()
                    || self.slots.len() >= MAX_RECORDS
                    || self.slots.iter().any(|s| s.record.same_slot(&record))
                    || self.slots.iter().any(|s| {
                        s.record.manual_pending
                            && s.record.layer.kind() == record.layer.kind()
                            && same_unit(&s.record.device, &record.device)
                    })
                {
                    return Err(ProviderError::new(
                        "An earlier automatic change needs recovery before another can start.",
                    ));
                }
                let mut next = self.copy_slots();
                next.push(Slot {
                    record,
                    pending: false,
                    rollback_manual: false,
                    startup: false,
                });
                self.replace(next)
            }
            Operation::Confirm(record) => {
                let mut next = self.copy_slots();
                let Some(slot) = next.iter_mut().find(|slot| slot.record == record) else {
                    return Err(ProviderError::new("The pending recovery record changed."));
                };
                slot.record.confirmed = true;
                self.replace(next)
            }
            Operation::FailedPrepare(record) => {
                if !self.slots.iter().any(|slot| slot.record == record) {
                    return Ok(());
                }
                let next: Vec<_> = self
                    .copy_slots()
                    .into_iter()
                    .filter(|s| s.record != record)
                    .collect();
                if next.len() == self.slots.len() {
                    return Ok(());
                }
                self.replace(next)
            }
            Operation::ValidateRestore {
                device,
                stamp,
                kind,
                expected,
                desired,
                startup,
            } => {
                let chain = self
                    .chain(&device, stamp, kind, expected, desired)
                    .ok_or_else(|| {
                        ProviderError::new(
                            "The original setting could not be verified for recovery.",
                        )
                    })?;
                startup.store(
                    chain.iter().all(|index| self.slots[*index].startup),
                    Ordering::Release,
                );
                Ok(())
            }
            Operation::Rebind {
                device,
                old_stamp,
                new_stamp,
                kind,
                expected,
                desired,
            } => {
                if !strong_identity(&device) {
                    return Err(ProviderError::new(
                        "The mouse identity is not strong enough to verify a new connection.",
                    ));
                }
                let indexes = self
                    .chain(&device, old_stamp, kind, expected, desired)
                    .filter(|indexes| indexes.iter().all(|index| self.slots[*index].startup))
                    .ok_or_else(|| {
                        ProviderError::new("The original connection could not be safely recovered.")
                    })?;
                if indexes.iter().all(|index| {
                    let record = &self.slots[*index].record;
                    record.rebound_stamp == (new_stamp != record.stamp).then_some(new_stamp)
                }) {
                    return Ok(());
                }
                let mut next = self.copy_slots();
                for index in indexes {
                    next[index].record.rebound_stamp =
                        (new_stamp != next[index].record.stamp).then_some(new_stamp);
                }
                self.replace(next)
            }
            Operation::FinishRestore {
                device,
                stamp,
                kind,
                expected,
                desired,
                actual,
            } => {
                let indexes = if actual == desired {
                    self.chain(&device, stamp, kind, expected, desired)
                        .unwrap_or_default()
                } else if actual != expected {
                    self.slots
                        .iter()
                        .enumerate()
                        .filter_map(|(i, slot)| {
                            (slot.record.device == device
                                && slot.record.matches_stamp(stamp)
                                && slot.record.layer.kind() == kind)
                                .then_some(i)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                if indexes.is_empty() {
                    return Ok(());
                }
                let next = self
                    .copy_slots()
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, slot)| (!indexes.contains(&i)).then_some(slot))
                    .collect();
                self.replace(next)
            }
            Operation::ManualBegin { device, kind, .. } => {
                if !self.slots.iter().any(|slot| {
                    slot.record.layer.kind() == kind && same_unit(&slot.record.device, &device)
                }) {
                    return Ok(());
                }
                let mut next = self.copy_slots();
                let mut changed = false;
                for slot in &mut next {
                    if slot.record.layer.kind() == kind && same_unit(&slot.record.device, &device) {
                        slot.rollback_manual = !slot.record.manual_pending;
                        slot.record.manual_pending = true;
                        slot.pending = false;
                        changed = true;
                    }
                }
                if !changed {
                    return Ok(());
                }
                self.replace(next)
            }
            Operation::ManualFinish {
                device,
                kind,
                completed,
            } => {
                if !self.slots.iter().any(|slot| {
                    slot.record.layer.kind() == kind
                        && same_unit(&slot.record.device, &device)
                        && (completed || slot.rollback_manual)
                }) {
                    return Ok(());
                }
                let mut next = self.copy_slots();
                let mut changed = false;
                if completed {
                    next.retain(|slot| {
                        let matched = slot.record.layer.kind() == kind
                            && same_unit(&slot.record.device, &device);
                        changed |= matched;
                        !matched
                    });
                } else {
                    for slot in &mut next {
                        if slot.record.layer.kind() == kind
                            && same_unit(&slot.record.device, &device)
                            && slot.rollback_manual
                        {
                            slot.record.manual_pending = false;
                            slot.pending = true;
                            slot.rollback_manual = false;
                            changed = true;
                        }
                    }
                }
                if !changed {
                    return Ok(());
                }
                self.replace(next)
            }
        }
    }
}

fn same_unit(a: &ConfigurationDevice, b: &ConfigurationDevice) -> bool {
    a.key == b.key
        || a.serial
            .as_deref()
            .zip(b.serial.as_deref())
            .is_some_and(|(a, b)| a.trim().eq_ignore_ascii_case(b.trim()))
            && a.shared_preference_key()
                .is_some_and(|key| b.shared_preference_key().as_ref() == Some(&key))
}

/// A changed native connection may be rebound only when the original hardware
/// serial is embedded in its stable key and Windows also supplies a container.
/// Provider enumeration must independently confirm both and reject ambiguity.
pub fn strong_identity(device: &ConfigurationDevice) -> bool {
    let Some(serial) = device.serial.as_deref().map(str::trim) else {
        return false;
    };
    let Some(container) = device.container.as_deref().map(str::trim) else {
        return false;
    };
    serial.len() >= 4
        && serial.len() <= 128
        && serial
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && serial
            .bytes()
            .any(|byte| byte.is_ascii_alphanumeric() && byte != b'0')
        && !["UNKNOWN", "NONE", "NULL"]
            .iter()
            .any(|v| serial.eq_ignore_ascii_case(v))
        && container.len() == 32
        && container.bytes().all(|byte| byte.is_ascii_hexdigit())
        && container.bytes().any(|byte| byte != b'0')
        && device
            .key
            .rsplit(':')
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case(serial))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(serial: &str) -> Reading {
        let mut reading = Reading::new(
            format!("razer:00bf:{serial}"),
            "Razer DeathAdder V4 Pro",
            "razer",
            100,
        );
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        reading.serial = Some(serial.into());
        reading.container = Some("00112233445566778899AABBCCDDEEFF".into());
        reading.level = Some(10);
        reading.charging = Some(false);
        reading
    }
    fn choices() -> LowBatteryMode {
        LowBatteryMode {
            threshold: 20,
            polling_rate: PollingRate::try_from(125).ok(),
            sleep_seconds: Some(60),
        }
    }
    fn record(serial: &str, layer: Layer, before: u32, applied: u32) -> Record {
        Record {
            device: ConfigurationDevice::from_reading(&reading(serial)),
            stamp: ConnectionStamp::new(1).unwrap(),
            rebound_stamp: None,
            before,
            applied,
            layer,
            confirmed: false,
            manual_pending: false,
        }
    }
    fn prepare(journal: &mut Journal, record: &Record, confirmed: bool) {
        journal.execute(Operation::Prepare(record.clone())).unwrap();
        if confirmed {
            journal.execute(Operation::Confirm(record.clone())).unwrap();
        }
    }
    fn restore(record: &Record, desired: u32) -> Operation {
        Operation::ValidateRestore {
            device: record.device.clone(),
            stamp: record.stamp,
            kind: record.layer.kind(),
            expected: record.applied,
            desired,
            startup: Arc::new(AtomicBool::new(false)),
        }
    }
    fn finish(record: &Record, desired: u32, actual: u32) -> Operation {
        Operation::FinishRestore {
            device: record.device.clone(),
            stamp: record.stamp,
            kind: record.layer.kind(),
            expected: record.applied,
            desired,
            actual,
        }
    }

    #[test]
    fn write_ahead_survives_restart_and_confirmed_status_is_persisted() {
        let folder = tempfile::tempdir().unwrap();
        let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        let mut journal = Journal::open(folder.path());
        prepare(&mut journal, &item, false);
        let journal = Journal::open(folder.path());
        assert!(journal.error().is_none(), "{:?}", journal.error());
        assert!(journal.has_pending());
        assert!(!journal.slots[0].record.confirmed);
        let mut journal = journal;
        journal.execute(Operation::Confirm(item)).unwrap();
        assert!(Journal::open(folder.path()).slots[0].record.confirmed);
    }

    #[test]
    fn layered_restore_requires_the_exact_device_and_connection() {
        for direct in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let mut journal = Journal::open(folder.path());
            let saver = record("TEST", Layer::SaverPolling(choices()), 1000, 125);
            let boost = record("TEST", Layer::BoostPolling, 125, 4000);
            prepare(&mut journal, &saver, true);
            prepare(&mut journal, &boost, true);
            let target = if direct { 1000 } else { 125 };
            assert!(journal.execute(restore(&boost, target)).is_ok());
            let mut changed = boost.clone();
            changed.stamp = ConnectionStamp::new(2).unwrap();
            assert!(journal.execute(restore(&changed, target)).is_err());
            let mut other = boost.clone();
            other.device.serial = Some("OTHER".into());
            assert!(journal.execute(restore(&other, target)).is_err());
            journal.execute(finish(&boost, target, target)).unwrap();
            if !direct {
                assert_eq!(journal.slots.len(), 1);
                journal.execute(restore(&saver, 1000)).unwrap();
                journal.execute(finish(&saver, 1000, 1000)).unwrap();
            }
            assert!(journal.slots.is_empty());
            assert_eq!(journal.slots.capacity(), 0);
            assert!(Journal::open(folder.path()).slots.is_empty());
        }
    }

    #[test]
    fn rebind_requires_restart_and_unique_serial_and_container() {
        let folder = tempfile::tempdir().unwrap();
        let item = record("TEST", Layer::SaverPolling(choices()), 1000, 125);
        let mut journal = Journal::open(folder.path());
        prepare(&mut journal, &item, true);
        let rebind = |device: ConfigurationDevice| Operation::Rebind {
            device,
            old_stamp: item.stamp,
            new_stamp: ConnectionStamp::new(2).unwrap(),
            kind: SettingKind::Polling,
            expected: 125,
            desired: 1000,
        };
        assert!(
            journal.execute(rebind(item.device.clone())).is_err(),
            "A live-session disconnect cannot silently reuse an old connection"
        );
        let mut journal = Journal::open(folder.path());
        let mut incomplete = item.device.clone();
        incomplete.container = None;
        assert!(journal.execute(rebind(incomplete)).is_err());
        let mut wrong = item.device.clone();
        wrong.serial = Some("OTHER".into());
        assert!(journal.execute(rebind(wrong)).is_err());
        journal.execute(rebind(item.device.clone())).unwrap();
        assert!(
            journal.execute(restore(&item, 1000)).is_ok(),
            "A lost rebind acknowledgement must not invalidate the original journal handle"
        );
        let mut rebound = item;
        rebound.stamp = ConnectionStamp::new(2).unwrap();
        assert!(journal.execute(restore(&rebound, 1000)).is_ok());
        assert!(
            Journal::open(folder.path())
                .execute(restore(&rebound, 1000))
                .is_ok()
        );
    }

    #[test]
    fn weak_or_placeholder_identities_never_authorize_rebinding() {
        let item = record("TEST", Layer::SaverPolling(choices()), 1000, 125);
        assert!(strong_identity(&item.device));
        for serial in ["", "0000", "UNKNOWN", "OTHER"] {
            let mut device = item.device.clone();
            device.serial = Some(serial.into());
            assert!(!strong_identity(&device));
        }
        let mut device = item.device;
        device.container = None;
        assert!(!strong_identity(&device));
    }

    #[test]
    fn recovery_rejects_reversed_automation_and_out_of_scope_boosts() {
        let folder = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(folder.path());
        let mut rate_mode = choices();
        rate_mode.polling_rate = PollingRate::try_from(1000).ok();
        let mut sleep_mode = choices();
        sleep_mode.sleep_seconds = Some(300);
        for item in [
            record("TEST", Layer::SaverPolling(rate_mode), 125, 1000),
            record("TEST", Layer::SaverSleep(sleep_mode), 60, 300),
            record("TEST", Layer::BoostPolling, 4000, 1000),
            record("TEST", Layer::BoostPolling, 125, 500),
        ] {
            assert!(journal.execute(Operation::Prepare(item.clone())).is_err());
            std::fs::write(
                folder.path().join("automatic-recovery.json"),
                serde_json::to_vec(&Disk {
                    version: VERSION,
                    records: vec![item],
                })
                .unwrap(),
            )
            .unwrap();
            assert!(Journal::open(folder.path()).error().is_some());
        }
    }

    #[test]
    fn external_changes_and_manual_choices_retire_only_the_affected_setting() {
        let folder = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(folder.path());
        let saver = record("TEST", Layer::SaverPolling(choices()), 1000, 125);
        let boost = record("TEST", Layer::BoostPolling, 125, 4000);
        let sleep = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        let other = record("OTHER", Layer::SaverPolling(choices()), 1000, 125);
        for item in [&saver, &boost, &sleep, &other] {
            prepare(&mut journal, item, true);
        }
        journal.execute(finish(&boost, 125, 2000)).unwrap();
        assert_eq!(journal.slots.len(), 2);
        assert!(
            journal
                .slots
                .iter()
                .any(|s| s.record.layer.kind() == SettingKind::Sleep)
        );
        assert!(
            journal
                .slots
                .iter()
                .any(|s| s.record.device.key == other.device.key)
        );
        let mut wired = sleep.device.clone();
        wired.key = "razer:00be:TEST".into();
        journal
            .execute(Operation::ManualBegin {
                device: wired.clone(),
                kind: SettingKind::Sleep,
                desired: 300,
            })
            .unwrap();
        journal
            .execute(Operation::ManualFinish {
                device: wired,
                kind: SettingKind::Sleep,
                completed: true,
            })
            .unwrap();
        assert_eq!(journal.slots.len(), 1);
        assert_eq!(journal.slots[0].record.device.key, other.device.key);
    }

    #[test]
    fn missing_stale_or_ambiguous_readings_do_not_claim_recovery() {
        let folder = tempfile::tempdir().unwrap();
        let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        let mut journal = Journal::open(folder.path());
        prepare(&mut journal, &item, true);
        let mut journal = Journal::open(folder.path());
        assert!(journal.error().is_none(), "{:?}", journal.error());
        let current = reading("TEST");
        assert!(journal.pending_records("razer", &[], 100).is_empty());
        assert!(
            journal
                .pending_records("razer", std::slice::from_ref(&current), 221)
                .is_empty()
        );
        let mut wrong = current.clone();
        wrong.serial = Some("OTHER".into());
        assert!(journal.pending_records("razer", &[wrong], 100).is_empty());
        assert!(
            journal
                .pending_records("razer", &[current.clone(), current.clone()], 100)
                .is_empty()
        );
        let pending = journal.pending_records("razer", &[current], 100);
        assert_eq!(pending.len(), 1);
        journal.claimed(&pending[0]);
        assert!(!journal.has_pending());
    }

    #[test]
    fn corrupt_and_oversized_recovery_files_are_preserved_and_block_changes() {
        for bad in [b"not json".to_vec(), vec![b' '; MAX_BYTES as usize + 1]] {
            let folder = tempfile::tempdir().unwrap();
            let path = folder.path().join("automatic-recovery.json");
            std::fs::write(&path, &bad).unwrap();
            let mut journal = Journal::open(folder.path());
            assert!(journal.error().is_some());
            let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
            assert!(journal.execute(Operation::Prepare(item)).is_err());
            assert_eq!(std::fs::read(path).unwrap(), bad);
        }
    }

    #[test]
    fn failed_persistence_never_authorizes_or_replaces_the_original_record() {
        let folder = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(folder.path());
        let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        std::fs::create_dir(folder.path().join("automatic-recovery.json")).unwrap();
        assert!(journal.execute(Operation::Prepare(item.clone())).is_err());
        assert!(journal.slots.is_empty());
        std::fs::remove_dir(folder.path().join("automatic-recovery.json")).unwrap();
        prepare(&mut journal, &item, false);
        assert!(journal.execute(Operation::Prepare(item.clone())).is_err());
        journal.execute(Operation::FailedPrepare(item)).unwrap();
        assert!(journal.slots.is_empty());
        assert_eq!(journal.slots.capacity(), 0);
    }

    #[test]
    fn interrupted_manual_change_never_replays_the_older_automatic_setting() {
        for completed in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
            let mut journal = Journal::open(folder.path());
            prepare(&mut journal, &item, true);
            journal
                .execute(Operation::ManualBegin {
                    device: item.device.clone(),
                    kind: SettingKind::Sleep,
                    desired: 600,
                })
                .unwrap();
            let mut restarted = Journal::open(folder.path());
            assert!(!restarted.has_pending());
            assert!(restarted.has_uncertain_manual());
            assert_eq!(restarted.devices().count(), 1);
            assert!(restarted.execute(restore(&item, 300)).is_err());
            restarted
                .execute(Operation::ManualFinish {
                    device: item.device.clone(),
                    kind: SettingKind::Sleep,
                    completed,
                })
                .unwrap();
            let recovered = Journal::open(folder.path());
            if completed {
                assert!(recovered.slots.is_empty());
            } else {
                assert!(!recovered.has_pending());
                assert!(recovered.has_uncertain_manual());
            }
        }
    }

    #[test]
    fn manual_failure_before_write_rearms_only_this_sessions_prior_ownership() {
        let folder = tempfile::tempdir().unwrap();
        let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        let mut journal = Journal::open(folder.path());
        prepare(&mut journal, &item, true);
        journal
            .execute(Operation::ManualBegin {
                device: item.device.clone(),
                kind: SettingKind::Sleep,
                desired: 600,
            })
            .unwrap();
        journal
            .execute(Operation::ManualFinish {
                device: item.device,
                kind: SettingKind::Sleep,
                completed: false,
            })
            .unwrap();
        let recovered = Journal::open(folder.path());
        assert!(recovered.has_pending());
        assert!(!recovered.has_uncertain_manual());
    }

    #[test]
    fn unrelated_recovery_operations_do_not_clone_the_active_journal() {
        let folder = tempfile::tempdir().unwrap();
        let item = record("TEST", Layer::SaverSleep(choices()), 300, 60);
        let other = record("OTHER", Layer::SaverSleep(choices()), 300, 60);
        let mut journal = Journal::open(folder.path());
        prepare(&mut journal, &item, true);
        for operation in [
            Operation::FailedPrepare(other.clone()),
            Operation::ManualBegin {
                device: other.device.clone(),
                kind: SettingKind::Sleep,
                desired: 300,
            },
            Operation::ManualFinish {
                device: other.device,
                kind: SettingKind::Sleep,
                completed: true,
            },
        ] {
            let (_, allocations) =
                crate::allocation_counter::measure(|| journal.execute(operation).unwrap());
            assert_eq!(allocations.bytes, 0);
        }
    }
}
