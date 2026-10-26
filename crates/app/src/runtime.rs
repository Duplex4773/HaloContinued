//! Engine and storage ownership. Workers move providers, never share mutable state.
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select};
use hb_core::*;
use hb_windows::{BluetoothProvider, ControllerProvider, WindowsHid};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Devices::Enumeration::{DeviceInformation, DeviceInformationUpdate, DeviceWatcher},
    Foundation::{EventHandler, TypedEventHandler},
    Gaming::Input::RawGameController,
    Win32::System::Com::*,
};
pub enum Command {
    ReleaseHistoryCache,
    Refresh,
    DeviceChanged {
        arrival_or_removal: bool,
    },
    ConfigurationVisible(bool),
    ConfigurationRefresh,
    Suspend,
    Resume,
    NotificationFailed(Notification),
    Settings(Settings),
    History {
        key: String,
        since: i64,
        until: i64,
        width: usize,
        request: u64,
    },
    UsageHistory {
        key: String,
        seconds: i64,
        until: i64,
        width: usize,
        request: u64,
    },
    Insights {
        key: String,
        until: i64,
        request: u64,
    },
    Polling(ControlRequest, u64, Option<u64>),
    Sleep(SleepRequest, u64, u64),
    Dpi(DpiRequest, u64, u64),
    DeviceInfo(Box<DeviceInfoRequest>, Arc<AtomicBool>, u64),
    SettingsChanged,
    EpochSuspend(u64),
    EpochResume(u64),
    Quit,
}
pub enum Event {
    ConfigurationInventory {
        generation: u64,
        devices: Vec<ConfigurationDevice>,
        failure: Option<String>,
    },
    Polling(Box<ControlOutcome>),
    Sleep(Box<SleepOutcome>),
    Dpi(Box<DpiOutcome>),
    DeviceInfo(Box<DeviceInfoOutcome>),
    PollingInvalidated(u64),
    Snapshot(Snapshot),
    Heartbeat(i64),
    Alert(Notification),
    History(u64, Result<HistorySeries, ProviderError>),
    Insights(u64, Result<Box<BatteryInsights>, ProviderError>),
    Error(String),
    Diagnostics(BTreeMap<String, Vec<String>>),
}
pub(super) enum Storage {
    ReleaseHistoryCache,
    DpiBackup(Box<DpiBackup>, Sender<Result<(), ProviderError>>),
    Recovery(
        Arc<Mutex<crate::recovery::Journal>>,
        Box<crate::recovery::Operation>,
        Sender<Result<(), ProviderError>>,
    ),
    PollingActivity(PollingActivity),
    UsageSample(Vec<UsageObservation>),
    Save(Settings),
    State(Estimator),
    Status(Snapshot, bool),
    RemoveStatus,
    History(String, i64, i64, usize, u64),
    UsageHistory(String, i64, i64, usize, u64),
    Insights(String, i64, u64),
    Quit,
}
#[cfg(test)]
impl Storage {
    fn unconfirmed(readings: Vec<Reading>) -> Self {
        Self::UsageSample(
            readings
                .into_iter()
                .map(|reading| UsageObservation {
                    reading,
                    polling_rate: None,
                    session: None,
                })
                .collect(),
        )
    }
}
struct Job {
    provider: Box<dyn BatteryProvider>,
    full_mode: bool,
    epoch: u64,
    generation: u64,
}
impl Job {
    fn new(provider: Box<dyn BatteryProvider>, settings: &Settings) -> Self {
        Self {
            provider,
            full_mode: settings.playstation_full_mode,
            epoch: 0,
            generation: 0,
        }
    }
}
struct Completed {
    provider: Box<dyn BatteryProvider>,
    result: PollResult,
    epoch: u64,
    generation: u64,
}
fn battery_completion_current(
    completed: &Completed,
    settings: &Settings,
    epoch: u64,
    generation: u64,
    suspended: bool,
) -> bool {
    completed.epoch == epoch
        && completed.generation == generation
        && !suspended
        && settings.enabled(completed.provider.id())
}
enum WorkerJob {
    DeviceInfo(Box<DeviceInfoRequest>, Arc<AtomicBool>, u64),
    Dpi(Box<DpiRequest>, Arc<AtomicBool>, u64, u64),
    Sleep(
        Box<SleepRequest>,
        Arc<AtomicBool>,
        u64,
        u64,
        Option<crate::recovery::Operation>,
    ),
    Battery(Job),
    Polling(
        Box<ControlRequest>,
        Arc<AtomicBool>,
        u64,
        Option<u64>,
        Option<crate::boost::Guard>,
        Option<crate::recovery::Operation>,
    ),
    Configuration(Arc<ConfigurationWatch>, u64, u64),
}
enum WorkerCompleted {
    DeviceInfo(Box<DeviceInfoOutcome>, u64),
    Dpi(Box<DpiOutcome>, u64, u64),
    Sleep(Box<SleepOutcome>, u64, u64, Option<Box<RecoveryReceipt>>),
    Battery(Completed),
    BatteryGateBusy(Job),
    Polling(
        Box<ControlOutcome>,
        u64,
        Option<u64>,
        Option<Box<RecoveryReceipt>>,
    ),
    Configuration(u64, u64, Result<Vec<ConfigurationDevice>, ProviderError>),
}
struct RecoveryReceipt {
    device: ConfigurationDevice,
}

fn recovery_receipt(target: ControlTarget, manual: bool) -> Option<Box<RecoveryReceipt>> {
    manual.then(|| {
        Box::new(RecoveryReceipt {
            device: target.device,
        })
    })
}

fn apply_recovery_receipt(
    receipt: Option<&RecoveryReceipt>,
    kind: crate::recovery::SettingKind,
    saver: &mut crate::battery_saver::BatterySaver,
    boost: &mut crate::boost::Boost,
) {
    if let Some(receipt) = receipt {
        saver.manual(&receipt.device, kind == crate::recovery::SettingKind::Sleep);
        if kind == crate::recovery::SettingKind::Polling {
            boost.manual_device(&receipt.device);
        }
    }
}
enum Work {
    Command(Result<Command, crossbeam_channel::RecvError>),
    Completed(Result<WorkerCompleted, crossbeam_channel::RecvError>),
    Idle,
}
fn effective_interval(interval: u64, quiet: bool) -> Duration {
    Duration::from_secs(if quiet { interval.max(300) } else { interval })
}
fn provider_delay(interval: u64, quiet: bool, pending: Option<Duration>) -> Duration {
    if quiet {
        effective_interval(interval, true)
    } else {
        pending
            .unwrap_or(effective_interval(interval, false))
            .max(Duration::from_secs(1))
    }
}
fn startup_discovery_delay(
    delay: Duration,
    empty: bool,
    elapsed: Duration,
    quiet: bool,
) -> Duration {
    if empty && !quiet && elapsed < Duration::from_secs(90) {
        delay.min(Duration::from_secs(10))
    } else {
        delay
    }
}

#[cfg(test)]
mod upstream115_scheduling_tests {
    use super::*;
    #[test]
    fn fast_startup_retry_is_bounded_and_does_not_override_gaming_or_known_devices() {
        for (empty, elapsed, quiet, want) in [
            (true, 0, false, 10),
            (true, 89, false, 10),
            (true, 90, false, 60),
            (false, 0, false, 60),
            (true, 0, true, 60),
        ] {
            assert_eq!(
                startup_discovery_delay(
                    Duration::from_secs(60),
                    empty,
                    Duration::from_secs(elapsed),
                    quiet
                ),
                Duration::from_secs(want)
            );
        }
        assert_eq!(
            startup_discovery_delay(Duration::from_secs(3), true, Duration::ZERO, false),
            Duration::from_secs(3)
        );
    }
    #[test]
    fn input_wake_only_checks_eligible_sleeping_devices_and_preserves_five_second_bound() {
        let now = Instant::now();
        let mut wake = InputWake::new(now);
        assert!(!wake.due(now, false));
        assert!(wake.due(now, true));
        assert!(!wake.observe(Some(10)));
        assert!(!wake.due(now + Duration::from_secs(4), true));
        assert!(wake.due(now + Duration::from_secs(5), true));
        assert!(wake.observe(Some(11)));
        assert!(wake.due(now + Duration::from_secs(10), true));
        assert!(!wake.observe(Some(11)));
        assert!(!wake.due(now + Duration::from_secs(20), false));
        assert!(wake.due(now + Duration::from_secs(20), true));
        assert!(!wake.observe(Some(50)));
        assert!(wake.due(now + Duration::from_secs(25), true));
        assert!(!wake.observe(None));
    }
}
struct InputWake {
    next: Instant,
    previous: Option<u32>,
}
impl InputWake {
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            previous: None,
        }
    }
    fn due(&mut self, now: Instant, wakeable: bool) -> bool {
        if !wakeable {
            self.previous = None;
            return false;
        }
        if now < self.next {
            return false;
        }
        self.next = now + Duration::from_secs(5);
        true
    }
    fn observe(&mut self, tick: Option<u32>) -> bool {
        let changed = self.previous.zip(tick).is_some_and(|(old, new)| old != new);
        self.previous = tick;
        changed
    }
}
fn battery_provider_delay(
    id: &str,
    interval: u64,
    quiet: bool,
    pending: Option<Duration>,
) -> Duration {
    // JBL only drains unsolicited reports from its already-open collection.
    // Gaming throttles active queries, not this bounded passive collection work.
    provider_delay(interval, quiet && id != "jbl", pending)
}
fn release_inactive_provider_handles(
    providers: &mut BTreeMap<&str, Box<dyn BatteryProvider>>,
    settings: &Settings,
    suspended: bool,
) {
    for (id, provider) in providers.iter_mut() {
        if suspended || !settings.enabled(id) {
            provider.invalidate();
        }
    }
}
#[cfg(test)]
mod passive_lifecycle_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Provider(&'static str, Arc<AtomicUsize>);
    impl BatteryProvider for Provider {
        fn id(&self) -> &'static str {
            self.0
        }
        fn poll(&mut self, _: &dyn HidTransport, _: &PollContext<'_>) -> PollResult {
            Ok(vec![])
        }
        fn diagnostics(&self) -> Vec<String> {
            vec![]
        }
        fn invalidate(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn disable_and_suspend_release_passive_handles_without_polling() {
        let jbl = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let mut providers: BTreeMap<&str, Box<dyn BatteryProvider>> = BTreeMap::from([
            (
                "jbl",
                Box::new(Provider("jbl", jbl.clone())) as Box<dyn BatteryProvider>,
            ),
            (
                "razer",
                Box::new(Provider("razer", other.clone())) as Box<dyn BatteryProvider>,
            ),
        ]);
        let mut settings = Settings::default();
        release_inactive_provider_handles(&mut providers, &settings, false);
        assert_eq!(jbl.load(Ordering::Relaxed), 0);
        settings.disabled_providers.insert("jbl".into());
        release_inactive_provider_handles(&mut providers, &settings, false);
        assert_eq!(jbl.load(Ordering::Relaxed), 1);
        assert_eq!(other.load(Ordering::Relaxed), 0);
        release_inactive_provider_handles(&mut providers, &settings, true);
        assert_eq!(jbl.load(Ordering::Relaxed), 2);
        assert_eq!(other.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn quiet_mode_throttles_queries_but_keeps_bounded_passive_collection() {
        assert_eq!(
            battery_provider_delay("jbl", 60, true, Some(Duration::from_secs(1))),
            Duration::from_secs(1)
        );
        assert_eq!(
            battery_provider_delay("razer", 60, true, Some(Duration::from_secs(1))),
            Duration::from_secs(300)
        );
        assert_eq!(
            battery_provider_delay("jbl", 60, true, None),
            Duration::from_secs(60)
        );
        assert_eq!(
            battery_provider_delay("jbl", 60, false, Some(Duration::ZERO)),
            Duration::from_secs(1)
        );
    }
}
fn schedule_completed_provider<'a>(
    due: &mut BTreeMap<&'a str, Instant>,
    id: &'a str,
    now: Instant,
    delay: Duration,
) {
    // Explicit connection/settings refreshes can already have scheduled this
    // provider while its old job was in flight. Preserve that request. With no
    // explicit refresh, a stale result must not create a zero-delay retry loop.
    due.entry(id)
        .or_insert(now + delay.max(Duration::from_secs(1)));
}
fn provider_queue_wait<'a>(
    settings: &Settings,
    due: impl Iterator<Item = (&'a str, Instant)>,
    now: Instant,
    paused: bool,
    hid_full: bool,
    winrt_full: bool,
) -> Duration {
    due.filter(|(id, _)| {
        !paused
            && settings.enabled(id)
            && if ["bluetooth", "xinput"].contains(id) {
                !winrt_full
            } else {
                !hid_full
            }
    })
    .map(|(_, at)| at.saturating_duration_since(now))
    .min()
    .unwrap_or(Duration::from_secs(5))
}
fn schedule_connection_refresh<'a>(
    due: &mut BTreeMap<&'a str, Instant>,
    invalidated: &mut BTreeSet<&'a str>,
    ids: impl Iterator<Item = &'a str>,
    now: Instant,
    settings: &Settings,
    quiet: bool,
) {
    let delay = if quiet {
        effective_interval(settings.interval, true)
    } else {
        Duration::from_secs(2)
    };
    for id in ids {
        invalidated.insert(id);
        let next = now + delay;
        due.entry(id)
            .and_modify(|at| *at = (*at).min(next))
            .or_insert(next);
    }
}
fn poll_is_due(
    settings: &Settings,
    id: &str,
    due: Option<Instant>,
    now: Instant,
    paused: bool,
) -> bool {
    !paused && settings.enabled(id) && due.is_some_and(|t| t <= now)
}
fn leave_quiet_mode<'a>(
    was_quiet: bool,
    quiet: bool,
    ids: impl Iterator<Item = &'a str>,
    due: &mut BTreeMap<&'a str, Instant>,
    now: Instant,
) {
    if was_quiet && !quiet {
        for id in ids {
            due.insert(id, now);
        }
    }
}
#[derive(Clone)]
pub(super) struct Events {
    tx: Sender<Event>,
    window: Arc<AtomicUsize>,
}
impl Events {
    fn wake(&self) {
        let hwnd = self.window.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(windows::Win32::Foundation::HWND(hwnd as *mut _)),
                    windows::Win32::UI::WindowsAndMessaging::WM_APP + 7,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    }
    pub(super) fn send(&self, event: Event) -> Result<(), crossbeam_channel::SendError<Event>> {
        let r = self.tx.send(event);
        if r.is_ok() {
            self.wake()
        }
        r
    }
    fn try_send(&self, event: Event) -> Result<(), crossbeam_channel::TrySendError<Event>> {
        let r = self.tx.try_send(event);
        if r.is_ok() {
            self.wake()
        }
        r
    }
}
fn remember_failed_notification(
    engine: &Engine,
    pending: &mut BTreeMap<(String, NotificationKind), (Instant, Notification)>,
    notification: Notification,
    now: Instant,
) {
    if !engine.notification_current(&notification) {
        return;
    }
    let key = (notification.key.clone(), notification.kind);
    if pending.len() < 512 || pending.contains_key(&key) {
        pending.insert(key, (now + Duration::from_secs(30), notification));
    }
}
fn update_diagnostics(
    diagnostics: &mut BTreeMap<String, Vec<String>>,
    key: &str,
    value: Vec<String>,
) -> bool {
    if let Some(previous) = diagnostics.get_mut(key) {
        if *previous == value {
            return false;
        }
        *previous = value;
    } else {
        diagnostics.insert(key.into(), value);
    }
    true
}
fn publish_diagnostics(
    events: &Events,
    diagnostics: &BTreeMap<String, Vec<String>>,
    dirty: &mut bool,
) {
    // Keep the pending update on backpressure; a later pass retries it.
    if *dirty
        && !events.tx.is_full()
        && events
            .try_send(Event::Diagnostics(diagnostics.clone()))
            .is_ok()
    {
        *dirty = false;
    }
}
/// Keep the five-second freshness/status heartbeat, without rebuilding and
/// exporting battery state for unrelated commands or empty provider completions.
struct SnapshotPublisher {
    changed: bool,
    next: Instant,
    unpublished: bool,
}
impl SnapshotPublisher {
    fn new(now: Instant) -> Self {
        Self {
            changed: true,
            next: now,
            unpublished: true,
        }
    }
    fn delay(&self, now: Instant) -> Duration {
        if self.changed {
            Duration::ZERO
        } else {
            self.next.saturating_duration_since(now)
        }
    }
    fn publish(
        &mut self,
        engine: &Engine,
        timestamp: i64,
        now: Instant,
        events: &Events,
        storage: &Sender<Storage>,
    ) {
        if !self.changed && now < self.next {
            return;
        }
        if self.changed || self.unpublished || engine.settings.status_file {
            let snapshot = engine.snapshot(timestamp);
            self.unpublished = if engine.settings.status_file {
                let rejected = events.try_send(Event::Snapshot(snapshot.clone())).is_err();
                let _ = storage.send(Storage::Status(snapshot, true));
                rejected
            } else {
                events.try_send(Event::Snapshot(snapshot)).is_err()
            };
        } else {
            // Freshness labels and theme recovery keep the same cadence without
            // rebuilding readings, estimates, strings and inventory every tick.
            let _ = events.try_send(Event::Heartbeat(timestamp));
        }
        self.changed = false;
        self.next = now + Duration::from_secs(5);
    }
}
fn battery_affects_snapshot(engine: &Engine, provider: &str, result: &PollResult) -> bool {
    match result {
        Ok(readings) => {
            engine.provider_error(provider).is_some()
                || engine.provider_readings(provider) != readings
        }
        Err(error) => engine.provider_error(provider) != Some(error.message.as_str()),
    }
}
/// A delayed/cached result can come from before a clock adjustment. Preserve
/// the last-known value, but never treat its future timestamp as fresh evidence.
fn reject_future_battery_readings(result: &mut PollResult, now: i64) -> bool {
    let mut future = false;
    if let Ok(readings) = result {
        for reading in readings {
            if reading.timestamp > now {
                reading.connection = Connection::Stale;
                future = true;
            }
        }
    }
    future
}
/// Availability is an event at detection time, not a correction to an old poll.
fn availability_boundary(mut reading: Reading, state: Connection, now: i64) -> Reading {
    // History assigns an ordered sequence to observations in the same second.
    // Never move a boundary into the future to avoid a timestamp collision.
    reading.timestamp = now;
    reading.connection = state;
    reading
}
fn completed_observations(
    previous: &[Reading],
    result: &PollResult,
    now: i64,
    allowed: bool,
) -> Vec<Reading> {
    if !allowed {
        return Vec::new();
    }
    let mut observations = result.as_ref().map_or_else(|_| Vec::new(), Clone::clone);
    for reading in &mut observations {
        if reading.level.is_some_and(|level| level > 100) {
            reading.level = None;
        }
    }
    for old in previous {
        if old.connection != Connection::Stale
            && !observations.iter().any(|reading| reading.key == old.key)
        {
            observations.push(availability_boundary(old.clone(), Connection::Stale, now));
        }
    }
    observations
}
pub(crate) struct ControlPermission {
    enabled: AtomicBool,
    epoch: AtomicU64,
    suspended: AtomicBool,
    desired_enabled: AtomicBool,
    acknowledged: AtomicU64,
    pending_settings: Mutex<Option<(Settings, u64)>>,
    pending_resume: AtomicU64,
}
impl ControlPermission {
    fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            epoch: AtomicU64::new(0),
            suspended: AtomicBool::new(false),
            desired_enabled: AtomicBool::new(enabled),
            acknowledged: AtomicU64::new(0),
            pending_settings: Mutex::new(None),
            pending_resume: AtomicU64::new(0),
        }
    }
}
struct Lifecycle {
    cancel: Arc<AtomicBool>,
    permission: Arc<ControlPermission>,
    configuration: Arc<ConfigurationWatch>,
}
#[derive(Default)]
struct ConfigurationWatch {
    visible: AtomicBool,
    epoch: AtomicU64,
}
impl ConfigurationWatch {
    fn set_visible(&self, visible: bool) -> bool {
        let changed = self.visible.swap(visible, Ordering::AcqRel) != visible;
        if changed {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
        changed
    }
    fn active(&self, epoch: u64) -> bool {
        self.visible.load(Ordering::Acquire) && self.epoch.load(Ordering::Acquire) == epoch
    }
}
struct WorkerAccess {
    storage: Sender<Storage>,
    recovery: Arc<Mutex<crate::recovery::Journal>>,
    gates: BTreeMap<u16, Mutex<()>>,
    permission: Arc<ControlPermission>,
    configuration: Arc<ConfigurationWatch>,
}

// Never hold some vendor gates while waiting for another: a stuck native call
// must not indefinitely occupy both I/O workers or lock unrelated vendors.
fn acquire_vendor_gates<'a>(
    vendors: &BTreeSet<u16>,
    gates: &'a BTreeMap<u16, Mutex<()>>,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<MutexGuard<'a, ()>>> {
    let mut guards = Vec::with_capacity(vendors.len());
    loop {
        if cancelled() {
            return None;
        }
        let mut blocked = false;
        for gate in vendors.iter().filter_map(|vendor| gates.get(vendor)) {
            match gate.try_lock() {
                Ok(guard) => guards.push(guard),
                Err(TryLockError::Poisoned(error)) => guards.push(error.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    blocked = true;
                    break;
                }
            }
        }
        if !blocked {
            return Some(guards);
        }
        guards.clear();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // This wait is active only while two jobs contend for a vendor.
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn gate_wait_cancelled(
    job: &WorkerJob,
    shutdown: &AtomicBool,
    configuration: &ConfigurationWatch,
    permission: &ControlPermission,
) -> bool {
    if shutdown.load(Ordering::Acquire) || permission.suspended.load(Ordering::Acquire) {
        return true;
    }
    let epoch = permission.epoch.load(Ordering::Acquire);
    match job {
        WorkerJob::Battery(job) => job.epoch != epoch,
        WorkerJob::DeviceInfo(_, cancel, visible) => {
            cancel.load(Ordering::Acquire) || !configuration.active(*visible)
        }
        WorkerJob::Dpi(_, cancel, requested, visible) => {
            *requested != epoch || cancel.load(Ordering::Acquire) || !configuration.active(*visible)
        }
        WorkerJob::Sleep(_, cancel, requested, visible, _) => {
            *requested != epoch
                || cancel.load(Ordering::Acquire)
                || (*visible != crate::battery_saver::BACKGROUND && !configuration.active(*visible))
        }
        WorkerJob::Polling(_, cancel, requested, visible, boost, _) => {
            *requested != epoch
                || cancel.load(Ordering::Acquire)
                || visible.is_some_and(|epoch| !configuration.active(epoch))
                || boost
                    .as_ref()
                    .is_some_and(|guard| guard.cancel.load(Ordering::Acquire))
        }
        WorkerJob::Configuration(watch, epoch, _) => !watch.active(*epoch),
    }
}

fn vendor_gate_busy(job: WorkerJob) -> WorkerCompleted {
    const MESSAGE: &str = "The device is busy. Try again shortly.";
    match job {
        WorkerJob::Battery(job) => WorkerCompleted::BatteryGateBusy(job),
        WorkerJob::DeviceInfo(request, _, visible) => WorkerCompleted::DeviceInfo(
            Box::new(DeviceInfoOutcome::failed(&request, MESSAGE)),
            visible,
        ),
        WorkerJob::Dpi(request, _, epoch, visible) => WorkerCompleted::Dpi(
            Box::new(DpiOutcome::failed(&request, MESSAGE)),
            epoch,
            visible,
        ),
        WorkerJob::Sleep(request, _, epoch, visible, _) => {
            let outcome = SleepOutcome::failed(&request, MESSAGE);
            let manual = request.seconds.is_some()
                && request.request & crate::battery_saver::REQUEST_BIT == 0;
            WorkerCompleted::Sleep(
                Box::new(outcome),
                epoch,
                visible,
                recovery_receipt(request.target, manual),
            )
        }
        WorkerJob::Polling(request, _, epoch, visible, _, _) => {
            let outcome = ControlOutcome::failed(&request, MESSAGE);
            let manual = manual_polling_change(&request);
            WorkerCompleted::Polling(
                Box::new(outcome),
                epoch,
                visible,
                recovery_receipt(request.target, manual),
            )
        }
        WorkerJob::Configuration(_, epoch, generation) => {
            WorkerCompleted::Configuration(epoch, generation, Err(ProviderError::new(MESSAGE)))
        }
    }
}
pub struct Runtime {
    commands: Sender<Command>,
    pub events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
    permission: Arc<ControlPermission>,
    sink: Events,
    cancel: Arc<AtomicBool>,
    window: Arc<AtomicUsize>,
    configuration: Arc<ConfigurationWatch>,
}
impl Runtime {
    pub fn start(dir: PathBuf, settings: Settings, simulate: bool) -> Result<Self, ProviderError> {
        let hid = Arc::new(WindowsHid::new()?);
        let cancel = Arc::new(AtomicBool::new(false));
        let (commands, rx) = bounded(32);
        let (events, events_rx) = bounded(64);
        let cancelled = cancel.clone();
        let permission = Arc::new(ControlPermission::new(settings.polling_controls));
        let configuration = Arc::new(ConfigurationWatch::default());
        let lifecycle = Lifecycle {
            cancel: cancelled,
            permission: permission.clone(),
            configuration: configuration.clone(),
        };
        let window = Arc::new(AtomicUsize::new(0));
        let sink = Events {
            tx: events,
            window: window.clone(),
        };
        let output = sink.clone();
        let thread = thread::Builder::new()
            .name("state".into())
            .stack_size(512 * 1024)
            .spawn(move || run(dir, settings, simulate, hid, lifecycle, rx, output))?;
        Ok(Self {
            commands,
            events: events_rx,
            thread: Some(thread),
            permission,
            sink,
            cancel,
            window,
            configuration,
        })
    }
    pub fn attach_window(&self, hwnd: usize) {
        self.window.store(hwnd, Ordering::Release);
        if hwnd != 0 && !self.events.is_empty() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(windows::Win32::Foundation::HWND(hwnd as *mut _)),
                    windows::Win32::UI::WindowsAndMessaging::WM_APP + 7,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    }
    pub fn send(&self, command: Command) {
        let command = match command {
            Command::ConfigurationVisible(visible) => {
                if !self.configuration.set_visible(visible) {
                    return;
                }
                Command::ConfigurationVisible(visible)
            }
            Command::Settings(settings) => {
                self.permission.enabled.store(false, Ordering::Release);
                let epoch = self.permission.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                self.permission
                    .desired_enabled
                    .store(settings.polling_controls, Ordering::Release);
                *self
                    .permission
                    .pending_settings
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some((settings, epoch));
                Command::SettingsChanged
            }
            Command::Suspend => {
                self.permission.suspended.store(true, Ordering::Release);
                self.permission.enabled.store(false, Ordering::Release);
                let epoch = self.permission.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                Command::EpochSuspend(epoch)
            }
            Command::Resume => {
                self.permission.suspended.store(false, Ordering::Release);
                self.permission.enabled.store(false, Ordering::Release);
                let epoch = self.permission.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                Command::EpochResume(epoch)
            }
            command => command,
        };
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(Command::EpochResume(epoch))) => {
                // Keep the exact epoch; a later suspend or settings change will
                // invalidate it before it can reauthorize device controls.
                self.permission
                    .pending_resume
                    .fetch_max(epoch, Ordering::AcqRel);
            }
            Err(error) => {
                let message = if matches!(error, TrySendError::Disconnected(_)) {
                    "Device monitoring stopped. Restart Halo Continued."
                } else {
                    "Command queue is busy; retry the action. Device configuration remains revoked until settings or resume is acknowledged."
                };
                let _ = self.sink.try_send(Event::Error(message.into()));
            }
        }
    }
    pub fn retry_pending_resume(&self) {
        let epoch = self.permission.pending_resume.load(Ordering::Acquire);
        if epoch == 0 {
            return;
        }
        let obsolete = self.permission.epoch.load(Ordering::Acquire) != epoch
            || self.permission.suspended.load(Ordering::Acquire);
        let done = obsolete
            || !matches!(
                self.commands.try_send(Command::EpochResume(epoch)),
                Err(TrySendError::Full(_))
            );
        if done {
            let _ = self.permission.pending_resume.compare_exchange(
                epoch,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
    pub fn configuration_visible(&self) -> bool {
        self.configuration.visible.load(Ordering::Acquire)
    }
    pub fn release_history_cache(&self) -> bool {
        self.commands.try_send(Command::ReleaseHistoryCache).is_ok()
    }
    pub fn request_history(&self, command: Command) -> Result<(), ProviderError> {
        if !matches!(
            command,
            Command::History { .. } | Command::UsageHistory { .. }
        ) {
            return Err(ProviderError::new("Not a battery history request"));
        }
        self.commands.try_send(command).map_err(|_| {
            ProviderError::new("Battery history is busy. Select Refresh to try again.")
        })
    }
    pub fn request_insights(
        &self,
        key: String,
        until: i64,
        request: u64,
    ) -> Result<(), ProviderError> {
        self.commands
            .try_send(Command::Insights {
                key,
                until,
                request,
            })
            .map_err(|_| {
                ProviderError::new("Battery history is busy. Select Refresh to try again.")
            })
    }
    pub fn request_device_info(
        &self,
        request: DeviceInfoRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), ProviderError> {
        let visible = self.configuration.epoch.load(Ordering::Acquire);
        if self.permission.suspended.load(Ordering::Acquire) || !self.configuration.active(visible)
        {
            return Err(ProviderError::new(
                "Open Devices to read device information.",
            ));
        }
        self.commands
            .try_send(Command::DeviceInfo(Box::new(request), cancel, visible))
            .map_err(|_| {
                ProviderError::new("Device workers are busy. Try Refresh information shortly.")
            })
    }
    pub fn submit_sleep(&self, request: SleepRequest) -> Result<(), ProviderError> {
        let epoch = self.permission.epoch.load(Ordering::Acquire);
        let visible = self.configuration.epoch.load(Ordering::Acquire);
        if !self.permission.enabled.load(Ordering::Acquire)
            || !self.permission.desired_enabled.load(Ordering::Acquire)
            || self.permission.suspended.load(Ordering::Acquire)
            || self.permission.acknowledged.load(Ordering::Acquire) != epoch
            || !self.configuration.active(visible)
        {
            return Err(ProviderError::new(
                "Allow device settings changes and open Devices first.",
            ));
        }
        self.commands
            .try_send(Command::Sleep(request, epoch, visible))
            .map_err(|_| ProviderError::new("Device workers are busy. Try again shortly."))
    }
    pub fn submit_dpi(&self, request: DpiRequest) -> Result<(), ProviderError> {
        let epoch = self.permission.epoch.load(Ordering::Acquire);
        let visible = self.configuration.epoch.load(Ordering::Acquire);
        if !self.permission.enabled.load(Ordering::Acquire)
            || !self.permission.desired_enabled.load(Ordering::Acquire)
            || self.permission.suspended.load(Ordering::Acquire)
            || self.permission.acknowledged.load(Ordering::Acquire) != epoch
            || !self.configuration.active(visible)
        {
            return Err(ProviderError::new(
                "Allow device settings changes and open Devices first.",
            ));
        }
        self.commands
            .try_send(Command::Dpi(request, epoch, visible))
            .map_err(|_| ProviderError::new("Device workers are busy. Try again shortly."))
    }
    pub fn submit_control(&self, request: ControlRequest) -> Result<(), ProviderError> {
        let epoch = self.permission.epoch.load(Ordering::Acquire);
        if !self.permission.enabled.load(Ordering::Acquire)
            || !self.permission.desired_enabled.load(Ordering::Acquire)
            || self.permission.suspended.load(Ordering::Acquire)
            || self.permission.acknowledged.load(Ordering::Acquire) != epoch
        {
            return Err(ProviderError::new(
                "Polling controls are disabled or awaiting settings/resume acknowledgment; retry Refresh",
            ));
        }
        let configuration_epoch = (request.target.device.kind == "keyboard")
            .then(|| self.configuration.epoch.load(Ordering::Acquire));
        if configuration_epoch.is_some_and(|epoch| !self.configuration.active(epoch)) {
            return Err(ProviderError::new(
                "Open Devices before reading or configuring a keyboard",
            ));
        }
        self.commands
            .try_send(Command::Polling(request, epoch, configuration_epoch))
            .map_err(|e| ProviderError::new(format!("Configuration request rejected: {e}")))
    }
    pub fn stop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.configuration.set_visible(false);
        let _ = self.commands.try_send(Command::Quit);
        if let Some(t) = self.thread.take()
            && wait_for_thread(&t, Instant::now() + Duration::from_secs(8), || {
                while self.events.try_recv().is_ok() {}
            })
        {
            let _ = t.join();
        }
    }
}
fn wait_for_thread<T>(thread: &JoinHandle<T>, until: Instant, mut progress: impl FnMut()) -> bool {
    while !thread.is_finished() {
        progress();
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
    true
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop()
    }
}
fn relevant_device_event(id: &str) -> bool {
    let id = id.to_ascii_uppercase();
    id.contains("BTH") || id.contains("HID#") || id.contains("BLUETOOTH")
}
/// The watcher's initial listing is discovery, not a connection transition.
/// Providers already perform startup discovery; forwarding that listing would
/// revoke a startup polling readback once for every installed HID device.
#[derive(Default)]
struct DeviceEventGate {
    enumeration_completed: AtomicBool,
}
impl DeviceEventGate {
    fn forward(&self, commands: &Sender<Command>, id: &str, arrival_or_removal: bool) {
        if self.enumeration_completed.load(Ordering::Acquire) && relevant_device_event(id) {
            let _ = commands.try_send(Command::DeviceChanged { arrival_or_removal });
        }
    }
}
struct DeviceEvents {
    watcher: DeviceWatcher,
    added: Option<i64>,
    updated: Option<i64>,
    removed: Option<i64>,
    enumeration_completed: Option<i64>,
}
impl DeviceEvents {
    fn new(commands: &Sender<Command>) -> Option<Self> {
        let watcher = DeviceInformation::CreateWatcher().ok()?;
        let gate = Arc::new(DeviceEventGate::default());
        let completed_gate = gate.clone();
        let enumeration_completed = watcher
            .EnumerationCompleted(&TypedEventHandler::<_, windows::core::IInspectable>::new(
                move |_, _| {
                    completed_gate
                        .enumeration_completed
                        .store(true, Ordering::Release);
                    Ok(())
                },
            ))
            .ok()?;
        let tx = commands.clone();
        let added_gate = gate.clone();
        let added = watcher
            .Added(&TypedEventHandler::<_, DeviceInformation>::new(
                move |_, args| {
                    if let Some(id) = args.as_ref().and_then(|a| a.Id().ok()) {
                        added_gate.forward(&tx, &id.to_string(), true);
                    }
                    Ok(())
                },
            ))
            .ok();
        let tx = commands.clone();
        let updated_gate = gate.clone();
        let updated = watcher
            .Updated(&TypedEventHandler::<_, DeviceInformationUpdate>::new(
                move |_, args| {
                    if let Some(id) = args.as_ref().and_then(|a| a.Id().ok()) {
                        updated_gate.forward(&tx, &id.to_string(), false);
                    }
                    Ok(())
                },
            ))
            .ok();
        let tx = commands.clone();
        let removed = watcher
            .Removed(&TypedEventHandler::<_, DeviceInformationUpdate>::new(
                move |_, args| {
                    if let Some(id) = args.as_ref().and_then(|a| a.Id().ok()) {
                        gate.forward(&tx, &id.to_string(), true);
                    }
                    Ok(())
                },
            ))
            .ok();
        let events = Self {
            watcher,
            added,
            updated,
            removed,
            enumeration_completed: Some(enumeration_completed),
        };
        events.watcher.Start().ok()?;
        Some(events)
    }
}
impl Drop for DeviceEvents {
    fn drop(&mut self) {
        let _ = self.watcher.Stop();
        if let Some(token) = self.added.take() {
            let _ = self.watcher.RemoveAdded(token);
        }
        if let Some(token) = self.updated.take() {
            let _ = self.watcher.RemoveUpdated(token);
        }
        if let Some(token) = self.removed.take() {
            let _ = self.watcher.RemoveRemoved(token);
        }
        if let Some(token) = self.enumeration_completed.take() {
            let _ = self.watcher.RemoveEnumerationCompleted(token);
        }
    }
}

fn recovery_operation(
    access: &WorkerAccess,
    operation: crate::recovery::Operation,
) -> Result<(), ProviderError> {
    let (reply, response) = bounded(1);
    access
        .storage
        .try_send(Storage::Recovery(
            access.recovery.clone(),
            Box::new(operation),
            reply,
        ))
        .map_err(|_| ProviderError::new("Recovery storage is busy."))?;
    response
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| ProviderError::new("Recovery storage did not respond."))?
}

fn needs_recovery_probe(operation: &crate::recovery::Operation) -> bool {
    match operation {
        crate::recovery::Operation::ValidateRestore {
            device, startup, ..
        } => startup.load(Ordering::Acquire) && crate::recovery::strong_identity(device),
        _ => false,
    }
}

/// Rebind only a journal loaded at startup and only after a provider has
/// independently read the same serial/container and its current value.
fn reconcile_recovery_probe(
    access: &WorkerAccess,
    operation: &mut crate::recovery::Operation,
    observed_stamp: ConnectionStamp,
    observed: u32,
) -> Result<(), ProviderError> {
    use crate::recovery::Operation;
    let Operation::ValidateRestore {
        device,
        stamp,
        kind,
        expected,
        desired,
        ..
    } = operation
    else {
        return Err(ProviderError::new("Recovery proof is unavailable."));
    };
    if observed != *expected {
        recovery_operation(
            access,
            Operation::FinishRestore {
                device: device.clone(),
                stamp: *stamp,
                kind: *kind,
                expected: *expected,
                desired: *desired,
                actual: observed,
            },
        )?;
        return Err(ProviderError::new(
            "The setting changed elsewhere; the previous automatic restore was cancelled.",
        ));
    }
    if observed_stamp != *stamp {
        recovery_operation(
            access,
            Operation::Rebind {
                device: device.clone(),
                old_stamp: *stamp,
                new_stamp: observed_stamp,
                kind: *kind,
                expected: *expected,
                desired: *desired,
            },
        )?;
        *stamp = observed_stamp;
    }
    Ok(())
}

fn probe_polling_recovery(
    request: &mut ControlRequest,
    operation: &mut crate::recovery::Operation,
    access: &WorkerAccess,
    read: impl FnOnce(&ControlRequest) -> ControlOutcome,
) -> Result<(), ProviderError> {
    if !needs_recovery_probe(operation) {
        return Ok(());
    }
    let mut probe = request.clone();
    probe.target.connection_stamp = None;
    probe.action = ControlAction::Read;
    let result = read(&probe);
    let observation = result
        .observation
        .as_ref()
        .filter(|observation| {
            result.failure.is_none()
                && !result.may_have_changed
                && observation.target.device == request.target.device
                && observation.target.generation == request.target.generation
        })
        .ok_or_else(|| {
            ProviderError::new("The mouse could not be safely verified after restart.")
        })?;
    let stamp = observation
        .target
        .connection_stamp
        .ok_or_else(|| ProviderError::new("The current mouse connection could not be verified."))?;
    let rate = observation
        .rate
        .ok_or_else(|| ProviderError::new("The current polling rate could not be verified."))?;
    reconcile_recovery_probe(access, operation, stamp, rate.hz())?;
    request.target.connection_stamp = Some(stamp);
    Ok(())
}

fn probe_sleep_recovery(
    request: &mut SleepRequest,
    operation: &mut crate::recovery::Operation,
    access: &WorkerAccess,
    read: impl FnOnce(&SleepRequest) -> SleepOutcome,
) -> Result<(), ProviderError> {
    if !needs_recovery_probe(operation) {
        return Ok(());
    }
    let mut probe = request.clone();
    probe.target.connection_stamp = None;
    probe.seconds = None;
    probe.expected = None;
    let result = read(&probe);
    if result.failure.is_some()
        || result.may_have_changed
        || result.key != request.target.device.key
        || result.generation != request.target.generation
    {
        return Err(ProviderError::new(
            "The mouse could not be safely verified after restart.",
        ));
    }
    let stamp = result
        .connection_stamp
        .ok_or_else(|| ProviderError::new("The current mouse connection could not be verified."))?;
    let seconds = result
        .seconds
        .ok_or_else(|| ProviderError::new("The current sleep setting could not be verified."))?;
    reconcile_recovery_probe(access, operation, stamp, u32::from(seconds))?;
    request.target.connection_stamp = Some(stamp);
    Ok(())
}

fn polling_recovery_operation(
    request: &ControlRequest,
    boost: Option<&crate::boost::Guard>,
    saver_request: bool,
    saver: Option<(bool, hb_core::settings::LowBatteryMode)>,
) -> Option<crate::recovery::Operation> {
    use crate::recovery::{Layer, Operation, Record, SettingKind};
    let desired = request.action.rate()?.hz();
    if manual_polling_change(request) {
        return Some(Operation::ManualBegin {
            device: request.target.device.clone(),
            kind: SettingKind::Polling,
            desired,
        });
    }
    let (restoring, layer) = if let Some(guard) = boost {
        (!guard.fullscreen, Layer::BoostPolling)
    } else if let Some((restoring, choices)) = saver {
        (restoring, Layer::SaverPolling(choices))
    } else if saver_request || request.request & crate::boost::REQUEST_BIT != 0 {
        return Some(Operation::Blocked);
    } else {
        return None;
    };
    let (Some(stamp), Some(expected)) =
        (request.target.connection_stamp, request.action.expected())
    else {
        return Some(Operation::Blocked);
    };
    Some(if restoring {
        Operation::ValidateRestore {
            device: request.target.device.clone(),
            stamp,
            kind: SettingKind::Polling,
            expected: expected.hz(),
            desired,
            startup: Arc::new(AtomicBool::new(false)),
        }
    } else {
        Operation::Prepare(Record {
            device: request.target.device.clone(),
            stamp,
            rebound_stamp: None,
            before: expected.hz(),
            applied: desired,
            layer,
            confirmed: false,
            manual_pending: false,
        })
    })
}

fn sleep_recovery_operation(
    request: &SleepRequest,
    saver_request: bool,
    saver: Option<(bool, hb_core::settings::LowBatteryMode)>,
) -> Option<crate::recovery::Operation> {
    use crate::recovery::{Layer, Operation, Record, SettingKind};
    let desired = u32::from(request.seconds?);
    let Some((restoring, choices)) = saver else {
        if saver_request {
            return Some(Operation::Blocked);
        }
        return Some(Operation::ManualBegin {
            device: request.target.device.clone(),
            kind: SettingKind::Sleep,
            desired,
        });
    };
    let (Some(stamp), Some(expected)) = (request.target.connection_stamp, request.expected) else {
        return Some(Operation::Blocked);
    };
    Some(if restoring {
        Operation::ValidateRestore {
            device: request.target.device.clone(),
            stamp,
            kind: SettingKind::Sleep,
            expected: u32::from(expected),
            desired,
            startup: Arc::new(AtomicBool::new(false)),
        }
    } else {
        Operation::Prepare(Record {
            device: request.target.device.clone(),
            stamp,
            rebound_stamp: None,
            before: u32::from(expected),
            applied: desired,
            layer: Layer::SaverSleep(choices),
            confirmed: false,
            manual_pending: false,
        })
    })
}

fn finish_polling_recovery(
    operation: &crate::recovery::Operation,
    outcome: &ControlOutcome,
) -> Option<crate::recovery::Operation> {
    use crate::recovery::Operation;
    match operation {
        Operation::Prepare(record) if outcome.failure.is_some() && !outcome.may_have_changed => {
            Some(Operation::FailedPrepare(record.clone()))
        }
        Operation::Prepare(record)
            if outcome.failure.is_none()
                && outcome.observation.as_ref().is_some_and(|observed| {
                    observed.target.device == record.device
                        && observed.target.connection_stamp == Some(record.stamp)
                        && observed
                            .rate
                            .is_some_and(|rate| rate.hz() == record.applied)
                }) =>
        {
            Some(Operation::Confirm(record.clone()))
        }
        Operation::ManualBegin {
            device,
            kind,
            desired,
        } => {
            let completed = outcome.failure.is_none()
                && outcome.observation.as_ref().is_some_and(|observed| {
                    observed.target.device == *device
                        && observed.rate.is_some_and(|rate| rate.hz() == *desired)
                });
            if completed || outcome.failure.is_some() && !outcome.may_have_changed {
                Some(Operation::ManualFinish {
                    device: device.clone(),
                    kind: *kind,
                    completed,
                })
            } else {
                None
            }
        }
        Operation::ValidateRestore {
            device,
            stamp,
            kind,
            expected,
            desired,
            ..
        } => {
            let observation = outcome.observation.as_ref()?;
            if observation.target.device != *device
                || observation.target.connection_stamp != Some(*stamp)
                || outcome.failure.is_some() && outcome.may_have_changed
            {
                return None;
            }
            Some(Operation::FinishRestore {
                device: device.clone(),
                stamp: *stamp,
                kind: *kind,
                expected: *expected,
                desired: *desired,
                actual: observation.rate?.hz(),
            })
        }
        _ => None,
    }
}

fn finish_sleep_recovery(
    operation: &crate::recovery::Operation,
    outcome: &SleepOutcome,
) -> Option<crate::recovery::Operation> {
    use crate::recovery::Operation;
    match operation {
        Operation::Prepare(record) if outcome.failure.is_some() && !outcome.may_have_changed => {
            Some(Operation::FailedPrepare(record.clone()))
        }
        Operation::Prepare(record)
            if outcome.failure.is_none()
                && outcome.key == record.device.key
                && outcome.connection_stamp == Some(record.stamp)
                && outcome
                    .seconds
                    .is_some_and(|seconds| u32::from(seconds) == record.applied) =>
        {
            Some(Operation::Confirm(record.clone()))
        }
        Operation::ManualBegin {
            device,
            kind,
            desired,
        } => {
            let completed = outcome.failure.is_none()
                && outcome.key == device.key
                && outcome
                    .seconds
                    .is_some_and(|seconds| u32::from(seconds) == *desired);
            if completed || outcome.failure.is_some() && !outcome.may_have_changed {
                Some(Operation::ManualFinish {
                    device: device.clone(),
                    kind: *kind,
                    completed,
                })
            } else {
                None
            }
        }
        Operation::ValidateRestore {
            device,
            stamp,
            kind,
            expected,
            desired,
            ..
        } => {
            if outcome.key != device.key
                || outcome.connection_stamp != Some(*stamp)
                || outcome.failure.is_some() && outcome.may_have_changed
            {
                return None;
            }
            Some(Operation::FinishRestore {
                device: device.clone(),
                stamp: *stamp,
                kind: *kind,
                expected: *expected,
                desired: *desired,
                actual: u32::from(outcome.seconds?),
            })
        }
        _ => None,
    }
}

struct RecoveryContext<'a> {
    settings: &'a Settings,
    generation: u64,
    saver: &'a mut crate::battery_saver::BatterySaver,
    boost: &'a mut crate::boost::Boost,
}
fn recover_available(
    journal: &Arc<Mutex<crate::recovery::Journal>>,
    provider: &str,
    readings: &[Reading],
    now: i64,
    context: RecoveryContext<'_>,
) -> bool {
    if !context.settings.polling_controls {
        return true;
    }
    let Ok(mut journal) = journal.try_lock() else {
        // The existing storage worker may be saving another change. The next
        // normal provider event is enough to retry; no recovery timer is added.
        return true;
    };
    for record in journal.pending_records(provider, readings, now) {
        let Some(reading) = readings
            .iter()
            .find(|reading| reading.key == record.device.key)
        else {
            continue;
        };
        let accepted = match record.layer {
            crate::recovery::Layer::BoostPolling => {
                context
                    .boost
                    .recover(&record, reading, context.settings, context.generation)
            }
            _ => context
                .saver
                .recover(&record, reading, context.settings, context.generation),
        };
        if accepted {
            journal.claimed(&record);
        }
    }
    journal.has_pending()
}

fn rearm_recovery(
    journal: &Arc<Mutex<crate::recovery::Journal>>,
    device: &ConfigurationDevice,
    kind: crate::recovery::SettingKind,
    deferred: &mut Vec<(ConfigurationDevice, crate::recovery::SettingKind)>,
) -> bool {
    let Ok(mut journal) = journal.try_lock() else {
        if deferred.len() < 1536 && !deferred.iter().any(|(d, k)| d == device && *k == kind) {
            deferred.push((device.clone(), kind));
        }
        return true;
    };
    journal.rearm(device, kind);
    journal.has_pending()
}

fn flush_recovery_rearms(
    journal: &Arc<Mutex<crate::recovery::Journal>>,
    deferred: &mut Vec<(ConfigurationDevice, crate::recovery::SettingKind)>,
    all_confirmed: &mut bool,
) -> Option<bool> {
    if deferred.is_empty() && !*all_confirmed {
        return None;
    }
    let Ok(mut journal) = journal.try_lock() else {
        return None;
    };
    if *all_confirmed {
        journal.rearm_confirmed();
        *all_confirmed = false;
    }
    for (device, kind) in deferred.drain(..) {
        journal.rearm(&device, kind);
    }
    // Event-only bursts retain no spare per-device buffer while idle.
    *deferred = Vec::new();
    Some(journal.has_pending())
}

fn worker(
    jobs: Receiver<WorkerJob>,
    results: Sender<WorkerCompleted>,
    hid: Arc<WindowsHid>,
    cancel: Arc<AtomicBool>,
    winrt: bool,
    commands: Sender<Command>,
    access: Arc<WorkerAccess>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(if winrt { "winrt" } else { "hid" }.into())
        .stack_size(512 * 1024)
        .spawn(move || {
            let _apartment = Apartment::new(winrt);
            let _device_events = if winrt {
                DeviceEvents::new(&commands)
            } else {
                None
            };
            let controller_token = if winrt {
                let tx = commands.clone();
                RawGameController::RawGameControllerAdded(&EventHandler::new(move |_, _| {
                    let _ = tx.try_send(Command::DeviceChanged {
                        arrival_or_removal: true,
                    });
                    Ok(())
                }))
                .ok()
            } else {
                None
            };
            let removed_token = if winrt {
                RawGameController::RawGameControllerRemoved(&EventHandler::new(move |_, _| {
                    let _ = commands.try_send(Command::DeviceChanged {
                        arrival_or_removal: true,
                    });
                    Ok(())
                }))
                .ok()
            } else {
                None
            };
            let clock = SystemClock::default();
            while let Ok(job) = jobs.recv() {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                // A vendor gate conservatively covers every physical receiver
                // used by a protocol family, including multi-collection sessions.
                // It prevents interleaving shared-vendor protocols without locking
                // individual opens (Logitech opens both long and short channels).
                let vendors: BTreeSet<_> = match &job {
                    WorkerJob::Battery(job) => hb_providers::catalog::DEVICES
                        .iter()
                        .filter(|d| d.provider == job.provider.id())
                        .map(|d| d.vid)
                        .collect(),
                    WorkerJob::Sleep(request, _, _, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::DeviceInfo(request, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::Dpi(request, _, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::Polling(request, _, _, _, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::Configuration(_, _, _) => {
                        hb_providers::configuration::CONFIGURATION_VENDORS
                            .iter()
                            .copied()
                            .collect()
                    }
                };
                let Some(_guards) = acquire_vendor_gates(
                    &vendors,
                    &access.gates,
                    Instant::now() + Duration::from_secs(30),
                    || {
                        gate_wait_cancelled(
                            &job,
                            &cancel,
                            &access.configuration,
                            &access.permission,
                        )
                    },
                ) else {
                    if cancel.load(Ordering::Acquire)
                        || results.send(vendor_gate_busy(job)).is_err()
                    {
                        break;
                    }
                    continue;
                };
                let completed = match job {
                    WorkerJob::DeviceInfo(request, request_cancel, visible) => {
                        let context = PollContext {
                            clock: &clock,
                            cancelled: &request_cancel,
                            deadline: clock.monotonic() + Duration::from_secs(3),
                            playstation_full_mode: false,
                        };
                        let transport = InfoTransport {
                            hid: &*hid,
                            access: InfoReadAccess {
                                shutdown: cancel.clone(),
                                cancelled: request_cancel.clone(),
                                permission: access.permission.clone(),
                                configuration: access.configuration.clone(),
                                visible,
                            },
                        };
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                hb_providers::device_info::execute(&request, &transport, &context)
                            }))
                            .unwrap_or_else(|_| {
                                DeviceInfoOutcome::failed(
                                    &request,
                                    "Firmware could not be read. Try Refresh information.",
                                )
                            });
                        WorkerCompleted::DeviceInfo(Box::new(outcome), visible)
                    }
                    WorkerJob::Dpi(request, request_cancel, epoch, visible) => {
                        let context = PollContext {
                            clock: &clock,
                            cancelled: &request_cancel,
                            deadline: clock.monotonic() + Duration::from_secs(20),
                            playstation_full_mode: false,
                        };
                        let transport = ControlTransport {
                            hid: &*hid,
                            shutdown: cancel.clone(),
                            cancelled: request_cancel.clone(),
                            permission: Some((access.permission.clone(), epoch)),
                            configuration: Some((access.configuration.clone(), visible)),
                            block_while_gaming: request.value.is_some(),
                            boost: None,
                        };
                        let mut save = |backup: &DpiBackup| {
                            let (tx, rx) = bounded(1);
                            access
                                .storage
                                .try_send(Storage::DpiBackup(Box::new(backup.clone()), tx))
                                .map_err(|_| {
                                    ProviderError::new(
                                        "Could not save the original DPI. Nothing was changed.",
                                    )
                                })?;
                            while context.active() {
                                match rx.recv_timeout(Duration::from_millis(100)) {
                                    Ok(result) => return result,
                                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                                }
                            }
                            Err(ProviderError::new(
                                "Original DPI backup was not confirmed. Nothing was changed.",
                            ))
                        };
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                hb_providers::dpi::execute(
                                    &request, &transport, &context, &mut save,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                let mut o = DpiOutcome::failed(
                                    &request,
                                    "DPI request failed. Read current DPI before retrying.",
                                );
                                o.may_have_changed = request.value.is_some();
                                o
                            });
                        WorkerCompleted::Dpi(Box::new(outcome), epoch, visible)
                    }
                    WorkerJob::Sleep(mut request, request_cancel, epoch, visible, mut recovery) => {
                        let manual = request.seconds.is_some()
                            && request.request & crate::battery_saver::REQUEST_BIT == 0;
                        let context = PollContext {
                            clock: &clock,
                            cancelled: &request_cancel,
                            deadline: clock.monotonic() + Duration::from_secs(12),
                            playstation_full_mode: false,
                        };
                        let transport = ControlTransport {
                            hid: &*hid,
                            shutdown: cancel.clone(),
                            cancelled: request_cancel.clone(),
                            permission: Some((access.permission.clone(), epoch)),
                            configuration: (visible != crate::battery_saver::BACKGROUND)
                                .then(|| (access.configuration.clone(), visible)),
                            block_while_gaming: request.seconds.is_some(),
                            boost: None,
                        };
                        let allowed = recovery.as_ref()
                            .map(|op| recovery_operation(&access, op.clone()))
                            .unwrap_or(Ok(()));
                        let allowed = allowed.and_then(|()| {
                            if let Some(operation) = recovery.as_mut() {
                                probe_sleep_recovery(&mut request, operation, &access, |probe| {
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        hb_providers::sleep_controls::execute(probe, &transport, &context)
                                    })).unwrap_or_else(|_| SleepOutcome::failed(probe, "Sleep read failed."))
                                })?;
                            }
                            Ok(())
                        });
                        let mut outcome = if let Err(error) = allowed {
                            SleepOutcome::failed(&request, format!(
                                "Could not protect the original sleep setting. No change was made: {error}"
                            ))
                        } else {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                hb_providers::sleep_controls::execute(
                                    &request, &transport, &context,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                let mut o = SleepOutcome::failed(
                                    &request,
                                    "Sleep request failed; state unknown. Refresh before retrying.",
                                );
                                o.may_have_changed = request.seconds.is_some();
                                o
                            })
                        };
                        if let Some(finish) = recovery.as_ref()
                            .and_then(|op| finish_sleep_recovery(op, &outcome))
                        {
                            let confirming = matches!(finish, crate::recovery::Operation::Confirm(_));
                            if let Err(error) = recovery_operation(&access, finish)
                                && !confirming
                            {
                                outcome.failure = Some(format!("Sleep setting may have changed, but its recovery record could not be updated: {error}"));
                                outcome.may_have_changed |= request.seconds.is_some();
                            }
                        }
                        WorkerCompleted::Sleep(Box::new(outcome), epoch, visible,
                            recovery_receipt(request.target, manual))
                    }

                    WorkerJob::Battery(job) => {
                        WorkerCompleted::Battery(execute_job(job, &*hid, &clock, &cancel))
                    }
                    WorkerJob::Polling(
                        mut request,
                        request_cancel,
                        epoch,
                        configuration_epoch,
                        boost,
                        mut recovery,
                    ) => {
                        let manual = manual_polling_change(&request);
                        let allowed = recovery.as_ref()
                            .map(|op| recovery_operation(&access, op.clone()))
                            .unwrap_or(Ok(()));
                        let allowed = allowed.and_then(|()| {
                            if let Some(operation) = recovery.as_mut() {
                                probe_polling_recovery(&mut request, operation, &access, |probe| {
                                    execute_control_inner(
                                        probe, &*hid, &clock, &cancel, &request_cancel,
                                        hb_windows::system::polling_apply_blocked(),
                                        ControlGuards {
                                            boost: boost.clone(),
                                            permission: Some((access.permission.clone(), epoch)),
                                            configuration: configuration_epoch
                                                .map(|epoch| (access.configuration.clone(), epoch)),
                                        },
                                    )
                                })?;
                            }
                            Ok(())
                        });
                        let mut outcome = if let Err(error) = allowed {
                            ControlOutcome::failed(&request, format!(
                                "Could not protect the original polling rate. No change was made: {error}"
                            ))
                        } else {
                            execute_control_inner(
                            &request,
                            &*hid,
                            &clock,
                            &cancel,
                            &request_cancel,
                            hb_windows::system::polling_apply_blocked(),
                            ControlGuards {
                                boost,
                                permission: Some((access.permission.clone(), epoch)),
                                configuration: configuration_epoch
                                    .map(|epoch| (access.configuration.clone(), epoch)),
                            },
                        )};
                        if let Some(finish) = recovery.as_ref()
                            .and_then(|op| finish_polling_recovery(op, &outcome))
                        {
                            let confirming = matches!(finish, crate::recovery::Operation::Confirm(_));
                            if let Err(error) = recovery_operation(&access, finish)
                                && !confirming
                            {
                                outcome.failure = Some(format!("Polling rate may have changed, but its recovery record could not be updated: {error}"));
                                outcome.may_have_changed |= request.action.rate().is_some();
                            }
                        }
                        WorkerCompleted::Polling(Box::new(outcome), epoch, configuration_epoch,
                            recovery_receipt(request.target, manual))
                    }
                    WorkerJob::Configuration(watch, epoch, generation) => {
                        WorkerCompleted::Configuration(
                            epoch,
                            generation,
                            discover_configuration(
                                &*hid, &clock, &cancel, &watch, epoch, generation,
                            ),
                        )
                    }
                };
                if results.send(completed).is_err() {
                    break;
                }
            }
            if let Some(t) = controller_token {
                let _ = RawGameController::RemoveRawGameControllerAdded(t);
            }
            if let Some(t) = removed_token {
                let _ = RawGameController::RemoveRawGameControllerRemoved(t);
            }
        })
        .expect("create I/O worker")
}
fn control_vendors(source: &str) -> Vec<u16> {
    match source {
        "razer" => vec![0x1532],
        "logitech" => vec![0x046d],
        "mchose" => vec![0x3837],
        "corsair" => vec![0x1b1c],
        "steelseries" => vec![0x1038],
        "asus" => vec![0x0b05],
        _ => Vec::new(),
    }
}
struct InventoryTransport<'a> {
    hid: &'a dyn HidTransport,
    watch: &'a ConfigurationWatch,
    epoch: u64,
    generation: u64,
    context: &'a PollContext<'a>,
}
impl InventoryTransport<'_> {
    fn active(&self) -> Result<(), ProviderError> {
        if self.watch.active(self.epoch)
            && self.context.active()
            && self.hid.generation() == self.generation
        {
            Ok(())
        } else {
            Err(ProviderError::new(
                "configuration inventory cancelled or connection changed",
            ))
        }
    }
}
impl HidTransport for InventoryTransport<'_> {
    fn generation(&self) -> u64 {
        self.hid.generation()
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        self.active()?;
        let result = self.hid.enumerate(vendor);
        self.active()?;
        result
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        Err(ProviderError::new(
            "configuration discovery cannot open device sessions",
        ))
    }
}
fn discover_configuration(
    hid: &dyn HidTransport,
    clock: &dyn Clock,
    cancel: &AtomicBool,
    watch: &ConfigurationWatch,
    epoch: u64,
    generation: u64,
) -> Result<Vec<ConfigurationDevice>, ProviderError> {
    let context = PollContext {
        clock,
        cancelled: cancel,
        deadline: clock.monotonic() + Duration::from_secs(12),
        playstation_full_mode: false,
    };
    let transport = InventoryTransport {
        hid,
        watch,
        epoch,
        generation,
        context: &context,
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        transport.active()?;
        let mut devices = hb_providers::configuration::discover_keyboards(&transport, &context)?;
        devices.extend(hb_providers::sleep_controls::discover(
            &transport, &context,
        )?);
        Ok(devices)
    }))
    .unwrap_or_else(|_| {
        Err(ProviderError::new(
            "configuration discovery failed; refresh to retry",
        ))
    })
}
fn configuration_matches(target: &ConfigurationDevice, observed: &ConfigurationDevice) -> bool {
    target.key == observed.key
        && target.source == observed.source
        && target.kind == observed.kind
        && target.via == observed.via
        && target.serial == observed.serial
        && target.container == observed.container
        && target.capability == observed.capability
        && observed.online()
}
/// Reserved request-ID space for process startup polling transactions.
pub(super) const STARTUP_POLLING_REQUEST_BIT: u64 = 1 << 63;
pub(super) const STARTUP_POLLING_READ_BIT: u64 = 1 << 62;

fn manual_polling_change(request: &ControlRequest) -> bool {
    let background =
        STARTUP_POLLING_REQUEST_BIT | crate::boost::REQUEST_BIT | crate::battery_saver::REQUEST_BIT;
    request.request & background == 0 && request.action.rate().is_some()
}
fn startup_restore_request(request: u64) -> bool {
    request & STARTUP_POLLING_REQUEST_BIT != 0
        && request
            & (STARTUP_POLLING_READ_BIT
                | crate::boost::REQUEST_BIT
                | crate::battery_saver::REQUEST_BIT)
            == 0
}

/// Startup reads and event-fed recovery share the same bounded state/workers.
/// Saved requests never count as readback evidence; retries only issue GETs.
struct InitialReadAttempt {
    request: u64,
    attempts: u8,
    retry_at: Option<Instant>,
    recovery: bool,
}
struct InitialPollingReads {
    until: Instant,
    initial_open: bool,
    enabled: bool,
    attempted: BTreeMap<String, Option<u64>>,
    pending: BTreeMap<String, InitialReadAttempt>,
    next_request: u64,
}
impl InitialPollingReads {
    fn new(settings: &Settings, now: Instant) -> Self {
        Self {
            until: now + Duration::from_secs(300),
            initial_open: settings.polling_controls,
            enabled: settings.polling_controls,
            attempted: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_request: STARTUP_POLLING_REQUEST_BIT | STARTUP_POLLING_READ_BIT,
        }
    }
    fn cancel(&mut self) {
        self.enabled = false;
        self.initial_open = false;
        self.attempted.clear();
        self.pending.clear();
    }
    fn observe_request(&mut self, key: &str) {
        self.pending.remove(key);
        if let Some(episode) = self.attempted.get_mut(key) {
            *episode = None;
        } else if self.enabled && self.attempted.len() < 512 {
            self.attempted.insert(key.into(), None);
        }
    }
    fn expire_initial(&mut self, now: Instant) {
        if !self.initial_open || now < self.until {
            return;
        }
        self.initial_open = false;
        // Only observed missing-rate episodes survive startup. No periodic
        // discovery or rate queries are introduced after the initial window.
        self.pending.retain(|key, attempt| {
            attempt.recovery |= self.attempted.get(key).is_some_and(Option::is_some);
            attempt.recovery
        });
        self.attempted.retain(|_, episode| episode.is_some());
    }
    fn active(&self) -> bool {
        self.enabled && (self.initial_open || !self.pending.is_empty())
    }
    fn observe_usage(
        &mut self,
        samples: &[UsageObservation],
        enabled: bool,
        now: Instant,
        unix: i64,
    ) {
        if !enabled {
            self.cancel();
            return;
        }
        self.enabled = true;
        self.expire_initial(now);
        for sample in samples {
            let reading = &sample.reading;
            let Some(session) = sample.session else {
                continue;
            };
            if !recovery_reading_ready(reading, unix) {
                continue;
            }
            if !polling_battery_source(&reading.source) {
                continue;
            }
            if self.attempted.get(&reading.key) == Some(&Some(session)) {
                if sample.polling_rate.is_some() {
                    self.pending.remove(&reading.key);
                }
                continue;
            }
            if self.attempted.len() >= 512 && !self.attempted.contains_key(&reading.key) {
                continue;
            }
            if let Some(episode) = self.attempted.get_mut(&reading.key) {
                *episode = Some(session);
            } else {
                self.attempted.insert(reading.key.clone(), Some(session));
            }
            // Check the allowlist once per episode, including unsupported
            // models, rather than allocating metadata on every observation.
            if !hb_providers::controls::polling_menu_candidate(&ConfigurationDevice::from_reading(
                reading,
            )) {
                continue;
            }
            if sample.polling_rate.is_some() {
                self.pending.remove(&reading.key);
            } else if !self
                .pending
                .get(&reading.key)
                .is_some_and(|p| !p.recovery && self.initial_open)
            {
                self.pending.insert(
                    reading.key.clone(),
                    InitialReadAttempt {
                        request: 0,
                        attempts: 0,
                        retry_at: Some(now),
                        recovery: true,
                    },
                );
            }
        }
    }
    fn needs_inventory<'a>(
        &self,
        now: Instant,
        unix: i64,
        mut readings: impl Iterator<Item = &'a Reading>,
        configuration: &[ConfigurationDevice],
    ) -> bool {
        self.active()
            && (readings.any(|reading| {
                reading.kind == "mouse"
                    && reading.online()
                    && polling_battery_source(&reading.source)
                    && ((self.initial_open
                        && self.attempted.len() < 512
                        && !self.attempted.contains_key(&reading.key))
                        || (recovery_reading_ready(reading, unix)
                            && self
                                .pending
                                .get(&reading.key)
                                .is_some_and(|p| p.retry_at.is_some_and(|at| now >= at))))
            }) || (self.initial_open
                && configuration.iter().any(|device| {
                    device.online()
                        && device.kind == "mouse"
                        && polling_battery_source(&device.source)
                        && self.attempted.len() < 512
                        && !self.attempted.contains_key(&device.key)
                })))
    }
    fn complete(&mut self, outcome: &ControlOutcome, now: Instant) {
        self.expire_initial(now);
        if !self.enabled {
            return;
        }
        let Some(pending) = self.pending.get_mut(&outcome.key) else {
            return;
        };
        if pending.request != outcome.request || pending.retry_at.is_some() {
            return;
        }
        let confirmed = outcome.failure.is_none()
            && outcome
                .observation
                .as_ref()
                .is_some_and(|o| o.target.device.key == outcome.key && o.rate.is_some());
        if confirmed || pending.attempts >= 3 {
            self.pending.remove(&outcome.key);
        } else {
            pending.retry_at = Some(now + Duration::from_secs(15));
        }
    }
    fn owns(&self, request: u64, key: &str) -> bool {
        self.pending
            .get(key)
            .is_some_and(|attempt| attempt.request == request)
    }
    fn next(
        &mut self,
        now: Instant,
        epoch: u64,
        generation: u64,
        mut devices: impl Iterator<Item = ConfigurationDevice>,
    ) -> Option<Command> {
        self.expire_initial(now);
        if !self.enabled {
            return None;
        }
        let device = devices.find(|device| {
            if !device.online() || device.kind != "mouse" || !polling_battery_source(&device.source)
            {
                return false;
            }
            if !((self.initial_open
                && self.attempted.len() < 512
                && !self.attempted.contains_key(&device.key))
                || self
                    .pending
                    .get(&device.key)
                    .is_some_and(|attempt| attempt.retry_at.is_some_and(|due| now >= due)))
            {
                return false;
            }
            if !hb_providers::controls::polling_menu_candidate(device) {
                // Remember a startup allowlist miss in the existing bounded
                // state. An unconfigurable model must not clone the combined
                // inventory again on every idle wake for the next five minutes.
                if self.initial_open
                    && self.attempted.len() < 512
                    && !self.attempted.contains_key(&device.key)
                {
                    self.attempted.insert(device.key.clone(), None);
                }
                return false;
            }
            true
        })?;
        if !self.attempted.contains_key(&device.key) {
            self.attempted.insert(device.key.clone(), None);
        }
        let recovery = self.pending.get(&device.key).is_some_and(|p| p.recovery);
        let attempts = self.pending.get(&device.key).map_or(1, |p| p.attempts + 1);
        self.pending.insert(
            device.key.clone(),
            InitialReadAttempt {
                request: self.next_request,
                attempts,
                retry_at: None,
                recovery,
            },
        );
        let request = ControlRequest {
            request: self.next_request,
            target: ControlTarget {
                device,
                generation,
                connection_stamp: None,
            },
            action: ControlAction::Read,
        };
        self.next_request += 1;
        Some(Command::Polling(request, epoch, None))
    }
}
fn polling_battery_source(source: &str) -> bool {
    source == "simulation" || hb_providers::controls::POLLING_PROVIDERS.contains(&source)
}
fn recovery_reading_ready(reading: &Reading, now: i64) -> bool {
    reading.kind == "mouse"
        && reading.online()
        && reading.precision == Precision::Exact
        && reading.level.is_some_and(|level| level <= 100)
        && reading.charging == Some(false)
        && !reading.charging_inferred
        && reading.timestamp <= now
        && now.saturating_sub(reading.timestamp) <= 120
}

struct StartupPolling {
    restore: StartupPollingRestore,
    reads: InitialPollingReads,
}
impl StartupPolling {
    fn new(settings: &Settings, now: Instant) -> Self {
        Self {
            restore: StartupPollingRestore::new(settings, now),
            reads: InitialPollingReads::new(settings, now),
        }
    }
    fn active(&self) -> bool {
        self.reads.active() || !self.restore.pending.is_empty()
    }
    fn blocks_automation(&self, now: Instant) -> bool {
        // A slow receiver must not extend the original automation startup wait.
        self.active() && now < self.restore.until
    }
    fn cancel(&mut self) {
        self.restore.cancel();
        self.reads.cancel();
    }
    fn next(
        &mut self,
        now: Instant,
        epoch: u64,
        generation: u64,
        devices: impl Iterator<Item = ConfigurationDevice> + Clone,
        keyboard_epoch: Option<u64>,
        worker_available: bool,
    ) -> Option<Command> {
        // Only this state owner sends worker jobs. Reserve nothing until a slot
        // is available; no automatic request is consumed on local backpressure.
        if !worker_available {
            return None;
        }
        if !self.restore.pending.is_empty()
            && let Some(command) =
                self.restore
                    .next(now, epoch, generation, devices.clone(), keyboard_epoch)
        {
            if let Command::Polling(request, _, _) = &command {
                // Successful restoration already confirms the rate. Failed or
                // uncertain restoration may be followed by GETs, never another SET.
                self.reads.observe_request(&request.target.device.key);
                if self.reads.enabled
                    && self
                        .reads
                        .attempted
                        .contains_key(&request.target.device.key)
                {
                    self.reads.pending.insert(
                        request.target.device.key.clone(),
                        InitialReadAttempt {
                            request: request.request,
                            attempts: 0,
                            retry_at: None,
                            recovery: false,
                        },
                    );
                }
            }
            return Some(command);
        }
        self.reads.next(now, epoch, generation, devices)
    }
}

/// Process-local, one-shot restoration. Saved choices are requests, never
/// evidence of hardware state. All requests use the ordinary guarded controller.
struct StartupPollingRestore {
    until: Instant,
    pending: BTreeMap<String, PollingRate>,
    inflight: BTreeMap<u64, (ConfigurationDevice, Arc<AtomicBool>)>,
    next_request: u64,
}
impl StartupPollingRestore {
    fn new(settings: &Settings, now: Instant) -> Self {
        Self {
            until: now + Duration::from_secs(60),
            pending: if settings.polling_controls && settings.restore_polling_on_startup {
                settings
                    .devices
                    .iter()
                    .filter_map(|(key, settings)| {
                        settings
                            .requested_polling_rate
                            .map(|rate| (key.clone(), rate))
                    })
                    .take(512)
                    .collect()
            } else {
                BTreeMap::new()
            },
            inflight: BTreeMap::new(),
            // UI request IDs use the low half of the sequence space.
            next_request: STARTUP_POLLING_REQUEST_BIT,
        }
    }
    fn cancel(&mut self) {
        self.pending.clear();
        for (_, cancelled) in self.inflight.values() {
            cancelled.store(true, Ordering::Release);
        }
    }
    /// A manual choice takes precedence even if the startup SET was already
    /// queued. Serial aliases here only cancel work; they never authorize it.
    fn cancel_device(&mut self, device: &ConfigurationDevice) {
        let alias = device.shared_preference_key();
        self.pending.remove(&device.key);
        if let Some(alias) = &alias {
            self.pending.remove(alias);
        }
        for (target, cancelled) in self.inflight.values() {
            if target.key == device.key
                || alias
                    .as_ref()
                    .is_some_and(|alias| target.shared_preference_key().as_ref() == Some(alias))
            {
                cancelled.store(true, Ordering::Release);
            }
        }
    }
    fn guard(&self, request: u64) -> Option<Arc<AtomicBool>> {
        self.inflight.get(&request).map(|(_, guard)| guard.clone())
    }
    fn complete(&mut self, request: u64) -> Option<bool> {
        self.inflight
            .remove(&request)
            .map(|(_, guard)| guard.load(Ordering::Acquire))
    }
    fn next(
        &mut self,
        now: Instant,
        epoch: u64,
        generation: u64,
        devices: impl Iterator<Item = ConfigurationDevice>,
        keyboard_epoch: Option<u64>,
    ) -> Option<Command> {
        if now >= self.until {
            self.cancel();
            return None;
        }
        for device in devices {
            let supported = if device.kind == "keyboard" {
                keyboard_epoch.is_some()
                    && matches!(device.capability, PollingCapability::ReadWrite)
                    && hb_providers::controls::POLLING_PROVIDERS.contains(&device.source.as_str())
            } else {
                hb_providers::controls::polling_menu_candidate(&device)
            };
            if !device.online() || !supported {
                continue;
            }
            let preference_key = device
                .shared_preference_key()
                .unwrap_or_else(|| device.key.clone());
            let Some(rate) = self
                .pending
                .remove(&preference_key)
                .or_else(|| self.pending.remove(&device.key))
            else {
                continue;
            };
            self.pending.remove(&device.key);
            if device.kind == "mouse"
                && !hb_providers::controls::polling_menu_rates(&device).contains(&rate.hz())
            {
                continue;
            }
            let configuration_epoch = (device.kind == "keyboard")
                .then_some(keyboard_epoch)
                .flatten();
            let request = ControlRequest {
                request: self.next_request,
                target: ControlTarget {
                    device,
                    generation,
                    connection_stamp: None,
                },
                action: ControlAction::Apply(rate),
            };
            self.inflight.insert(
                request.request,
                (
                    request.target.device.clone(),
                    Arc::new(AtomicBool::new(false)),
                ),
            );
            self.next_request += 1;
            // Consume before dispatch: busy, blocked, cancelled, uncertain or
            // failed requests are never retried, including after reconnection.
            return Some(Command::Polling(request, epoch, configuration_epoch));
        }
        None
    }
}
fn simulated_keyboards() -> Vec<ConfigurationDevice> {
    [
        (
            "razer:026c:SIMULATED-KEYBOARD",
            "Razer Huntsman V2",
            "razer",
            PollingCapability::ReadWrite,
        ),
        (
            "corsair:1bb3:SIMULATED-CORSAIR",
            "Corsair K70 RGB Pro",
            "corsair",
            PollingCapability::Unavailable(hb_providers::configuration::CORSAIR_UNAVAILABLE.into()),
        ),
    ]
    .into_iter()
    .map(|(key, name, source, capability)| ConfigurationDevice {
        key: key.into(),
        name: name.into(),
        kind: "keyboard".into(),
        source: source.into(),
        via: "usb".into(),
        connection: Connection::Online,
        serial: Some(key.into()),
        container: Some(format!("simulation:{source}")),
        capability,
    })
    .collect()
}
fn execute_job(
    mut job: Job,
    hid: &dyn HidTransport,
    clock: &dyn Clock,
    cancel: &AtomicBool,
) -> Completed {
    let context = PollContext {
        clock,
        cancelled: cancel,
        deadline: clock.monotonic() + Duration::from_secs(25),
        playstation_full_mode: job.full_mode,
    };
    let result = if cancel.load(Ordering::Relaxed) {
        Err(ProviderError::new("cancelled"))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            job.provider.poll(hid, &context)
        }))
        .unwrap_or_else(|_| {
            Err(ProviderError::new(
                "provider panicked; next refresh will retry",
            ))
        })
    };
    // Some bounded providers finish their loop with an empty or partial list.
    // Expiry/cancellation is not a completed inventory and cannot prove absence.
    let result = if result.is_ok() && !context.active() {
        Err(ProviderError::new(
            "Battery query cancelled or deadline exceeded",
        ))
    } else {
        result
    };
    Completed {
        provider: job.provider,
        result,
        epoch: job.epoch,
        generation: job.generation,
    }
}
// Cancellation is checked before every native HID exchange, including jobs that
// were already queued when settings, suspend or shutdown revoked permission.
// Short-lived wrappers restrict this path to the single read-only firmware command.
// It stays independent of configuration permission and is cancelled by suspend,
// navigation, shutdown or the caller; no resources are retained between reads.
#[derive(Clone)]
struct InfoReadAccess {
    shutdown: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    permission: Arc<ControlPermission>,
    configuration: Arc<ConfigurationWatch>,
    visible: u64,
}
impl InfoReadAccess {
    fn active(&self) -> Result<(), ProviderError> {
        if self.shutdown.load(Ordering::Acquire)
            || self.cancelled.load(Ordering::Acquire)
            || self.permission.suspended.load(Ordering::Acquire)
            || !self.configuration.active(self.visible)
        {
            Err(ProviderError::new(
                "Firmware reading was cancelled. Open Device info again.",
            ))
        } else {
            Ok(())
        }
    }
}
struct InfoTransport<'a> {
    hid: &'a dyn HidTransport,
    access: InfoReadAccess,
}
struct InfoSession {
    inner: Box<dyn HidSession>,
    access: InfoReadAccess,
}
impl HidTransport for InfoTransport<'_> {
    fn generation(&self) -> u64 {
        self.hid.generation()
    }
    fn connection_stamp(&self, info: &HidInfo) -> Option<ConnectionStamp> {
        self.hid.connection_stamp(info)
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        self.access.active()?;
        if vendor != 0x1532 {
            return Err(ProviderError::new(
                "Firmware reads are unavailable for this brand.",
            ));
        }
        let value = self.hid.enumerate(vendor)?;
        self.access.active()?;
        Ok(value)
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        self.access.active()?;
        let inner = self.hid.open(info)?;
        self.access.active()?;
        Ok(Box::new(InfoSession {
            inner,
            access: self.access.clone(),
        }))
    }
}
impl HidSession for InfoSession {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        Err(ProviderError::new(
            "Device information cannot write settings.",
        ))
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::new(
            "Device information uses only firmware reports.",
        ))
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.access.active()?;
        if !hb_providers::device_info::is_firmware_query(data) {
            return Err(ProviderError::new("Only firmware reading is allowed."));
        }
        self.inner.send_feature(data)?;
        self.access.active()
    }
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        self.access.active()?;
        if id != 0 || length != 91 {
            return Err(ProviderError::new("Only firmware reading is allowed."));
        }
        let value = self.inner.feature(id, length)?;
        self.access.active()?;
        Ok(value)
    }
}

struct ControlTransport<'a> {
    hid: &'a dyn HidTransport,
    shutdown: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    permission: Option<(Arc<ControlPermission>, u64)>,
    configuration: Option<(Arc<ConfigurationWatch>, u64)>,
    block_while_gaming: bool,
    boost: Option<crate::boost::Guard>,
}
struct ControlSession {
    inner: Box<dyn HidSession>,
    shutdown: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    permission: Option<(Arc<ControlPermission>, u64)>,
    configuration: Option<(Arc<ConfigurationWatch>, u64)>,
    block_while_gaming: bool,
    boost: Option<crate::boost::Guard>,
    notification_state: fn() -> bool,
}
impl ControlSession {
    fn active(&self) -> Result<(), ProviderError> {
        if self.boost.as_ref().is_some_and(|g| !g.active()) {
            return Err(ProviderError::new(
                "Fullscreen polling transition cancelled",
            ));
        }
        if self.shutdown.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed) {
            Err(ProviderError::new("configuration cancelled"))
        } else if self
            .configuration
            .as_ref()
            .is_some_and(|(watch, epoch)| !watch.active(*epoch))
        {
            Err(ProviderError::new(
                "Keyboard configuration cancelled: Devices is no longer visible",
            ))
        } else if self.permission.as_ref().is_some_and(|(p, epoch)| {
            !p.enabled.load(Ordering::Acquire)
                || p.suspended.load(Ordering::Acquire)
                || !p.desired_enabled.load(Ordering::Acquire)
                || p.acknowledged.load(Ordering::Acquire) != *epoch
                || p.epoch.load(Ordering::Acquire) != *epoch
        }) {
            Err(ProviderError::new("configuration permission revoked"))
        } else if self.block_while_gaming && (self.notification_state)() {
            Err(ProviderError::new(
                "Close the game or presentation and verify the Windows notification state before changing the rate",
            ))
        } else {
            Ok(())
        }
    }
}
impl HidSession for ControlSession {
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.active()?;
        self.inner.write(data)
    }
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        self.active()?;
        self.inner.read(length, timeout)
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.active()?;
        self.inner.send_feature(data)
    }
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        self.active()?;
        self.inner.feature(id, length)
    }
}
impl HidTransport for ControlTransport<'_> {
    fn generation(&self) -> u64 {
        self.hid.generation()
    }
    fn connection_stamp(&self, info: &HidInfo) -> Option<ConnectionStamp> {
        self.hid.connection_stamp(info)
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        if self.boost.as_ref().is_some_and(|g| !g.active()) {
            return Err(ProviderError::new(
                "Fullscreen polling transition cancelled",
            ));
        }
        if self.shutdown.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed) {
            return Err(ProviderError::new("configuration cancelled"));
        }
        if self
            .configuration
            .as_ref()
            .is_some_and(|(watch, epoch)| !watch.active(*epoch))
        {
            return Err(ProviderError::new(
                "Keyboard configuration cancelled: Devices is no longer visible",
            ));
        }
        self.hid.enumerate(vendor)
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        if self.boost.as_ref().is_some_and(|g| !g.active()) {
            return Err(ProviderError::new(
                "Fullscreen polling transition cancelled",
            ));
        }
        if self.shutdown.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed) {
            return Err(ProviderError::new("configuration cancelled"));
        }
        if self
            .configuration
            .as_ref()
            .is_some_and(|(watch, epoch)| !watch.active(*epoch))
        {
            return Err(ProviderError::new(
                "Keyboard configuration cancelled: Devices is no longer visible",
            ));
        }
        Ok(Box::new(ControlSession {
            inner: self.hid.open(info)?,
            shutdown: self.shutdown.clone(),
            cancelled: self.cancelled.clone(),
            permission: self.permission.clone(),
            configuration: self.configuration.clone(),
            block_while_gaming: self.block_while_gaming,
            boost: self.boost.clone(),
            notification_state: hb_windows::system::polling_apply_blocked,
        }))
    }
}
pub(crate) fn execute_control(
    request: &ControlRequest,
    hid: &dyn HidTransport,
    clock: &dyn Clock,
    shutdown: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    gaming: bool,
    permission: Option<(Arc<ControlPermission>, u64)>,
) -> ControlOutcome {
    execute_control_inner(
        request,
        hid,
        clock,
        shutdown,
        cancelled,
        gaming,
        ControlGuards {
            boost: None,
            permission,
            configuration: None,
        },
    )
}
#[derive(Default)]
struct ControlGuards {
    boost: Option<crate::boost::Guard>,
    permission: Option<(Arc<ControlPermission>, u64)>,
    configuration: Option<(Arc<ConfigurationWatch>, u64)>,
}
fn execute_control_inner(
    request: &ControlRequest,
    hid: &dyn HidTransport,
    clock: &dyn Clock,
    shutdown: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    gaming: bool,
    guards: ControlGuards,
) -> ControlOutcome {
    if shutdown.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
        return ControlOutcome::failed(request, "Configuration cancelled");
    }
    // Uses Shell's public notification state only; no game-process inspection.
    if guards.boost.as_ref().is_some_and(|g| !g.active()) {
        return ControlOutcome::failed(request, "Fullscreen polling transition cancelled");
    }
    if gaming && guards.boost.is_none() && request.action.rate().is_some() {
        return ControlOutcome::failed(
            request,
            "Close the game or presentation and verify the Windows notification state before changing the rate",
        );
    }
    let context = PollContext {
        clock,
        cancelled,
        deadline: clock.monotonic() + Duration::from_secs(12),
        playstation_full_mode: false,
    };
    let transport = ControlTransport {
        hid,
        shutdown: shutdown.clone(),
        cancelled: cancelled.clone(),
        permission: guards.permission,
        configuration: guards.configuration,
        block_while_gaming: request.action.rate().is_some() && guards.boost.is_none(),
        boost: guards.boost,
    };
    control_transaction(request, || {
        hb_providers::controls::HidDeviceController.execute(request, &transport, &context)
    })
}
fn control_transaction(
    request: &ControlRequest,
    transaction: impl FnOnce() -> ControlOutcome,
) -> ControlOutcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(transaction)).unwrap_or_else(|_| {
        let mut outcome = ControlOutcome::failed(
            request,
            "Configuration worker failed; device state is unknown. Refresh before retrying",
        );
        // A panic can occur after a SET reached the device. Do not present that
        // write as safely unapplied or retain an obsolete rate observation.
        outcome.may_have_changed = request.action.rate().is_some();
        outcome
    })
}
fn simulate_control(
    request: &ControlRequest,
    current: &mut PollingRate,
    generation: u64,
    timestamp: i64,
) -> ControlOutcome {
    if let PollingCapability::Unavailable(reason) = &request.target.device.capability {
        return ControlOutcome::failed(request, reason.clone());
    }
    if request.action.rate().is_some() && request.target.generation != generation {
        return ControlOutcome::failed(request, "Connection changed; refresh before applying");
    }
    let supported = if request.target.device.kind == "keyboard" {
        vec![125, 250, 500, 1000, 2000, 4000, 8000]
    } else {
        vec![125, 500, 1000, 2000, 4000, 8000]
    };
    let previous = *current;
    if request
        .action
        .expected()
        .is_some_and(|expected| expected != *current)
    {
        return ControlOutcome::failed(
            request,
            "Rate changed externally; automatic change cancelled",
        );
    }
    if let Some(rate) = request.action.rate() {
        if !supported.contains(&rate.hz()) {
            return ControlOutcome::failed(request, "Unsupported rate for simulated device");
        }
        *current = rate;
    }
    let mut target = request.target.clone();
    target.generation = generation;
    target.connection_stamp = ConnectionStamp::new(1);
    ControlOutcome {
        request: request.request,
        key: target.device.key.clone(),
        observation: Some(PollingObservation {
            target,
            supported: supported
                .into_iter()
                .map(|r| PollingRate::try_from(r).unwrap())
                .collect(),
            rate: Some(*current),
            timestamp,
            evidence: "Simulation; no hardware accessed".into(),
        }),
        previous: Some(previous),
        may_have_changed: previous != *current,
        failure: None,
    }
}
/// A healthy refresh can confirm a change after a previous partial SET.
fn changed_configuration(
    outcome: &ControlOutcome,
    observed: &mut BTreeMap<String, PollingRate>,
) -> bool {
    let first_confirmation = !observed.contains_key(&outcome.key);
    if observed.len() >= 512 && !observed.contains_key(&outcome.key) {
        observed.pop_first();
    }
    if outcome.failure.is_some() {
        return outcome.may_have_changed;
    }
    let Some(rate) = outcome.observation.as_ref().and_then(|o| o.rate) else {
        return false;
    };
    let changed = observed
        .insert(outcome.key.clone(), rate)
        .is_none_or(|before| before != rate);
    first_confirmation || changed || outcome.confirmed_change()
}
fn finish_sleep_outcome(
    saver: &mut crate::battery_saver::BatterySaver,
    outcome: &SleepOutcome,
) -> bool {
    let owned = saver.sleep_complete(outcome);
    outcome.request & crate::battery_saver::REQUEST_BIT == 0 || owned
}
/// Revocation can arrive while the state owner is waiting for a completion,
/// before its next settings acknowledgment. Preserve only pre-write ownership;
/// the policy still requires its original connection proof before another SET.
fn control_epoch_current(
    epoch: u64,
    current: u64,
    boost: &mut crate::boost::Boost,
    saver: &mut crate::battery_saver::BatterySaver,
) -> bool {
    if epoch == current {
        return true;
    }
    boost.settings_changed();
    saver.settings_changed();
    false
}
fn finish_polling_outcome(
    boost: &mut crate::boost::Boost,
    saver: &mut crate::battery_saver::BatterySaver,
    outcome: &ControlOutcome,
) -> bool {
    let boost_owned = boost.complete(outcome);
    let saver_owned = saver.polling_complete(outcome);
    if outcome.request & crate::boost::REQUEST_BIT != 0 {
        boost_owned
    } else if outcome.request & crate::battery_saver::REQUEST_BIT != 0 {
        saver_owned
    } else {
        true
    }
}
fn validate_polling_completion(outcome: &mut ControlOutcome, generation: u64, permitted: bool) {
    if !permitted
        || outcome
            .observation
            .as_ref()
            .is_some_and(|o| o.target.generation != generation)
    {
        outcome.observation = None;
        outcome.failure =
            Some("Configuration permission or connection changed; refresh before retrying".into());
    }
}
/// Session-scoped readback evidence, never restored from requested settings.
struct UsageTracker {
    actions: BTreeMap<u64, (PollingActivityKind, PollingRate)>,
    session: u64,
    generation: u64,
    enabled: bool,
    rates: BTreeMap<String, PollingObservation>,
    device_sessions: BTreeMap<String, u64>,
    connections: BTreeMap<String, Connection>,
    wall_clock: Option<i64>,
}
impl UsageTracker {
    fn new(generation: u64, enabled: bool) -> Self {
        let session = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(i64::MAX as u128 - 1) as u64;
        Self {
            actions: BTreeMap::new(),
            session,
            generation,
            enabled,
            rates: BTreeMap::new(),
            device_sessions: BTreeMap::new(),
            connections: BTreeMap::new(),
            wall_clock: None,
        }
    }
    fn invalidate(&mut self, generation: u64, enabled: bool) {
        self.session = self.session.saturating_add(1);
        self.generation = generation;
        self.enabled = enabled;
        self.rates.clear();
        self.actions.clear();
        self.device_sessions.clear();
        self.connections.clear();
    }
    fn synchronize(&mut self, generation: u64, enabled: bool) {
        if generation != self.generation || enabled != self.enabled {
            self.invalidate(generation, enabled);
        }
    }
    fn observe(&mut self, outcome: &ControlOutcome) {
        // Configuration-only keyboards have no battery observations to learn.
        if outcome
            .observation
            .as_ref()
            .is_some_and(|o| o.target.device.kind == "keyboard")
        {
            self.rates.remove(&outcome.key);
            self.device_sessions.remove(&outcome.key);
            return;
        }
        // A failed/partial exchange cannot establish the resulting rate.
        let previous = self.rates.remove(&outcome.key);
        let next = outcome.observation.as_ref().and_then(|o| o.rate);
        if (previous.is_some() || next.is_some())
            && (outcome.failure.is_some() || previous.as_ref().and_then(|o| o.rate) != next)
        {
            self.session = self.session.saturating_add(1);
            self.set_device_session(&outcome.key);
        }
        if self.enabled
            && outcome.failure.is_none()
            && let Some(observation) = &outcome.observation
            && observation.target.generation == self.generation
            && observation.rate.is_some()
            && observation.target.device.key == outcome.key
        {
            if self.rates.len() >= 512 {
                self.rates.pop_first();
            }
            self.rates.insert(outcome.key.clone(), observation.clone());
        }
    }
    fn begin_control(&mut self, request: &ControlRequest, automatic: Option<bool>) {
        if request.request & STARTUP_POLLING_REQUEST_BIT != 0 && automatic.is_none() {
            return;
        }
        if let Some(rate) = request.action.rate() {
            if self.actions.len() >= 32 {
                self.actions.pop_first();
            }
            self.actions.insert(
                request.request,
                (
                    match automatic {
                        Some(true) => PollingActivityKind::Boost,
                        Some(false) => PollingActivityKind::Restore,
                        None => PollingActivityKind::Manual,
                    },
                    rate,
                ),
            );
        }
    }
    fn begin_saver_control(&mut self, request: &ControlRequest, restoring: bool) {
        if let Some(rate) = request.action.rate() {
            if self.actions.len() >= 32 {
                self.actions.pop_first();
            }
            self.actions.insert(
                request.request,
                (
                    if restoring {
                        PollingActivityKind::SaverRestore
                    } else {
                        PollingActivityKind::Saver
                    },
                    rate,
                ),
            );
        }
    }
    fn take_activity(&mut self, outcome: &ControlOutcome) -> Option<PollingActivity> {
        let (kind, expected) = self.actions.remove(&outcome.request)?;
        let observation = outcome.observation.as_ref()?;
        if outcome.failure.is_some()
            || observation.rate != Some(expected)
            || observation.target.device.key != outcome.key
            || observation.target.device.kind != "mouse"
            || !self.enabled
            || observation.target.generation != self.generation
        {
            return None;
        }
        Some(PollingActivity {
            key: outcome.key.clone(),
            timestamp: observation.timestamp,
            session: *self.device_sessions.get(&outcome.key)?,
            kind,
            rate: expected,
            previous: outcome.previous,
        })
    }
    fn synchronize_clock(&mut self, now: i64) -> bool {
        let moved_back = self.wall_clock.is_some_and(|previous| now < previous);
        if moved_back {
            self.invalidate(self.generation, self.enabled);
        }
        self.wall_clock = Some(now);
        moved_back
    }
    fn samples(&mut self, readings: Vec<Reading>) -> Vec<UsageObservation> {
        readings
            .into_iter()
            .map(|reading| {
                let unavailable_transition = !reading.online()
                    && self.connections.get(&reading.key) != Some(&reading.connection);
                // A successful observation of a different unit/transport must
                // revoke readback, not merely hide it until old metadata returns.
                let revoked_rate = self.rates.get(&reading.key).is_some_and(|observed| {
                    reading.connection == Connection::Stale
                        || !observed.target.device.matches_reading(&reading)
                });
                if revoked_rate {
                    self.rates.remove(&reading.key);
                }
                if unavailable_transition || revoked_rate {
                    self.session = self.session.saturating_add(1);
                    self.set_device_session(&reading.key);
                }
                if !self.device_sessions.contains_key(&reading.key) {
                    self.set_device_session(&reading.key);
                }
                if let Some(connection) = self.connections.get_mut(&reading.key) {
                    *connection = reading.connection.clone();
                } else {
                    self.connections
                        .insert(reading.key.clone(), reading.connection.clone());
                }
                let polling_rate = self
                    .rates
                    .get(&reading.key)
                    .filter(|o| {
                        reading.timestamp >= o.timestamp
                            && o.target.device.matches_reading(&reading)
                    })
                    .and_then(|o| o.rate);
                let session = self.device_sessions.get(&reading.key).copied();
                UsageObservation {
                    reading,
                    polling_rate,
                    session,
                }
            })
            .collect()
    }
    fn set_device_session(&mut self, key: &str) {
        if self.device_sessions.len() >= 512
            && !self.device_sessions.contains_key(key)
            && let Some((expired, _)) = self.device_sessions.pop_first()
        {
            self.rates.remove(&expired);
            self.connections.remove(&expired);
        }
        if let Some(session) = self.device_sessions.get_mut(key) {
            *session = self.session;
        } else {
            self.device_sessions.insert(key.to_owned(), self.session);
        }
    }
}
fn synchronize_battery_clock(engine: &mut Engine, tracker: &mut UsageTracker, now: i64) -> bool {
    let moved_back = tracker.synchronize_clock(now);
    if moved_back {
        engine.estimator.clock_changed(now);
    }
    moved_back
}
struct Apartment(bool);
impl Apartment {
    fn new(enabled: bool) -> Self {
        Self(enabled && unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
fn run(
    dir: PathBuf,
    settings: Settings,
    simulate: bool,
    hid: Arc<WindowsHid>,
    lifecycle: Lifecycle,
    commands: Receiver<Command>,
    events: Events,
) {
    let monitoring_started = Instant::now();
    let mut input_wake = InputWake::new(monitoring_started);
    let mut startup_polling = StartupPolling::new(&settings, monitoring_started);
    let journal = crate::recovery::Journal::open(&dir);
    let recovery_broken = journal.error().is_some();
    if let Some(error) = journal.error() {
        let _ = events.send(Event::Error(format!(
            "Automatic recovery data could not be read; device changes are paused: {error}"
        )));
    }
    if journal.has_uncertain_manual() {
        let _ = events.send(Event::Error(
            "An earlier manual device change was interrupted. Check that device's settings before changing them again.".into()
        ));
    }
    for device in journal.devices() {
        startup_polling.restore.cancel_device(device);
    }
    if recovery_broken {
        startup_polling.restore.cancel();
    }
    let mut recovery_pending = journal.has_pending();
    let mut deferred_recovery_rearms = Vec::new();
    let mut recovery_rearm_confirmed = false;
    let recovery = Arc::new(Mutex::new(journal));
    let mut boost = crate::boost::Boost::new(Instant::now());
    let mut saver = crate::battery_saver::BatterySaver::default();
    let mut saver_dirty = true;
    let mut saver_generation = hid.generation();
    let Lifecycle {
        cancel,
        permission,
        configuration,
    } = lifecycle;
    let simulate_keyboards = simulate && std::env::args().any(|a| a == "--simulate-keyboards");
    let (storage, store_rx) = bounded(32);
    let (boot, boot_rx) = bounded(1);
    let store_events = events.clone();
    let folder = dir.clone();
    let store_thread = thread::Builder::new()
        .name("storage".into())
        .stack_size(512 * 1024)
        .spawn(move || crate::storage_worker::run(folder, store_rx, boot, store_events))
        .expect("create storage worker");
    let mut engine = Engine::new(settings, boot_rx.recv().unwrap_or_default());
    if !engine.settings.status_file {
        let _ = storage.send(Storage::RemoveStatus);
    }
    let clock = SystemClock::default();
    let (jobs, job_rx) = bounded(2);
    let (winrt, winrt_rx) = bounded(2);
    let (results, result_rx) = bounded(4);
    let (event_tx, event_rx) = bounded(8);
    let mut workers = Vec::new();
    let gates = Arc::new(WorkerAccess {
        storage: storage.clone(),
        recovery: recovery.clone(),
        permission: permission.clone(),
        configuration: configuration.clone(),
        gates: hb_providers::catalog::DEVICES
            .iter()
            .map(|d| d.vid)
            .chain(
                hb_providers::configuration::CONFIGURATION_VENDORS
                    .iter()
                    .copied(),
            )
            .chain([0x1532, 0x046d, 0x3837, 0x1b1c])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|v| (v, Mutex::new(())))
            .collect::<BTreeMap<_, _>>(),
    });
    for _ in 0..2 {
        workers.push(worker(
            job_rx.clone(),
            results.clone(),
            hid.clone(),
            cancel.clone(),
            false,
            event_tx.clone(),
            gates.clone(),
        ))
    }
    workers.push(worker(
        winrt_rx,
        results.clone(),
        hid.clone(),
        cancel.clone(),
        true,
        event_tx,
        gates,
    ));
    drop(results);
    let mut providers: BTreeMap<&str, Box<dyn BatteryProvider>> = hb_providers::providers()
        .into_iter()
        .map(|p| (p.id(), p))
        .collect();
    providers.insert("bluetooth", Box::new(BluetoothProvider::default()));
    providers.insert("xinput", Box::new(ControllerProvider));
    let mut inflight = 0usize;
    let mut due: BTreeMap<&str, Instant> =
        providers.keys().map(|id| (*id, Instant::now())).collect();
    let mut invalidated = BTreeSet::new();
    let mut diagnostics = BTreeMap::new();
    let mut diagnostics_dirty = true;
    let mut snapshots = SnapshotPublisher::new(Instant::now());
    let mut control_cancel = Arc::new(AtomicBool::new(!engine.settings.polling_controls));
    let mut simulated_rates = BTreeMap::new();
    let mut configuration_devices: Vec<ConfigurationDevice> = Vec::new();
    let mut configuration_generation = hid.generation();
    let mut configuration_valid = false;
    let mut configuration_failure = None;
    let mut configuration_visible = false;
    let mut configuration_inflight = false;
    let mut configuration_due = Instant::now();
    let mut configuration_dirty = false;
    let mut observed_rates = BTreeMap::new();
    let mut usage_tracker = UsageTracker::new(hid.generation(), engine.settings.polling_controls);
    let mut estimator_dirty = false;
    let mut estimator_checkpoint = Instant::now();
    let _ = events.send(Event::PollingInvalidated(hid.generation()));
    let mut stop = false;
    let mut suspended = false;
    let mut applied_resume_epoch = None;
    let mut was_quiet = false;
    let mut failed_delivery: BTreeMap<(String, NotificationKind), (Instant, Notification)> =
        BTreeMap::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            stop = true;
            startup_polling.cancel();
            control_cancel.store(true, Ordering::Relaxed);
        }
        let latest_settings = permission
            .pending_settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((s, epoch)) = latest_settings {
            if !engine.settings.polling_controls && s.polling_controls {
                recovery_rearm_confirmed = true;
                recovery_pending = true;
            }
            startup_polling.cancel();
            boost.settings_changed();
            saver.settings_changed();
            let previous = engine.readings();
            snapshots.changed = true;
            let remove = engine.settings.status_file && !s.status_file;
            control_cancel.store(true, Ordering::Relaxed);
            control_cancel = Arc::new(AtomicBool::new(!s.polling_controls));
            engine.update_settings(s.clone());
            release_inactive_provider_handles(&mut providers, &engine.settings, suspended);
            let current = engine.readings();
            let removed = previous
                .into_iter()
                .filter(|old| !current.iter().any(|r| r.key == old.key))
                .map(|r| availability_boundary(r, Connection::Stale, clock.unix()))
                .collect();
            let _ = storage.send(Storage::UsageSample(usage_tracker.samples(removed)));
            estimator_dirty = true;
            let _ = storage.send(Storage::Save(s));
            if remove {
                let _ = storage.send(Storage::RemoveStatus);
            }
            for id in providers.keys() {
                due.insert(id, Instant::now());
            }
            permission.enabled.store(
                engine.settings.polling_controls
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && !permission.suspended.load(Ordering::Acquire),
                Ordering::Release,
            );
            // A stale acknowledgment cannot authorize work with a newer epoch.
            permission.acknowledged.store(epoch, Ordering::Release);
        }
        let requested_suspend = permission.suspended.load(Ordering::Acquire);
        // Revocation is immediate even if its command/settings acknowledgment
        // is queued. Re-enabling during this process cannot restart restoration.
        if permission.epoch.load(Ordering::Acquire) != 0 || requested_suspend {
            startup_polling.cancel();
        }
        if requested_suspend != suspended {
            snapshots.changed = true;
            suspended = requested_suspend;
            control_cancel.store(true, Ordering::Relaxed);
            hid.invalidate();
            usage_tracker.invalidate(
                hid.generation(),
                engine.settings.polling_controls && !suspended,
            );
            let _ = events.send(Event::PollingInvalidated(hid.generation()));
            if suspended {
                applied_resume_epoch = None;
                release_inactive_provider_handles(&mut providers, &engine.settings, true);
                engine.suspend();
                let boundaries = engine
                    .readings()
                    .into_iter()
                    .map(|r| availability_boundary(r, Connection::Sleeping, clock.unix()))
                    .collect();
                let _ = storage.send(Storage::UsageSample(usage_tracker.samples(boundaries)));
                estimator_dirty = true;
            } else {
                engine.resume();
                control_cancel = Arc::new(AtomicBool::new(!engine.settings.polling_controls));
                applied_resume_epoch = Some(permission.epoch.load(Ordering::Acquire));
                for id in hb_providers::provider::FAMILIES
                    .iter()
                    .map(|p| p.0)
                    .chain(["bluetooth", "xinput"])
                {
                    invalidated.insert(id);
                    due.insert(id, Instant::now());
                }
            }
        }
        usage_tracker.synchronize(
            hid.generation(),
            engine.settings.polling_controls && !suspended,
        );
        boost.observe_generation(hid.generation());
        if saver_generation != hid.generation() {
            saver.observe_generation(hid.generation());
        }
        if suspended || stop || !engine.settings.polling_controls {
            if suspended && !stop && engine.settings.polling_controls {
                boost.suspend();
                saver.suspend();
            } else {
                boost.clear();
                saver.pause();
            }
        }
        startup_polling.reads.expire_initial(Instant::now());
        if !suspended && !stop && boost.due(Instant::now()) && boost.wanted(&engine.settings) {
            boost.sample(
                Instant::now(),
                hb_windows::system::fullscreen_polling_state(),
            );
        }
        // Capture changes before snapshot publication clears its dirty flag.
        saver_dirty |= snapshots.changed || saver_generation != hid.generation();
        saver_generation = hid.generation();
        let quiet = engine.settings.quiet_fullscreen && hb_windows::system::gaming();
        leave_quiet_mode(
            was_quiet,
            quiet,
            providers.keys().copied(),
            &mut due,
            Instant::now(),
        );
        was_quiet = quiet;
        let now = Instant::now();
        let wakeable = !simulate
            && !suspended
            && !stop
            && !quiet
            && engine
                .provider_readings("pulsar")
                .iter()
                .any(|r| r.connection == Connection::Sleeping);
        if input_wake.due(now, wakeable)
            && input_wake.observe(hb_windows::system::last_input_tick())
        {
            due.insert("pulsar", now);
        }

        let visible = configuration.visible.load(Ordering::Acquire) && !suspended && !stop;
        if visible && !configuration_visible {
            configuration_due = Instant::now();
            configuration_dirty = true;
        }
        configuration_visible = visible;
        if configuration_generation != hid.generation() {
            configuration_generation = hid.generation();
            configuration_valid = false;
            for device in &mut configuration_devices {
                device.connection = Connection::Stale;
            }
            configuration_due = Instant::now();
            configuration_dirty = true;
        }
        if visible && !configuration_inflight && Instant::now() >= configuration_due {
            if simulate {
                let devices = if simulate_keyboards {
                    simulated_keyboards()
                } else {
                    Vec::new()
                };
                configuration_dirty |= configuration_devices != devices
                    || !configuration_valid
                    || configuration_failure.is_some();
                configuration_devices = devices;
                configuration_valid = true;
                configuration_failure = None;
                configuration_due = Instant::now() + Duration::from_secs(30);
            } else if jobs
                .try_send(WorkerJob::Configuration(
                    configuration.clone(),
                    configuration.epoch.load(Ordering::Acquire),
                    hid.generation(),
                ))
                .is_ok()
            {
                configuration_inflight = true;
                inflight += 1;
                configuration_due = Instant::now() + Duration::from_secs(30);
            }
        }
        if visible
            && configuration_dirty
            && !events.tx.is_full()
            && events
                .try_send(Event::ConfigurationInventory {
                    generation: configuration_generation,
                    devices: configuration_devices.clone(),
                    failure: configuration_failure.clone(),
                })
                .is_ok()
        {
            configuration_dirty = false;
        }
        let ready = providers
            .keys()
            .filter(|id| {
                poll_is_due(
                    &engine.settings,
                    id,
                    due.get(*id).copied(),
                    Instant::now(),
                    stop || suspended,
                )
            })
            .copied()
            .collect::<Vec<_>>();
        if simulate && !ready.is_empty() {
            snapshots.changed = true;
            let mut r = Reading::new(
                "simulated:mouse",
                "Simulated mouse",
                "simulation",
                clock.unix(),
            );
            r.level = Some(73);
            r.charging = Some(true);
            r.kind = "mouse".into();
            engine.apply(
                "simulation",
                Ok(vec![r]),
                clock.monotonic().as_secs_f64(),
                false,
            );
            estimator_dirty = true;
            for t in due.values_mut() {
                *t = Instant::now() + effective_interval(engine.settings.interval, quiet);
            }
            usage_tracker.synchronize(
                hid.generation(),
                engine.settings.polling_controls && !suspended,
            );
            let samples = usage_tracker.samples(engine.readings());
            startup_polling.reads.observe_usage(
                &samples,
                engine.settings.polling_controls && !suspended && !stop,
                Instant::now(),
                clock.unix(),
            );
            let _ = storage.send(Storage::UsageSample(samples));
        } else {
            for id in ready {
                let target = if ["bluetooth", "xinput"].contains(&id) {
                    &winrt
                } else {
                    &jobs
                };
                let mut provider = providers.remove(id).unwrap();
                if invalidated.remove(id) {
                    provider.invalidate();
                }
                let mut job = Job::new(provider, &engine.settings);
                job.epoch = permission.epoch.load(Ordering::Acquire);
                job.generation = hid.generation();
                match target.try_send(WorkerJob::Battery(job)) {
                    Ok(()) => {
                        inflight += 1;
                        due.remove(id);
                    }
                    Err(e) => {
                        if let WorkerJob::Battery(j) = e.into_inner() {
                            providers.insert(id, j.provider);
                        }
                    }
                }
            }
        }
        if stop {
            break;
        }
        let wait = provider_queue_wait(
            &engine.settings,
            due.iter()
                .filter(|(id, _)| providers.contains_key(*id))
                .map(|(id, at)| (*id, *at)),
            Instant::now(),
            suspended || stop,
            jobs.is_full(),
            winrt.is_full(),
        )
        .min(Duration::from_secs(5))
        .min(snapshots.delay(Instant::now()))
        .max(Duration::from_millis(20));
        let keyboard_epoch =
            (visible && configuration_valid && configuration_generation == hid.generation())
                .then(|| configuration.epoch.load(Ordering::Acquire));
        let startup_worker_available = simulate || !jobs.is_full();
        let initial_read_window = startup_polling.reads.initial_open;
        let initial_work = initial_read_window || !startup_polling.restore.pending.is_empty();
        let readback_ready = startup_polling.active()
            && !suspended
            && !stop
            && engine.settings.polling_controls
            && (initial_work
                || (inflight == 0
                    && commands.is_empty()
                    && !boost.ready(&engine.settings, |id| engine.provider_readings(id))))
            && (!startup_polling.restore.pending.is_empty()
                || startup_polling.reads.needs_inventory(
                    Instant::now(),
                    clock.unix(),
                    engine.raw_readings(),
                    &configuration_devices,
                ));
        let startup_command =
            if !startup_polling.active() || !startup_worker_available || !readback_ready {
                None
            } else {
                startup_polling.next(
                    Instant::now(),
                    permission.epoch.load(Ordering::Acquire),
                    hid.generation(),
                    engine
                        .readings()
                        .iter()
                        .filter(|reading| {
                            reading.kind == "mouse"
                                && (initial_work || recovery_reading_ready(reading, clock.unix()))
                        })
                        .map(ConfigurationDevice::from_reading)
                        .chain(
                            configuration_devices
                                .iter()
                                .filter(|_| initial_work)
                                .cloned(),
                        ),
                    keyboard_epoch,
                    startup_worker_available,
                )
            };
        let boost_command = if startup_command.is_none()
            && !recovery_broken
            && !suspended
            && !stop
            && inflight == 0
            && !startup_polling.blocks_automation(Instant::now())
            && !jobs.is_full()
            && commands.is_empty()
            && boost.ready(&engine.settings, |id| engine.provider_readings(id))
        {
            boost
                .next(&engine.settings, hid.generation(), &engine.readings())
                .map(|request| {
                    Command::Polling(request, permission.epoch.load(Ordering::Acquire), None)
                })
        } else {
            None
        };
        let saver_command = if startup_command.is_none()
            && !recovery_broken
            && boost_command.is_none()
            && !suspended
            && !stop
            && inflight == 0
            && !startup_polling.blocks_automation(Instant::now())
            && !jobs.is_full()
            && commands.is_empty()
            && engine.settings.polling_controls
            && saver.wanted(&engine.settings)
        {
            let mut readings = None;
            if saver_dirty {
                let latest = engine.readings();
                saver.observe(&engine.settings, hid.generation(), &latest, clock.unix());
                readings = Some(latest);
                saver_dirty = false;
            }
            if saver.ready(&boost, clock.unix(), |id| engine.provider_readings(id))
                && hb_windows::system::fullscreen_polling_state() == Some(false)
            {
                saver
                    .next(
                        &engine.settings,
                        hid.generation(),
                        readings.get_or_insert_with(|| engine.readings()),
                        clock.unix(),
                        &boost,
                    )
                    .map(|action| match action {
                        crate::battery_saver::Action::Polling(request) => Command::Polling(
                            request,
                            permission.epoch.load(Ordering::Acquire),
                            None,
                        ),
                        crate::battery_saver::Action::Sleep(request) => Command::Sleep(
                            request,
                            permission.epoch.load(Ordering::Acquire),
                            crate::battery_saver::BACKGROUND,
                        ),
                    })
            } else {
                None
            }
        } else {
            None
        };
        let work = if let Some(command) = startup_command.or(boost_command).or(saver_command) {
            Work::Command(Ok(command))
        } else {
            select! {recv(commands)->m=>Work::Command(m),recv(event_rx)->m=>Work::Command(m),recv(result_rx)->r=>Work::Completed(r),default(wait)=>Work::Idle}
        };
        let observed_now = clock.unix();
        if synchronize_battery_clock(&mut engine, &mut usage_tracker, observed_now) {
            snapshots.changed = true;
            estimator_dirty = true;
        }
        match work {
            Work::Command(Ok(Command::ReleaseHistoryCache)) => {
                // Preserve FIFO ordering behind any already accepted history
                // query, including when the bounded storage queue is full.
                let _ = storage.send(Storage::ReleaseHistoryCache);
            }
            Work::Command(Ok(Command::Quit)) | Work::Command(Err(_)) => {
                stop = true;
                control_cancel.store(true, Ordering::Relaxed);
                cancel.store(true, Ordering::Relaxed);
            }
            Work::Command(Ok(Command::EpochSuspend(epoch)))
                if epoch == permission.epoch.load(Ordering::Acquire) && !suspended =>
            {
                snapshots.changed = true;
                suspended = true;
                applied_resume_epoch = None;
                release_inactive_provider_handles(&mut providers, &engine.settings, true);
                control_cancel.store(true, Ordering::Relaxed);
                hid.invalidate();
                usage_tracker.invalidate(
                    hid.generation(),
                    engine.settings.polling_controls && !suspended,
                );
                let _ = events.send(Event::PollingInvalidated(hid.generation()));
                engine.suspend();
                let boundaries = engine
                    .readings()
                    .into_iter()
                    .map(|r| availability_boundary(r, Connection::Sleeping, clock.unix()))
                    .collect();
                let _ = storage.send(Storage::UsageSample(usage_tracker.samples(boundaries)));
                estimator_dirty = true;
            }
            Work::Command(Ok(Command::EpochResume(epoch)))
                if epoch == permission.epoch.load(Ordering::Acquire) =>
            {
                if applied_resume_epoch != Some(epoch) {
                    snapshots.changed = true;
                    suspended = false;
                    control_cancel = Arc::new(AtomicBool::new(!engine.settings.polling_controls));
                    engine.resume();
                    hid.invalidate();
                    usage_tracker.invalidate(
                        hid.generation(),
                        engine.settings.polling_controls && !suspended,
                    );
                    let _ = events.send(Event::PollingInvalidated(hid.generation()));
                    for id in hb_providers::provider::FAMILIES
                        .iter()
                        .map(|p| p.0)
                        .chain(["bluetooth", "xinput"])
                    {
                        invalidated.insert(id);
                        due.insert(id, Instant::now());
                    }
                    applied_resume_epoch = Some(epoch);
                }
                permission.enabled.store(
                    engine.settings.polling_controls
                        && permission.desired_enabled.load(Ordering::Acquire)
                        && !permission.suspended.load(Ordering::Acquire),
                    Ordering::Release,
                );
                permission.acknowledged.store(epoch, Ordering::Release);
            }
            Work::Command(Ok(Command::NotificationFailed(n))) => {
                remember_failed_notification(&engine, &mut failed_delivery, n, Instant::now());
            }
            Work::Command(Ok(Command::Refresh)) => {
                hid.invalidate();
                usage_tracker.invalidate(
                    hid.generation(),
                    engine.settings.polling_controls && !suspended,
                );
                let _ = events.send(Event::PollingInvalidated(hid.generation()));
                for id in hb_providers::provider::FAMILIES
                    .iter()
                    .map(|p| p.0)
                    .chain(["bluetooth", "xinput"])
                {
                    invalidated.insert(id);
                    due.insert(id, Instant::now());
                }
            }
            Work::Command(Ok(Command::ConfigurationVisible(_))) => {}
            Work::Command(Ok(Command::ConfigurationRefresh)) => {
                configuration_due = Instant::now();
            }
            Work::Command(Ok(Command::History {
                key,
                since,
                until,
                width,
                request,
            })) => {
                let _ = storage.send(Storage::History(key, since, until, width, request));
            }
            Work::Command(Ok(Command::UsageHistory {
                key,
                seconds,
                until,
                width,
                request,
            })) => {
                let _ = storage.send(Storage::UsageHistory(key, seconds, until, width, request));
            }
            Work::Command(Ok(Command::Insights {
                key,
                until,
                request,
            })) => {
                let _ = storage.send(Storage::Insights(key, until, request));
            }
            Work::Command(Ok(Command::DeviceChanged { arrival_or_removal })) => {
                // Plug/unplug remains responsive while gaming. Routine property
                // updates respect quiet cadence. Both paths coalesce event bursts
                // through the existing due map instead of immediately polling.
                let quiet_refresh = quiet && !arrival_or_removal;
                hid.invalidate();
                usage_tracker.invalidate(
                    hid.generation(),
                    engine.settings.polling_controls && !suspended,
                );
                let _ = events.send(Event::PollingInvalidated(hid.generation()));
                schedule_connection_refresh(
                    &mut due,
                    &mut invalidated,
                    hb_providers::provider::FAMILIES
                        .iter()
                        .map(|family| family.0)
                        .chain(["bluetooth", "xinput"]),
                    Instant::now(),
                    &engine.settings,
                    quiet_refresh,
                );
            }
            Work::Completed(Ok(WorkerCompleted::Battery(mut r))) => {
                inflight = inflight.saturating_sub(1);
                let timestamps_confirmed =
                    !reject_future_battery_readings(&mut r.result, observed_now);
                let id = r.provider.id();
                let current = battery_completion_current(
                    &r,
                    &engine.settings,
                    permission.epoch.load(Ordering::Acquire),
                    hid.generation(),
                    suspended,
                );
                snapshots.changed |= current && battery_affects_snapshot(&engine, id, &r.result);
                diagnostics_dirty |=
                    update_diagnostics(&mut diagnostics, id, r.provider.diagnostics());
                let observations = completed_observations(
                    engine.provider_readings(id),
                    &r.result,
                    observed_now,
                    current,
                );
                if current {
                    let inventory_confirmed = r.result.is_ok() && timestamps_confirmed;
                    for n in engine.apply(id, r.result, clock.monotonic().as_secs_f64(), quiet) {
                        let _ = events.send(Event::Alert(n));
                    }
                    // Inspect the existing provider slice: no inventory clone,
                    // timer, or extra discovery is needed for disconnect safety.
                    // A communication failure only pauses writes. Successful
                    // discovery of a missing/changed unit revokes its originals,
                    // except a bounded wait for a proven wired charging transition.
                    if inventory_confirmed {
                        saver.observe_provider(id, engine.provider_readings(id), observed_now);
                        saver.coordinate_charging_boost(id, &mut boost);
                        boost.observe_provider(id, engine.provider_readings(id), observed_now);
                        if let Some(pending) = flush_recovery_rearms(
                            &recovery,
                            &mut deferred_recovery_rearms,
                            &mut recovery_rearm_confirmed,
                        ) {
                            recovery_pending |= pending;
                        }
                        if recovery_pending {
                            recovery_pending = recover_available(
                                &recovery,
                                id,
                                engine.provider_readings(id),
                                observed_now,
                                RecoveryContext {
                                    settings: &engine.settings,
                                    generation: hid.generation(),
                                    saver: &mut saver,
                                    boost: &mut boost,
                                },
                            );
                            saver_dirty = true;
                        }
                    }
                    if let Some(level) = engine.take_low_battery_sound() {
                        let _ = hb_windows::system::play_low_battery_sound(level);
                    }
                }
                estimator_dirty |= !observations.is_empty();
                if !current {
                    r.provider.invalidate();
                }
                let delay = battery_provider_delay(
                    id,
                    engine.settings.interval,
                    quiet,
                    r.provider.next_poll_delay(),
                );
                let delay = startup_discovery_delay(
                    delay,
                    !engine.has_readings(),
                    monitoring_started.elapsed(),
                    quiet,
                );
                schedule_completed_provider(&mut due, id, Instant::now(), delay);
                providers.insert(id, r.provider);
                usage_tracker.synchronize(
                    hid.generation(),
                    engine.settings.polling_controls && !suspended,
                );
                if !observations.is_empty() {
                    let samples = usage_tracker.samples(observations);
                    startup_polling.reads.observe_usage(
                        &samples,
                        engine.settings.polling_controls && !suspended && !stop,
                        Instant::now(),
                        observed_now,
                    );
                    let _ = storage.send(Storage::UsageSample(samples));
                }
            }
            Work::Completed(Ok(WorkerCompleted::BatteryGateBusy(mut job))) => {
                inflight = inflight.saturating_sub(1);
                let id = job.provider.id();
                let revoked = job.epoch != permission.epoch.load(Ordering::Acquire)
                    || job.generation != hid.generation()
                    || suspended
                    || !engine.settings.enabled(id);
                if revoked {
                    job.provider.invalidate();
                }
                if !revoked {
                    diagnostics_dirty |= update_diagnostics(
                        &mut diagnostics,
                        id,
                        vec!["The device is busy. Monitoring will retry.".into()],
                    );
                }
                providers.insert(id, job.provider);
                // Avoid a tight retry loop when another native call remains stuck.
                due.insert(
                    id,
                    Instant::now()
                        + if revoked {
                            Duration::ZERO
                        } else {
                            Duration::from_secs(30)
                        },
                );
            }
            Work::Completed(Ok(WorkerCompleted::Configuration(epoch, generation, result))) => {
                inflight = inflight.saturating_sub(1);
                configuration_inflight = false;
                if configuration.active(epoch)
                    && generation == hid.generation()
                    && !suspended
                    && !stop
                {
                    configuration_generation = generation;
                    match result {
                        Ok(devices) => {
                            configuration_dirty |= configuration_devices != devices
                                || !configuration_valid
                                || configuration_failure.is_some();
                            configuration_devices = devices;
                            configuration_valid = true;
                            configuration_failure = None;
                        }
                        Err(error) => {
                            configuration_dirty = true;
                            configuration_valid = false;
                            for device in &mut configuration_devices {
                                device.connection = Connection::Stale;
                            }
                            configuration_failure = Some(error.to_string());
                        }
                    }
                } else {
                    configuration_due = Instant::now();
                }
            }
            Work::Command(Ok(Command::DeviceInfo(request, cancelled, visible))) => {
                let allowed = !suspended
                    && !stop
                    && configuration.active(visible)
                    && !cancelled.load(Ordering::Acquire)
                    && request.target.generation == hid.generation()
                    && (engine
                        .readings()
                        .iter()
                        .any(|r| request.target.device.matches_reading(r) && r.online())
                        || (configuration_valid
                            && configuration_generation == hid.generation()
                            && configuration_devices
                                .iter()
                                .any(|d| configuration_matches(&request.target.device, d))));
                if !allowed || simulate {
                    let _ = events.send(Event::DeviceInfo(Box::new(DeviceInfoOutcome::failed(
                        &request,
                        if simulate {
                            "Firmware is not read from simulated devices."
                        } else {
                            "The device or connection changed. Open Device info again."
                        },
                    ))));
                } else {
                    match jobs.try_send(WorkerJob::DeviceInfo(request.clone(), cancelled, visible))
                    {
                        Ok(()) => inflight += 1,
                        Err(_) => {
                            let _ = events.send(Event::DeviceInfo(Box::new(
                                DeviceInfoOutcome::failed(
                                    &request,
                                    "Device workers are busy. Try Refresh information shortly.",
                                ),
                            )));
                        }
                    }
                }
            }
            Work::Completed(Ok(WorkerCompleted::DeviceInfo(mut outcome, visible))) => {
                inflight = inflight.saturating_sub(1);
                if suspended
                    || stop
                    || !configuration.active(visible)
                    || outcome.target.generation != hid.generation()
                {
                    outcome.firmware = None;
                    outcome.failure =
                        Some("The connection changed. Open Device info again.".into());
                }
                let _ = events.send(Event::DeviceInfo(outcome));
            }
            Work::Command(Ok(Command::Dpi(request, epoch, visible))) => {
                let allowed = epoch == permission.epoch.load(Ordering::Acquire)
                    && permission.enabled.load(Ordering::Acquire)
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && permission.acknowledged.load(Ordering::Acquire) == epoch
                    && engine.settings.polling_controls
                    && !suspended
                    && !stop
                    && configuration.active(visible)
                    && request.target.generation == hid.generation()
                    && (engine
                        .readings()
                        .iter()
                        .any(|r| request.target.device.matches_reading(r) && r.online())
                        || (configuration_valid
                            && configuration_generation == hid.generation()
                            && configuration_devices
                                .iter()
                                .any(|d| configuration_matches(&request.target.device, d))));
                if !allowed || simulate {
                    let _ = events.send(Event::Dpi(Box::new(DpiOutcome::failed(
                        &request,
                        if simulate {
                            "DPI changes are disabled in simulation."
                        } else {
                            "Connection or permission changed. Refresh Devices."
                        },
                    ))));
                } else {
                    match jobs.try_send(WorkerJob::Dpi(
                        Box::new(request.clone()),
                        control_cancel.clone(),
                        epoch,
                        visible,
                    )) {
                        Ok(()) => inflight += 1,
                        Err(_) => {
                            let _ = events.send(Event::Dpi(Box::new(DpiOutcome::failed(
                                &request,
                                "Device workers are busy. Try again shortly.",
                            ))));
                        }
                    }
                }
            }
            Work::Completed(Ok(WorkerCompleted::Dpi(mut outcome, epoch, visible))) => {
                inflight = inflight.saturating_sub(1);
                if epoch != permission.epoch.load(Ordering::Acquire)
                    || !permission.enabled.load(Ordering::Acquire)
                    || !permission.desired_enabled.load(Ordering::Acquire)
                    || suspended
                    || stop
                    || !configuration.active(visible)
                    || outcome.generation != hid.generation()
                {
                    outcome.current = None;
                    outcome.failure =
                        Some("Connection or permission changed. Read current DPI again.".into());
                }
                let _ = events.send(Event::Dpi(outcome));
            }
            Work::Command(Ok(Command::Sleep(request, epoch, visible))) => {
                let saver_cancel = saver.cancel(request.request);
                let manual_sleep_change = saver_cancel.is_none() && request.seconds.is_some();
                if manual_sleep_change {
                    if recovery_pending && let Ok(mut journal) = recovery.try_lock() {
                        journal
                            .suppress(&request.target.device, crate::recovery::SettingKind::Sleep);
                        recovery_pending = journal.has_pending();
                    }
                    saver.observe(
                        &engine.settings,
                        hid.generation(),
                        &engine.readings(),
                        clock.unix(),
                    );
                    saver.manual(&request.target.device, true);
                }
                let allowed = epoch == permission.epoch.load(Ordering::Acquire)
                    && permission.enabled.load(Ordering::Acquire)
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && permission.acknowledged.load(Ordering::Acquire) == epoch
                    && engine.settings.polling_controls
                    && !suspended
                    && !stop
                    && (configuration.active(visible)
                        || (visible == crate::battery_saver::BACKGROUND && saver_cancel.is_some()))
                    && request.target.generation == hid.generation()
                    && (engine
                        .readings()
                        .iter()
                        .any(|r| request.target.device.matches_reading(r) && r.online())
                        || (configuration_valid
                            && configuration_generation == hid.generation()
                            && configuration_devices
                                .iter()
                                .any(|d| configuration_matches(&request.target.device, d))));
                if !allowed || simulate {
                    if manual_sleep_change {
                        recovery_pending |= rearm_recovery(
                            &recovery,
                            &request.target.device,
                            crate::recovery::SettingKind::Sleep,
                            &mut deferred_recovery_rearms,
                        );
                    }
                    let message = if simulate {
                        "Sleep settings do not send commands in simulation mode."
                    } else {
                        "Device connection or permission changed. Refresh Devices before trying again."
                    };
                    let outcome = SleepOutcome::failed(&request, message);
                    let publish = finish_sleep_outcome(&mut saver, &outcome);
                    saver_dirty = true;
                    if publish {
                        let _ = events.send(Event::Sleep(Box::new(outcome)));
                    }
                } else {
                    let saver_info = saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request));
                    let recovery_op =
                        sleep_recovery_operation(&request, saver_cancel.is_some(), saver_info);
                    match jobs.try_send(WorkerJob::Sleep(
                        Box::new(request.clone()),
                        saver_cancel.unwrap_or_else(|| control_cancel.clone()),
                        epoch,
                        visible,
                        recovery_op,
                    )) {
                        Ok(()) => inflight += 1,
                        Err(_) => {
                            if manual_sleep_change {
                                recovery_pending |= rearm_recovery(
                                    &recovery,
                                    &request.target.device,
                                    crate::recovery::SettingKind::Sleep,
                                    &mut deferred_recovery_rearms,
                                );
                            }
                            let outcome = SleepOutcome::failed(
                                &request,
                                "Device workers are busy. Try again shortly.",
                            );
                            let publish = finish_sleep_outcome(&mut saver, &outcome);
                            saver_dirty = true;
                            if publish {
                                let _ = events.send(Event::Sleep(Box::new(outcome)));
                            }
                        }
                    }
                }
            }
            Work::Completed(Ok(WorkerCompleted::Sleep(mut outcome, epoch, visible, receipt))) => {
                inflight = inflight.saturating_sub(1);
                apply_recovery_receipt(
                    receipt.as_deref(),
                    crate::recovery::SettingKind::Sleep,
                    &mut saver,
                    &mut boost,
                );
                if !control_epoch_current(
                    epoch,
                    permission.epoch.load(Ordering::Acquire),
                    &mut boost,
                    &mut saver,
                ) || !permission.enabled.load(Ordering::Acquire)
                    || !permission.desired_enabled.load(Ordering::Acquire)
                    || suspended
                    || stop
                    || (visible != crate::battery_saver::BACKGROUND
                        && !configuration.active(visible))
                    || outcome.generation != hid.generation()
                {
                    outcome.seconds = None;
                    outcome.failure=Some("Connection or permission changed; sleep setting is unknown. Refresh before retrying.".into());
                }
                if visible != crate::battery_saver::BACKGROUND
                    && outcome.failure.is_some()
                    && !outcome.may_have_changed
                    && let Some(RecoveryReceipt { device }) = receipt.as_deref()
                {
                    recovery_pending |= rearm_recovery(
                        &recovery,
                        device,
                        crate::recovery::SettingKind::Sleep,
                        &mut deferred_recovery_rearms,
                    );
                }
                let publish = finish_sleep_outcome(&mut saver, &outcome);
                saver_dirty = true;
                if publish {
                    let _ = events.send(Event::Sleep(outcome));
                }
            }
            Work::Command(Ok(Command::Polling(request, epoch, configuration_epoch))) => {
                let automatic = request.request & crate::boost::REQUEST_BIT != 0;
                let saver_cancel = saver.cancel(request.request);
                let saver_request = request.request & crate::battery_saver::REQUEST_BIT != 0;
                if manual_polling_change(&request) {
                    if recovery_pending && let Ok(mut journal) = recovery.try_lock() {
                        journal.suppress(
                            &request.target.device,
                            crate::recovery::SettingKind::Polling,
                        );
                        recovery_pending = journal.has_pending();
                    }
                    startup_polling
                        .restore
                        .cancel_device(&request.target.device);
                    saver.observe(
                        &engine.settings,
                        hid.generation(),
                        &engine.readings(),
                        clock.unix(),
                    );
                    saver.manual(&request.target.device, false);
                    boost.manual_device(&request.target.device);
                }
                let boost_guard = boost.guard(request.request);
                let startup_restore = startup_restore_request(request.request);
                let startup_guard = startup_restore
                    .then(|| startup_polling.restore.guard(request.request))
                    .flatten();
                if saver_request {
                    usage_tracker.begin_saver_control(
                        &request,
                        saver.restoring(request.request).unwrap_or(false),
                    );
                } else {
                    usage_tracker.begin_control(
                        &request,
                        boost_guard.as_ref().map(|guard| guard.fullscreen),
                    );
                }
                if request.request & STARTUP_POLLING_REQUEST_BIT == 0 {
                    startup_polling
                        .reads
                        .observe_request(&request.target.device.key);
                }
                let allowed = (!automatic || boost_guard.is_some())
                    && (!saver_request || saver_cancel.is_some())
                    && (!startup_restore
                        || startup_guard
                            .as_ref()
                            .is_some_and(|guard| !guard.load(Ordering::Acquire)))
                    && epoch == permission.epoch.load(Ordering::Acquire)
                    && permission.enabled.load(Ordering::Acquire)
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && permission.acknowledged.load(Ordering::Acquire) == epoch
                    && engine.settings.polling_controls
                    && !suspended
                    && !stop
                    && (request.target.device.kind != "keyboard"
                        || configuration_epoch.is_some_and(|epoch| configuration.active(epoch)))
                    && (engine
                        .readings()
                        .iter()
                        .any(|r| request.target.device.matches_reading(r) && r.online())
                        || (configuration_valid
                            && configuration_generation == hid.generation()
                            && configuration_devices
                                .iter()
                                .any(|d| configuration_matches(&request.target.device, d))));
                if !allowed {
                    if manual_polling_change(&request) {
                        recovery_pending |= rearm_recovery(
                            &recovery,
                            &request.target.device,
                            crate::recovery::SettingKind::Polling,
                            &mut deferred_recovery_rearms,
                        );
                    }
                    let outcome = ControlOutcome::failed(
                        &request,
                        "Enable polling controls and select an online device before configuring it",
                    );
                    usage_tracker.take_activity(&outcome);
                    let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                    saver_dirty = true;
                    startup_polling.restore.complete(request.request);
                    startup_polling.reads.complete(&outcome, Instant::now());
                    if publish {
                        let _ = events.send(Event::Polling(Box::new(outcome)));
                    }
                } else if simulate {
                    let outcome = simulate_control(
                        &request,
                        simulated_rates
                            .entry(request.target.device.key.clone())
                            .or_insert_with(|| PollingRate::try_from(1000).unwrap()),
                        hid.generation(),
                        clock.unix(),
                    );
                    if changed_configuration(&outcome, &mut observed_rates) {
                        engine.reset_estimate(&outcome.key);
                        snapshots.changed = true;
                        let _ = storage.send(Storage::State(engine.estimator.clone()));
                    }
                    usage_tracker.synchronize(
                        hid.generation(),
                        engine.settings.polling_controls && !suspended,
                    );
                    usage_tracker.observe(&outcome);
                    if let Some(activity) = usage_tracker.take_activity(&outcome) {
                        let _ = storage.send(Storage::PollingActivity(activity));
                    }
                    let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                    saver_dirty = true;
                    startup_polling.restore.complete(request.request);
                    startup_polling.reads.complete(&outcome, Instant::now());
                    if publish {
                        let _ = events.send(Event::Polling(Box::new(outcome)));
                    }
                } else {
                    let saver_info = saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request));
                    let recovery_op = polling_recovery_operation(
                        &request,
                        boost_guard.as_ref(),
                        saver_request,
                        saver_info,
                    );
                    match jobs.try_send(WorkerJob::Polling(
                        Box::new(request.clone()),
                        saver_cancel
                            .or(startup_guard)
                            .unwrap_or_else(|| control_cancel.clone()),
                        epoch,
                        configuration_epoch,
                        boost_guard,
                        recovery_op,
                    )) {
                        Ok(()) => inflight += 1,
                        Err(_) => {
                            if manual_polling_change(&request) {
                                recovery_pending |= rearm_recovery(
                                    &recovery,
                                    &request.target.device,
                                    crate::recovery::SettingKind::Polling,
                                    &mut deferred_recovery_rearms,
                                );
                            }
                            let outcome = ControlOutcome::failed(
                                &request,
                                "Device workers are busy; retry Refresh or Apply",
                            );
                            usage_tracker.take_activity(&outcome);
                            let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                            saver_dirty = true;
                            startup_polling.restore.complete(request.request);
                            startup_polling.reads.complete(&outcome, Instant::now());
                            if publish {
                                let _ = events.send(Event::Polling(Box::new(outcome)));
                            }
                        }
                    }
                }
            }
            Work::Completed(Ok(WorkerCompleted::Polling(
                mut outcome,
                epoch,
                configuration_epoch,
                receipt,
            ))) => {
                inflight = inflight.saturating_sub(1);
                apply_recovery_receipt(
                    receipt.as_deref(),
                    crate::recovery::SettingKind::Polling,
                    &mut saver,
                    &mut boost,
                );
                if startup_restore_request(outcome.request)
                    && startup_polling
                        .restore
                        .complete(outcome.request)
                        .unwrap_or(true)
                {
                    // A newer manual choice or revoked permission owns the
                    // device. Never publish this older completion as current.
                    continue;
                }
                if outcome.request & (STARTUP_POLLING_REQUEST_BIT | STARTUP_POLLING_READ_BIT)
                    == (STARTUP_POLLING_REQUEST_BIT | STARTUP_POLLING_READ_BIT)
                    && !startup_polling.reads.owns(outcome.request, &outcome.key)
                {
                    // A manual/boost action or a newer connection episode owns
                    // this device now. An old GET must not replace its evidence.
                    continue;
                }
                let permitted = control_epoch_current(
                    epoch,
                    permission.epoch.load(Ordering::Acquire),
                    &mut boost,
                    &mut saver,
                ) && permission.enabled.load(Ordering::Acquire)
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && permission.acknowledged.load(Ordering::Acquire) == epoch
                    && engine.settings.polling_controls
                    && !suspended
                    && !stop
                    && configuration_epoch.is_none_or(|epoch| configuration.active(epoch));
                validate_polling_completion(&mut outcome, hid.generation(), permitted);
                if outcome.request
                    & ((1 << 63) | crate::boost::REQUEST_BIT | crate::battery_saver::REQUEST_BIT)
                    == 0
                    && outcome.failure.is_some()
                    && !outcome.may_have_changed
                    && let Some(RecoveryReceipt { device }) = receipt.as_deref()
                {
                    recovery_pending |= rearm_recovery(
                        &recovery,
                        device,
                        crate::recovery::SettingKind::Polling,
                        &mut deferred_recovery_rearms,
                    );
                }
                if changed_configuration(&outcome, &mut observed_rates) {
                    engine.reset_estimate(&outcome.key);
                    snapshots.changed = true;
                    let _ = storage.send(Storage::State(engine.estimator.clone()));
                }
                diagnostics_dirty |= update_diagnostics(
                    &mut diagnostics,
                    "polling_controls",
                    vec![format!(
                        "request {}: {}",
                        outcome.request,
                        outcome
                            .failure
                            .as_deref()
                            .unwrap_or("hardware readback verified")
                    )],
                );
                usage_tracker.synchronize(
                    hid.generation(),
                    engine.settings.polling_controls && !suspended,
                );
                usage_tracker.observe(&outcome);
                if let Some(activity) = usage_tracker.take_activity(&outcome) {
                    let _ = storage.send(Storage::PollingActivity(activity));
                }
                let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                saver_dirty = true;
                startup_polling.reads.complete(&outcome, Instant::now());
                if publish {
                    let _ = events.send(Event::Polling(outcome));
                }
            }
            Work::Command(Ok(
                Command::SettingsChanged
                | Command::EpochSuspend(_)
                | Command::EpochResume(_)
                | Command::Settings(_)
                | Command::Suspend
                | Command::Resume,
            )) => {}
            Work::Completed(Err(_)) | Work::Idle => {}
        }
        // Battery completions below the scheduler can publish immediately; keep
        // their evidence pending for saver evaluation on the next engine turn.
        saver_dirty |= snapshots.changed;
        snapshots.publish(&engine, clock.unix(), Instant::now(), &events, &storage);
        if estimator_dirty && estimator_checkpoint.elapsed() >= Duration::from_secs(60) {
            // Bound attempts too: a busy storage queue must not clone the
            // learned state on every runtime wake until it catches up.
            estimator_checkpoint = Instant::now();
            if storage
                .try_send(Storage::State(engine.estimator.clone()))
                .is_ok()
            {
                estimator_dirty = false;
            }
        }
        publish_diagnostics(&events, &diagnostics, &mut diagnostics_dirty);
        let retry = failed_delivery
            .iter()
            .filter(|(_, v)| v.0 <= Instant::now())
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in retry {
            if let Some((_, n)) = failed_delivery.remove(&key) {
                engine.notification_failed(n);
            }
        }
        if !suspended
            && engine.has_held_notifications()
            && (!engine.settings.quiet_fullscreen || !hb_windows::system::gaming())
        {
            for n in engine.flush_held() {
                let _ = events.send(Event::Alert(n));
            }
        }
    }
    drop(jobs);
    drop(winrt);
    drop(result_rx);
    // Native feature-report calls may outlive cooperative cancellation. Never
    // let one stall history flushing or hold the UI's Exit operation forever.
    let until = Instant::now() + Duration::from_millis(500);
    for w in workers {
        if wait_for_thread(&w, until, || {}) {
            let _ = w.join();
        }
        // Dropping an unfinished JoinHandle detaches its owned worker; it cannot
        // access UI resources and cancellation forbids subsequent device calls.
    }
    let boundaries = engine
        .readings()
        .into_iter()
        .filter(|r| r.connection != Connection::Stale)
        .map(|r| availability_boundary(r, Connection::Stale, clock.unix()))
        .collect();
    let _ = storage.send(Storage::UsageSample(usage_tracker.samples(boundaries)));
    let _ = storage.send(Storage::State(engine.estimator.clone()));
    if engine.settings.status_file {
        let _ = storage.send(Storage::Status(engine.snapshot(clock.unix()), false));
    }
    let _ = storage.send(Storage::Quit);
    let _ = store_thread.join();
}

#[cfg(test)]
mod tests {
    fn recovery_audit_fixture(
        layer: crate::recovery::Layer,
        before: u32,
        applied: u32,
    ) -> (
        tempfile::TempDir,
        Reading,
        Settings,
        crate::recovery::Record,
        Arc<Mutex<crate::recovery::Journal>>,
    ) {
        use crate::recovery::{Journal, Operation, Record};
        let folder = tempfile::tempdir().unwrap();
        let mut reading = Reading::new("razer:00bf:AUDIT", "Razer DeathAdder V4 Pro", "razer", 100);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        reading.serial = Some("AUDIT".into());
        reading.container = Some("00112233445566778899aabbccddeeff".into());
        reading.level = Some(100);
        reading.charging = Some(false);
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        if let crate::recovery::Layer::SaverPolling(mode)
        | crate::recovery::Layer::SaverSleep(mode) = layer
        {
            settings.device_mut(&reading.key).low_battery_mode = Some(mode);
        }
        let record = Record {
            device: ConfigurationDevice::from_reading(&reading),
            stamp: ConnectionStamp::new(1).unwrap(),
            rebound_stamp: None,
            before,
            applied,
            layer,
            confirmed: false,
            manual_pending: false,
        };
        let mut journal = Journal::open(folder.path());
        journal.execute(Operation::Prepare(record.clone())).unwrap();
        journal.execute(Operation::Confirm(record.clone())).unwrap();
        let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
        (folder, reading, settings, record, journal)
    }

    #[test]
    fn cancelled_rebound_restore_keeps_live_and_durable_connection_proofs_in_sync() {
        use crate::recovery::{Layer, Operation, SettingKind};
        let mode = hb_core::settings::LowBatteryMode {
            threshold: 20,
            polling_rate: PollingRate::try_from(125).ok(),
            sleep_seconds: Some(60),
        };
        for kind in 0..3 {
            let (layer, before, applied) = match kind {
                0 => (Layer::SaverPolling(mode), 1000, 125),
                1 => (Layer::SaverSleep(mode), 300, 60),
                _ => (Layer::BoostPolling, 1000, 4000),
            };
            let (_folder, reading, settings, record, journal) =
                recovery_audit_fixture(layer, before, applied);
            let now = Instant::now();
            let mut saver = crate::battery_saver::BatterySaver::default();
            let mut boost = crate::boost::Boost::new(now);
            assert!(!recover_available(
                &journal,
                "razer",
                std::slice::from_ref(&reading),
                100,
                RecoveryContext {
                    settings: &settings,
                    generation: 1,
                    saver: &mut saver,
                    boost: &mut boost
                }
            ));
            boost.sample(now, Some(false));
            boost.sample(now + Duration::from_secs(15), Some(false));
            let action = if kind == 2 {
                crate::battery_saver::Action::Polling(
                    boost
                        .next(&settings, 1, std::slice::from_ref(&reading))
                        .unwrap(),
                )
            } else {
                saver
                    .next(&settings, 1, std::slice::from_ref(&reading), 100, &boost)
                    .unwrap()
            };
            journal
                .lock()
                .unwrap()
                .execute(Operation::Rebind {
                    device: record.device.clone(),
                    old_stamp: record.stamp,
                    new_stamp: ConnectionStamp::new(2).unwrap(),
                    kind: if kind == 1 {
                        SettingKind::Sleep
                    } else {
                        SettingKind::Polling
                    },
                    expected: applied,
                    desired: before,
                })
                .unwrap();
            saver.settings_changed();
            boost.settings_changed();
            match &action {
                crate::battery_saver::Action::Polling(request) => {
                    finish_polling_outcome(
                        &mut boost,
                        &mut saver,
                        &ControlOutcome::failed(request, "Settings changed before SET"),
                    );
                }
                crate::battery_saver::Action::Sleep(request) => {
                    finish_sleep_outcome(
                        &mut saver,
                        &SleepOutcome::failed(request, "Settings changed before SET"),
                    );
                }
            }
            let retry = if kind == 2 {
                crate::battery_saver::Action::Polling(
                    boost
                        .next(&settings, 1, std::slice::from_ref(&reading))
                        .unwrap(),
                )
            } else {
                saver
                    .next(&settings, 1, std::slice::from_ref(&reading), 100, &boost)
                    .unwrap()
            };
            let operation = match &retry {
                crate::battery_saver::Action::Polling(request) => polling_recovery_operation(
                    request,
                    boost.guard(request.request).as_ref(),
                    kind != 2,
                    saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request)),
                ),
                crate::battery_saver::Action::Sleep(request) => sleep_recovery_operation(
                    request,
                    true,
                    saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request)),
                ),
            }
            .unwrap();
            assert!(
                journal.lock().unwrap().execute(operation).is_ok(),
                "A cancellation after rebind lost the original connection proof for kind {kind}"
            );
        }
    }

    #[test]
    fn late_recovery_cannot_override_a_completed_manual_choice() {
        use crate::recovery::{Layer, Operation, SettingKind};
        let (_folder, reading, settings, record, journal) =
            recovery_audit_fixture(Layer::BoostPolling, 1000, 4000);
        let now = Instant::now();
        let mut boost = crate::boost::Boost::new(now);
        let mut saver = crate::battery_saver::BatterySaver::default();
        boost.manual_device(&record.device);
        // Simulate storage temporarily holding the journal when the manual
        // command tried to suppress pending startup recovery.
        {
            let _busy = journal.lock().unwrap();
            assert!(journal.try_lock().is_err());
        }
        assert!(!recover_available(
            &journal,
            "razer",
            std::slice::from_ref(&reading),
            100,
            RecoveryContext {
                settings: &settings,
                generation: 1,
                saver: &mut saver,
                boost: &mut boost
            }
        ));
        journal
            .lock()
            .unwrap()
            .execute(Operation::ManualBegin {
                device: record.device.clone(),
                kind: SettingKind::Polling,
                desired: 4000,
            })
            .unwrap();
        journal
            .lock()
            .unwrap()
            .execute(Operation::ManualFinish {
                device: record.device.clone(),
                kind: SettingKind::Polling,
                completed: true,
            })
            .unwrap();
        apply_recovery_receipt(
            Some(&RecoveryReceipt {
                device: record.device.clone(),
            }),
            SettingKind::Polling,
            &mut saver,
            &mut boost,
        );
        boost.sample(now, Some(false));
        boost.sample(now + Duration::from_secs(15), Some(false));
        assert!(
            boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .is_none(),
            "An automatic restore revived while a manual SET was waiting and overrode it"
        );
    }

    #[test]
    fn recovery_rearm_survives_storage_contention_and_control_reenable() {
        use crate::recovery::{Layer, SettingKind};
        let mode = hb_core::settings::LowBatteryMode {
            threshold: 20,
            polling_rate: None,
            sleep_seconds: Some(60),
        };
        for reenable in [false, true] {
            let (_folder, reading, settings, record, journal) =
                recovery_audit_fixture(Layer::SaverSleep(mode), 300, 60);
            let mut saver = crate::battery_saver::BatterySaver::default();
            let mut boost = crate::boost::Boost::new(Instant::now());
            assert!(!recover_available(
                &journal,
                "razer",
                std::slice::from_ref(&reading),
                100,
                RecoveryContext {
                    settings: &settings,
                    generation: 1,
                    saver: &mut saver,
                    boost: &mut boost
                }
            ));
            saver.pause();
            boost.clear();
            let mut deferred = Vec::new();
            let mut all_confirmed = reenable;
            {
                let _busy = journal.lock().unwrap();
                if !reenable {
                    assert!(rearm_recovery(
                        &journal,
                        &record.device,
                        SettingKind::Sleep,
                        &mut deferred
                    ));
                    assert!(rearm_recovery(
                        &journal,
                        &record.device,
                        SettingKind::Sleep,
                        &mut deferred
                    ));
                    assert_eq!(deferred.len(), 1);
                }
                assert!(
                    flush_recovery_rearms(&journal, &mut deferred, &mut all_confirmed).is_none()
                );
            }
            assert_eq!(
                flush_recovery_rearms(&journal, &mut deferred, &mut all_confirmed),
                Some(true)
            );
            assert!(deferred.is_empty());
            assert_eq!(deferred.capacity(), 0);
            assert!(!all_confirmed);
            assert!(!recover_available(
                &journal,
                "razer",
                std::slice::from_ref(&reading),
                100,
                RecoveryContext {
                    settings: &settings,
                    generation: 1,
                    saver: &mut saver,
                    boost: &mut boost
                }
            ));
            assert!(
                matches!(saver.next(&settings, 1, std::slice::from_ref(&reading), 100, &boost),
                Some(crate::battery_saver::Action::Sleep(request)) if request.seconds == Some(300)
                    && request.expected == Some(60))
            );
        }
    }

    #[test]
    fn deferred_recovery_replaces_only_unwritten_preflights() {
        use crate::recovery::Layer;
        let mode = hb_core::settings::LowBatteryMode {
            threshold: 20,
            polling_rate: PollingRate::try_from(125).ok(),
            sleep_seconds: None,
        };
        let (_folder, mut reading, settings, _record, journal) =
            recovery_audit_fixture(Layer::SaverPolling(mode), 1000, 125);
        reading.level = Some(10);
        let mut saver = crate::battery_saver::BatterySaver::default();
        let mut boost = crate::boost::Boost::new(Instant::now());
        saver.observe(&settings, 1, std::slice::from_ref(&reading), 100);
        assert!(!recover_available(
            &journal,
            "razer",
            std::slice::from_ref(&reading),
            100,
            RecoveryContext {
                settings: &settings,
                generation: 1,
                saver: &mut saver,
                boost: &mut boost
            }
        ));
        assert!(
            saver
                .next(&settings, 1, std::slice::from_ref(&reading), 100, &boost)
                .is_none()
        );
        reading.level = Some(100);
        assert!(
            matches!(saver.next(&settings, 1, std::slice::from_ref(&reading), 100, &boost),
            Some(crate::battery_saver::Action::Polling(request)) if request.action == ControlAction::ApplyIf {
                rate: PollingRate::try_from(1000).unwrap(), expected: PollingRate::try_from(125).unwrap(),
            })
        );
    }

    #[test]
    fn unconfirmed_recovery_is_reconciled_even_if_the_battery_is_still_low() {
        use crate::recovery::{Journal, Layer, Operation, Record};
        use hb_core::settings::LowBatteryMode;
        for confirmed in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            reading.level = Some(10);
            reading.charging = Some(false);
            let mode = LowBatteryMode {
                threshold: 20,
                polling_rate: PollingRate::try_from(125).ok(),
                sleep_seconds: None,
            };
            let record = Record {
                device: ConfigurationDevice::from_reading(&reading),
                stamp: ConnectionStamp::new(1).unwrap(),
                rebound_stamp: None,
                before: 1000,
                applied: 125,
                layer: Layer::SaverPolling(mode),
                confirmed: false,
                manual_pending: false,
            };
            let mut journal = Journal::open(folder.path());
            journal.execute(Operation::Prepare(record.clone())).unwrap();
            if confirmed {
                journal.execute(Operation::Confirm(record)).unwrap();
            }
            let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
            let mut settings = Settings {
                polling_controls: true,
                ..Default::default()
            };
            settings.device_mut(&reading.key).low_battery_mode = Some(mode);
            let mut saver = crate::battery_saver::BatterySaver::default();
            let mut boost = crate::boost::Boost::new(Instant::now());
            assert!(!recover_available(
                &journal,
                "razer",
                std::slice::from_ref(&reading),
                100,
                RecoveryContext {
                    settings: &settings,
                    generation: 1,
                    saver: &mut saver,
                    boost: &mut boost,
                }
            ));
            let action = saver.next(&settings, 1, std::slice::from_ref(&reading), 100, &boost);
            if confirmed {
                assert!(
                    action.is_none(),
                    "A confirmed low-battery setting should remain active"
                );
            } else {
                assert!(
                    matches!(action, Some(crate::battery_saver::Action::Polling(request))
                    if request.action == ControlAction::ApplyIf {
                        rate: PollingRate::try_from(1000).unwrap(), expected: PollingRate::try_from(125).unwrap()
                    })
                );
            }
        }
    }

    #[test]
    fn recovery_write_ahead_is_acknowledged_by_the_existing_storage_worker() {
        use crate::recovery::Journal;
        use hb_core::settings::LowBatteryMode;
        let folder = tempfile::tempdir().unwrap();
        let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
        let (storage, receiver) = bounded(8);
        let (boot, ready) = bounded(1);
        let (events, _event_rx) = bounded(16);
        let sink = Events {
            tx: events,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let path = folder.path().to_owned();
        let worker = thread::spawn(move || crate::storage_worker::run(path, receiver, boot, sink));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let access = WorkerAccess {
            storage: storage.clone(),
            recovery: journal.clone(),
            gates: BTreeMap::new(),
            permission: Arc::new(ControlPermission::new(true)),
            configuration: Arc::new(ConfigurationWatch::default()),
        };
        let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        let request = ControlRequest {
            request: crate::battery_saver::REQUEST_BIT | 1,
            target: ControlTarget {
                device: ConfigurationDevice::from_reading(&reading),
                generation: 1,
                connection_stamp: ConnectionStamp::new(1),
            },
            action: ControlAction::ApplyIf {
                rate: PollingRate::try_from(125).unwrap(),
                expected: PollingRate::try_from(1000).unwrap(),
            },
        };
        let mode = LowBatteryMode {
            threshold: 20,
            polling_rate: PollingRate::try_from(125).ok(),
            sleep_seconds: None,
        };
        let operation =
            polling_recovery_operation(&request, None, true, Some((false, mode))).unwrap();
        recovery_operation(&access, operation).unwrap();
        assert!(Journal::open(folder.path()).has_pending());
        storage.send(Storage::Quit).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn restarted_recovery_rebinds_only_after_a_read_only_identity_and_value_check() {
        use crate::recovery::{Journal, Layer, Operation, Record};
        use hb_core::settings::LowBatteryMode;
        for changed_elsewhere in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            reading.container = Some("00112233445566778899AABBCCDDEEFF".into());
            let device = ConfigurationDevice::from_reading(&reading);
            let mode = LowBatteryMode {
                threshold: 20,
                polling_rate: PollingRate::try_from(125).ok(),
                sleep_seconds: None,
            };
            let record = Record {
                device: device.clone(),
                stamp: ConnectionStamp::new(1).unwrap(),
                rebound_stamp: None,
                before: 1000,
                applied: 125,
                layer: Layer::SaverPolling(mode),
                confirmed: false,
                manual_pending: false,
            };
            let mut journal = Journal::open(folder.path());
            journal.execute(Operation::Prepare(record.clone())).unwrap();
            journal.execute(Operation::Confirm(record)).unwrap();
            drop(journal);
            let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
            let (storage, receiver) = bounded(8);
            let (boot, ready) = bounded(1);
            let (events, _event_rx) = bounded(16);
            let sink = Events {
                tx: events,
                window: Arc::new(AtomicUsize::new(0)),
            };
            let path = folder.path().to_owned();
            let worker =
                thread::spawn(move || crate::storage_worker::run(path, receiver, boot, sink));
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let access = WorkerAccess {
                storage: storage.clone(),
                recovery: journal,
                gates: BTreeMap::new(),
                permission: Arc::new(ControlPermission::new(true)),
                configuration: Arc::new(ConfigurationWatch::default()),
            };
            let mut request = ControlRequest {
                request: crate::battery_saver::REQUEST_BIT | 1,
                target: ControlTarget {
                    device,
                    generation: 2,
                    connection_stamp: ConnectionStamp::new(1),
                },
                action: ControlAction::ApplyIf {
                    rate: PollingRate::try_from(1000).unwrap(),
                    expected: PollingRate::try_from(125).unwrap(),
                },
            };
            let mut operation =
                polling_recovery_operation(&request, None, true, Some((true, mode))).unwrap();
            recovery_operation(&access, operation.clone()).unwrap();
            let mut hardware =
                PollingRate::try_from(if changed_elsewhere { 2000 } else { 125 }).unwrap();
            let result = probe_polling_recovery(&mut request, &mut operation, &access, |probe| {
                assert_eq!(probe.action, ControlAction::Read);
                assert!(probe.target.connection_stamp.is_none());
                let mut outcome = simulate_control(probe, &mut hardware, 2, 100);
                outcome
                    .observation
                    .as_mut()
                    .unwrap()
                    .target
                    .connection_stamp = ConnectionStamp::new(2);
                outcome
            });
            if changed_elsewhere {
                assert!(result.is_err());
                assert_eq!(hardware.hz(), 2000);
                assert_eq!(Journal::open(folder.path()).devices().count(), 0);
            } else {
                result.unwrap();
                assert_eq!(
                    hardware.hz(),
                    125,
                    "The verification probe must not send a SET"
                );
                assert_eq!(request.target.connection_stamp, ConnectionStamp::new(2));
                assert!(Journal::open(folder.path()).execute(operation).is_ok());
            }
            storage.send(Storage::Quit).unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn restarted_sleep_recovery_uses_a_read_only_connection_check() {
        use crate::recovery::{Journal, Layer, Operation, Record};
        use hb_core::settings::LowBatteryMode;
        for actual in [60, 300, 120] {
            let folder = tempfile::tempdir().unwrap();
            let mut reading = Reading::new("razer:00bf:TEST", "Mouse", "razer", 100);
            reading.kind = "mouse".into();
            reading.via = "usb".into();
            reading.serial = Some("TEST".into());
            reading.container = Some("00112233445566778899AABBCCDDEEFF".into());
            let device = ConfigurationDevice::from_reading(&reading);
            let mode = LowBatteryMode {
                threshold: 20,
                polling_rate: None,
                sleep_seconds: Some(60),
            };
            let record = Record {
                device: device.clone(),
                stamp: ConnectionStamp::new(1).unwrap(),
                rebound_stamp: None,
                before: 300,
                applied: 60,
                layer: Layer::SaverSleep(mode),
                confirmed: false,
                manual_pending: false,
            };
            let mut journal = Journal::open(folder.path());
            journal.execute(Operation::Prepare(record.clone())).unwrap();
            journal.execute(Operation::Confirm(record)).unwrap();
            drop(journal);
            let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
            let (storage, receiver) = bounded(8);
            let (boot, ready) = bounded(1);
            let (events, _event_rx) = bounded(16);
            let sink = Events {
                tx: events,
                window: Arc::new(AtomicUsize::new(0)),
            };
            let path = folder.path().to_owned();
            let worker =
                thread::spawn(move || crate::storage_worker::run(path, receiver, boot, sink));
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let access = WorkerAccess {
                storage: storage.clone(),
                recovery: journal,
                gates: BTreeMap::new(),
                permission: Arc::new(ControlPermission::new(true)),
                configuration: Arc::new(ConfigurationWatch::default()),
            };
            let mut request = SleepRequest {
                request: crate::battery_saver::REQUEST_BIT | 1,
                target: ControlTarget {
                    device,
                    generation: 2,
                    connection_stamp: ConnectionStamp::new(1),
                },
                seconds: Some(300),
                expected: Some(60),
            };
            let mut operation =
                sleep_recovery_operation(&request, true, Some((true, mode))).unwrap();
            recovery_operation(&access, operation.clone()).unwrap();
            let result = probe_sleep_recovery(&mut request, &mut operation, &access, |probe| {
                assert!(probe.seconds.is_none() && probe.expected.is_none());
                assert!(probe.target.connection_stamp.is_none());
                SleepOutcome {
                    request: probe.request,
                    key: probe.target.device.key.clone(),
                    generation: 2,
                    connection_stamp: ConnectionStamp::new(2),
                    seconds: Some(actual),
                    may_have_changed: false,
                    failure: None,
                }
            });
            if actual == 60 {
                result.unwrap();
                assert_eq!(request.target.connection_stamp, ConnectionStamp::new(2));
                assert!(Journal::open(folder.path()).execute(operation).is_ok());
            } else {
                assert!(result.is_err());
                assert_eq!(Journal::open(folder.path()).devices().count(), 0);
            }
            storage.send(Storage::Quit).unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn durable_recovery_restores_boost_then_battery_saver_after_restart() {
        use crate::battery_saver::Action;
        use crate::recovery::Journal;
        use hb_core::settings::LowBatteryMode;

        fn polling(
            journal: &mut Journal,
            request: &ControlRequest,
            boost: Option<&crate::boost::Guard>,
            saver: Option<(bool, LowBatteryMode)>,
            hardware: &mut PollingRate,
        ) -> ControlOutcome {
            let operation = polling_recovery_operation(
                request,
                boost,
                request.request & crate::battery_saver::REQUEST_BIT != 0,
                saver,
            );
            if let Some(operation) = &operation {
                journal.execute(operation.clone()).unwrap();
            }
            let outcome = simulate_control(request, hardware, request.target.generation, 100);
            if let Some(finish) = operation
                .as_ref()
                .and_then(|op| finish_polling_recovery(op, &outcome))
            {
                journal.execute(finish).unwrap();
            }
            outcome
        }
        fn sleep(
            journal: &mut Journal,
            request: &SleepRequest,
            saver: &crate::battery_saver::BatterySaver,
            hardware: &mut u16,
        ) -> SleepOutcome {
            let operation = sleep_recovery_operation(
                request,
                true,
                saver
                    .restoring(request.request)
                    .zip(saver.recovery_choices(request.request)),
            );
            if let Some(operation) = &operation {
                journal.execute(operation.clone()).unwrap();
            }
            if let Some(expected) = request.expected {
                assert_eq!(*hardware, expected);
            }
            let before = *hardware;
            if let Some(seconds) = request.seconds {
                *hardware = seconds;
            }
            let outcome = SleepOutcome {
                request: request.request,
                key: request.target.device.key.clone(),
                generation: request.target.generation,
                connection_stamp: ConnectionStamp::new(1),
                seconds: Some(*hardware),
                may_have_changed: before != *hardware,
                failure: None,
            };
            if let Some(finish) = operation
                .as_ref()
                .and_then(|op| finish_sleep_recovery(op, &outcome))
            {
                journal.execute(finish).unwrap();
            }
            outcome
        }
        let folder = tempfile::tempdir().unwrap();
        let mut reading = Reading::new("razer:00bf:TEST", "Razer DeathAdder V4 Pro", "razer", 100);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        reading.serial = Some("TEST".into());
        reading.container = Some("receiver".into());
        reading.level = Some(10);
        reading.charging = Some(false);
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        let preferences = settings.device_mut(&reading.key);
        preferences.low_battery_mode = Some(LowBatteryMode {
            threshold: 20,
            polling_rate: PollingRate::try_from(125).ok(),
            sleep_seconds: Some(60),
        });
        preferences.fullscreen_boost_rate = PollingRate::try_from(4000).ok();
        let now = Instant::now();
        let mut boost = crate::boost::Boost::new(now);
        let mut saver = crate::battery_saver::BatterySaver::default();
        let mut journal = Journal::open(folder.path());
        let mut rate = PollingRate::try_from(1000).unwrap();
        let mut seconds = 300;

        for _ in 0..4 {
            let action = saver
                .next(&settings, 1, std::slice::from_ref(&reading), 100, &boost)
                .unwrap();
            match action {
                Action::Polling(request) => {
                    let info = saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request));
                    let outcome = polling(&mut journal, &request, None, info, &mut rate);
                    assert!(saver.polling_complete(&outcome));
                }
                Action::Sleep(request) => {
                    let outcome = sleep(&mut journal, &request, &saver, &mut seconds);
                    assert!(saver.sleep_complete(&outcome));
                }
            }
        }
        assert_eq!((rate.hz(), seconds), (125, 60));
        boost.sample(now, Some(true));
        boost.sample(now + Duration::from_secs(10), Some(true));
        for _ in 0..2 {
            let request = boost
                .next(&settings, 1, std::slice::from_ref(&reading))
                .unwrap();
            let guard = boost.guard(request.request).unwrap();
            let outcome = polling(&mut journal, &request, Some(&guard), None, &mut rate);
            assert!(boost.complete(&outcome));
        }
        assert_eq!(rate.hz(), 4000);
        drop(journal);

        // A new process reads only durable records. It has no inherited state.
        let journal = Arc::new(Mutex::new(Journal::open(folder.path())));
        let mut boost = crate::boost::Boost::new(now + Duration::from_secs(30));
        let mut saver = crate::battery_saver::BatterySaver::default();
        reading.level = Some(100);
        assert!(!recover_available(
            &journal,
            "razer",
            std::slice::from_ref(&reading),
            100,
            RecoveryContext {
                settings: &settings,
                generation: 2,
                saver: &mut saver,
                boost: &mut boost,
            }
        ));
        assert!(
            saver
                .next(&settings, 2, std::slice::from_ref(&reading), 100, &boost)
                .is_none()
        );
        boost.sample(now + Duration::from_secs(30), Some(false));
        boost.sample(now + Duration::from_secs(45), Some(false));
        let request = boost
            .next(&settings, 2, std::slice::from_ref(&reading))
            .unwrap();
        assert_eq!(
            request.action,
            ControlAction::ApplyIf {
                rate: PollingRate::try_from(125).unwrap(),
                expected: PollingRate::try_from(4000).unwrap(),
            }
        );
        let guard = boost.guard(request.request).unwrap();
        let outcome = polling(
            &mut journal.lock().unwrap(),
            &request,
            Some(&guard),
            None,
            &mut rate,
        );
        assert!(boost.complete(&outcome));
        for _ in 0..2 {
            let action = saver
                .next(&settings, 2, std::slice::from_ref(&reading), 100, &boost)
                .unwrap();
            match action {
                Action::Polling(request) => {
                    let info = saver
                        .restoring(request.request)
                        .zip(saver.recovery_choices(request.request));
                    let outcome = polling(
                        &mut journal.lock().unwrap(),
                        &request,
                        None,
                        info,
                        &mut rate,
                    );
                    assert!(saver.polling_complete(&outcome));
                }
                Action::Sleep(request) => {
                    let outcome =
                        sleep(&mut journal.lock().unwrap(), &request, &saver, &mut seconds);
                    assert!(saver.sleep_complete(&outcome));
                }
            }
        }
        assert_eq!((rate.hz(), seconds), (1000, 300));
        assert_eq!(Journal::open(folder.path()).devices().count(), 0);
    }
    #[test]
    fn permission_revocation_before_settings_acknowledgment_preserves_only_unwritten_restores() {
        fn next(
            boosting: bool,
            boost: &mut crate::boost::Boost,
            saver: &mut crate::battery_saver::BatterySaver,
            settings: &Settings,
            readings: &[Reading],
        ) -> Option<ControlRequest> {
            if boosting {
                boost.next(settings, 1, readings)
            } else {
                saver.next(settings, 1, readings, 100, boost).map(|action| {
                    let crate::battery_saver::Action::Polling(request) = action else {
                        panic!("No sleep policy configured in this fixture");
                    };
                    request
                })
            }
        }
        for boosting in [false, true] {
            for may_have_changed in [false, true] {
                let now = Instant::now();
                let mut boost = crate::boost::Boost::new(now);
                let mut saver = crate::battery_saver::BatterySaver::default();
                let mut settings = Settings {
                    polling_controls: true,
                    ..Default::default()
                };
                let mut readings = vec![control_reading()];
                readings[0].timestamp = 100;
                readings[0].level = Some(19);
                readings[0].charging = Some(false);
                let device = settings.device_mut(&readings[0].key);
                device.fullscreen_boost_rate = PollingRate::try_from(2000).ok();
                device.low_battery_mode = Some(hb_core::settings::LowBatteryMode {
                    threshold: 20,
                    polling_rate: PollingRate::try_from(125).ok(),
                    sleep_seconds: None,
                });
                if boosting {
                    boost.sample(now, Some(true));
                    boost.sample(now + Duration::from_secs(10), Some(true));
                }
                let mut hardware = PollingRate::try_from(1000).unwrap();
                for _ in 0..2 {
                    let request =
                        next(boosting, &mut boost, &mut saver, &settings, &readings).unwrap();
                    let outcome = simulate_control(&request, &mut hardware, 1, 100);
                    assert!(outcome.failure.is_none());
                    assert!(finish_polling_outcome(&mut boost, &mut saver, &outcome));
                }
                readings[0].level = Some(25);
                if boosting {
                    boost.sample(now + Duration::from_secs(30), Some(false));
                    boost.sample(now + Duration::from_secs(45), Some(false));
                }
                let restore = next(boosting, &mut boost, &mut saver, &settings, &readings).unwrap();
                let cancelled = if boosting {
                    boost.guard(restore.request).unwrap().cancel
                } else {
                    saver.cancel(restore.request).unwrap()
                };
                assert!(control_epoch_current(1, 1, &mut boost, &mut saver));
                assert!(!cancelled.load(Ordering::Acquire));
                // UI revocation won the race; the state owner has not applied
                // the settings mailbox yet. Process this old completion first.
                let permitted = control_epoch_current(1, 2, &mut boost, &mut saver);
                assert!(!permitted);
                assert!(cancelled.load(Ordering::Acquire));
                let mut outcome = ControlOutcome::failed(&restore, "Permission revoked before SET");
                outcome.may_have_changed = may_have_changed;
                validate_polling_completion(&mut outcome, 1, permitted);
                assert!(finish_polling_outcome(&mut boost, &mut saver, &outcome));
                let retry = next(boosting, &mut boost, &mut saver, &settings, &readings);
                assert_eq!(retry.is_some(), !may_have_changed);
                if let Some(retry) = retry {
                    assert_eq!(
                        retry.target.connection_stamp,
                        restore.target.connection_stamp
                    );
                    assert_eq!(retry.action, restore.action);
                }
            }
        }
    }
    #[test]
    fn cancelled_automatic_replies_cannot_replace_manual_polling_or_sleep_status() {
        use hb_core::settings::LowBatteryMode;
        for sleep in [false, true] {
            for pause in [false, true] {
                let mut reading = Reading::new("razer:00bf:TEST", "Test mouse", "razer", 100);
                reading.kind = "mouse".into();
                reading.via = "usb".into();
                reading.level = Some(20);
                reading.charging = Some(false);
                let mut settings = Settings {
                    polling_controls: true,
                    ..Default::default()
                };
                settings
                    .devices
                    .entry(reading.key.clone())
                    .or_default()
                    .low_battery_mode = Some(LowBatteryMode {
                    threshold: 20,
                    polling_rate: (!sleep).then(|| PollingRate::try_from(125).unwrap()),
                    sleep_seconds: sleep.then_some(60),
                });
                let mut boost = crate::boost::Boost::new(Instant::now());
                let mut saver = crate::battery_saver::BatterySaver::default();
                let action = saver
                    .next(&settings, 1, std::slice::from_ref(&reading), 100, &boost)
                    .unwrap();
                let guard = saver.cancel(action.id()).unwrap();
                if pause {
                    saver.pause();
                } else {
                    saver.manual(&ConfigurationDevice::from_reading(&reading), sleep);
                }
                assert!(guard.load(Ordering::Acquire));
                match action {
                    crate::battery_saver::Action::Sleep(q) => {
                        let mut reply = SleepOutcome::failed(&q, "cancelled before SET");
                        assert!(!finish_sleep_outcome(&mut saver, &reply));
                        reply.request = 9;
                        reply.seconds = Some(300);
                        reply.failure = None;
                        assert!(finish_sleep_outcome(&mut saver, &reply));
                    }
                    crate::battery_saver::Action::Polling(q) => {
                        let mut reply = ControlOutcome::failed(&q, "cancelled before SET");
                        assert!(!finish_polling_outcome(&mut boost, &mut saver, &reply));
                        reply.request = 9;
                        assert!(finish_polling_outcome(&mut boost, &mut saver, &reply));
                    }
                }
            }
        }
    }
    #[test]
    fn cancelled_boost_reply_is_not_published_after_manual_override() {
        let now = Instant::now();
        let mut boost = crate::boost::Boost::new(now);
        let mut saver = crate::battery_saver::BatterySaver::default();
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        let mut reading = Reading::new("simulated:mouse", "Test mouse", "simulation", 100);
        reading.kind = "mouse".into();
        settings
            .devices
            .entry(reading.key.clone())
            .or_default()
            .fullscreen_boost_rate = PollingRate::try_from(2000).ok();
        boost.sample(now, Some(true));
        boost.sample(now + Duration::from_secs(10), Some(true));
        let request = boost.next(&settings, 1, &[reading]).unwrap();
        boost.manual(&request.target.device.key);
        let reply = ControlOutcome::failed(&request, "cancelled before SET");
        assert!(!finish_polling_outcome(&mut boost, &mut saver, &reply));
    }
    #[test]
    fn panic_after_polling_write_does_not_claim_the_hardware_is_unchanged() {
        let mut hardware_rate = 1000;
        let request = control_request(
            ControlAction::Apply(PollingRate::try_from(4000).unwrap()),
            0,
        );
        let outcome = control_transaction(&request, || {
            hardware_rate = 4000;
            panic!("synthetic failure after device accepted write");
        });
        assert_eq!(hardware_rate, 4000);
        assert!(outcome.may_have_changed);
        assert!(outcome.failure.is_some());
        assert!(outcome.observation.is_none());
        assert!(!outcome.confirmed_change());
        let read = control_request(ControlAction::Read, 0);
        let outcome = control_transaction(&read, || panic!("synthetic read failure"));
        assert!(!outcome.may_have_changed);
        assert!(outcome.observation.is_none());
    }
    #[test]
    fn blocked_worker_queues_do_not_create_twenty_millisecond_wakes() {
        let settings = Settings::default();
        let now = Instant::now();
        let due = [
            ("logitech", now),
            ("bluetooth", now + Duration::from_secs(3)),
        ];
        assert_eq!(
            provider_queue_wait(&settings, due.into_iter(), now, false, true, false),
            Duration::from_secs(3)
        );
        assert_eq!(
            provider_queue_wait(&settings, due.into_iter(), now, false, true, true),
            Duration::from_secs(5)
        );
        assert_eq!(
            provider_queue_wait(&settings, due.into_iter(), now, false, false, true),
            Duration::ZERO
        );
        assert_eq!(
            provider_queue_wait(&settings, due.into_iter(), now, true, false, false),
            Duration::from_secs(5)
        );
    }
    #[test]
    fn native_connection_events_refresh_hid_providers_without_defeating_quiet_cadence() {
        let settings = Settings::default();
        let now = Instant::now();
        let mut due = BTreeMap::new();
        let mut invalidated = BTreeSet::new();
        schedule_connection_refresh(
            &mut due,
            &mut invalidated,
            ["razer", "logitech", "bluetooth", "xinput"].into_iter(),
            now,
            &settings,
            false,
        );
        for id in ["razer", "logitech", "bluetooth", "xinput"] {
            assert_eq!(due[id], now + Duration::from_secs(2));
            assert!(invalidated.contains(id));
        }
        due.clear();
        due.insert("razer", now + Duration::from_secs(120));
        schedule_connection_refresh(
            &mut due,
            &mut invalidated,
            ["razer", "logitech"].into_iter(),
            now,
            &settings,
            true,
        );
        assert_eq!(due["razer"], now + Duration::from_secs(120));
        assert_eq!(due["logitech"], now + Duration::from_secs(300));
    }
    #[test]
    fn failed_collection_preserves_sibling_and_unrelated_provider_results_with_bounded_retry() {
        struct Provider(&'static str);
        impl BatteryProvider for Provider {
            fn id(&self) -> &'static str {
                self.0
            }
            fn diagnostics(&self) -> Vec<String> {
                vec![]
            }
            fn poll(&mut self, _: &dyn HidTransport, _: &PollContext<'_>) -> PollResult {
                unreachable!()
            }
        }
        let settings = Settings::default();
        let now = Instant::now();
        let mut due = BTreeMap::new();
        let mut engine = Engine::new(settings.clone(), Estimator::default());
        // A provider can successfully read one collection while another open
        // fails. Access failure expires metadata but leaves connection epoch 7.
        for id in ["logitech", "mchose"] {
            let mut reading = Reading::new(format!("{id}:healthy"), "Healthy sibling", id, 100);
            reading.level = Some(75);
            let completed = Completed {
                provider: Box::new(Provider(id)),
                result: Ok(vec![reading]),
                epoch: 0,
                generation: 7,
            };
            assert!(battery_completion_current(
                &completed, &settings, 0, 7, false
            ));
            engine.apply(id, completed.result, 100.0, false);
            schedule_completed_provider(&mut due, id, now, Duration::from_secs(60));
            assert!(!poll_is_due(
                &settings,
                id,
                due.get(id).copied(),
                now + Duration::from_secs(1),
                false
            ));
        }
        assert_eq!(engine.readings().len(), 2);
        // A real connection change still rejects every older in-flight result.
        let old = Completed {
            provider: Box::new(Provider("logitech")),
            result: Err(ProviderError::new("access denied")),
            epoch: 0,
            generation: 7,
        };
        assert!(!battery_completion_current(&old, &settings, 0, 8, false));
        assert!(!battery_completion_current(&old, &settings, 1, 7, false));
        assert!(!battery_completion_current(&old, &settings, 0, 7, true));
        due.clear();
        schedule_completed_provider(&mut due, "logitech", now, Duration::from_secs(60));
        assert_eq!(due["logitech"], now + Duration::from_secs(60));
        // An explicitly scheduled reconnect refresh remains authoritative.
        due.insert("logitech", now + Duration::from_secs(2));
        schedule_completed_provider(&mut due, "logitech", now, Duration::from_secs(60));
        assert_eq!(due["logitech"], now + Duration::from_secs(2));
    }
    #[test]
    fn completion_history_preserves_online_observation_and_records_event_boundaries() {
        let mut old = Reading::new("synthetic", "Synthetic", "razer", 100);
        old.level = Some(80);
        old.charging = Some(false);
        let failure = Err(ProviderError::new("synthetic failure"));
        let boundary = completed_observations(&[old.clone()], &failure, 120, true);
        assert_eq!(boundary.len(), 1);
        assert_eq!(boundary[0].timestamp, 120);
        assert_eq!(boundary[0].connection, Connection::Stale);
        assert_eq!(old.timestamp, 100);
        assert!(old.online());
        assert!(completed_observations(&boundary, &failure, 140, true).is_empty());

        let directory = tempfile::tempdir().unwrap();
        let mut store = hb_storage::Store::open(&directory.path().join("history.db")).unwrap();
        store.record(&old).unwrap();
        store.record(&boundary[0]).unwrap();
        store.flush().unwrap();
        let rows = store.query("synthetic", 0, 200, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].online());
        assert_eq!(rows[0].timestamp, 100);
        assert_eq!(rows[1].timestamp, 120);
        assert_eq!(
            store.query_usage("synthetic", 200, 3600, 10).unwrap().until,
            0
        );

        let collision = availability_boundary(old.clone(), Connection::Sleeping, 100);
        assert_eq!(collision.timestamp, 100);
        assert_eq!(collision.connection, Connection::Sleeping);
        let backwards = availability_boundary(old, Connection::Stale, 50);
        assert_eq!(backwards.timestamp, 50);
    }
    #[test]
    fn provider_completion_stores_fresh_rows_and_missing_devices_only() {
        let old = Reading::new("missing", "Missing", "razer", 100);
        let fresh = Reading::new("fresh", "Fresh", "razer", 120);
        let result = Ok(vec![fresh.clone()]);
        assert!(completed_observations(std::slice::from_ref(&old), &result, 125, false).is_empty());
        let rows = completed_observations(&[old], &result, 125, true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], fresh);
        assert_eq!(rows[1].timestamp, 125);
        assert_eq!(rows[1].connection, Connection::Stale);
        assert_eq!(rows[1].key, "missing");
    }
    #[test]
    fn confirming_one_device_preserves_other_device_rate_and_session() {
        let mut tracker = UsageTracker::new(2, true);
        let request = control_request(ControlAction::Read, 2);
        let mut reading = control_reading();
        reading.timestamp = 30;
        let mut rate = PollingRate::try_from(1000).unwrap();
        tracker.observe(&simulate_control(&request, &mut rate, 2, 20));
        let original = tracker.samples(vec![reading.clone()]).remove(0);
        let mut other_request = request.clone();
        other_request.target.device.key = "synthetic-other".into();
        let mut other = reading.clone();
        other.key = "synthetic-other".into();
        tracker.observe(&simulate_control(&other_request, &mut rate, 2, 25));
        tracker.samples(vec![other]);
        reading.timestamp = 40;
        let subsequent = tracker.samples(vec![reading]).remove(0);
        assert_eq!(subsequent.session, original.session);
        assert_eq!(subsequent.polling_rate, original.polling_rate);
    }
    #[test]
    fn unavailable_boundary_session_survives_same_second_replacement_without_rate_confirmation() {
        let mut tracker = UsageTracker::new(0, false);
        let mut old = Reading::new("synthetic", "Synthetic", "razer", 100);
        old.charging = Some(false);
        old.level = Some(90);
        let first = tracker.samples(vec![old.clone()]).remove(0);
        let boundary = availability_boundary(old.clone(), Connection::Sleeping, 100);
        let pause = tracker.samples(vec![boundary.clone()]).remove(0);
        assert_ne!(pause.session, first.session);
        let repeated = tracker.samples(vec![boundary]).remove(0);
        assert_eq!(pause.session, repeated.session);
        let mut fresh = old;
        fresh.timestamp = 100;
        fresh.level = Some(89);
        let resumed = tracker.samples(vec![fresh]).remove(0);
        assert_eq!(resumed.session, pause.session);
        let mut insights = InsightsBuilder::default();
        insights.push(first.clone());
        insights.push(pause.clone());
        insights.push(resumed.clone());
        assert_eq!(insights.finish().cycles[0].awake_seconds, 0);
        let directory = tempfile::tempdir().unwrap();
        let mut store = hb_storage::Store::open(&directory.path().join("history.db")).unwrap();
        for observation in [&first, &pause, &resumed] {
            store.record_usage(observation).unwrap();
        }
        store.flush().unwrap();
        let rows = store.query("synthetic", 100, 100, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].online());
        assert_eq!(rows[1].connection, Connection::Sleeping);
        assert!(rows[2].online());
    }
    #[test]
    fn delayed_polling_completion_cannot_restore_revoked_rate_evidence() {
        let request = control_request(ControlAction::Read, 2);
        let mut rate = PollingRate::try_from(1000).unwrap();
        let mut outcome = simulate_control(&request, &mut rate, 2, 20);
        // Permission was revoked and enabled again while HID generation stayed 2.
        validate_polling_completion(&mut outcome, 2, false);
        assert!(outcome.observation.is_none());
        assert!(outcome.failure.is_some());
        let mut tracker = UsageTracker::new(2, true);
        tracker.observe(&outcome);
        assert!(tracker.rates.is_empty());

        let mut uncertain = simulate_control(&request, &mut rate, 2, 20);
        uncertain.may_have_changed = true;
        validate_polling_completion(&mut uncertain, 2, false);
        assert!(changed_configuration(&uncertain, &mut BTreeMap::new()));
        let mut disconnected = simulate_control(&request, &mut rate, 2, 20);
        validate_polling_completion(&mut disconnected, 3, true);
        assert!(disconnected.observation.is_none());
    }
    #[test]
    fn unchanged_snapshots_use_lightweight_ticks_and_retry_rejected_state_at_normal_cadence() {
        let (tx, events) = bounded(1);
        let sink = Events {
            tx,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let (storage, statuses) = bounded(1);
        let mut engine = Engine::new(Settings::default(), Estimator::default());
        let mut reading = Reading::new("device", "Device", "test", 100);
        reading.level = Some(80);
        engine.apply("test", Ok(vec![reading]), 1.0, false);
        let start = Instant::now();
        let mut publisher = SnapshotPublisher::new(start);
        sink.try_send(Event::Heartbeat(99)).unwrap();
        publisher.publish(&engine, 100, start, &sink, &storage);
        assert!(publisher.unpublished);
        assert_eq!(publisher.delay(start), Duration::from_secs(5));
        assert!(matches!(events.try_recv(), Ok(Event::Heartbeat(99))));
        publisher.publish(
            &engine,
            101,
            start + Duration::from_secs(1),
            &sink,
            &storage,
        );
        assert!(events.is_empty());
        publisher.publish(
            &engine,
            105,
            start + Duration::from_secs(5),
            &sink,
            &storage,
        );
        let Event::Snapshot(snapshot) = events.try_recv().unwrap() else {
            panic!("state was lost");
        };
        assert_eq!(snapshot.devices[0].reading.level, Some(80));
        assert!(!publisher.unpublished);
        // Wall-clock correction still updates the UI; only monotonic time schedules work.
        publisher.publish(
            &engine,
            90,
            start + Duration::from_secs(10),
            &sink,
            &storage,
        );
        assert!(matches!(events.try_recv(), Ok(Event::Heartbeat(90))));
        assert!(statuses.is_empty());
        publisher.changed = true;
        publisher.publish(
            &engine,
            91,
            start + Duration::from_secs(11),
            &sink,
            &storage,
        );
        assert!(matches!(events.try_recv(), Ok(Event::Snapshot(_))));
    }
    #[test]
    fn snapshots_skip_empty_provider_bursts_but_keep_updates_and_freshness_heartbeat() {
        let (tx, events) = bounded(8);
        let sink = Events {
            tx,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let (storage, statuses) = bounded(8);
        let mut engine = Engine::new(
            Settings {
                status_file: true,
                ..Default::default()
            },
            Estimator::default(),
        );
        let start = Instant::now();
        let mut publisher = SnapshotPublisher::new(start);
        publisher.publish(&engine, 100, start, &sink, &storage);
        assert!(matches!(events.try_recv(), Ok(Event::Snapshot(_))));
        assert!(matches!(statuses.try_recv(), Ok(Storage::Status(_, true))));
        // A full discovery burst from absent devices must not cause repeated
        // UI wakeups, JSON exports or atomic file replacements.
        for index in 0..25 {
            let id = format!("absent-{index}");
            let result = Ok(Vec::new());
            publisher.changed |= battery_affects_snapshot(&engine, &id, &result);
            engine.apply(&id, result, 0.0, false);
            publisher.publish(
                &engine,
                101,
                start + Duration::from_secs(1),
                &sink,
                &storage,
            );
        }
        assert!(events.is_empty());
        assert!(statuses.is_empty());
        assert_eq!(
            publisher.delay(start + Duration::from_secs(4)),
            Duration::from_secs(1)
        );
        // Heartbeats use monotonic time, including after a wall-clock correction.
        publisher.publish(&engine, 90, start + Duration::from_secs(5), &sink, &storage);
        let Event::Snapshot(snapshot) = events.try_recv().unwrap() else {
            panic!("missing heartbeat")
        };
        assert_eq!(snapshot.timestamp, 90);
        assert!(matches!(statuses.try_recv(), Ok(Storage::Status(_, true))));
        let mut reading = Reading::new("mouse", "Mouse", "razer", 91);
        reading.level = Some(70);
        let result = Ok(vec![reading]);
        publisher.changed |= battery_affects_snapshot(&engine, "razer", &result);
        engine.apply("razer", result, 6.0, false);
        publisher.publish(&engine, 91, start + Duration::from_secs(6), &sink, &storage);
        let Event::Snapshot(snapshot) = events.try_recv().unwrap() else {
            panic!("missing new reading")
        };
        assert_eq!(snapshot.devices[0].reading.level, Some(70));
        assert!(matches!(statuses.try_recv(), Ok(Storage::Status(_, true))));
        for (seconds, expected_devices) in [(7, 1), (8, 0)] {
            let result = Ok(Vec::new());
            publisher.changed |= battery_affects_snapshot(&engine, "razer", &result);
            engine.apply("razer", result, seconds as f64, false);
            publisher.publish(
                &engine,
                92,
                start + Duration::from_secs(seconds),
                &sink,
                &storage,
            );
            let Event::Snapshot(snapshot) = events.try_recv().unwrap() else {
                panic!("missing disconnect")
            };
            assert_eq!(snapshot.devices.len(), expected_devices);
            if expected_devices > 0 {
                assert_eq!(snapshot.devices[0].reading.connection, Connection::Stale);
            }
            assert!(matches!(statuses.try_recv(), Ok(Storage::Status(_, true))));
        }
        // Failures and recovery from an error remain immediately visible even
        // for a provider that has never produced a battery device.
        for (seconds, result, errors) in [
            (9, Err(ProviderError::new("inaccessible")), 1),
            (10, Ok(Vec::new()), 0),
        ] {
            publisher.changed |= battery_affects_snapshot(&engine, "corsair", &result);
            engine.apply("corsair", result, seconds as f64, false);
            publisher.publish(
                &engine,
                93,
                start + Duration::from_secs(seconds),
                &sink,
                &storage,
            );
            let Event::Snapshot(snapshot) = events.try_recv().unwrap() else {
                panic!("missing failure/recovery")
            };
            assert_eq!(snapshot.errors.len(), errors);
            assert!(matches!(statuses.try_recv(), Ok(Storage::Status(_, true))));
        }
        engine.settings.status_file = false;
        publisher.changed = true;
        publisher.publish(
            &engine,
            94,
            start + Duration::from_secs(11),
            &sink,
            &storage,
        );
        assert!(matches!(events.try_recv(), Ok(Event::Snapshot(_))));
        assert!(statuses.is_empty());
    }

    #[test]
    fn identical_cached_readings_and_repeated_errors_do_not_republish_snapshots() {
        let mut engine = Engine::new(Settings::default(), Estimator::default());
        let reading = Reading::new("bt:one", "Headset", "bluetooth", 100);
        engine.apply("bluetooth", Ok(vec![reading.clone()]), 1.0, false);
        assert!(!battery_affects_snapshot(
            &engine,
            "bluetooth",
            &Ok(vec![reading.clone()])
        ));
        let mut fresh = reading.clone();
        fresh.timestamp += 1;
        assert!(battery_affects_snapshot(
            &engine,
            "bluetooth",
            &Ok(vec![fresh])
        ));
        let error = ProviderError::new("Unavailable");
        assert!(battery_affects_snapshot(
            &engine,
            "bluetooth",
            &Err(error.clone())
        ));
        engine.apply("bluetooth", Err(error.clone()), 2.0, false);
        assert!(!battery_affects_snapshot(&engine, "bluetooth", &Err(error)));
        assert!(battery_affects_snapshot(
            &engine,
            "bluetooth",
            &Ok(vec![reading])
        ));
    }
    #[derive(Default)]
    struct PassiveHid {
        enumerations: AtomicU64,
        generation: AtomicU64,
        close_after_enumeration: Option<Arc<ConfigurationWatch>>,
    }
    impl HidTransport for PassiveHid {
        fn generation(&self) -> u64 {
            self.generation.load(Ordering::Relaxed)
        }
        fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
            self.enumerations.fetch_add(1, Ordering::Relaxed);
            if let Some(watch) = &self.close_after_enumeration {
                watch.set_visible(false);
            }
            Ok(if vendor == 0x1532 {
                vec![HidInfo {
                    vendor_id: vendor,
                    product_id: 0x026c,
                    interface: 3,
                    feature_length: Some(91),
                    path: "hid#test-keyboard".into(),
                    serial: "TEST-KEYBOARD".into(),
                    container: Some("test-container".into()),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            })
        }
        fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            panic!("passive discovery cannot open")
        }
    }
    #[test]
    fn configuration_discovery_stops_when_closed_and_rejects_old_visibility_and_connection_epochs()
    {
        let hid = PassiveHid::default();
        let watch = ConfigurationWatch::default();
        let clock = SystemClock::default();
        let cancel = AtomicBool::new(false);
        assert!(discover_configuration(&hid, &clock, &cancel, &watch, 0, 0).is_err());
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 0);
        watch.set_visible(true);
        let epoch = watch.epoch.load(Ordering::Acquire);
        assert_eq!(
            discover_configuration(&hid, &clock, &cancel, &watch, epoch, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 5);
        watch.set_visible(false);
        watch.set_visible(true);
        assert!(discover_configuration(&hid, &clock, &cancel, &watch, epoch, 0).is_err());
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 5);
        hid.generation.store(1, Ordering::Relaxed);
        assert!(
            discover_configuration(
                &hid,
                &clock,
                &cancel,
                &watch,
                watch.epoch.load(Ordering::Acquire),
                0
            )
            .is_err()
        );
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 5);
        let watch = Arc::new(ConfigurationWatch::default());
        watch.set_visible(true);
        let hid = PassiveHid {
            close_after_enumeration: Some(watch.clone()),
            ..Default::default()
        };
        assert!(discover_configuration(&hid, &clock, &cancel, &watch, 1, 0).is_err());
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn keyboard_requests_do_not_access_hid_after_dashboard_closes_or_reopens() {
        let hid = PassiveHid::default();
        let watch = Arc::new(ConfigurationWatch::default());
        watch.set_visible(true);
        let epoch = watch.epoch.load(Ordering::Acquire);
        watch.set_visible(false);
        watch.set_visible(true);
        let clock = SystemClock::default();
        let cancel = Arc::new(AtomicBool::new(false));
        for action in [
            ControlAction::Read,
            ControlAction::Apply(PollingRate::try_from(250).unwrap()),
        ] {
            let request = ControlRequest {
                request: 1,
                target: ControlTarget {
                    device: simulated_keyboards().remove(0),
                    generation: 0,
                    connection_stamp: None,
                },
                action,
            };
            let outcome = execute_control_inner(
                &request,
                &hid,
                &clock,
                &cancel,
                &cancel,
                false,
                ControlGuards {
                    boost: None,
                    permission: None,
                    configuration: Some((watch.clone(), epoch)),
                },
            );
            assert!(outcome.failure.unwrap().contains("no longer visible"));
            assert!(!outcome.may_have_changed);
        }
        assert_eq!(hid.enumerations.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn queued_keyboard_commands_keep_submission_epoch_even_when_close_notifications_cannot_enqueue()
    {
        for action in [
            ControlAction::Read,
            ControlAction::Apply(PollingRate::try_from(250).unwrap()),
        ] {
            let (commands, rx) = bounded(1);
            let (tx, events) = bounded(4);
            let configuration = Arc::new(ConfigurationWatch::default());
            configuration.set_visible(true);
            let runtime = Runtime {
                commands,
                events,
                thread: None,
                permission: Arc::new(ControlPermission::new(true)),
                sink: Events {
                    tx,
                    window: Arc::new(AtomicUsize::new(0)),
                },
                cancel: Arc::new(AtomicBool::new(false)),
                window: Arc::new(AtomicUsize::new(0)),
                configuration,
            };
            // Native resize/show notifications can repeat the same visibility.
            // They must not fill the command queue or wake the engine each time.
            for _ in 0..1000 {
                runtime.send(Command::ConfigurationVisible(true));
            }
            assert!(rx.is_empty());
            assert!(runtime.events.is_empty());
            assert_eq!(runtime.configuration.epoch.load(Ordering::Acquire), 1);
            runtime
                .submit_control(ControlRequest {
                    request: 1,
                    target: ControlTarget {
                        device: simulated_keyboards().remove(0),
                        generation: 0,
                        connection_stamp: None,
                    },
                    action,
                })
                .unwrap();
            runtime.send(Command::ConfigurationVisible(false));
            runtime.send(Command::ConfigurationVisible(true));
            let Command::Polling(_, _, Some(submitted_epoch)) = rx.recv().unwrap() else {
                panic!("keyboard command missing submission epoch")
            };
            assert_eq!(submitted_epoch, 1);
            assert!(!runtime.configuration.active(submitted_epoch));
            assert!(runtime.configuration.visible.load(Ordering::Acquire));
        }
    }
    #[test]
    fn batteryless_configuration_never_creates_battery_samples_and_has_independent_rates() {
        let mut devices = simulated_keyboards();
        let keyboard = devices.remove(0);
        let unavailable = devices.remove(0);
        let mut rate = PollingRate::try_from(1000).unwrap();
        let request = ControlRequest {
            request: 1,
            target: ControlTarget {
                device: keyboard.clone(),
                generation: 0,
                connection_stamp: None,
            },
            action: ControlAction::Apply(PollingRate::try_from(250).unwrap()),
        };
        let outcome = simulate_control(&request, &mut rate, 0, 10);
        assert_eq!(rate.hz(), 250);
        let mut tracker = UsageTracker::new(0, true);
        tracker.observe(&outcome);
        assert!(tracker.samples(Vec::new()).is_empty());
        assert!(tracker.rates.is_empty());
        assert!(tracker.device_sessions.is_empty());
        let mut changed = keyboard.clone();
        changed.container = Some("different".into());
        assert!(!configuration_matches(&keyboard, &changed));
        changed = keyboard.clone();
        changed.capability = PollingCapability::Unavailable("refused".into());
        assert!(!configuration_matches(&keyboard, &changed));
        let mut engine = Engine::new(
            Settings {
                disabled_providers: BTreeSet::from(["razer".into()]),
                ..Default::default()
            },
            Estimator::default(),
        );
        assert!(engine.readings().is_empty());
        assert!(configuration_matches(&keyboard, &keyboard));
        assert!(engine.snapshot(10).devices.is_empty());
        assert!(engine.flush_held().is_empty());
        let request = ControlRequest {
            target: ControlTarget {
                device: unavailable,
                generation: 0,
                connection_stamp: None,
            },
            ..request
        };
        assert!(
            simulate_control(&request, &mut rate, 0, 11)
                .failure
                .is_some()
        );
        assert_eq!(rate.hz(), 250);
    }
    #[test]
    fn unchanged_empty_diagnostics_do_not_allocate_provider_keys() {
        let mut diagnostics = BTreeMap::new();
        update_diagnostics(&mut diagnostics, "razer", Vec::new());
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!update_diagnostics(&mut diagnostics, "razer", Vec::new()));
            }
        });
        println!("1,000 unchanged diagnostic updates: {allocations:?}");
        assert_eq!(allocations.calls, 0);
        assert_eq!(allocations.bytes, 0);
    }
    #[test]
    fn obsolete_delivery_failure_cannot_replace_current_retry_or_delay_it() {
        let mut engine = Engine::new(Settings::default(), Estimator::default());
        let mut reading = Reading::new("synthetic:mouse", "Test mouse", "razer", 0);
        reading.level = Some(10);
        reading.charging = Some(false);
        let old = engine
            .apply("razer", Ok(vec![reading.clone()]), 0.0, false)
            .pop()
            .unwrap();
        reading.timestamp = 60;
        reading.level = Some(80);
        engine.apply("razer", Ok(vec![reading.clone()]), 60.0, false);
        reading.timestamp = 120;
        reading.level = Some(10);
        let current = engine
            .apply("razer", Ok(vec![reading]), 120.0, false)
            .pop()
            .unwrap();
        let now = Instant::now();
        let mut pending = BTreeMap::new();
        remember_failed_notification(&engine, &mut pending, current, now);
        remember_failed_notification(&engine, &mut pending, old, now + Duration::from_secs(20));
        assert_eq!(pending.len(), 1);
        let (due, notification) = pending.into_values().next().unwrap();
        assert_eq!(due, now + Duration::from_secs(30));
        engine.notification_failed(notification);
        assert_eq!(engine.flush_held().len(), 1);
    }
    #[test]
    fn diagnostics_publish_only_changes_and_retry_after_backpressure() {
        let (tx, rx) = bounded(1);
        let events = Events {
            tx,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let mut diagnostics = BTreeMap::new();
        let mut dirty = update_diagnostics(&mut diagnostics, "test", vec!["ready".into()]);
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert!(!dirty);
        assert!(!update_diagnostics(
            &mut diagnostics,
            "test",
            vec!["ready".into()]
        ));
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert_eq!(rx.len(), 1);
        dirty |= update_diagnostics(&mut diagnostics, "test", vec!["changed".into()]);
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert!(dirty);
        let Event::Diagnostics(first) = rx.recv().unwrap() else {
            panic!("diagnostics expected")
        };
        assert_eq!(first["test"], ["ready"]);
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert!(!dirty);
        let Event::Diagnostics(second) = rx.recv().unwrap() else {
            panic!("diagnostics expected")
        };
        assert_eq!(second["test"], ["changed"]);
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert!(rx.is_empty());
    }
    use super::*;
    #[test]
    fn polling_activity_requires_success_and_preserves_automatic_reason() {
        let mut tracker = UsageTracker::new(2, true);
        let mut rate = PollingRate::try_from(1000).unwrap();
        let mut request = control_request(
            ControlAction::ApplyIf {
                rate: PollingRate::try_from(2000).unwrap(),
                expected: rate,
            },
            2,
        );
        request.request |= STARTUP_POLLING_REQUEST_BIT | crate::boost::REQUEST_BIT;
        tracker.begin_control(&request, Some(true));
        let outcome = simulate_control(&request, &mut rate, 2, 20);
        tracker.observe(&outcome);
        assert_eq!(
            tracker.take_activity(&outcome).unwrap().kind,
            PollingActivityKind::Boost
        );
        assert!(tracker.take_activity(&outcome).is_none());
        request.request = 1;
        tracker.begin_control(&request, None);
        assert!(
            tracker
                .take_activity(&ControlOutcome::failed(&request, "synthetic failure"))
                .is_none()
        );
        let mut startup = request.clone();
        startup.request |= STARTUP_POLLING_REQUEST_BIT;
        tracker.begin_control(&startup, None);
        assert!(tracker.actions.is_empty());
        startup.request |= crate::battery_saver::REQUEST_BIT;
        tracker.begin_control(&startup, None);
        assert!(
            tracker.actions.is_empty(),
            "battery saver must not count as a manual change"
        );
    }
    #[test]
    fn same_rate_confirmation_retains_manual_ownership_evidence() {
        let mut tracker = UsageTracker::new(2, true);
        let mut rate = PollingRate::try_from(1000).unwrap();
        let request = control_request(ControlAction::Apply(rate), 2);
        tracker.begin_control(&request, None);
        let outcome = simulate_control(&request, &mut rate, 2, 20);
        tracker.observe(&outcome);
        assert!(!outcome.may_have_changed);
        // Choosing the already boosted rate still takes manual ownership, so
        // retain this event. Storage excludes it from actual change counts.
        let event = tracker.take_activity(&outcome).unwrap();
        assert_eq!(event.kind, PollingActivityKind::Manual);
        assert_eq!(event.previous, Some(event.rate));
        assert!(tracker.actions.is_empty());
        assert_eq!(tracker.rates[&outcome.key].rate, Some(rate));

        let request = control_request(ControlAction::Apply(2000.try_into().unwrap()), 2);
        tracker.begin_control(&request, None);
        let outcome = simulate_control(&request, &mut rate, 2, 21);
        tracker.observe(&outcome);
        assert_eq!(
            tracker.take_activity(&outcome).unwrap().kind,
            PollingActivityKind::Manual
        );
    }
    #[test]
    fn usage_learning_requires_fresh_readback_and_matching_connection() {
        let mut tracker = UsageTracker::new(2, true);
        let request = control_request(ControlAction::Read, 2);
        let mut reading = control_reading();
        reading.level = Some(80);
        reading.charging = Some(false);
        reading.timestamp = 19;
        assert!(
            tracker.samples(vec![reading.clone()])[0]
                .polling_rate
                .is_none()
        );
        let mut rate = PollingRate::try_from(1000).unwrap();
        let outcome = simulate_control(&request, &mut rate, 2, 20);
        tracker.observe(&outcome);
        // Never attach a new confirmation to an older cached observation.
        assert!(
            tracker.samples(vec![reading.clone()])[0]
                .polling_rate
                .is_none()
        );
        reading.timestamp = 21;
        assert_eq!(
            tracker.samples(vec![reading.clone()])[0].polling_rate,
            Some(rate)
        );
        let previous_session = tracker.samples(vec![reading.clone()])[0].session;
        tracker.synchronize(3, true);
        let disconnected = tracker.samples(vec![reading.clone()]);
        assert!(disconnected[0].polling_rate.is_none());
        assert_ne!(disconnected[0].session, previous_session);
        tracker.observe(&outcome);
        assert!(
            tracker.samples(vec![reading.clone()])[0]
                .polling_rate
                .is_none()
        );
        tracker.synchronize(2, true);
        tracker.observe(&outcome);
        reading.via = "other transport".into();
        assert!(tracker.samples(vec![reading])[0].polling_rate.is_none());
    }
    #[test]
    fn changed_identity_revokes_rate_evidence_until_a_new_readback() {
        for field in 0..5 {
            let mut tracker = UsageTracker::new(2, true);
            let request = control_request(ControlAction::Read, 2);
            let mut rate = PollingRate::try_from(1000).unwrap();
            let outcome = simulate_control(&request, &mut rate, 2, 20);
            tracker.observe(&outcome);
            let mut reading = control_reading();
            reading.timestamp = 21;
            let original_session = tracker.samples(vec![reading.clone()])[0].session;
            let mut changed = reading.clone();
            match field {
                0 => changed.kind = "headset".into(),
                1 => changed.source = "other-provider".into(),
                2 => changed.via = "other-connection".into(),
                3 => changed.serial = Some("different-unit".into()),
                _ => changed.container = Some("different-container".into()),
            }
            let sample = tracker.samples(vec![changed]).remove(0);
            assert!(sample.polling_rate.is_none(), "identity field {field}");
            assert_ne!(sample.session, original_session, "identity field {field}");
            assert!(!tracker.rates.contains_key(&reading.key));
            assert!(
                tracker.samples(vec![reading.clone()])[0]
                    .polling_rate
                    .is_none(),
                "returning metadata must not revive old evidence: {field}"
            );
            tracker.observe(&outcome);
            assert_eq!(tracker.samples(vec![reading])[0].polling_rate, Some(rate));
        }
    }
    #[test]
    fn automatic_boost_restore_history_keeps_rates_and_transition_boundaries_separate() {
        let mut tracker = UsageTracker::new(2, true);
        let mut builder = InsightsBuilder::default();
        let mut current = PollingRate::try_from(1000).unwrap();
        let mut reading = control_reading();
        reading.charging = Some(false);
        let mut level = 90;
        for (segment, hz) in [1000, 2000, 1000].into_iter().enumerate() {
            let action = if segment == 0 {
                ControlAction::Read
            } else {
                ControlAction::ApplyIf {
                    rate: PollingRate::try_from(hz).unwrap(),
                    expected: current,
                }
            };
            let mut request = control_request(action, 2);
            request.request = crate::boost::REQUEST_BIT | segment as u64;
            let timestamp = segment as i64 * 4200;
            tracker.observe(&simulate_control(&request, &mut current, 2, timestamp));
            for step in 0..=6 {
                reading.timestamp = timestamp + step * 600;
                reading.level = Some(level);
                builder.push(tracker.samples(vec![reading.clone()]).remove(0));
                level -= if hz == 2000 { 2 } else { 1 };
            }
        }
        let data = builder.finish();
        assert_eq!(data.rates.len(), 2);
        assert_eq!(data.rates[0].awake_seconds, 7200);
        assert_eq!(data.rates[1].awake_seconds, 3600);
        assert_eq!(data.rates[0].consumed_percent, 12);
        assert_eq!(data.rates[1].consumed_percent, 12);
        assert_eq!(data.cycles.len(), 1);
        assert!((data.mixed_full_charge_hours().unwrap() - 12.5).abs() < 0.001);
    }
    #[test]
    fn boost_and_restore_between_battery_samples_cannot_count_as_normal_rate_drain() {
        let mut tracker = UsageTracker::new(2, true);
        let mut rate = PollingRate::try_from(1000).unwrap();
        tracker.observe(&simulate_control(
            &control_request(ControlAction::Read, 2),
            &mut rate,
            2,
            0,
        ));
        let mut reading = control_reading();
        reading.timestamp = 0;
        reading.level = Some(80);
        reading.charging = Some(false);
        let first = tracker.samples(vec![reading.clone()]).remove(0);
        for (timestamp, hz) in [(10, 2000), (40, 1000)] {
            let mut request = control_request(
                ControlAction::ApplyIf {
                    rate: PollingRate::try_from(hz).unwrap(),
                    expected: rate,
                },
                2,
            );
            request.request |= crate::boost::REQUEST_BIT;
            tracker.observe(&simulate_control(&request, &mut rate, 2, timestamp));
        }
        reading.timestamp = 60;
        reading.level = Some(79);
        let last = tracker.samples(vec![reading]).remove(0);
        assert_eq!(first.polling_rate, last.polling_rate);
        assert_ne!(first.session, last.session);
        let mut builder = InsightsBuilder::default();
        builder.push(first);
        builder.push(last);
        let summary = builder.finish();
        assert!(summary.rates.is_empty());
        assert_eq!(summary.coverage.awake_seconds, 0);
    }
    #[test]
    fn failed_changes_disable_and_backward_clock_revoke_usage_evidence() {
        let request = control_request(ControlAction::Read, 2);
        let mut reading = control_reading();
        reading.timestamp = 30;
        let mut rate = PollingRate::try_from(1000).unwrap();
        let confirmed = simulate_control(&request, &mut rate, 2, 20);
        let mut tracker = UsageTracker::new(2, true);
        tracker.observe(&confirmed);
        let mut failed = confirmed.clone();
        failed.failure = Some("partial write".into());
        tracker.observe(&failed);
        assert!(
            tracker.samples(vec![reading.clone()])[0]
                .polling_rate
                .is_none()
        );
        tracker.observe(&confirmed);
        tracker.synchronize(2, false);
        tracker.observe(&confirmed);
        assert!(
            tracker.samples(vec![reading.clone()])[0]
                .polling_rate
                .is_none()
        );
        tracker.synchronize(2, true);
        tracker.observe(&confirmed);
        tracker.synchronize_clock(30);
        tracker.synchronize_clock(29);
        assert!(tracker.samples(vec![reading])[0].polling_rate.is_none());
    }
    #[test]
    fn wall_clock_rollback_resumes_estimates_and_rejects_future_cached_readings() {
        let mut engine = Engine::new(Settings::default(), Estimator::default());
        let mut tracker = UsageTracker::new(2, true);
        let mut reading = control_reading();
        reading.charging = Some(false);
        for step in 0..=180 {
            reading.timestamp = 1000 + step * 60;
            reading.level = Some(100 - (step / 12) as u8);
            engine.apply(
                "simulation",
                Ok(vec![reading.clone()]),
                step as f64 * 60.0,
                false,
            );
        }
        let key = reading.key.clone();
        let before = engine.estimator.seconds_left(&key, reading.level);
        assert!(before.is_some());
        assert!(!synchronize_battery_clock(&mut engine, &mut tracker, 11800));
        assert!(synchronize_battery_clock(&mut engine, &mut tracker, 5000));
        assert_eq!(engine.estimator.devices[&key].seen, 5000);
        assert_eq!(engine.estimator.seconds_left(&key, reading.level), before);
        let mut cached = reading.clone();
        cached.charging = Some(true);
        let mut late = Ok(vec![cached]);
        assert!(reject_future_battery_readings(&mut late, 5000));
        assert_eq!(late.as_ref().unwrap()[0].connection, Connection::Stale);
        assert!(engine.apply("simulation", late, 10830.0, false).is_empty());
        assert_eq!(engine.estimator.seconds_left(&key, reading.level), before);
        assert_eq!(engine.estimator.devices[&key].seen, 5000);
        reading.timestamp = 5000;
        let mut fresh = Ok(vec![reading.clone()]);
        assert!(!reject_future_battery_readings(&mut fresh, 5000));
        engine.apply("simulation", fresh, 10860.0, false);
        assert_eq!(engine.estimator.devices[&key].usage, 10800.0);
        reading.timestamp = 5060;
        engine.apply("simulation", Ok(vec![reading]), 10920.0, false);
        assert_eq!(engine.estimator.devices[&key].usage, 10860.0);
        assert!(!synchronize_battery_clock(&mut engine, &mut tracker, 5060));
        let mut failed = Err(ProviderError::new("synthetic failure"));
        assert!(!reject_future_battery_readings(&mut failed, 5060));
        assert!(failed.is_err());
    }
    #[test]
    fn insights_worker_flushes_rate_metadata_before_query() {
        let directory = tempfile::tempdir().unwrap();
        let (tx, rx) = bounded(16);
        let (boot_tx, boot_rx) = bounded(1);
        let (event_tx, events) = bounded(16);
        let sink = Events {
            tx: event_tx,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let folder = directory.path().to_path_buf();
        let worker = thread::spawn(move || crate::storage_worker::run(folder, rx, boot_tx, sink));
        let _ = boot_rx.recv().unwrap();
        let rate = PollingRate::try_from(1000).unwrap();
        for i in 0..=4 {
            let mut reading = Reading::new("synthetic", "Test mouse", "test", 100 + i * 600);
            reading.level = Some(80 - i as u8);
            reading.charging = Some(false);
            tx.send(Storage::UsageSample(vec![UsageObservation {
                reading,
                polling_rate: Some(rate),
                session: Some(1),
            }]))
            .unwrap();
        }
        tx.send(Storage::Insights("synthetic".into(), 2500, 12))
            .unwrap();
        let Event::Insights(12, result) = events.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("missing insights")
        };
        let summary = result.unwrap();
        assert_eq!(summary.rates[0].awake_seconds, 2400);
        assert_eq!(summary.rates[0].consumed_percent, 4);
        assert_eq!(summary.rates[0].projection_seconds, 1800);
        assert_eq!(summary.rates[0].projection_consumed_percent, 3);
        assert!(summary.rates[0].projected_full_charge_hours.is_some());
        tx.send(Storage::Quit).unwrap();
        worker.join().unwrap();
        let reopened = hb_storage::Store::read_only(&directory.path().join("history.db")).unwrap();
        assert_eq!(
            reopened.query_insights("synthetic", 2500).unwrap().rates[0].consumed_percent,
            4
        );
    }
    #[test]
    fn connection_events_cover_hid_bluetooth_and_le_case_insensitively() {
        for id in [
            "\\\\?\\hid#vid_1532&pid_00bf",
            "BTHENUM\\DEV_0123456789AB",
            "bthledevice#battery",
            "Bluetooth#battery",
        ] {
            assert!(relevant_device_event(id), "{id}");
        }
        for id in ["", "USB#printer", "SWD#audioendpoint"] {
            assert!(!relevant_device_event(id));
        }
    }
    #[test]
    fn gaming_cadence_never_speeds_up_a_slow_setting() {
        assert_eq!(effective_interval(60, false), Duration::from_secs(60));
        assert_eq!(effective_interval(60, true), Duration::from_secs(300));
        assert_eq!(effective_interval(600, true), Duration::from_secs(600));
        assert_eq!(
            provider_delay(60, true, Some(Duration::from_secs(3))),
            Duration::from_secs(300)
        );
        assert_eq!(
            provider_delay(600, true, Some(Duration::from_secs(3))),
            Duration::from_secs(600)
        );
        assert_eq!(
            provider_delay(60, false, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
    }
    #[test]
    fn leaving_gaming_mode_refreshes_immediately_but_entering_does_not() {
        let now = Instant::now();
        let future = now + Duration::from_secs(300);
        let mut due = BTreeMap::from([("razer", future), ("bluetooth", future)]);
        leave_quiet_mode(
            false,
            true,
            ["razer", "bluetooth"].into_iter(),
            &mut due,
            now,
        );
        assert!(due.values().all(|v| *v == future));
        leave_quiet_mode(
            true,
            false,
            ["razer", "bluetooth"].into_iter(),
            &mut due,
            now,
        );
        assert!(due.values().all(|v| *v == now));
    }
    #[test]
    fn disabled_bluetooth_providers_and_suspended_jobs_are_not_dispatched() {
        let now = Instant::now();
        let mut s = Settings::default();
        assert!(poll_is_due(&s, "razer", Some(now), now, false));
        assert!(!poll_is_due(&s, "razer", Some(now), now, true));
        assert!(!poll_is_due(&s, "razer", None, now, false));
        assert!(!poll_is_due(
            &s,
            "razer",
            Some(now + Duration::from_secs(1)),
            now,
            false,
        ));
        s.bluetooth = false;
        s.disabled_providers.insert("razer".into());
        assert!(!poll_is_due(&s, "razer", Some(now), now, false));
        assert!(!poll_is_due(&s, "bluetooth", Some(now), now, false));
        assert!(poll_is_due(&s, "xinput", Some(now), now, false));
        s.playstation_full_mode = true;
        s.disabled_providers.insert("playstation".into());
        assert!(!poll_is_due(&s, "playstation", Some(now), now, false));
    }
    #[test]
    fn expired_or_cancelled_provider_success_cannot_confirm_disconnection() {
        struct Clock(std::sync::atomic::AtomicU64);
        impl hb_core::Clock for Clock {
            fn unix(&self) -> i64 {
                100 + self.0.load(Ordering::Relaxed) as i64
            }
            fn monotonic(&self) -> Duration {
                Duration::from_secs(self.0.load(Ordering::Relaxed))
            }
            fn sleep(&self, duration: Duration) {
                self.0.fetch_add(duration.as_secs(), Ordering::Relaxed);
            }
        }
        struct Provider(u8);
        impl BatteryProvider for Provider {
            fn id(&self) -> &'static str {
                "simulation"
            }
            fn diagnostics(&self) -> Vec<String> {
                vec![]
            }
            fn poll(&mut self, _: &dyn HidTransport, context: &PollContext<'_>) -> PollResult {
                if self.0 == 1 || self.0 == 2 {
                    context.sleep(Duration::from_secs(25));
                } else if self.0 == 3 {
                    context.cancelled.store(true, Ordering::Relaxed);
                }
                Ok(if self.0 == 2 {
                    vec![Reading::new("partial", "Partial result", "simulation", 125)]
                } else {
                    vec![]
                })
            }
        }
        struct NoHid;
        impl HidTransport for NoHid {
            fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
                panic!("No native discovery in this test")
            }
            fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
                panic!("No hardware access in this test")
            }
        }
        for mode in 0..=3 {
            let clock = Clock(std::sync::atomic::AtomicU64::new(0));
            let cancel = AtomicBool::new(false);
            let job = Job::new(Box::new(Provider(mode)), &Settings::default());
            let completed = execute_job(job, &NoHid, &clock, &cancel);
            let mut engine = Engine::new(Settings::default(), Estimator::default());
            let mut reading = Reading::new("known", "Known mouse", "simulation", 100);
            reading.level = Some(75);
            engine.apply("simulation", Ok(vec![reading]), 0.0, false);
            if mode == 0 {
                assert!(completed.result.as_ref().unwrap().is_empty());
                engine.apply("simulation", completed.result, 1.0, false);
            } else {
                assert!(
                    completed.result.is_err(),
                    "mode {mode} must not confirm inventory"
                );
                engine.apply("simulation", completed.result, 25.0, false);
                assert_eq!(engine.readings()[0].key, "known");
                assert_eq!(engine.readings()[0].connection, Connection::Stale);
            }
        }
    }
    #[test]
    fn full_mode_setting_reaches_provider_context_and_cancelled_jobs_do_not_poll() {
        struct Provider(Arc<Mutex<Vec<bool>>>);
        impl BatteryProvider for Provider {
            fn diagnostics(&self) -> Vec<String> {
                vec![]
            }
            fn id(&self) -> &'static str {
                "playstation"
            }
            fn poll(&mut self, _: &dyn HidTransport, c: &PollContext<'_>) -> PollResult {
                self.0.lock().unwrap().push(c.playstation_full_mode);
                Ok(vec![])
            }
        }
        struct NoHid;
        impl HidTransport for NoHid {
            fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
                panic!("unexpected HID")
            }
            fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
                panic!("unexpected HID")
            }
        }
        let observed = Arc::new(Mutex::new(vec![]));
        let mut s = Settings::default();
        let cancel = AtomicBool::new(false);
        let clock = SystemClock::default();
        for full in [false, true] {
            s.playstation_full_mode = full;
            let job = Job::new(Box::new(Provider(observed.clone())), &s);
            assert!(execute_job(job, &NoHid, &clock, &cancel).result.is_ok());
        }
        cancel.store(true, Ordering::Relaxed);
        let job = Job::new(Box::new(Provider(observed.clone())), &s);
        assert!(execute_job(job, &NoHid, &clock, &cancel).result.is_err());
        assert_eq!(*observed.lock().unwrap(), [false, true]);
    }
    fn control_reading() -> Reading {
        let mut reading = Reading::new("simulated:mouse", "Mouse", "simulation", 10);
        reading.kind = "mouse".into();
        reading
    }
    fn control_request(action: ControlAction, generation: u64) -> ControlRequest {
        let reading = control_reading();
        ControlRequest {
            request: 1,
            target: ControlTarget {
                device: ConfigurationDevice::from_reading(&reading),
                generation,
                connection_stamp: None,
            },
            action,
        }
    }
    fn startup_settings() -> Settings {
        let mut settings = Settings {
            polling_controls: true,
            restore_polling_on_startup: true,
            ..Default::default()
        };
        settings
            .devices
            .entry("simulated:mouse".into())
            .or_default()
            .requested_polling_rate = PollingRate::try_from(8000).ok();
        settings
    }
    #[test]
    fn startup_restore_does_not_override_fullscreen_boost() {
        let now = Instant::now();
        let mut settings = startup_settings();
        let preferences = settings.devices.get_mut("simulated:mouse").unwrap();
        preferences.requested_polling_rate = PollingRate::try_from(1000).ok();
        preferences.fullscreen_boost_rate = PollingRate::try_from(4000).ok();
        let readings = [control_reading()];
        let mut startup = StartupPolling::new(&settings, now);
        let mut boost = crate::boost::Boost::new(now);
        boost.sample(now, Some(true));
        let Command::Polling(request, _, _) = startup
            .next(
                now,
                0,
                1,
                readings.iter().map(ConfigurationDevice::from_reading),
                None,
                true,
            )
            .unwrap()
        else {
            panic!("expected startup restore")
        };
        // The dispatcher must not treat a blocked startup restore as a manual
        // override when the app starts while a game is already fullscreen.
        assert!(!manual_polling_change(&request));
        if manual_polling_change(&request) {
            boost.manual(&request.target.device.key);
        }
        boost.sample(now + Duration::from_secs(60), Some(true));
        assert_eq!(
            boost.next(&settings, 1, &readings).unwrap().action,
            ControlAction::Read
        );
    }
    #[test]
    fn only_user_rate_writes_count_as_manual_overrides() {
        let rate = PollingRate::try_from(1000).unwrap();
        for action in [
            ControlAction::Read,
            ControlAction::Apply(rate),
            ControlAction::ApplyIf {
                rate,
                expected: rate,
            },
        ] {
            let mut request = control_request(action, 1);
            assert_eq!(manual_polling_change(&request), action.rate().is_some());
            for background in [
                STARTUP_POLLING_REQUEST_BIT,
                STARTUP_POLLING_REQUEST_BIT | STARTUP_POLLING_READ_BIT,
                STARTUP_POLLING_REQUEST_BIT | crate::boost::REQUEST_BIT,
                STARTUP_POLLING_REQUEST_BIT | crate::battery_saver::REQUEST_BIT,
            ] {
                request.request = background | 1;
                assert!(!manual_polling_change(&request));
            }
        }
    }
    #[test]
    fn startup_polling_waits_for_queue_space_without_consuming_restore_or_read() {
        let now = Instant::now();
        let device = ConfigurationDevice::from_reading(&control_reading());
        let (queue, received) = bounded(1);
        queue.send(()).unwrap();
        for restore in [false, true] {
            let mut settings = startup_settings();
            settings.restore_polling_on_startup = restore;
            let mut startup = StartupPolling::new(&settings, now);
            for _ in 0..4 {
                assert!(
                    startup
                        .next(
                            now,
                            0,
                            7,
                            std::iter::once(device.clone()),
                            None,
                            !queue.is_full()
                        )
                        .is_none()
                );
                assert!(startup.reads.attempted.is_empty());
                assert_eq!(startup.restore.pending.len(), usize::from(restore));
            }
            received.recv().unwrap();
            let Command::Polling(request, _, _) = startup
                .next(
                    now,
                    0,
                    7,
                    std::iter::once(device.clone()),
                    None,
                    !queue.is_full(),
                )
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(
                request.action,
                if restore {
                    ControlAction::Apply(PollingRate::try_from(8000).unwrap())
                } else {
                    ControlAction::Read
                }
            );
            assert!(
                startup
                    .next(now, 0, 8, std::iter::once(device.clone()), None, true)
                    .is_none()
            );
            queue.send(()).unwrap();
        }
    }
    #[test]
    fn initial_polling_reads_only_supported_online_mice_once_and_obeys_opt_in() {
        let now = Instant::now();
        let device = ConfigurationDevice::from_reading(&control_reading());
        let mut sleeping = device.clone();
        sleeping.connection = Connection::Sleeping;
        let mut keyboard = simulated_keyboards().remove(0);
        keyboard.key = "synthetic-keyboard".into();
        let mut unknown = device.clone();
        unknown.key = "unknown".into();
        unknown.source = "unknown".into();
        let mut second = device.clone();
        second.key = "simulated:second".into();
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        let mut reads = InitialPollingReads::new(&settings, now);
        assert!(
            reads
                .next(now, 0, 1, [sleeping, keyboard, unknown].into_iter())
                .is_none()
        );
        for target in [device.clone(), second] {
            let Command::Polling(request, epoch, keyboard_epoch) = reads
                .next(now, 9, 2, std::iter::once(target.clone()))
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(request.action, ControlAction::Read);
            assert_ne!(request.request & STARTUP_POLLING_READ_BIT, 0);
            assert_eq!(request.target.device, target);
            assert_eq!((epoch, keyboard_epoch), (9, None));
            assert!(reads.next(now, 9, 3, std::iter::once(target)).is_none());
        }
        settings.polling_controls = false;
        let mut disabled = InitialPollingReads::new(&settings, now);
        assert!(
            disabled
                .next(now, 0, 1, std::iter::once(device.clone()))
                .is_none()
        );
        let mut cancelled = InitialPollingReads::new(&startup_settings(), now);
        cancelled.cancel();
        assert!(
            cancelled
                .next(now, 0, 1, std::iter::once(device.clone()))
                .is_none()
        );
        let mut expired = InitialPollingReads::new(&startup_settings(), now);
        assert!(
            expired
                .next(
                    now + Duration::from_secs(300),
                    0,
                    1,
                    std::iter::once(device)
                )
                .is_none()
        );
        assert!(!expired.active());
        assert!(!expired.initial_open);
        assert!(expired.attempted.is_empty());
    }
    fn recovery_sample(timestamp: i64, session: u64) -> UsageObservation {
        let mut reading = control_reading();
        reading.timestamp = timestamp;
        reading.level = Some(60);
        reading.charging = Some(false);
        UsageObservation {
            reading,
            polling_rate: None,
            session: Some(session),
        }
    }
    fn recovery_request(
        reads: &mut InitialPollingReads,
        now: Instant,
        generation: u64,
        sample: &UsageObservation,
    ) -> ControlRequest {
        let Command::Polling(request, _, configuration_epoch) = reads
            .next(
                now,
                0,
                generation,
                std::iter::once(ConfigurationDevice::from_reading(&sample.reading)),
            )
            .expect("fresh missing-rate episode must get a readback")
        else {
            panic!()
        };
        assert_eq!(request.action, ControlAction::Read);
        assert_eq!(configuration_epoch, None);
        request
    }
    #[test]
    fn polling_recovery_after_startup_wake_refresh_and_clock_change_is_read_only() {
        let start = Instant::now();
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        let mut tracker = UsageTracker::new(1, true);
        let mut rate = PollingRate::try_from(1000).unwrap();
        let mut reading = recovery_sample(1000, 0).reading;
        let mut now = start + Duration::from_secs(3600);
        for generation in 1..=4 {
            if generation == 2 {
                // An inaccessible sleeping collection revokes rate evidence;
                // an ordinary sleep with retained evidence needs no extra GET.
                reading.connection = Connection::Stale;
                let asleep = tracker.samples(vec![reading.clone()]);
                reads.observe_usage(&asleep, true, now, reading.timestamp);
                assert!(!reads.needs_inventory(
                    now,
                    reading.timestamp,
                    std::iter::once(&reading),
                    &[]
                ));
                reading.connection = Connection::Online;
            } else if generation == 3 {
                tracker.invalidate(generation, true); // Refresh or HID change
            } else if generation == 4 {
                tracker.synchronize_clock(2000);
                assert!(tracker.synchronize_clock(900));
            }
            reading.timestamp += 60;
            let samples = tracker.samples(vec![reading.clone()]);
            assert_eq!(samples[0].polling_rate, None);
            reads.observe_usage(&samples, true, now, reading.timestamp);
            assert!(reads.needs_inventory(now, reading.timestamp, std::iter::once(&reading), &[]));
            let request = recovery_request(&mut reads, now, tracker.generation, &samples[0]);
            tracker.begin_control(&request, None);
            let outcome =
                simulate_control(&request, &mut rate, tracker.generation, reading.timestamp);
            assert!(!outcome.may_have_changed);
            assert!(reads.owns(outcome.request, &reading.key));
            tracker.observe(&outcome);
            assert!(
                tracker.take_activity(&outcome).is_none(),
                "GETs must not count as boost or manual rate changes"
            );
            reads.complete(&outcome, now);
            let known = tracker.samples(vec![reading.clone()]);
            assert_eq!(known[0].polling_rate, Some(rate));
            reads.observe_usage(&known, true, now, reading.timestamp);
            assert!(!reads.active());
            assert!(!reads.initial_open);
            now += Duration::from_secs(3600);
        }
    }
    #[test]
    fn polling_recovery_retries_are_bounded_per_episode_and_wait_for_fresh_readings() {
        let start = Instant::now();
        let mut now = start + Duration::from_secs(3600);
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        let mut sample = recovery_sample(1000, 1);
        reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        for attempt in 1..=3 {
            let request = recovery_request(&mut reads, now, 1, &sample);
            assert_eq!(reads.pending[&sample.reading.key].attempts, attempt);
            reads.complete(&ControlOutcome::failed(&request, "receiver busy"), now);
            // Ordinary battery updates cannot restart a failed recovery budget.
            reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
            assert!(!reads.needs_inventory(
                now + Duration::from_secs(14),
                1000,
                std::iter::once(&sample.reading),
                &[]
            ));
            if attempt < 3 {
                assert!(reads.needs_inventory(
                    now + Duration::from_secs(15),
                    1000,
                    std::iter::once(&sample.reading),
                    &[]
                ));
            }
            now += Duration::from_secs(15);
        }
        assert!(!reads.active());
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
                assert!(!reads.needs_inventory(now, 1000, std::iter::once(&sample.reading), &[]));
            }
        });
        assert_eq!(allocations.calls, 0);
        sample.session = Some(2);
        reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        assert!(reads.active());
        sample.reading.connection = Connection::Sleeping;
        assert!(!reads.needs_inventory(now, 1000, std::iter::once(&sample.reading), &[]));
        sample.reading.connection = Connection::Online;
        assert!(!reads.needs_inventory(now, 1121, std::iter::once(&sample.reading), &[]));
        assert!(reads.needs_inventory(now, 1000, std::iter::once(&sample.reading), &[]));
        recovery_request(&mut reads, now, 2, &sample);
    }
    #[test]
    fn polling_recovery_preserves_an_observed_episode_across_startup_expiry() {
        let start = Instant::now();
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        let sample = recovery_sample(1000, 1);
        let request = recovery_request(&mut reads, start, 1, &sample);
        reads.observe_usage(std::slice::from_ref(&sample), true, start, 1000);
        let later = start + Duration::from_secs(301);
        reads.expire_initial(later);
        assert!(!reads.initial_open);
        assert!(reads.owns(request.request, &sample.reading.key));
        reads.complete(&ControlOutcome::failed(&request, "late failure"), later);
        let read = recovery_request(&mut reads, later + Duration::from_secs(15), 2, &sample);
        assert_eq!(reads.pending[&sample.reading.key].attempts, 2);
        assert_ne!(request.request, read.request);
        // A different native identity invalidates UsageTracker's proof and gets
        // a fresh target; no stale serial/container may be reused by recovery.
        let mut tracker = UsageTracker::new(2, true);
        let outcome = simulate_control(&read, &mut PollingRate::try_from(1000).unwrap(), 2, 1000);
        tracker.observe(&outcome);
        reads.complete(&outcome, later);
        let known = tracker.samples(vec![sample.reading.clone()]);
        reads.observe_usage(&known, true, later, 1000);
        let mut replacement = sample.reading;
        replacement.serial = Some("DIFFERENT_TEST_UNIT".into());
        replacement.container = Some("DIFFERENT_TEST_CONTAINER".into());
        let unknown = tracker.samples(vec![replacement.clone()]);
        assert!(unknown[0].polling_rate.is_none());
        reads.observe_usage(&unknown, true, later, 1000);
        let request = recovery_request(&mut reads, later, 2, &unknown[0]);
        assert!(request.target.device.matches_reading(&replacement));
        assert!(!request.target.device.matches_reading(&known[0].reading));
    }
    #[test]
    fn polling_recovery_rejects_unavailable_charging_coarse_unknown_and_disabled_devices() {
        let start = Instant::now();
        let now = start + Duration::from_secs(3600);
        let base = recovery_sample(1000, 1);
        for case in 0..12 {
            let mut sample = base.clone();
            match case {
                0 => sample.reading.connection = Connection::Sleeping,
                1 => sample.reading.connection = Connection::Stale,
                2 => sample.reading.charging = Some(true),
                3 => sample.reading.charging = None,
                4 => sample.reading.charging_inferred = true,
                5 => sample.reading.level = None,
                6 => sample.reading.timestamp = 1001,
                7 => sample.reading.timestamp = 879,
                8 => sample.reading.source = "unknown".into(),
                9 => sample.reading.kind = "keyboard".into(),
                10 => sample.session = None,
                11 => sample.reading.precision = Precision::Coarse,
                _ => unreachable!(),
            }
            let mut reads = InitialPollingReads::new(&startup_settings(), start);
            reads.observe_usage(&[sample], true, now, 1000);
            assert!(reads.pending.is_empty(), "case {case}");
        }
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        reads.observe_usage(std::slice::from_ref(&base), false, now, 1000);
        assert!(!reads.active());
        assert!(reads.attempted.is_empty());
        reads.observe_usage(std::slice::from_ref(&base), true, now, 1000);
        recovery_request(&mut reads, now, 1, &base);
    }
    #[test]
    fn polling_recovery_discards_superseded_reads_and_does_not_restart_startup_restoration() {
        let start = Instant::now();
        let now = start + Duration::from_secs(3600);
        let mut startup = StartupPolling::new(&startup_settings(), start);
        startup.cancel(); // settings edit / suspend
        let mut sample = recovery_sample(1000, 1);
        startup
            .reads
            .observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        assert!(
            startup.restore.pending.is_empty(),
            "resume must not reapply saved choices"
        );
        assert!(!startup.blocks_automation(now));
        let previous = recovery_request(&mut startup.reads, now, 1, &sample);
        sample.session = Some(2); // reconnect / changed hardware identity
        startup
            .reads
            .observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        assert!(!startup.reads.owns(previous.request, &sample.reading.key));
        let current = recovery_request(&mut startup.reads, now, 2, &sample);
        startup
            .reads
            .complete(&ControlOutcome::failed(&previous, "late read"), now);
        assert!(startup.reads.owns(current.request, &sample.reading.key));
        startup.reads.observe_request(&sample.reading.key); // manual or boost request
        assert!(!startup.reads.owns(current.request, &sample.reading.key));
        startup
            .reads
            .complete(&ControlOutcome::failed(&current, "late read"), now);
        assert!(!startup.reads.active());
    }
    #[test]
    fn polling_recovery_state_is_bounded_and_confirmed_observations_do_not_allocate() {
        let start = Instant::now();
        let now = start + Duration::from_secs(3600);
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        let mut sample = recovery_sample(1000, 1);
        sample.polling_rate = PollingRate::try_from(1000).ok();
        reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
                assert!(!reads.needs_inventory(now, 1000, std::iter::once(&sample.reading), &[]));
            }
        });
        assert_eq!(allocations.calls, 0);
        assert_eq!(allocations.bytes, 0);
        sample.polling_rate = None;
        for index in 1..=600 {
            sample.reading.key = format!("simulated:{index}");
            reads.observe_usage(std::slice::from_ref(&sample), true, now, 1000);
        }
        assert_eq!(reads.attempted.len(), 512);
        assert_eq!(reads.pending.len(), 511);
    }
    #[test]
    fn initial_polling_reads_allow_late_discovery_and_bound_read_only_retries() {
        let start = Instant::now();
        let mut now = start + Duration::from_secs(120);
        let device = ConfigurationDevice::from_reading(&control_reading());
        let mut reads = InitialPollingReads::new(&startup_settings(), start);
        for generation in 1..=3 {
            let Command::Polling(request, _, _) = reads
                .next(now, 0, generation, std::iter::once(device.clone()))
                .expect("late receiver must get a bounded startup read")
            else {
                panic!()
            };
            assert_eq!(request.action, ControlAction::Read);
            assert_eq!(request.target.generation, generation);
            assert!(
                reads
                    .next(now, 0, generation, std::iter::once(device.clone()))
                    .is_none()
            );
            reads.complete(&ControlOutcome::failed(&request, "receiver not ready"), now);
            assert!(
                reads
                    .next(
                        now + Duration::from_secs(14),
                        0,
                        generation,
                        std::iter::once(device.clone())
                    )
                    .is_none()
            );
            now += Duration::from_secs(15);
        }
        assert!(reads.next(now, 0, 4, std::iter::once(device)).is_none());
        assert!(reads.pending.is_empty());
    }
    #[test]
    fn startup_rate_reads_do_not_rebuild_inventory_for_unsupported_mice() {
        let now = Instant::now();
        let mut reading = recovery_sample(1000, 1).reading;
        reading.source = "razer".into();
        reading.key = "razer:0001:SYNTHETIC".into();
        reading.via = "usb".into();
        let mut reads = InitialPollingReads::new(&startup_settings(), now);
        assert!(reads.needs_inventory(now, 1000, std::iter::once(&reading), &[]));
        assert!(
            reads
                .next(
                    now,
                    0,
                    1,
                    std::iter::once(ConfigurationDevice::from_reading(&reading))
                )
                .is_none()
        );
        assert!(
            !reads.needs_inventory(now, 1000, std::iter::once(&reading), &[]),
            "An unsupported model was already checked"
        );
        let (_, allocations) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                assert!(!reads.needs_inventory(now, 1000, std::iter::once(&reading), &[]));
            }
        });
        assert_eq!(allocations.calls, 0);
        assert_eq!(allocations.bytes, 0);
        let reads = InitialPollingReads::new(&startup_settings(), now);
        reading.source = "corsair".into();
        reading.key = "corsair:SYNTHETIC".into();
        assert!(
            !reads.needs_inventory(now, 1000, std::iter::once(&reading), &[]),
            "Battery-only providers cannot expose polling controls"
        );
    }
    #[test]
    fn startup_uncertain_restore_only_rechecks_and_confirmation_stops_recovery() {
        let now = Instant::now();
        let device = ConfigurationDevice::from_reading(&control_reading());
        let mut startup = StartupPolling::new(&startup_settings(), now);
        let Command::Polling(restore, _, _) = startup
            .next(now, 0, 1, std::iter::once(device.clone()), None, true)
            .unwrap()
        else {
            panic!()
        };
        assert!(matches!(restore.action, ControlAction::Apply(_)));
        let mut uncertain = ControlOutcome::failed(&restore, "acknowledgment missing");
        uncertain.may_have_changed = true;
        startup.reads.complete(&uncertain, now);
        let later = now + Duration::from_secs(15);
        let Command::Polling(read, _, _) = startup
            .next(later, 0, 2, std::iter::once(device.clone()), None, true)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(read.action, ControlAction::Read);
        let mut rate = PollingRate::try_from(2000).unwrap();
        let outcome = simulate_control(&read, &mut rate, 2, 100);
        startup.reads.complete(&outcome, later);
        assert!(
            startup
                .next(
                    later + Duration::from_secs(30),
                    0,
                    2,
                    std::iter::once(device),
                    None,
                    true
                )
                .is_none()
        );
        assert!(!startup.blocks_automation(now + Duration::from_secs(60)));
        assert!(startup.restore.pending.is_empty());
        assert!(startup.reads.pending.is_empty());
    }
    #[test]
    fn startup_read_recovery_yields_to_manual_requests_and_cancellation() {
        let now = Instant::now();
        let device = ConfigurationDevice::from_reading(&control_reading());
        for cancel in [false, true] {
            let mut reads = InitialPollingReads::new(&startup_settings(), now);
            let Command::Polling(request, _, _) = reads
                .next(now, 0, 1, std::iter::once(device.clone()))
                .unwrap()
            else {
                panic!()
            };
            if cancel {
                reads.cancel();
            } else {
                reads.observe_request(&device.key);
            }
            reads.complete(&ControlOutcome::failed(&request, "late completion"), now);
            assert!(
                reads
                    .next(
                        now + Duration::from_secs(20),
                        0,
                        2,
                        std::iter::once(device.clone())
                    )
                    .is_none()
            );
            assert!(reads.pending.is_empty());
        }
    }
    #[test]
    fn device_watcher_initial_inventory_does_not_invalidate_startup_readback() {
        let (commands, received) = bounded(8);
        let gate = DeviceEventGate::default();
        for id in [
            "HID#initial-mouse",
            "BTH-initial-controller",
            "Bluetooth-initial-keyboard",
        ] {
            gate.forward(&commands, id, true);
        }
        assert!(received.try_recv().is_err());
        // Added, Updated and Removed callbacks share this same gate. Once the
        // initial enumeration finishes, real HID/Bluetooth transitions retain
        // their existing refresh/invalidation behavior.
        gate.enumeration_completed.store(true, Ordering::Release);
        for (id, arrival_or_removal) in [
            ("HID#new-mouse", true),
            ("BTH-changed-controller", false),
            ("Bluetooth-removed-keyboard", true),
        ] {
            gate.forward(&commands, id, arrival_or_removal);
            assert!(
                matches!(received.try_recv(), Ok(Command::DeviceChanged {arrival_or_removal: actual}) if actual == arrival_or_removal)
            );
        }
        gate.forward(&commands, "USB-unrelated-storage", true);
        assert!(received.try_recv().is_err());
    }
    #[test]
    fn startup_restore_requires_both_opt_ins_and_a_saved_choice() {
        let now = Instant::now();
        for (controls, restore) in [(false, false), (true, false), (false, true)] {
            let mut settings = startup_settings();
            settings.polling_controls = controls;
            settings.restore_polling_on_startup = restore;
            let mut startup = StartupPollingRestore::new(&settings, now);
            assert!(
                startup
                    .next(
                        now,
                        0,
                        1,
                        std::iter::once(ConfigurationDevice::from_reading(&control_reading())),
                        None
                    )
                    .is_none()
            );
        }
        let mut settings = startup_settings();
        settings.devices.clear();
        let mut startup = StartupPollingRestore::new(&settings, now);
        assert!(
            startup
                .next(
                    now,
                    0,
                    1,
                    std::iter::once(ConfigurationDevice::from_reading(&control_reading())),
                    None
                )
                .is_none()
        );
    }
    #[test]
    fn startup_alias_keeps_the_wired_target_and_rejects_rates_above_its_ceiling() {
        let now = Instant::now();
        for (wired_pid, wireless_pid, hz, applies) in
            [("00be", "00bf", 8000, true), ("00b6", "00b7", 8000, false)]
        {
            let mut reading = control_reading();
            reading.key = format!("razer:{wired_pid}:TESTUNIT1234");
            reading.source = "razer".into();
            reading.via = "usb".into();
            reading.serial = Some("TESTUNIT1234".into());
            let mut s = startup_settings();
            s.devices.clear();
            s.device_mut(&format!("razer:{wireless_pid}:TESTUNIT1234"))
                .requested_polling_rate = Some(hz.try_into().unwrap());
            let mut startup = StartupPollingRestore::new(&s, now);
            let result = startup.next(
                now,
                0,
                7,
                std::iter::once(ConfigurationDevice::from_reading(&reading)),
                None,
            );
            assert_eq!(result.is_some(), applies);
            if let Some(Command::Polling(request, _, _)) = result {
                assert_eq!(request.target.device.key, reading.key);
                assert_eq!(request.action.rate().unwrap().hz(), hz);
            }
            assert!(startup.pending.is_empty());
        }
    }
    #[test]
    fn manual_choice_cancels_only_matching_pending_and_inflight_startup_restores() {
        let now = Instant::now();
        let mut wireless = control_reading();
        wireless.key = "razer:00bf:TESTUNIT1234".into();
        wireless.source = "razer".into();
        wireless.via = "usb".into();
        wireless.serial = Some("TESTUNIT1234".into());
        let wireless = ConfigurationDevice::from_reading(&wireless);
        let mut wired = wireless.clone();
        wired.key = "razer:00be:TESTUNIT1234".into();
        let mut other = wireless.clone();
        other.key = "razer:00bf:OTHERUNIT1234".into();
        other.serial = Some("OTHERUNIT1234".into());
        let mut settings = startup_settings();
        settings.devices.clear();
        settings.device_mut(&wireless.key).requested_polling_rate = Some(8000.try_into().unwrap());
        settings.device_mut(&other.key).requested_polling_rate = Some(1000.try_into().unwrap());

        let mut pending = StartupPollingRestore::new(&settings, now);
        pending.cancel_device(&wired);
        assert!(!pending.pending.contains_key(&wireless.key));
        assert!(pending.pending.contains_key(&other.key));
        assert!(
            pending
                .next(now, 0, 7, std::iter::once(wireless.clone()), None)
                .is_none()
        );

        let mut inflight = StartupPollingRestore::new(&settings, now);
        let Command::Polling(request, epoch, visible) = inflight
            .next(now, 0, 7, std::iter::once(wireless.clone()), None)
            .unwrap()
        else {
            panic!("Expected startup rate request")
        };
        let guard = inflight.guard(request.request).unwrap();
        let job = WorkerJob::Polling(
            Box::new(request.clone()),
            guard.clone(),
            epoch,
            visible,
            None,
            None,
        );
        let Command::Polling(other_request, _, _) = inflight
            .next(now, 0, 7, std::iter::once(other.clone()), None)
            .unwrap()
        else {
            panic!("Expected other-device startup request")
        };
        let other_guard = inflight.guard(other_request.request).unwrap();
        let shutdown = AtomicBool::new(false);
        let configuration = ConfigurationWatch::default();
        let permission = ControlPermission::new(true);
        assert!(!gate_wait_cancelled(
            &job,
            &shutdown,
            &configuration,
            &permission
        ));
        inflight.cancel_device(&wired);
        assert!(guard.load(Ordering::Acquire));
        assert!(
            !other_guard.load(Ordering::Acquire),
            "An unrelated device must remain unaffected"
        );
        assert!(gate_wait_cancelled(
            &job,
            &shutdown,
            &configuration,
            &permission
        ));
        assert_eq!(inflight.complete(request.request), Some(true));
        assert_eq!(inflight.complete(request.request), None);
        assert_eq!(inflight.complete(other_request.request), Some(false));
    }
    #[test]
    fn startup_restore_applies_once_without_retry_or_reconnect_enforcement() {
        let now = Instant::now();
        let mut startup = StartupPollingRestore::new(&startup_settings(), now);
        let device = ConfigurationDevice::from_reading(&control_reading());
        let Command::Polling(request, epoch, keyboard_epoch) = startup
            .next(now, 0, 7, std::iter::once(device.clone()), None)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(epoch, 0);
        assert_eq!(keyboard_epoch, None);
        assert!(request.request >= 1 << 63);
        assert_eq!(request.target.generation, 7);
        let mut current = PollingRate::try_from(1000).unwrap();
        let outcome = simulate_control(&request, &mut current, 7, 10);
        assert!(outcome.confirmed_change());
        assert_eq!(current.hz(), 8000);
        // Success, busy/failure, or uncertain outcome does not feed the
        // scheduler. A new generation cannot make a consumed choice eligible.
        assert!(
            startup
                .next(now, 0, 8, std::iter::once(device.clone()), None)
                .is_none()
        );
        let mut startup = StartupPollingRestore::new(&startup_settings(), now);
        let Command::Polling(request, _, _) = startup
            .next(now, 0, 7, std::iter::once(device.clone()), None)
            .unwrap()
        else {
            panic!()
        };
        let failed = ControlOutcome::failed(&request, "Device workers are busy");
        assert!(failed.failure.is_some());
        assert!(
            startup
                .next(now, 0, 8, std::iter::once(device), None)
                .is_none()
        );
    }
    #[test]
    fn startup_restore_same_rate_is_confirmed_without_change() {
        let now = Instant::now();
        let mut startup = StartupPollingRestore::new(&startup_settings(), now);
        let Command::Polling(request, _, _) = startup
            .next(
                now,
                0,
                1,
                std::iter::once(ConfigurationDevice::from_reading(&control_reading())),
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        let mut current = PollingRate::try_from(8000).unwrap();
        let outcome = simulate_control(&request, &mut current, 1, 10);
        assert!(outcome.failure.is_none());
        assert!(!outcome.may_have_changed);
        assert_eq!(outcome.observation.unwrap().rate, Some(current));
    }
    #[test]
    fn startup_restore_simulated_runtime_emits_confirmed_unsolicited_outcome() {
        let _native_guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::start(dir.path().to_owned(), startup_settings(), true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut confirmed = None;
        while Instant::now() < deadline {
            if let Ok(Event::Polling(outcome)) =
                runtime.events.recv_timeout(Duration::from_millis(100))
                && outcome.request & STARTUP_POLLING_REQUEST_BIT != 0
            {
                confirmed = Some(outcome);
                break;
            }
        }
        runtime.stop();
        let outcome = confirmed.expect("startup restore must emit an unsolicited polling outcome");
        assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
        assert_eq!(
            outcome.observation.as_ref().unwrap().rate.unwrap().hz(),
            8000
        );
    }
    #[test]
    fn startup_restore_expires_and_cancellation_is_permanent() {
        let now = Instant::now();
        let device = ConfigurationDevice::from_reading(&control_reading());
        let mut startup = StartupPollingRestore::new(&startup_settings(), now);
        assert!(
            startup
                .next(
                    now + Duration::from_secs(60),
                    0,
                    1,
                    std::iter::once(device.clone()),
                    None
                )
                .is_none()
        );
        assert!(startup.pending.is_empty());
        let mut startup = StartupPollingRestore::new(&startup_settings(), now);
        startup.cancel();
        assert!(
            startup
                .next(now, 1, 2, std::iter::once(device), None)
                .is_none()
        );
    }
    #[test]
    fn startup_restore_waits_for_online_supported_devices_and_visible_keyboard_inventory() {
        let now = Instant::now();
        let mut settings = startup_settings();
        let mut keyboard = simulated_keyboards().remove(0);
        settings
            .devices
            .entry(keyboard.key.clone())
            .or_default()
            .requested_polling_rate = PollingRate::try_from(1000).ok();
        let mut startup = StartupPollingRestore::new(&settings, now);
        let mut mouse = ConfigurationDevice::from_reading(&control_reading());
        mouse.connection = Connection::Stale;
        assert!(
            startup
                .next(
                    now,
                    0,
                    1,
                    [mouse.clone(), keyboard.clone()].into_iter(),
                    None
                )
                .is_none()
        );
        mouse.connection = Connection::Online;
        mouse.source = "unknown".into();
        assert!(
            startup
                .next(now, 0, 1, std::iter::once(mouse), None)
                .is_none()
        );
        keyboard.capability = PollingCapability::Unavailable("unsupported".into());
        assert!(
            startup
                .next(now, 0, 1, std::iter::once(keyboard.clone()), Some(3))
                .is_none()
        );
        keyboard.capability = PollingCapability::ReadWrite;
        let Command::Polling(request, _, Some(configuration_epoch)) = startup
            .next(now, 0, 1, std::iter::once(keyboard), Some(3))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(request.target.device.kind, "keyboard");
        assert_eq!(configuration_epoch, 3);
        assert!(
            startup
                .next(now, 0, 2, simulated_keyboards().into_iter(), Some(4))
                .is_none()
        );
    }
    #[test]
    fn explicit_simulation_read_apply_restore_and_stale_epoch() {
        let mut current = PollingRate::try_from(1000).unwrap();
        let read = simulate_control(
            &control_request(ControlAction::Read, 0),
            &mut current,
            2,
            10,
        );
        assert_eq!(read.observation.unwrap().rate.unwrap().hz(), 1000);
        assert!(!read.may_have_changed);
        let target = control_request(
            ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
            2,
        );
        let applied = simulate_control(&target, &mut current, 2, 11);
        assert!(applied.confirmed_change());
        assert_eq!(applied.previous.unwrap().hz(), 1000);
        assert_eq!(current.hz(), 8000);
        assert!(
            simulate_control(&target, &mut current, 3, 12)
                .failure
                .is_some()
        );
        let restored = simulate_control(
            &control_request(ControlAction::Apply(applied.previous.unwrap()), 2),
            &mut current,
            2,
            13,
        );
        assert!(restored.confirmed_change());
        assert_eq!(current.hz(), 1000);
    }
    #[test]
    fn gaming_or_cancelled_controls_never_open_hid() {
        struct NeverHid;
        impl HidTransport for NeverHid {
            fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
                panic!("unexpected access")
            }
            fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
                panic!("unexpected access")
            }
        }
        let clock = SystemClock::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut request = control_request(
            ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
            0,
        );
        for provider in hb_providers::controls::POLLING_PROVIDERS {
            request.target.device.source = (*provider).into();
            assert!(
                execute_control(
                    &request, &NeverHid, &clock, &shutdown, &cancelled, true, None
                )
                .failure
                .unwrap()
                .contains("Close the game")
            );
        }
        cancelled.store(true, Ordering::Relaxed);
        assert!(
            execute_control(
                &request, &NeverHid, &clock, &shutdown, &cancelled, false, None
            )
            .failure
            .is_some()
        );
        cancelled.store(false, Ordering::Relaxed);
        shutdown.store(true, Ordering::Relaxed);
        assert!(
            execute_control(
                &request, &NeverHid, &clock, &shutdown, &cancelled, false, None
            )
            .failure
            .is_some()
        );
    }
    #[test]
    fn full_command_queue_reports_rejection_without_silent_drop() {
        let (commands, _rx) = bounded(1);
        commands.send(Command::Refresh).unwrap();
        let (event_tx, events) = bounded(1);
        let runtime = Runtime {
            configuration: Arc::new(ConfigurationWatch::default()),
            commands,
            events,
            thread: None,
            permission: Arc::new(ControlPermission::new(true)),
            sink: Events {
                tx: event_tx,
                window: Arc::new(AtomicUsize::new(0)),
            },
            cancel: Arc::new(AtomicBool::new(false)),
            window: Arc::new(AtomicUsize::new(0)),
        };
        assert!(
            runtime
                .submit_control(control_request(ControlAction::Read, 0))
                .is_err()
        );
        assert!(runtime.request_insights("device".into(), 100, 1).is_err());
        assert!(!runtime.release_history_cache());
        assert!(runtime.events.is_empty());
        let history = |usage, request| {
            if usage {
                Command::UsageHistory {
                    key: "device".into(),
                    seconds: 100,
                    until: 100,
                    width: 50,
                    request,
                }
            } else {
                Command::History {
                    key: "device".into(),
                    since: 0,
                    until: 100,
                    width: 50,
                    request,
                }
            }
        };
        for usage in [false, true] {
            assert!(runtime.request_history(history(usage, 1)).is_err());
        }
        let _ = _rx.recv().unwrap();
        assert!(runtime.request_history(Command::Resume).is_err());
        assert!(_rx.is_empty());
        for usage in [false, true] {
            runtime.request_history(history(usage, 2)).unwrap();
            assert!(matches!(
                _rx.recv().unwrap(),
                Command::History { request: 2, .. } | Command::UsageHistory { request: 2, .. }
            ));
        }
        assert!(runtime.release_history_cache());
        assert!(matches!(_rx.recv().unwrap(), Command::ReleaseHistoryCache));
        runtime.request_insights("device".into(), 100, 2).unwrap();
        assert!(matches!(
            _rx.recv().unwrap(),
            Command::Insights { request: 2, .. }
        ));
        drop(_rx);
        assert!(runtime.request_insights("device".into(), 100, 3).is_err());
        assert!(runtime.request_history(history(false, 3)).is_err());
        assert!(runtime.request_history(history(true, 3)).is_err());
    }

    #[test]
    fn vendor_gate_timeout_releases_other_gates_and_never_claims_a_write() {
        let gates = BTreeMap::from([(1, Mutex::new(())), (2, Mutex::new(()))]);
        let vendors = BTreeSet::from([1, 2]);
        let held = gates[&2].lock().unwrap();
        assert!(acquire_vendor_gates(&vendors, &gates, Instant::now(), || false).is_none());
        assert!(
            gates[&1].try_lock().is_ok(),
            "partial acquisition must be released"
        );
        drop(held);
        let guards = acquire_vendor_gates(&vendors, &gates, Instant::now(), || false).unwrap();
        assert_eq!(guards.len(), 2);
        drop(guards);
        assert!(acquire_vendor_gates(&vendors, &gates, Instant::now(), || true).is_none());

        let request = control_request(
            ControlAction::Apply(PollingRate::try_from(1000).unwrap()),
            0,
        );
        let completed = vendor_gate_busy(WorkerJob::Polling(
            Box::new(request),
            Arc::new(AtomicBool::new(false)),
            0,
            None,
            None,
            None,
        ));
        let WorkerCompleted::Polling(outcome, _, _, _) = completed else {
            panic!("polling result")
        };
        assert!(!outcome.may_have_changed);
        assert!(outcome.observation.is_none());
        assert!(outcome.failure.is_some());
    }
    #[test]
    fn full_queue_retries_resume_without_authorizing_an_obsolete_epoch() {
        let (commands, received) = bounded(1);
        commands.send(Command::Refresh).unwrap();
        let (event_tx, events) = bounded(8);
        let permission = Arc::new(ControlPermission::new(true));
        let runtime = Runtime {
            configuration: Arc::new(ConfigurationWatch::default()),
            commands,
            events,
            thread: None,
            permission: permission.clone(),
            sink: Events {
                tx: event_tx,
                window: Arc::new(AtomicUsize::new(0)),
            },
            cancel: Arc::new(AtomicBool::new(false)),
            window: Arc::new(AtomicUsize::new(0)),
        };
        runtime.send(Command::Suspend);
        runtime.send(Command::Resume);
        let resume = permission.epoch.load(Ordering::Acquire);
        assert_eq!(permission.pending_resume.load(Ordering::Acquire), resume);
        runtime.retry_pending_resume();
        assert_eq!(permission.pending_resume.load(Ordering::Acquire), resume);
        assert!(matches!(received.try_recv(), Ok(Command::Refresh)));
        runtime.retry_pending_resume();
        assert!(matches!(received.try_recv(), Ok(Command::EpochResume(epoch)) if epoch == resume));
        assert_eq!(permission.pending_resume.load(Ordering::Acquire), 0);
        assert!(!permission.enabled.load(Ordering::Acquire));
        assert_ne!(permission.acknowledged.load(Ordering::Acquire), resume);

        runtime.commands.send(Command::Refresh).unwrap();
        runtime.send(Command::Resume);
        let stale = permission.pending_resume.load(Ordering::Acquire);
        assert_ne!(stale, 0);
        runtime.send(Command::Suspend);
        runtime.retry_pending_resume();
        assert_eq!(permission.pending_resume.load(Ordering::Acquire), 0);
        assert!(matches!(received.try_recv(), Ok(Command::Refresh)));
        assert!(received.try_recv().is_err());
        assert!(permission.suspended.load(Ordering::Acquire));
        assert_ne!(permission.epoch.load(Ordering::Acquire), stale);
    }
    #[test]
    fn full_queue_cannot_lose_configuration_revocation_or_revive_old_jobs() {
        let (commands, _rx) = bounded(1);
        commands.send(Command::Refresh).unwrap();
        let (event_tx, events) = bounded(4);
        let permission = Arc::new(ControlPermission::new(true));
        let runtime = Runtime {
            configuration: Arc::new(ConfigurationWatch::default()),
            commands,
            events,
            thread: None,
            permission: permission.clone(),
            sink: Events {
                tx: event_tx,
                window: Arc::new(AtomicUsize::new(0)),
            },
            cancel: Arc::new(AtomicBool::new(false)),
            window: Arc::new(AtomicUsize::new(0)),
        };
        runtime.send(Command::Settings(Settings::default()));
        assert!(!permission.enabled.load(Ordering::Acquire));
        assert_eq!(permission.epoch.load(Ordering::Acquire), 1);
        assert!(
            runtime
                .submit_control(control_request(ControlAction::Read, 0))
                .is_err()
        );
        assert!(matches!(runtime.events.try_recv(), Ok(Event::Error(_))));
        runtime.send(Command::Suspend);
        assert!(permission.suspended.load(Ordering::Acquire));
        assert_eq!(permission.epoch.load(Ordering::Acquire), 2);
        // Even a later enabling acknowledgment cannot authorize a job with epoch0.
        permission.enabled.store(true, Ordering::Release);
        struct NeverSession;
        impl HidSession for NeverSession {
            fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
                panic!("native write")
            }
            fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
                panic!("native read")
            }
            fn send_feature(&mut self, _: &[u8]) -> Result<(), ProviderError> {
                panic!("native feature")
            }
            fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
                panic!("native feature")
            }
        }
        let mut session = ControlSession {
            configuration: None,
            inner: Box::new(NeverSession),
            shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            permission: Some((permission.clone(), 0)),
            block_while_gaming: false,
            boost: None,
            notification_state: || false,
        };
        assert!(session.send_feature(&[0]).is_err());
        permission.suspended.store(false, Ordering::Release);
        assert!(session.write(&[0]).is_err());
        permission.desired_enabled.store(true, Ordering::Release);
        permission.acknowledged.store(2, Ordering::Release);
        session.permission = Some((permission, 2));
        session.block_while_gaming = true;
        session.notification_state = || true;
        assert!(
            session
                .send_feature(&[0])
                .unwrap_err()
                .message
                .contains("Close the game")
        );
    }
    #[test]
    fn verified_refresh_after_partial_change_resets_estimate_without_claiming_partial_success() {
        let mut current = PollingRate::try_from(1000).unwrap();
        let mut observed = BTreeMap::new();
        let read = simulate_control(
            &control_request(ControlAction::Read, 0),
            &mut current,
            0,
            10,
        );
        assert!(changed_configuration(&read, &mut observed));
        let mut partial = simulate_control(
            &control_request(
                ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
                0,
            ),
            &mut current,
            0,
            11,
        );
        partial.failure = Some("second SET acknowledgment failed".into());
        assert!(changed_configuration(&partial, &mut observed));
        assert_eq!(observed["simulated:mouse"].hz(), 1000);
        let confirmed = simulate_control(
            &control_request(ControlAction::Read, 0),
            &mut current,
            0,
            12,
        );
        assert!(changed_configuration(&confirmed, &mut observed));
        assert!(!changed_configuration(&confirmed, &mut observed));
    }
    #[test]
    fn settings_survive_suspend_resume_and_full_mailboxes_shutdown() {
        // WinRT workers own native windows; serialize process-wide handle audits.
        let _native_guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::start(
            d.path().to_owned(),
            Settings {
                polling_controls: true,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        runtime.send(Command::Settings(Settings {
            polling_controls: false,
            low: 17,
            ..Default::default()
        }));
        runtime.send(Command::Suspend);
        runtime.send(Command::Resume);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            while runtime.events.try_recv().is_ok() {}
            if d.path().join("config.json").exists() {
                let s = hb_storage::load_settings(&d.path().join("config.json"));
                if s.low == 17 && !s.polling_controls {
                    break;
                }
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            runtime
                .submit_control(control_request(ControlAction::Read, 0))
                .is_err()
        );
        // Artificially fill both queues. Stop must drain events before waiting
        // for a Quit slot; its shared cancellation mailbox ends the owner loop.
        while runtime.sink.try_send(Event::Error(String::new())).is_ok() {}
        while runtime.commands.try_send(Command::Refresh).is_ok() {}
        let started = Instant::now();
        runtime.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn simulation_rejects_unadvertised_rate_without_changing_current() {
        let mut current = PollingRate::try_from(1000).unwrap();
        let result = simulate_control(
            &control_request(ControlAction::Apply(PollingRate::try_from(250).unwrap()), 0),
            &mut current,
            0,
            10,
        );
        assert!(result.failure.is_some());
        assert!(!result.may_have_changed);
        assert_eq!(current.hz(), 1000);
    }
    #[test]
    fn background_simulation_reads_actual_rate_without_applying_saved_selection() {
        // WinRT workers own native windows; serialize process-wide handle audits.
        let _native_guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        settings
            .devices
            .entry("simulated:mouse".into())
            .or_default()
            .requested_polling_rate = PollingRate::try_from(8000).ok();
        let mut runtime = Runtime::start(d.path().to_owned(), settings, true).unwrap();
        loop {
            if let Event::Polling(outcome) = runtime
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                assert_ne!(outcome.request & STARTUP_POLLING_READ_BIT, 0);
                assert!(outcome.failure.is_none());
                assert!(!outcome.may_have_changed);
                assert_eq!(outcome.observation.unwrap().rate.unwrap().hz(), 1000);
                break;
            }
        }
        runtime.send(Command::Refresh);
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if let Ok(Event::Polling(_)) = runtime.events.recv_timeout(Duration::from_millis(20)) {
                panic!("initial rate query must not repeat after refresh");
            }
        }
        runtime.stop();
    }
    #[test]
    fn status_stays_absent_when_disabled_and_shutdown_drains_queues() {
        // WinRT workers own native windows; serialize process-wide handle audits.
        let _native_guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::start(d.path().to_owned(), Settings::default(), true).unwrap();
        loop {
            if let Event::Snapshot(snapshot) = runtime
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                && !snapshot.devices.is_empty()
            {
                break;
            }
        }
        runtime.stop();
        assert!(!d.path().join("status.json").exists());
        let s = hb_storage::Store::read_only(&d.path().join("history.db")).unwrap();
        assert!(
            !s.query("simulated:mouse", 0, i64::MAX / 2, 100)
                .unwrap()
                .is_empty()
        );
    }
    fn storage(folder: PathBuf) -> (Sender<Storage>, Receiver<Event>, JoinHandle<()>) {
        let (tx, rx) = bounded(32);
        let (events, output) = bounded(32);
        let (boot, ready) = bounded(1);
        let sink = Events {
            tx: events,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let worker = thread::spawn(move || crate::storage_worker::run(folder, rx, boot, sink));
        let _ = ready.recv_timeout(Duration::from_secs(5)).unwrap();
        (tx, output, worker)
    }
    #[test]
    fn database_failure_preserves_configuration_and_answers_queries() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("history.db"), b"invalid SQLite database").unwrap();
        let (tx, events, worker) = storage(d.path().to_owned());
        tx.send(Storage::Save(Settings {
            low: 17,
            ..Default::default()
        }))
        .unwrap();
        tx.send(Storage::Status(Snapshot::default(), true)).unwrap();
        tx.send(Storage::History("mouse".into(), 0, 100, 50, 9))
            .unwrap();
        loop {
            if let Event::History(9, result) = events.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                assert!(result.is_err());
                break;
            }
        }
        tx.send(Storage::Quit).unwrap();
        worker.join().unwrap();
        assert_eq!(
            hb_storage::load_settings(&d.path().join("config.json")).low,
            17
        );
        assert!(d.path().join("status.json").exists());
        assert_eq!(
            std::fs::read(d.path().join("history.db")).unwrap(),
            b"invalid SQLite database"
        );
    }
    #[test]
    fn history_queries_recover_deferred_samples_without_waiting_for_the_minute_retry() {
        for query in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("history.db");
            std::fs::create_dir(&database).unwrap();
            let (tx, events, worker) = storage(directory.path().to_owned());
            let mut reading = Reading::new("mouse", "Mouse", "test", 100);
            reading.level = Some(80);
            reading.charging = Some(false);
            tx.send(Storage::unconfirmed(vec![reading])).unwrap();
            tx.send(Storage::History("mouse".into(), 0, 100, 40, 99))
                .unwrap();
            loop {
                if let Event::History(99, result) =
                    events.recv_timeout(Duration::from_secs(5)).unwrap()
                {
                    assert!(result.is_err());
                    break;
                }
            }
            std::fs::remove_dir(&database).unwrap();
            tx.send(match query {
                0 => Storage::History("mouse".into(), 0, 100, 40, 1),
                1 => Storage::UsageHistory("mouse".into(), 3600, 100, 40, 1),
                _ => Storage::Insights("mouse".into(), 100, 1),
            })
            .unwrap();
            loop {
                match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                    Event::History(1, result) => {
                        let series = result.unwrap();
                        assert_eq!(series.samples.len(), 1);
                        assert_eq!(series.samples[0].reading.level, Some(80));
                        break;
                    }
                    Event::Insights(1, result) => {
                        assert_eq!(result.unwrap().coverage.observation_count, 1);
                        break;
                    }
                    _ => {}
                }
            }
            tx.send(Storage::Quit).unwrap();
            worker.join().unwrap();
        }
    }
    #[test]
    fn usage_history_flushes_pending_rows_and_preserves_real_timestamps() {
        let d = tempfile::tempdir().unwrap();
        let (tx, events, worker) = storage(d.path().to_owned());
        for (timestamp, level) in [(100, 80), (160, 79), (220, 78)] {
            let mut reading = Reading::new("mouse", "Mouse", "razer", timestamp);
            reading.level = Some(level);
            reading.charging = Some(false);
            tx.send(Storage::unconfirmed(vec![reading])).unwrap();
        }
        tx.send(Storage::UsageHistory("mouse".into(), 3600, 220, 40, 11))
            .unwrap();
        loop {
            if let Event::History(11, result) = events.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                let series = result.unwrap();
                assert_eq!(series.axis, HistoryAxis::Usage);
                assert_eq!(series.until, 120);
                assert_eq!(series.samples.first().unwrap().reading.timestamp, 100);
                assert_eq!(series.samples.last().unwrap().reading.timestamp, 220);
                assert_eq!(series.samples.last().unwrap().position, 120);
                break;
            }
        }
        tx.send(Storage::Quit).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn queued_history_flushes_and_status_can_be_disabled() {
        let d = tempfile::tempdir().unwrap();
        let (tx, events, worker) = storage(d.path().to_owned());
        let mut r = Reading::new("mouse", "Mouse", "razer", 100);
        r.level = Some(80);
        tx.send(Storage::unconfirmed(vec![r])).unwrap();
        tx.send(Storage::Status(Snapshot::default(), true)).unwrap();
        tx.send(Storage::RemoveStatus).unwrap();
        tx.send(Storage::History("mouse".into(), 0, 100, 50, 10))
            .unwrap();
        loop {
            if let Event::History(10, result) = events.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                assert_eq!(result.unwrap().samples[0].reading.level, Some(80));
                break;
            }
        }
        tx.send(Storage::Quit).unwrap();
        worker.join().unwrap();
        assert!(!d.path().join("status.json").exists());
        let store = hb_storage::Store::read_only(&d.path().join("history.db")).unwrap();
        assert_eq!(store.query("mouse", 0, 100, 50).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod information_guard_tests {
    use super::*;
    struct Session {
        calls: Arc<AtomicUsize>,
        cancel_on_read: Option<Arc<AtomicBool>>,
    }
    impl HidSession for Session {
        fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
            panic!("Output report reached device")
        }
        fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
            panic!("Input report reached device")
        }
        fn send_feature(&mut self, _: &[u8]) -> Result<(), ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(flag) = &self.cancel_on_read {
                flag.store(true, Ordering::Release);
            }
            Ok(vec![0; 91])
        }
    }
    fn access() -> InfoReadAccess {
        let configuration = Arc::new(ConfigurationWatch::default());
        configuration.set_visible(true);
        InfoReadAccess {
            shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            permission: Arc::new(ControlPermission::new(false)),
            visible: configuration.epoch.load(Ordering::Acquire),
            configuration,
        }
    }
    #[test]
    fn information_reads_ignore_write_permission_and_refuse_configuration_packets() {
        let access = access();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(!access.permission.enabled.load(Ordering::Acquire));
        let mut session = InfoSession {
            inner: Box::new(Session {
                calls: calls.clone(),
                cancel_on_read: None,
            }),
            access,
        };
        session
            .send_feature(&hb_providers::device_info::firmware_query())
            .unwrap();
        let settings = hb_providers::razer_controls::request(
            hb_providers::razer_controls::Protocol::Extended,
            Some((2000, 0)),
        )
        .unwrap();
        assert!(session.send_feature(&settings).is_err());
        assert!(session.write(&[1, 2]).is_err());
        assert!(session.read(64, Duration::ZERO).is_err());
        assert!(session.feature(1, 91).is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn information_reads_stop_on_cancel_suspend_navigation_and_shutdown() {
        for scenario in 0..4 {
            let access = access();
            let calls = Arc::new(AtomicUsize::new(0));
            match scenario {
                0 => access.cancelled.store(true, Ordering::Release),
                1 => access.permission.suspended.store(true, Ordering::Release),
                2 => {
                    access.configuration.set_visible(false);
                }
                _ => access.shutdown.store(true, Ordering::Release),
            };
            let mut session = InfoSession {
                inner: Box::new(Session {
                    calls: calls.clone(),
                    cancel_on_read: None,
                }),
                access,
            };
            assert!(
                session
                    .send_feature(&hb_providers::device_info::firmware_query())
                    .is_err()
            );
            assert_eq!(calls.load(Ordering::Relaxed), 0);
        }
    }
    #[test]
    fn information_read_rechecks_cancellation_after_a_native_response() {
        let access = access();
        let calls = Arc::new(AtomicUsize::new(0));
        let cancel = access.cancelled.clone();
        let mut session = InfoSession {
            inner: Box::new(Session {
                calls: calls.clone(),
                cancel_on_read: Some(cancel),
            }),
            access,
        };
        assert!(session.feature(0, 91).is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod audit_shutdown_and_saver_tests {
    use super::*;
    #[test]
    fn stalled_worker_wait_has_a_deadline_and_normal_workers_still_join() {
        let (tx, rx) = bounded::<()>(1);
        let worker = thread::spawn(move || rx.recv().unwrap());
        let start = Instant::now();
        assert!(!wait_for_thread(
            &worker,
            start + Duration::from_millis(20),
            || {}
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        tx.send(()).unwrap();
        assert!(wait_for_thread(
            &worker,
            Instant::now() + Duration::from_secs(1),
            || {}
        ));
        worker.join().unwrap();
    }
    #[test]
    fn saver_actions_are_recorded_separately_from_manual_and_boost_changes() {
        let mut tracker = UsageTracker::new(0, true);
        let mut r = Reading::new("razer:00bf:TEST-UNIT", "Mouse", "razer", 100);
        r.kind = "mouse".into();
        r.via = "usb".into();
        let request = ControlRequest {
            request: (1 << 63) | crate::battery_saver::REQUEST_BIT | 1,
            target: ControlTarget {
                device: ConfigurationDevice::from_reading(&r),
                generation: 0,
                connection_stamp: None,
            },
            action: ControlAction::Apply(PollingRate::try_from(125).unwrap()),
        };
        for (restore, kind) in [
            (false, PollingActivityKind::Saver),
            (true, PollingActivityKind::SaverRestore),
        ] {
            tracker.begin_saver_control(&request, restore);
            assert_eq!(tracker.actions[&request.request].0, kind);
        }
    }
}
