//! Platform-independent contracts and state transitions for Halo Continued.
pub mod controls;
pub mod dpi;
pub use dpi::*;
pub mod device_info;
pub use device_info::*;
pub mod engine;
pub use controls::*;
pub mod history;
pub mod history_plot;
pub mod insights;
pub use insights::*;
mod adaptive;
pub use adaptive::*;
pub mod settings;

pub use engine::{DeviceView, Engine, Notification, NotificationKind, Snapshot};
pub use history::Estimator;
pub use history_plot::{HistoryAxis, HistorySample, HistorySeries};
use serde::{Deserialize, Serialize};
pub use settings::{DevicePreferences, Settings};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    #[default]
    Exact,
    Coarse,
    Estimated,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    #[default]
    Online,
    Sleeping,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub key: String,
    pub name: String,
    pub level: Option<u8>,
    pub charging: Option<bool>,
    pub charging_inferred: bool,
    pub connection: Connection,
    pub precision: Precision,
    pub approx: Option<String>,
    pub kind: String,
    pub source: String,
    pub via: String,
    pub serial: Option<String>,
    pub container: Option<String>,
    pub timestamp: i64,
}
impl Reading {
    pub fn new(
        key: impl Into<String>,
        name: impl Into<String>,
        source: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            key: key.into(),
            name: name.into(),
            source: source.into(),
            timestamp,
            level: None,
            charging: None,
            charging_inferred: false,
            connection: Connection::Online,
            precision: Precision::Exact,
            approx: None,
            kind: String::new(),
            via: String::new(),
            serial: None,
            container: None,
        }
    }
    pub fn online(&self) -> bool {
        self.connection == Connection::Online
    }
    /// Name changes preserve evidence. Transport changes are separate interval
    /// boundaries; a changed unit/container must never inherit a battery fit.
    pub(crate) fn same_battery_identity(&self, other: &Self) -> bool {
        self.key == other.key
            && self.source == other.source
            && self.kind == other.kind
            && self.serial == other.serial
            && self.container == other.container
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HidInfo {
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface: i32,
    pub product: String,
    pub serial: String,
    pub container: Option<String>,
    pub output_length: Option<usize>,
    pub feature_length: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ProviderError {
    pub message: String,
}
impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for ProviderError {}
impl From<std::io::Error> for ProviderError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}
pub type PollResult = Result<Vec<Reading>, ProviderError>;

pub trait HidSession: Send {
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError>;
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError>;
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError>;
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError>;
    fn input_report(&mut self, _id: u8, _length: usize) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::new(
            "input report operation unsupported by transport",
        ))
    }
}
pub trait HidTransport: Send + Sync {
    /// Changes whenever enumeration is invalidated. Configuration writes must
    /// revalidate their observed target after obtaining the receiver lock.
    fn generation(&self) -> u64 {
        0
    }
    /// Read-only proof of this collection's current connection incarnation.
    /// Called for configuration transactions, never for routine battery polling.
    fn connection_stamp(&self, _info: &HidInfo) -> Option<ConnectionStamp> {
        None
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError>;
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError>;
}
pub trait Clock: Send + Sync {
    fn unix(&self) -> i64;
    fn monotonic(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}
pub struct SystemClock {
    start: Instant,
}
impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}
impl Clock for SystemClock {
    fn unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }
    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
pub struct PollContext<'a> {
    pub clock: &'a dyn Clock,
    pub cancelled: &'a AtomicBool,
    pub deadline: Duration,
    pub playstation_full_mode: bool,
}
impl PollContext<'_> {
    pub fn active(&self) -> bool {
        !self.cancelled.load(Ordering::Relaxed) && self.clock.monotonic() < self.deadline
    }
    pub fn sleep(&self, duration: Duration) {
        if self.active() {
            self.clock
                .sleep(duration.min(self.deadline.saturating_sub(self.clock.monotonic())));
        }
    }
}
pub trait BatteryProvider: Send {
    fn id(&self) -> &'static str;
    fn poll(&mut self, hid: &dyn HidTransport, context: &PollContext<'_>) -> PollResult;
    fn diagnostics(&self) -> Vec<String>;
    /// Real connection events invalidate provider-specific connection caches.
    fn invalidate(&mut self) {}
    /// Additional scheduled retries (for delayed Windows battery properties).
    fn next_poll_delay(&self) -> Option<Duration> {
        None
    }
}
pub trait NotificationSink {
    fn deliver(&mut self, notification: &Notification) -> Result<(), ProviderError>;
}
pub trait HistoryStore {
    fn record(&mut self, reading: &Reading) -> Result<(), ProviderError>;
    fn query(
        &self,
        key: &str,
        since: i64,
        until: i64,
        max_points: usize,
    ) -> Result<Vec<Reading>, ProviderError>;
    fn flush(&mut self) -> Result<(), ProviderError>;
}

/// Deterministic application-owned GUID bytes. Two independent FNV streams avoid
/// runtime-randomized hashes; this is an identity, not a security primitive.
pub fn stable_guid(key: &str) -> u128 {
    let mut a = 0xcbf29ce484222325u64;
    let mut b = 0x84222325cbf29ce4u64;
    for x in key.bytes() {
        a = (a ^ u64::from(x)).wrapping_mul(0x100000001b3);
        b = (b ^ u64::from(x)).wrapping_mul(0x100000001b3);
    }
    ((a as u128) << 64) | b as u128
}

pub mod sleep;
pub use sleep::*;
