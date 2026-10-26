//! Optional, explicit device configuration. No game or input-event access.
use crate::{Connection, HidTransport, PollContext, Reading};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct PollingRate(u32);
impl PollingRate {
    pub fn hz(self) -> u32 {
        self.0
    }
}
impl TryFrom<u32> for PollingRate {
    type Error = &'static str;
    fn try_from(hz: u32) -> Result<Self, Self::Error> {
        match hz {
            125 | 250 | 500 | 1000 | 2000 | 4000 | 8000 => Ok(Self(hz)),
            _ => Err("unsupported polling rate"),
        }
    }
}
impl From<PollingRate> for u32 {
    fn from(rate: PollingRate) -> Self {
        rate.0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PollingCapability {
    ReadWrite,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationDevice {
    pub key: String,
    pub name: String,
    pub kind: String,
    pub source: String,
    pub via: String,
    pub connection: Connection,
    pub serial: Option<String>,
    pub container: Option<String>,
    pub capability: PollingCapability,
}
impl ConfigurationDevice {
    /// A receiver also uses USB; only exact wired model IDs establish external power.
    pub fn wired_mouse(&self) -> bool {
        wired_mouse(&self.key, &self.kind, &self.source, &self.via)
    }

    /// Preferences only: never use this alias for HID authorization or observations.
    pub fn shared_preference_key(&self) -> Option<String> {
        shared_preference_key(
            &self.key,
            &self.kind,
            &self.source,
            &self.via,
            self.serial.as_deref(),
        )
    }
    pub fn from_reading(reading: &Reading) -> Self {
        Self {
            key: reading.key.clone(),
            name: reading.name.clone(),
            kind: reading.kind.clone(),
            source: reading.source.clone(),
            via: reading.via.clone(),
            connection: reading.connection.clone(),
            serial: reading.serial.clone(),
            container: reading.container.clone(),
            capability: PollingCapability::ReadWrite,
        }
    }
    pub fn online(&self) -> bool {
        self.connection == Connection::Online
    }
    pub fn matches_reading(&self, reading: &Reading) -> bool {
        self.key == reading.key
            && self.kind == reading.kind
            && self.source == reading.source
            && self.via == reading.via
            && self.serial == reading.serial
            && self.container == reading.container
    }
}

impl Reading {
    pub fn wired_mouse(&self) -> bool {
        wired_mouse(&self.key, &self.kind, &self.source, &self.via)
    }
}
pub(crate) fn shared_preference_key(
    key: &str,
    kind: &str,
    source: &str,
    via: &str,
    serial: Option<&str>,
) -> Option<String> {
    let unit = preference_unit(key, kind, source, via, serial)?;
    Some(format!("razer:{}:{}", unit.wireless_pid, unit.serial))
}

pub(crate) struct PreferenceUnit<'a> {
    pub wired_pid: &'static str,
    pub wireless_pid: &'static str,
    pub serial: &'a str,
}
pub(crate) fn preference_unit<'a>(
    key: &'a str,
    kind: &str,
    source: &str,
    via: &str,
    serial: Option<&str>,
) -> Option<PreferenceUnit<'a>> {
    if kind != "mouse" || source != "razer" || via != "usb" {
        return None;
    }
    let ("razer", pid, suffix) = preference_parts(key)? else {
        return None;
    };
    let (wired_pid, wireless_pid) = match pid {
        "00be" | "00bf" => ("00be", "00bf"),
        "00b6" | "00b7" => ("00b6", "00b7"),
        "00c0" | "00c1" => ("00c0", "00c1"),
        _ => return None,
    };
    // Provider keys contain the canonical ASCII serial. Validate the borrowed
    // suffix instead of allocating an uppercase copy on every observation.
    if !valid_unit_serial(suffix)
        || suffix.bytes().any(|byte| byte.is_ascii_lowercase())
        || !suffix.eq_ignore_ascii_case(serial?.trim())
    {
        return None;
    }
    Some(PreferenceUnit {
        wired_pid,
        wireless_pid,
        serial: suffix,
    })
}
fn wired_mouse(key: &str, kind: &str, source: &str, via: &str) -> bool {
    kind == "mouse"
        && source == "razer"
        && via == "usb"
        && ["razer:00be:", "razer:00b6:", "razer:00c0:"]
            .iter()
            .any(|p| key.starts_with(p))
}

fn preference_parts(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, ':');
    Some((parts.next()?, parts.next()?, parts.next()?))
}
fn valid_unit_serial(serial: &str) -> bool {
    (4..=128).contains(&serial.len())
        && serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !["UNKNOWN", "NONE", "NULL"].contains(&serial)
        && serial
            .bytes()
            .any(|b| b.is_ascii_alphanumeric() && b != b'0')
}
/// Validate persisted links as one-way aliases for the same model and unit serial.
pub(crate) fn valid_preference_link(alias: &str, target: &str) -> bool {
    let Some(("razer", pid, serial)) = preference_parts(alias) else {
        return false;
    };
    let wireless = match pid {
        "00be" => "00bf",
        "00b6" => "00b7",
        "00c0" => "00c1",
        _ => return false,
    };
    valid_unit_serial(serial) && target == format!("razer:{wireless}:{serial}")
}
#[derive(Clone, Debug)]
pub struct ControlTarget {
    pub device: ConfigurationDevice,
    /// Enumeration epoch of the last successful configuration read.
    pub generation: u64,
    /// Native connection incarnation, independent of enumeration cache refreshes.
    /// Missing evidence keeps the conservative generation-only safety boundary.
    pub connection_stamp: Option<ConnectionStamp>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionStamp(std::num::NonZeroU64);
impl ConnectionStamp {
    pub const fn new(value: u64) -> Option<Self> {
        match std::num::NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Read,
    Apply(PollingRate),
    ApplyIf {
        rate: PollingRate,
        expected: PollingRate,
    },
}
impl ControlAction {
    pub fn rate(self) -> Option<PollingRate> {
        match self {
            Self::Read => None,
            Self::Apply(r) | Self::ApplyIf { rate: r, .. } => Some(r),
        }
    }
    pub fn expected(self) -> Option<PollingRate> {
        match self {
            Self::ApplyIf { expected, .. } => Some(expected),
            _ => None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ControlRequest {
    pub request: u64,
    pub target: ControlTarget,
    pub action: ControlAction,
}
#[derive(Clone, Debug)]
pub struct PollingObservation {
    pub target: ControlTarget,
    pub supported: Vec<PollingRate>,
    pub rate: Option<PollingRate>,
    pub timestamp: i64,
    pub evidence: String,
}
#[derive(Clone, Debug)]
pub struct ControlOutcome {
    pub request: u64,
    pub key: String,
    pub observation: Option<PollingObservation>,
    pub previous: Option<PollingRate>,
    pub may_have_changed: bool,
    pub failure: Option<String>,
}
impl ControlOutcome {
    pub fn failed(request: &ControlRequest, message: impl Into<String>) -> Self {
        Self {
            request: request.request,
            key: request.target.device.key.clone(),
            observation: None,
            previous: None,
            may_have_changed: false,
            failure: Some(message.into()),
        }
    }
    pub fn confirmed_change(&self) -> bool {
        self.failure.is_none()
            && self.may_have_changed
            && self
                .observation
                .as_ref()
                .and_then(|o| o.rate)
                .is_some_and(|rate| Some(rate) != self.previous)
    }
}
pub trait DeviceController: Send {
    fn execute(
        &mut self,
        request: &ControlRequest,
        hid: &dyn HidTransport,
        context: &PollContext<'_>,
    ) -> ControlOutcome;
}
