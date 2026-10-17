//! Engine and storage ownership. Workers move providers, never share mutable state.
use crossbeam_channel::{Receiver, Sender, bounded, select};
use hb_core::*;
use hb_windows::{BluetoothProvider, ControllerProvider, WindowsHid};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
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
    DpiBackup(Box<DpiBackup>, Sender<Result<(), ProviderError>>),
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
    Dpi(Box<DpiRequest>, Arc<AtomicBool>, u64, u64),
    Sleep(Box<SleepRequest>, Arc<AtomicBool>, u64, u64),
    Battery(Job),
    Polling(
        Box<ControlRequest>,
        Arc<AtomicBool>,
        u64,
        Option<u64>,
        Option<crate::boost::Guard>,
    ),
    Configuration(Arc<ConfigurationWatch>, u64, u64),
}
enum WorkerCompleted {
    Dpi(Box<DpiOutcome>, u64, u64),
    Sleep(Box<SleepOutcome>, u64, u64),
    Battery(Completed),
    Polling(Box<ControlOutcome>, u64, Option<u64>),
    Configuration(u64, u64, Result<Vec<ConfigurationDevice>, ProviderError>),
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
fn update_diagnostics(
    diagnostics: &mut BTreeMap<String, Vec<String>>,
    key: String,
    value: Vec<String>,
) -> bool {
    if diagnostics.get(&key) == Some(&value) {
        return false;
    }
    diagnostics.insert(key, value);
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
/// Availability is an event at detection time, not a correction to an old poll.
fn availability_boundary(mut reading: Reading, state: Connection, now: i64) -> Reading {
    reading.timestamp = if now == reading.timestamp {
        now.saturating_add(1)
    } else {
        now
    };
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
    gates: BTreeMap<u16, Mutex<()>>,
    permission: Arc<ControlPermission>,
    configuration: Arc<ConfigurationWatch>,
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
        if self.commands.try_send(command).is_err() {
            let _ = self.sink.try_send(Event::Error("Command queue is busy; retry the action. Device configuration remains revoked until settings or resume is acknowledged.".into()));
        }
    }
    pub fn configuration_visible(&self) -> bool {
        self.configuration.visible.load(Ordering::Acquire)
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
        if let Some(t) = self.thread.take() {
            while !t.is_finished() {
                while self.events.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(10));
            }
            let _ = t.join();
        }
    }
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
                    WorkerJob::Sleep(request, _, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::Dpi(request, _, _, _) => {
                        control_vendors(&request.target.device.source)
                            .into_iter()
                            .collect()
                    }
                    WorkerJob::Polling(request, _, _, _, _) => {
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
                let _guards = vendors
                    .iter()
                    .filter_map(|v| access.gates.get(v))
                    .map(|m| m.lock().unwrap_or_else(|p| p.into_inner()))
                    .collect::<Vec<_>>();
                let completed = match job {
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
                    WorkerJob::Sleep(request, request_cancel, epoch, visible) => {
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
                        let outcome =
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
                            });
                        WorkerCompleted::Sleep(Box::new(outcome), epoch, visible)
                    }

                    WorkerJob::Battery(job) => {
                        WorkerCompleted::Battery(execute_job(job, &*hid, &clock, &cancel))
                    }
                    WorkerJob::Polling(
                        request,
                        request_cancel,
                        epoch,
                        configuration_epoch,
                        boost,
                    ) => WorkerCompleted::Polling(
                        Box::new(execute_control_inner(
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
                        )),
                        epoch,
                        configuration_epoch,
                    ),
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

/// Bounded startup readback recovery using existing discovery and workers.
/// Saved requests never count as readback evidence; retries only issue GETs.
struct InitialReadAttempt {
    request: u64,
    attempts: u8,
    retry_at: Option<Instant>,
}
struct InitialPollingReads {
    until: Instant,
    enabled: bool,
    attempted: BTreeSet<String>,
    pending: BTreeMap<String, InitialReadAttempt>,
    next_request: u64,
}
impl InitialPollingReads {
    fn new(settings: &Settings, now: Instant) -> Self {
        Self {
            until: now + Duration::from_secs(300),
            enabled: settings.polling_controls,
            attempted: BTreeSet::new(),
            pending: BTreeMap::new(),
            next_request: STARTUP_POLLING_REQUEST_BIT | STARTUP_POLLING_READ_BIT,
        }
    }
    fn cancel(&mut self) {
        self.enabled = false;
        self.attempted.clear();
        self.pending.clear();
    }
    fn observe_request(&mut self, key: &str) {
        self.pending.remove(key);
        if self.enabled && self.attempted.len() < 512 {
            self.attempted.insert(key.into());
        }
    }
    fn complete(&mut self, outcome: &ControlOutcome, now: Instant) {
        if !self.enabled || now >= self.until {
            return;
        }
        let Some(pending) = self.pending.get_mut(&outcome.key) else {
            return;
        };
        if pending.request != outcome.request || pending.retry_at.is_some() {
            return;
        }
        let confirmed = outcome.failure.is_none()
            && outcome.observation.as_ref().and_then(|o| o.rate).is_some();
        if confirmed || pending.attempts >= 3 {
            self.pending.remove(&outcome.key);
        } else {
            pending.retry_at = Some(now + Duration::from_secs(15));
        }
    }
    fn next(
        &mut self,
        now: Instant,
        epoch: u64,
        generation: u64,
        mut devices: impl Iterator<Item = ConfigurationDevice>,
    ) -> Option<Command> {
        if now >= self.until {
            self.cancel();
        }
        if !self.enabled {
            return None;
        }
        let device = devices.find(|device| {
            device.online()
                && hb_providers::controls::polling_menu_candidate(device)
                && ((!self.attempted.contains(&device.key) && self.attempted.len() < 512)
                    || self
                        .pending
                        .get(&device.key)
                        .is_some_and(|attempt| attempt.retry_at.is_some_and(|due| now >= due)))
        })?;
        self.attempted.insert(device.key.clone());
        let attempts = self.pending.get(&device.key).map_or(1, |p| p.attempts + 1);
        self.pending.insert(
            device.key.clone(),
            InitialReadAttempt {
                request: self.next_request,
                attempts,
                retry_at: None,
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
        self.reads.enabled || !self.restore.pending.is_empty()
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
                if self.reads.enabled && self.reads.attempted.contains(&request.target.device.key) {
                    self.reads.pending.insert(
                        request.target.device.key.clone(),
                        InitialReadAttempt {
                            request: request.request,
                            attempts: 0,
                            retry_at: None,
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
            // UI request IDs use the low half of the sequence space.
            next_request: STARTUP_POLLING_REQUEST_BIT,
        }
    }
    fn cancel(&mut self) {
        self.pending.clear();
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
    Completed {
        provider: job.provider,
        result,
        epoch: job.epoch,
        generation: job.generation,
    }
}
// Cancellation is checked before every native HID exchange, including jobs that
// were already queued when settings, suspend or shutdown revoked permission.
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
    fn synchronize_clock(&mut self, now: i64) {
        if self.wall_clock.is_some_and(|previous| now < previous) {
            self.invalidate(self.generation, self.enabled);
        }
        self.wall_clock = Some(now);
    }
    fn samples(&mut self, readings: Vec<Reading>) -> Vec<UsageObservation> {
        readings
            .into_iter()
            .map(|reading| {
                let unavailable_transition = !reading.online()
                    && self.connections.get(&reading.key) != Some(&reading.connection);
                let revoked_rate = reading.connection == Connection::Stale
                    && self.rates.remove(&reading.key).is_some();
                if unavailable_transition || revoked_rate {
                    self.session = self.session.saturating_add(1);
                    self.set_device_session(&reading.key);
                }
                if !self.device_sessions.contains_key(&reading.key) {
                    self.set_device_session(&reading.key);
                }
                self.connections
                    .insert(reading.key.clone(), reading.connection.clone());
                let polling_rate = self
                    .rates
                    .get(&reading.key)
                    .filter(|o| {
                        let target = &o.target.device;
                        reading.timestamp >= o.timestamp
                            && reading.source == target.source
                            && reading.via == target.via
                            && reading.serial == target.serial
                            && reading.container == target.container
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
        self.device_sessions.insert(key.to_owned(), self.session);
    }
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
            startup_polling.cancel();
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
                for id in providers.keys() {
                    due.insert(id, Instant::now());
                }
            }
        }
        usage_tracker.synchronize(
            hid.generation(),
            engine.settings.polling_controls && !suspended,
        );
        usage_tracker.synchronize_clock(clock.unix());
        boost.observe_generation(hid.generation());
        if saver_generation != hid.generation() {
            saver.observe_generation(hid.generation());
        }
        if suspended || stop || !engine.settings.polling_controls {
            boost.clear();
            saver.pause();
        }
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
            let _ = storage.send(Storage::UsageSample(
                usage_tracker.samples(engine.readings()),
            ));
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
        if stop && inflight == 0 {
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
        let startup_command = if !startup_polling.active() {
            None
        } else {
            startup_polling.next(
                Instant::now(),
                permission.epoch.load(Ordering::Acquire),
                hid.generation(),
                engine
                    .readings()
                    .iter()
                    .filter(|reading| reading.kind == "mouse")
                    .map(ConfigurationDevice::from_reading)
                    .chain(configuration_devices.iter().cloned()),
                keyboard_epoch,
                simulate || !jobs.is_full(),
            )
        };
        let boost_command = if startup_command.is_none()
            && boost.needs_inventory(&engine.settings)
            && !suspended
            && !stop
            && inflight == 0
            && !startup_polling.blocks_automation(Instant::now())
            && !jobs.is_full()
            && commands.is_empty()
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
        match work {
            Work::Command(Ok(Command::Quit)) | Work::Command(Err(_)) => {
                stop = true;
                control_cancel.store(true, Ordering::Relaxed);
                cancel.store(true, Ordering::Relaxed);
            }
            Work::Command(Ok(Command::EpochSuspend(epoch)))
                if epoch == permission.epoch.load(Ordering::Acquire) =>
            {
                snapshots.changed = true;
                suspended = true;
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
                snapshots.changed = true;
                suspended = false;
                control_cancel = Arc::new(AtomicBool::new(!engine.settings.polling_controls));
                engine.resume();
                permission.enabled.store(
                    engine.settings.polling_controls
                        && permission.desired_enabled.load(Ordering::Acquire)
                        && !permission.suspended.load(Ordering::Acquire),
                    Ordering::Release,
                );
                permission.acknowledged.store(epoch, Ordering::Release);
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
            Work::Command(Ok(Command::NotificationFailed(n))) => {
                if failed_delivery.len() < 512 {
                    failed_delivery.insert(
                        (n.key.clone(), n.kind),
                        (Instant::now() + Duration::from_secs(30), n),
                    );
                }
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
                    update_diagnostics(&mut diagnostics, id.to_string(), r.provider.diagnostics());
                let observations = completed_observations(
                    engine.provider_readings(id),
                    &r.result,
                    clock.unix(),
                    current,
                );
                if current {
                    let inventory_confirmed = r.result.is_ok();
                    for n in engine.apply(id, r.result, clock.monotonic().as_secs_f64(), quiet) {
                        let _ = events.send(Event::Alert(n));
                    }
                    // Inspect the existing provider slice: no inventory clone,
                    // timer, or extra discovery is needed for disconnect safety.
                    // A communication failure only pauses writes. Successful
                    // discovery of a missing/changed unit revokes its originals.
                    if inventory_confirmed {
                        boost.observe_provider(id, engine.provider_readings(id));
                        saver.observe_provider(id, engine.provider_readings(id));
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
                    let _ = storage.send(Storage::UsageSample(usage_tracker.samples(observations)));
                }
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
                if saver_cancel.is_none() && request.seconds.is_some() {
                    saver.observe(
                        &engine.settings,
                        hid.generation(),
                        &engine.readings(),
                        clock.unix(),
                    );
                    saver.manual(&request.target.device.key, true);
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
                    match jobs.try_send(WorkerJob::Sleep(
                        Box::new(request.clone()),
                        saver_cancel.unwrap_or_else(|| control_cancel.clone()),
                        epoch,
                        visible,
                    )) {
                        Ok(()) => inflight += 1,
                        Err(_) => {
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
            Work::Completed(Ok(WorkerCompleted::Sleep(mut outcome, epoch, visible))) => {
                inflight = inflight.saturating_sub(1);
                if epoch != permission.epoch.load(Ordering::Acquire)
                    || !permission.enabled.load(Ordering::Acquire)
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
                    saver.observe(
                        &engine.settings,
                        hid.generation(),
                        &engine.readings(),
                        clock.unix(),
                    );
                    saver.manual(&request.target.device.key, false);
                    boost.manual(&request.target.device.key);
                }
                let boost_guard = boost.guard(request.request);
                usage_tracker
                    .begin_control(&request, boost_guard.as_ref().map(|guard| guard.fullscreen));
                if request.request & STARTUP_POLLING_REQUEST_BIT == 0 {
                    startup_polling
                        .reads
                        .observe_request(&request.target.device.key);
                }
                let allowed = (!automatic || boost_guard.is_some())
                    && (!saver_request || saver_cancel.is_some())
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
                    let outcome = ControlOutcome::failed(
                        &request,
                        "Enable polling controls and select an online device before configuring it",
                    );
                    usage_tracker.take_activity(&outcome);
                    let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                    saver_dirty = true;
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
                    startup_polling.reads.complete(&outcome, Instant::now());
                    if publish {
                        let _ = events.send(Event::Polling(Box::new(outcome)));
                    }
                } else {
                    match jobs.try_send(WorkerJob::Polling(
                        Box::new(request.clone()),
                        saver_cancel.unwrap_or_else(|| control_cancel.clone()),
                        epoch,
                        configuration_epoch,
                        boost_guard,
                    )) {
                        Ok(()) => inflight += 1,
                        Err(_) => {
                            let outcome = ControlOutcome::failed(
                                &request,
                                "Device workers are busy; retry Refresh or Apply",
                            );
                            usage_tracker.take_activity(&outcome);
                            let publish = finish_polling_outcome(&mut boost, &mut saver, &outcome);
                            saver_dirty = true;
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
            ))) => {
                inflight = inflight.saturating_sub(1);
                let permitted = epoch == permission.epoch.load(Ordering::Acquire)
                    && permission.enabled.load(Ordering::Acquire)
                    && permission.desired_enabled.load(Ordering::Acquire)
                    && permission.acknowledged.load(Ordering::Acquire) == epoch
                    && engine.settings.polling_controls
                    && !suspended
                    && !stop
                    && configuration_epoch.is_none_or(|epoch| configuration.active(epoch));
                validate_polling_completion(&mut outcome, hid.generation(), permitted);
                if changed_configuration(&outcome, &mut observed_rates) {
                    engine.reset_estimate(&outcome.key);
                    snapshots.changed = true;
                    let _ = storage.send(Storage::State(engine.estimator.clone()));
                }
                diagnostics_dirty |= update_diagnostics(
                    &mut diagnostics,
                    "polling_controls".into(),
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
    for w in workers {
        let _ = w.join();
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
                    saver.manual(&reading.key, sleep);
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
        assert_eq!(collision.timestamp, 101);
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
        fresh.timestamp = 101;
        fresh.level = Some(89);
        let resumed = tracker.samples(vec![fresh]).remove(0);
        assert_eq!(resumed.session, pause.session);
        let mut insights = InsightsBuilder::default();
        // SQLite's same-second replacement leaves only these two endpoints.
        insights.push(first);
        insights.push(resumed);
        assert_eq!(insights.finish().cycles[0].awake_seconds, 0);
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
    fn diagnostics_publish_only_changes_and_retry_after_backpressure() {
        let (tx, rx) = bounded(1);
        let events = Events {
            tx,
            window: Arc::new(AtomicUsize::new(0)),
        };
        let mut diagnostics = BTreeMap::new();
        let mut dirty = update_diagnostics(&mut diagnostics, "test".into(), vec!["ready".into()]);
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert!(!dirty);
        assert!(!update_diagnostics(
            &mut diagnostics,
            "test".into(),
            vec!["ready".into()]
        ));
        publish_diagnostics(&events, &diagnostics, &mut dirty);
        assert_eq!(rx.len(), 1);
        dirty |= update_diagnostics(&mut diagnostics, "test".into(), vec!["changed".into()]);
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
        assert!(!expired.enabled);
        assert!(expired.attempted.is_empty());
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
        let _ = _rx.recv().unwrap();
        runtime.request_insights("device".into(), 100, 2).unwrap();
        assert!(matches!(
            _rx.recv().unwrap(),
            Command::Insights { request: 2, .. }
        ));
        drop(_rx);
        assert!(runtime.request_insights("device".into(), 100, 3).is_err());
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
