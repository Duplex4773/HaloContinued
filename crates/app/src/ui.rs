//! A single UI thread owns all HWND, HICON and Direct2D resources.
use crate::{
    chart::Chart,
    dashboard_theme::{DashboardTheme, Palette, color_brush, rounded_surface},
    icons::{self, Icon},
    runtime::{Command, Event, Runtime},
};
use hb_core::*;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Devices::HumanInterfaceDevice::HidD_GetHidGuid,
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{
                DRAWITEMSTRUCT, EM_SETMARGINS, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED,
                ODS_GRAYED, ODS_NOACCEL, ODS_SELECTED, ODT_MENU, SetScrollInfo,
            },
            HiDpi::*,
            Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, PCWSTR, w},
};
const TRAY: u32 = WM_APP + 1;
const PAGE_TOP: i32 = 60;
const PAGE_FOOTER: i32 = 62;
const ICON_CHOICES: &[&str] = &[
    "Automatic",
    "Mouse",
    "Keyboard",
    "Headset",
    "Controller",
    "Bluetooth",
    "PlayStation 4",
    "PlayStation 5",
];
const THEME_CHOICES: &[&str] = &[
    "Automatic",
    "White icons",
    "Black icons",
    "Windows style",
    "Top bar",
];
fn providers() -> Vec<&'static str> {
    hb_providers::provider::FAMILIES
        .iter()
        .map(|p| p.0)
        .chain(["bluetooth", "xinput"])
        .collect()
}
fn dpi_limits_text(limits: Option<&DpiLimits>) -> String {
    fn number(value: u16) -> String {
        if value < 1000 {
            value.to_string()
        } else {
            format!("{},{:03}", value / 1000, value % 1000)
        }
    }
    fn axis(ranges: &[(u16, u16, u16)], model: bool) -> String {
        let mut ranges: Vec<_> = ranges
            .iter()
            .filter_map(|&(lo, hi, step)| {
                if step == 0 || hi < lo {
                    return None;
                }
                let first = u32::from(lo)
                    + u32::from(100u16.saturating_sub(lo)).div_ceil(u32::from(step))
                        * u32::from(step);
                let last = u32::from(lo) + u32::from((hi - lo) / step) * u32::from(step);
                (first <= last).then_some((first as u16, last as u16, step))
            })
            .collect();
        ranges.sort_unstable();
        ranges.dedup();
        let meaningful: Vec<_> = ranges
            .iter()
            .copied()
            .filter(|&(lo, hi, _)| {
                lo != hi
                    || !ranges.iter().any(|&(a, b, step)| {
                        a < b && lo >= a && lo <= b && (lo - a).is_multiple_of(step)
                    })
            })
            .collect();
        if meaningful.is_empty() {
            return "Not available".into();
        }
        if meaningful.len() > 4 || (meaningful.len() > 1 && meaningful.iter().any(|r| r.0 != r.1)) {
            let lo = meaningful.iter().map(|r| r.0).min().unwrap();
            let hi = meaningful.iter().map(|r| r.1).max().unwrap();
            return format!("{}–{} (selected values only)", number(lo), number(hi));
        }
        meaningful
            .iter()
            .map(|&(lo, hi, step)| {
                if lo == hi {
                    number(lo)
                } else if !model && step > 1 {
                    format!("{}–{} ({} DPI steps)", number(lo), number(hi), number(step))
                } else {
                    format!("{}–{}", number(lo), number(hi))
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
    let Some(limits) = limits else {
        return "Supported DPI: Select Read current to see the range.".into();
    };
    let x = axis(&limits.x, limits.from_model);
    let y = axis(&limits.y, limits.from_model);
    if x == y {
        format!(
            "Supported DPI: {x}{}",
            if limits.from_model {
                " (model range)"
            } else {
                ""
            }
        )
    } else {
        format!("Supported DPI: Horizontal {x}\r\nVertical {y}")
    }
}
fn dpi_input(text: &str, limits: Option<&DpiLimits>) -> Result<DpiValue, String> {
    let value = text
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| *value >= 100)
        .ok_or_else(|| "Enter a whole DPI value of at least 100.".to_owned())?;
    let accepts = |ranges: &[(u16, u16, u16)]| {
        ranges.iter().any(|&(lo, hi, step)| {
            step > 0 && value >= lo && value <= hi && (value - lo).is_multiple_of(step)
        })
    };
    if limits.is_some_and(|limits| !accepts(&limits.x) || !accepts(&limits.y)) {
        return Err(
            "Choose a DPI value that matches the supported range and steps shown above.".into(),
        );
    }
    Ok(DpiValue::uniform(value))
}
fn user_message(message: &str) -> &str {
    if message.is_empty()
        || message.starts_with("Enter ")
        || message.starts_with("Low battery alert ")
        || message == "Settings saved"
        || message == "Device settings saved"
        || message == "Device preferences reset"
        || message == "Automatic boost settings saved"
        || message == "Low-battery settings saved"
    {
        return message;
    }
    if message.starts_with("Exported ") {
        return "Support report saved in the app's data folder.";
    }
    if message.starts_with("History") {
        return "Couldn't load battery history. Select Refresh to try again.";
    }
    if message.starts_with("Storage") {
        return "Couldn't save this change. Try again or save a support report.";
    }
    if message.starts_with("Settings save:") {
        return "Settings couldn't be saved yet. The app will try again automatically.";
    }
    "Couldn't finish this action. Try again or save a support report."
}
fn polling_message(message: &str) -> &str {
    if message.starts_with("Hardware may have changed.") {
        return "Refresh rate to check. The change may have applied and will not be retried.";
    }
    if message.starts_with("Confirmed configured rate:")
        || message.starts_with("Rate set to ")
        || message.starts_with("Reading ")
        || message.starts_with("Applying ")
        || message.starts_with("Restoring ")
        || message.starts_with("Rate checked")
        || message == "Automatic fullscreen rate verified."
        || message.starts_with("Low-battery mode:")
    {
        return message;
    }
    if message.contains("Startup rate") || message.contains("startup rate verified") {
        return "The rate was checked when the app started.";
    }
    if message.contains("game") || message.contains("presentation") {
        return "Close the game or presentation before changing the rate.";
    }
    if message.contains("cancel") || message.contains("changed") || message.contains("generation") {
        return "The device changed or disconnected. Select Refresh rate before trying again.";
    }
    if message.contains("permission")
        || message.contains("disabled")
        || message.contains("unavailable for")
    {
        return "Allow device settings changes in Settings, then select Refresh rate.";
    }
    if message.contains("pending") || message.contains("progress") {
        return "A rate check or change is already in progress.";
    }
    if message.contains("unsupported") || message.contains("supported") {
        return "This connection does not support that rate. Refresh and choose another rate.";
    }
    if message.contains("not been read") || message.contains("needs verification") {
        return "Select Refresh rate to check the current setting.";
    }
    if message.contains("Hardware rate read") {
        return "Rate checked. Select Apply to change it.";
    }
    if message.contains("did not report") {
        return "This device did not report its current rate.";
    }
    "Couldn't confirm the rate. Wake the device and select Refresh rate."
}
fn device_control_error(message: &str, changed: bool, setting: &str, readable: bool) -> String {
    if changed {
        return if readable {
            format!("The {setting} may have changed. Select Read current before trying again.")
        } else {
            format!("The {setting} may have changed. Check the mouse before trying again.")
        };
    }
    let message = message.to_ascii_lowercase();
    let text = if message.contains("backup") || message.contains("recovery") {
        "Couldn't save the original settings. Check that the app's data folder is writable, then try again."
    } else if message.contains("game") || message.contains("presentation") {
        "Close the game or presentation before changing this setting."
    } else if message.contains("busy") {
        "The mouse is busy. Wait a moment, then try again."
    } else if message.contains("unsupported")
        || message.contains("not support")
        || message.contains("does not accept")
    {
        "This setting is not supported on the current connection."
    } else if message.contains("range")
        || message.contains("at least")
        || message.contains("duration")
    {
        "Choose a value within the supported range shown above."
    } else if message.contains("cancel")
        || message.contains("changed")
        || message.contains("profile")
        || message.contains("stage")
    {
        if readable {
            "The device or its settings changed. Read the current setting before trying again."
        } else {
            "The device or its settings changed. Reconnect the mouse before trying again."
        }
    } else if message.contains("read the current") || message.contains("read current") {
        "Select Read current before applying a change."
    } else if message.contains("permission") || message.contains("disabled") {
        "Turn on Allow device settings changes in Settings first."
    } else {
        "Couldn't contact the mouse. Wake or reconnect it, then try again."
    };
    text.into()
}
fn settings_number(text: &str, minimum: u64, maximum: u64) -> Option<u64> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| (minimum..=maximum).contains(n))
}
fn polling_unavailable(device: &ConfigurationDevice) -> &'static str {
    if device.source == "corsair" {
        "Rate changes are unavailable. This model needs software running continuously, which this app does not use."
    } else if matches!(&device.capability, PollingCapability::Unavailable(reason) if reason.contains("multiple"))
    {
        "More than one connection was found for this device. Reconnect it and select Refresh."
    } else {
        "This device's rate controls are unavailable. Reconnect it and select Refresh."
    }
}
const CHECKS: &[(&str, &str)] = &[
    ("notify", "Show low-battery notifications"),
    ("full_alert", "Alert when fully charged"),
    ("bluetooth", "Include Bluetooth devices"),
    ("animation", "Animate charging icons"),
    ("badges", "Show a charging symbol"),
    ("fluent_menu", "Show devices in the tray menu"),
    ("time_left", "Show estimated battery time left"),
    ("percent_in_icon", "Show battery percentage in icons"),
    (
        "quiet_fullscreen",
        "Pause popups during games and presentations",
    ),
    ("status_file", "Save battery information for other apps"),
    (
        "playstation_full_mode",
        "Allow extra PlayStation Bluetooth checks",
    ),
    ("update_check", "Check for updates (currently unavailable)"),
    (
        "low_sound",
        "Play a sound when battery is low (every 5 minutes)",
    ),
    ("keep_disconnected", "Keep disconnected icons until exit"),
];
fn setting_check_id(index: usize) -> u16 {
    // 112 and 113 belong to the separate polling permission controls.
    100 + index as u16 + if index >= 12 { 2 } else { 0 }
}
#[cfg(test)]
#[test]
fn low_sound_checkbox_keeps_polling_permission_controls_separate() {
    let index = CHECKS
        .iter()
        .position(|(key, _)| *key == "low_sound")
        .unwrap();
    assert_eq!(setting_check_id(index), 114);
    let ids: std::collections::BTreeSet<_> = (0..CHECKS.len()).map(setting_check_id).collect();
    assert_eq!(ids.len(), CHECKS.len());
    assert!(!ids.contains(&112));
    assert!(!ids.contains(&113));
    let settings = serde_json::to_value(Settings::default()).unwrap();
    assert_eq!(settings[CHECKS[index].0], false);
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayUpdate {
    Changed,
    Redraw,
    ExplorerRecovery,
}
#[derive(Default)]
struct TrayThemeCache {
    value: Option<(Instant, String, bool)>,
}
#[cfg(test)]
mod tray_theme_cache_tests {
    use super::*;
    #[test]
    fn caches_short_snapshot_bursts_but_rechecks_settings_recovery_and_expiry() {
        let mut cache = TrayThemeCache::default();
        let now = Instant::now();
        let calls = Cell::new(0);
        let sample = || {
            calls.set(calls.get() + 1);
            true
        };
        assert!(cache.read(now, "auto", false, sample));
        assert!(
            cache.read(now + Duration::from_millis(500), "auto", false, || panic!(
                "cached"
            ))
        );
        assert!(cache.read(now + Duration::from_secs(2), "auto", false, sample));
        assert!(cache.read(now + Duration::from_secs(2), "auto", true, sample));
        assert!(cache.read(now + Duration::from_secs(2), "black", false, sample));
        assert_eq!(calls.get(), 4);
    }
}
impl TrayThemeCache {
    fn read(
        &mut self,
        now: Instant,
        icon_theme: &str,
        force: bool,
        query: impl FnOnce() -> bool,
    ) -> bool {
        if !force
            && let Some((sampled, key, value)) = &self.value
            && key == icon_theme
            && now.saturating_duration_since(*sampled) < Duration::from_secs(2)
        {
            return *value;
        }
        let value = query();
        self.value = Some((now, icon_theme.to_owned(), value));
        value
    }
}
fn tray_message_update(message: u32, taskbar_created: u32) -> Option<TrayUpdate> {
    if taskbar_created != 0 && message == taskbar_created {
        Some(TrayUpdate::ExplorerRecovery)
    } else if message == WM_SETTINGCHANGE {
        Some(TrayUpdate::Redraw)
    } else {
        None
    }
}
#[derive(Default)]
struct TrayRegistration {
    registered: bool,
}
impl TrayRegistration {
    fn update_with(
        &mut self,
        data: &NOTIFYICONDATAW,
        update: TrayUpdate,
        changed: bool,
        notify: &mut impl FnMut(NOTIFY_ICON_MESSAGE, &NOTIFYICONDATAW) -> bool,
    ) {
        if !self.registered {
            self.registered = notify(NIM_ADD, data);
        } else if (changed || update != TrayUpdate::Changed)
            && !notify(NIM_MODIFY, data)
            && update == TrayUpdate::ExplorerRecovery
        {
            // Explorer may still own the GUID. Add only after it reports the
            // icon missing; deleting an existing icon loses its pinned placement.
            self.registered = notify(NIM_ADD, data);
        }
    }
    fn remove_with(
        &mut self,
        data: &NOTIFYICONDATAW,
        notify: &mut impl FnMut(NOTIFY_ICON_MESSAGE, &NOTIFYICONDATAW) -> bool,
    ) {
        if self.registered {
            let _ = notify(NIM_DELETE, data);
            self.registered = false;
        }
    }
}
fn shell_notify(command: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
    unsafe { Shell_NotifyIconW(command, data) }.as_bool()
}
struct Tray {
    registration: TrayRegistration,
    data: NOTIFYICONDATAW,
    frames: Vec<Icon>,
    signature: IconSignature,
    frame: usize,
}
impl Drop for Tray {
    fn drop(&mut self) {
        self.registration.remove_with(&self.data, &mut shell_notify);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollingIntent {
    Read,
    Apply { rate: PollingRate, restore: bool },
}
#[derive(Clone, Debug)]
struct PendingPolling {
    request: u64,
    key: String,
    device: ConfigurationDevice,
    intent: PollingIntent,
}
#[derive(Default)]
struct PollingUi {
    sequence: u64,
    generation: u64,
    pending: Option<PendingPolling>,
    observations: BTreeMap<String, PollingObservation>,
    previous: BTreeMap<String, PollingRate>,
    status: BTreeMap<String, String>,
}
impl PollingUi {
    fn observe_background(&mut self, outcome: &ControlOutcome) {
        // An automatic transaction supersedes prior readback, including when
        // the resulting hardware rate is uncertain or the connection changed.
        self.observations.remove(&outcome.key);
        let verified = outcome.observation.as_ref().filter(|observation| {
            outcome.failure.is_none()
                && observation.target.generation == self.generation
                && observation.target.device.key == outcome.key
                && observation.rate.is_some()
        });
        if let Some(observation) = verified {
            self.observations
                .insert(outcome.key.clone(), observation.clone());
            if outcome.may_have_changed
                && let Some(previous) = outcome.previous
            {
                self.previous.insert(outcome.key.clone(), previous);
            }
        } else {
            self.previous.remove(&outcome.key);
        }
    }
    fn revoke_observation(&mut self, key: &str) {
        self.observations.remove(key);
        self.status
            .insert(key.into(), "Current rate needs verification.".into());
    }
    fn abandon(&mut self) {
        self.pending = None;
    }
    fn invalidate(&mut self, generation: u64) {
        if generation < self.generation {
            return;
        }
        self.generation = generation;
        self.observations.clear();
        self.previous.clear();
        self.status.clear();
        self.pending = None;
    }
    #[cfg(test)]
    fn retain_devices(&mut self, devices: &[ConfigurationDevice]) {
        self.retain_inventory(&[], devices);
    }
    fn retain_inventory(
        &mut self,
        batteries: &[DeviceView],
        configuration: &[ConfigurationDevice],
    ) {
        let mut keys = std::collections::BTreeSet::new();
        for key in batteries
            .iter()
            .map(|d| d.reading.key.as_str())
            .chain(configuration.iter().map(|d| d.key.as_str()))
        {
            if keys.len() < 512 {
                keys.insert(key);
            }
        }
        self.observations.retain(|key, observation| {
            keys.contains(key.as_str())
                && inventory_contains_device(batteries, configuration, &observation.target.device)
        });
        self.previous.retain(|key, _| keys.contains(key.as_str()));
        self.status.retain(|key, _| keys.contains(key.as_str()));
        if self.pending.as_ref().is_some_and(|pending| {
            !inventory_contains_device(batteries, configuration, &pending.device)
        }) {
            self.abandon();
        }
    }
    fn begin(
        &mut self,
        reading: ConfigurationDevice,
        intent: PollingIntent,
    ) -> Result<ControlRequest, &'static str> {
        self.begin_inner(reading, intent, false)
    }
    fn begin_tray(
        &mut self,
        reading: ConfigurationDevice,
        intent: PollingIntent,
    ) -> Result<ControlRequest, &'static str> {
        self.begin_inner(reading, intent, true)
    }
    fn begin_inner(
        &mut self,
        reading: ConfigurationDevice,
        intent: PollingIntent,
        tray: bool,
    ) -> Result<ControlRequest, &'static str> {
        if self.pending.is_some() {
            return Err("A device request is already pending");
        }
        if !reading.online() || !matches!(reading.capability, PollingCapability::ReadWrite) {
            return Err("Polling configuration is unavailable for this device");
        }
        let (target, action) = match intent {
            PollingIntent::Read => (
                ControlTarget {
                    device: reading,
                    generation: 0,
                    connection_stamp: None,
                },
                ControlAction::Read,
            ),
            PollingIntent::Apply { rate, restore } => {
                let observation = self.observations.get(&reading.key).filter(|o| {
                    o.target.device == reading && o.target.generation == self.generation
                });
                if let Some(observation) = observation {
                    if !observation.supported.contains(&rate)
                        || (observation.rate.is_none() && (!tray || restore))
                    {
                        return Err("Select a rate supported by this connection");
                    }
                    (observation.target.clone(), ControlAction::Apply(rate))
                } else if tray
                    && !restore
                    && hb_providers::controls::polling_menu_rates(&reading).contains(&rate.hz())
                {
                    // This is an explicit user selection, not an observed rate.
                    // Existing controllers resolve exact hardware capabilities,
                    // GET the current value, then SET and verify in one job.
                    (
                        ControlTarget {
                            device: reading,
                            generation: self.generation,
                            connection_stamp: None,
                        },
                        ControlAction::Apply(rate),
                    )
                } else {
                    return Err("Refresh the hardware rate and select a supported value");
                }
            }
        };
        self.sequence =
            (self.sequence.wrapping_add(1) & !crate::runtime::STARTUP_POLLING_REQUEST_BIT).max(1);
        self.pending = Some(PendingPolling {
            request: self.sequence,
            key: target.device.key.clone(),
            device: target.device.clone(),
            intent,
        });
        self.status.insert(
            target.device.key.clone(),
            match intent {
                PollingIntent::Read => "Reading the device's rate…".into(),
                PollingIntent::Apply { restore: true, .. } => "Restoring the previous rate…".into(),
                _ => "Applying and checking the new rate…".into(),
            },
        );
        Ok(ControlRequest {
            request: self.sequence,
            target,
            action,
        })
    }
    fn accept(&mut self, outcome: &ControlOutcome, selected: Option<&str>) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.request != outcome.request
            || pending.key != outcome.key
            || selected != Some(outcome.key.as_str())
        {
            return false;
        }
        let pending = self.pending.take().unwrap();
        if let Some(observation) = &outcome.observation {
            if observation.target.device != pending.device
                || observation.target.device.key != outcome.key
                || observation.target.generation < self.generation
            {
                self.observations.remove(&outcome.key);
                self.status.insert(
                    outcome.key.clone(),
                    "Device configuration changed; Refresh before applying".into(),
                );
                return true;
            }
            self.generation = observation.target.generation;
            self.observations
                .insert(outcome.key.clone(), observation.clone());
        } else {
            self.observations.remove(&outcome.key);
        }
        if let PollingIntent::Apply { restore: false, .. } = pending.intent
            && let Some(previous) = outcome.previous
        {
            // Keep the verified before-value for this explicit operation. A no-op
            // must not discard the recovery value from an earlier change.
            if let PollingIntent::Apply { rate, .. } = pending.intent
                && (rate != previous || outcome.failure.is_some())
            {
                self.previous.insert(outcome.key.clone(), previous);
            }
        }
        let observed = outcome.observation.as_ref().and_then(|o| o.rate);
        let message = if let Some(failure) = &outcome.failure {
            if outcome.may_have_changed {
                format!("Hardware may have changed. {failure} Refresh to verify.")
            } else {
                failure.clone()
            }
        } else {
            match pending.intent {
                PollingIntent::Read if observed.is_some() => {
                    "Rate checked. Select Apply to change it.".into()
                }
                PollingIntent::Read => "Hardware did not report a polling rate.".into(),
                PollingIntent::Apply { rate, .. } if observed == Some(rate) => {
                    format!("Rate set to {} Hz.", rate.hz())
                }
                _ => "Change was not verified. Refresh the hardware rate before continuing.".into(),
            }
        };
        self.status.insert(outcome.key.clone(), message);
        true
    }
}
fn timestamp_utc(timestamp: i64) -> Option<SYSTEMTIME> {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1601..=30827).contains(&year) {
        return None;
    }
    Some(SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDay: day as u16,
        wHour: (seconds / 3600) as u16,
        wMinute: (seconds / 60 % 60) as u16,
        wSecond: (seconds % 60) as u16,
        ..Default::default()
    })
}
fn timestamp_local(timestamp: i64) -> Option<SYSTEMTIME> {
    let utc = timestamp_utc(timestamp)?;
    let mut local = SYSTEMTIME::default();
    unsafe {
        windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime(None, &utc, &mut local)
            .ok()?;
    }
    Some(local)
}
fn month_name(month: u16) -> &'static str {
    [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .get(month.saturating_sub(1) as usize)
    .copied()
    .unwrap_or("")
}
fn polling_timestamp(timestamp: i64) -> String {
    timestamp_local(timestamp)
        .map(|date| {
            format!(
                "{} {} {}, {:02}:{:02}",
                date.wDay,
                month_name(date.wMonth),
                date.wYear,
                date.wHour,
                date.wMinute
            )
        })
        .unwrap_or_else(|| "Date unavailable".into())
}
struct HistorySelection {
    axis: HistoryAxis,
    usage_index: usize,
    calendar_index: usize,
}
impl Default for HistorySelection {
    fn default() -> Self {
        Self {
            axis: HistoryAxis::Usage,
            usage_index: 2,
            calendar_index: 0,
        }
    }
}
impl HistorySelection {
    fn index(&self) -> usize {
        match self.axis {
            HistoryAxis::Usage => self.usage_index,
            HistoryAxis::Calendar => self.calendar_index,
        }
    }
    fn set_index(&mut self, index: usize) {
        match self.axis {
            HistoryAxis::Usage => self.usage_index = index.min(2),
            HistoryAxis::Calendar => self.calendar_index = index.min(2),
        }
    }
    fn labels(&self) -> Vec<String> {
        match self.axis {
            HistoryAxis::Usage => ["2 hours used", "8 hours used", "24 hours used"],
            HistoryAxis::Calendar => ["24 hours", "7 days", "30 days"],
        }
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    fn description(&self) -> &'static str {
        match self.axis {
            HistoryAxis::Usage => {
                "Estimated time used · Pauses during sleep, charging or disconnection"
            }
            HistoryAxis::Calendar => {
                "Calendar time · Shows the last known battery level between readings"
            }
        }
    }
    fn command(&self, key: String, until: i64, width: usize, request: u64) -> Command {
        match self.axis {
            HistoryAxis::Usage => Command::UsageHistory {
                key,
                seconds: [2, 8, 24][self.index()] * 3600,
                until,
                width,
                request,
            },
            HistoryAxis::Calendar => Command::History {
                key,
                since: until - [1, 7, 30][self.index()] * 86400,
                until,
                width,
                request,
            },
        }
    }
}
#[derive(Default)]
struct InsightsUi {
    sequence: u64,
    pending: Option<(u64, String)>,
    key: Option<String>,
    data: Option<BatteryInsights>,
    status: String,
}
impl InsightsUi {
    fn begin(&mut self, key: &str) -> Option<u64> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| pending == key)
        {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.pending = Some((self.sequence, key.into()));
        Some(self.sequence)
    }
    fn abandon(&mut self) {
        self.pending = None;
    }
    fn select(&mut self, key: Option<String>) {
        if self.key != key {
            self.abandon();
            self.key = key;
            self.data = None;
            self.status.clear();
        }
    }
    fn accept(
        &mut self,
        request: u64,
        selected: Option<&str>,
        result: Result<BatteryInsights, ProviderError>,
    ) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|(id, key)| *id == request && selected == Some(key.as_str()))
        {
            return false;
        }
        self.pending = None;
        match result {
            Ok(data) => {
                self.status = insight_coverage_text(&data);
                self.data = Some(data);
            }
            Err(_) => {
                self.data = None;
                self.status = "Couldn't load battery insights. Select Refresh to try again.".into();
            }
        }
        true
    }
}
const INSIGHTS_EMPTY: &str = "To compare rates, turn on Allow device settings changes in Settings, then select Refresh rate in Devices or the tray menu. Only rates confirmed by the device are included.";
fn insight_coverage_text(data: &BatteryInsights) -> String {
    let c = &data.coverage;
    format!(
        "History through {} · {} of estimated use in the last 30 days. {}",
        c.last_reading_timestamp
            .map(polling_timestamp)
            .unwrap_or_else(|| "no saved readings".into()),
        insight_hours(c.awake_seconds),
        if c.unreadable_row_count > 0 {
            "Some saved readings could not be used."
        } else {
            "Sleeping, charging and gaps are excluded."
        }
    )
}
fn insight_empty_text(data: Option<&BatteryInsights>, supports_polling: bool) -> String {
    if !supports_polling {
        return "Polling-rate comparisons are not available for this device. Its recorded battery use appears under Recent battery sessions.".into();
    }
    let Some(data) = data else {
        return INSIGHTS_EMPTY.into();
    };
    let explanation = if data.coverage.observation_count == 0 {
        "No battery history yet. Keep the device connected while using it."
    } else if data.coverage.discharge_sample_count == 0 {
        "More time on battery is needed to build an estimate. Sleeping and charging do not count."
    } else {
        "Battery history is available. Check the device's current rate to start a comparison."
    };
    format!("{explanation}\r\n\r\n{INSIGHTS_EMPTY}")
}
fn insight_hours(seconds: u64) -> String {
    if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else {
        format!("{:.1} hours", seconds as f64 / 3600.0)
    }
}
fn insight_estimate(hours: Option<f64>) -> String {
    hours
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| {
            if value < 1.0 {
                "less than 1 hour".into()
            } else if value.round() == 1.0 {
                "about 1 hour".into()
            } else {
                format!("about {value:.0} hours")
            }
        })
        .unwrap_or_else(|| "Keep using the device to build an estimate".into())
}
fn insight_missing_rate_data(
    data: Option<&BatteryInsights>,
    prediction: &AdaptivePrediction,
) -> bool {
    data.is_some_and(|d| d.recent.total_seconds >= 1800)
        && prediction.full_hours.is_none()
        && prediction.coverage_percent < 80.0
}
fn insight_prediction_explanation(
    data: Option<&BatteryInsights>,
    prediction: &AdaptivePrediction,
    controls_enabled: bool,
) -> String {
    match data {
        None => "Keep using your device to build battery insights.".into(),
        Some(d) if d.coverage.observation_count == 0 => {
            "No battery history yet. Keep your device connected while using it.".into()
        }
        Some(_) if prediction.full_hours.is_some() && prediction.remaining_hours.is_none() => {
            "Waiting for a fresh battery and rate reading to estimate time left.".into()
        }
        Some(_) if prediction.full_hours.is_some() => {
            "Your estimate follows your recent mix of everyday, boosted and low-battery use.".into()
        }
        Some(_) if insight_missing_rate_data(data, prediction) => {
            let recovery = if controls_enabled {
                "Rate checks resume when your mouse wakes or its connection changes."
            } else {
                "Turn on Allow device settings changes in Settings to resume rate checks."
            };
            format!(
                "Polling rate was confirmed for {:.0}% of recent use (80% needed). {recovery} Older unknown periods stay excluded.",
                prediction.coverage_percent
            )
        }
        Some(_) if !prediction.missing_rates.is_empty() => format!(
            "Still learning at {}. See progress under Battery life by rate.",
            prediction.missing_rates.iter().map(|r| format!("{r} Hz")).collect::<Vec<_>>().join(", ")
        ),
        Some(_) => "Estimates need at least 30 minutes of use with a confirmed polling rate. Keep using your device on battery.".into(),
    }
}
fn rate_insight_text(rate: &RateInsight) -> String {
    let estimate = rate
        .projected_full_charge_hours
        .filter(|h| h.is_finite() && *h > 0.0);
    let remaining = rate.remaining_hours.filter(|h| h.is_finite() && *h >= 0.0);
    let explanation = if rate.drop_count == 0 {
        "No battery drop recorded yet at this rate."
    } else if rate.projection_seconds == 0 {
        "Waiting for two drops in an uninterrupted period."
    } else {
        "Learning combines separate sessions. Rate changes and gaps are excluded."
    };
    let evidence = if estimate.is_some() {
        format!(
            "Estimate based on · {} of battery use\nBattery used in that time · {}%",
            insight_hours(rate.projection_seconds),
            rate.projection_consumed_percent
        )
    } else {
        format!(
            "Learning time · {} / 30 min\nBattery used · {} / 3 percentage points",
            rate.projection_seconds / 60,
            rate.projection_consumed_percent
        )
    };
    format!(
        "{} Hz · {}\nFull battery · {}\nAt this rate, time left · {}\n{evidence}\n{explanation}",
        rate.hz,
        if estimate.is_none() {
            "Still learning"
        } else if rate.confidence == InsightConfidence::Moderate {
            "Based on recorded use"
        } else {
            "Early estimate"
        },
        estimate
            .map(|h| insight_estimate(Some(h)))
            .unwrap_or_else(|| "Still learning".into()),
        remaining
            .map(|h| insight_estimate(Some(h)))
            .unwrap_or_else(|| "Waiting for a fresh reading".into()),
    )
}
fn rate_insight_row(rate: &RateInsight) -> String {
    let estimate = rate
        .projected_full_charge_hours
        .filter(|hours| hours.is_finite() && *hours >= 0.0)
        .map(|hours| insight_estimate(Some(hours)))
        .unwrap_or_else(|| "Still learning".into());
    format!("{} Hz · {estimate}", rate.hz)
}
fn charge_cycle_row(cycle: &ChargeCycle) -> String {
    let timestamp = timestamp_local(cycle.start_timestamp)
        .map(|date| {
            format!(
                "{} {} {:02}:{:02}",
                date.wDay,
                month_name(date.wMonth),
                date.wHour,
                date.wMinute
            )
        })
        .unwrap_or_else(|| "Date unavailable".into());
    format!("{} · {}", timestamp, insight_hours(cycle.awake_seconds))
}
fn charge_cycle_text(cycle: &ChargeCycle) -> String {
    let total_drop = u64::from(cycle.start_percent.saturating_sub(cycle.end_percent));
    let excluded = total_drop.saturating_sub(cycle.consumed_percent);
    let evidence = match cycle.evidence {
        CycleEvidence::ObservedCharge => "Started after charging",
        CycleEvidence::InferredCharge => "Possible charging detected from a rising battery level",
        CycleEvidence::Partial => "Charging was not recorded at the start",
    };
    let drain = if cycle.awake_seconds > 0 {
        format!(
            "{:.1}% per hour",
            cycle.consumed_percent as f64 * 3600.0 / cycle.awake_seconds as f64
        )
    } else {
        "Not enough recorded use yet".into()
    };
    format!(
        "Battery: {}% to {}%\r\nObserved use: {} percentage points · {excluded} outside counted intervals\r\nEstimated use: {} · Average battery use: {}\r\n{} session · {}\r\n{} to {}\r\nThis session may cover only part of a charge.",
        cycle.start_percent,
        cycle.end_percent,
        cycle.consumed_percent,
        insight_hours(cycle.awake_seconds),
        drain,
        if cycle.current { "Latest" } else { "Previous" },
        evidence,
        polling_timestamp(cycle.start_timestamp),
        polling_timestamp(cycle.end_timestamp),
    )
}
struct UiContext {
    state: RefCell<State>,
    monitor: Cell<HWND>,
    // Native child painting can reenter while State is being updated. Keep an
    // immutable, owned theme available without borrowing application state.
    paint: RefCell<Option<(HWND, Rc<DashboardTheme>)>>,
    popup: RefCell<Option<Rc<PopupAppearance>>>,
}
fn merged_device_rows(
    batteries: &[DeviceView],
    configuration: &[ConfigurationDevice],
) -> Vec<(ConfigurationDevice, Option<DeviceView>)> {
    let mut rows: Vec<_> = batteries
        .iter()
        .map(|d| {
            let device = configuration
                .iter()
                .find(|c| c.matches_reading(&d.reading))
                .cloned()
                .unwrap_or_else(|| ConfigurationDevice::from_reading(&d.reading));
            (device, Some(d.clone()))
        })
        .collect();
    for device in configuration {
        if !rows.iter().any(|(d, _)| d.key == device.key) {
            rows.push((device.clone(), None));
        }
    }
    rows
}
fn inventory_contains_device(
    batteries: &[DeviceView],
    configuration: &[ConfigurationDevice],
    device: &ConfigurationDevice,
) -> bool {
    if let Some(battery) = batteries.iter().find(|d| d.reading.key == device.key) {
        if let Some(configured) = configuration
            .iter()
            .find(|d| d.matches_reading(&battery.reading))
        {
            return configured == device;
        }
        let reading = &battery.reading;
        device.matches_reading(reading)
            && device.name == reading.name
            && device.kind == reading.kind
            && device.connection == reading.connection
            && matches!(device.capability, PollingCapability::ReadWrite)
    } else {
        configuration
            .iter()
            .find(|d| d.key == device.key)
            .is_some_and(|d| d == device)
    }
}
fn device_row_index(
    batteries: &[DeviceView],
    configuration: &[ConfigurationDevice],
    key: &str,
) -> Option<usize> {
    if let Some(index) = batteries.iter().position(|d| d.reading.key == key) {
        return Some(index);
    }
    let mut row = batteries.len();
    for (index, device) in configuration.iter().enumerate() {
        if batteries.iter().any(|d| d.reading.key == device.key)
            || configuration[..index].iter().any(|d| d.key == device.key)
        {
            continue;
        }
        if device.key == key {
            return Some(row);
        }
        row += 1;
    }
    None
}
fn selected_device_sources<'a>(
    batteries: &'a [DeviceView],
    configuration: &'a [ConfigurationDevice],
    selected: Option<&str>,
) -> (Option<&'a ConfigurationDevice>, Option<&'a DeviceView>) {
    let battery = selected.and_then(|key| batteries.iter().find(|d| d.reading.key == key));
    let standalone = selected.and_then(|key| configuration.iter().find(|d| d.key == key));
    if let Some(battery) = battery.or_else(|| {
        if standalone.is_none() {
            batteries.first()
        } else {
            None
        }
    }) {
        let device = configuration
            .iter()
            .find(|d| d.matches_reading(&battery.reading));
        (device, Some(battery))
    } else {
        (standalone.or_else(|| configuration.first()), None)
    }
}
fn current_device_row(
    batteries: &[DeviceView],
    configuration: &[ConfigurationDevice],
    selected: Option<&str>,
) -> Option<(ConfigurationDevice, Option<DeviceView>)> {
    let (configured, battery) = selected_device_sources(batteries, configuration, selected);
    let device = configured
        .cloned()
        .or_else(|| battery.map(|battery| ConfigurationDevice::from_reading(&battery.reading)))?;
    Some((device, battery.cloned()))
}
/// Temporary borrowed metadata, never a retained cache or a copy of display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeviceDescriptorRef<'a> {
    key: &'a str,
    name: &'a str,
    kind: &'a str,
    source: &'a str,
    via: &'a str,
    connection: &'a Connection,
    serial: Option<&'a str>,
    container: Option<&'a str>,
    capability: &'a PollingCapability,
}
impl DeviceDescriptorRef<'_> {
    fn same_identity(self, other: Self) -> bool {
        self.key == other.key
            && self.kind == other.kind
            && self.source == other.source
            && self.via == other.via
            && self.serial == other.serial
            && self.container == other.container
    }
}
impl<'a> From<&'a ConfigurationDevice> for DeviceDescriptorRef<'a> {
    fn from(device: &'a ConfigurationDevice) -> Self {
        Self {
            key: &device.key,
            name: &device.name,
            kind: &device.kind,
            source: &device.source,
            via: &device.via,
            connection: &device.connection,
            serial: device.serial.as_deref(),
            container: device.container.as_deref(),
            capability: &device.capability,
        }
    }
}
impl<'a> From<&'a Reading> for DeviceDescriptorRef<'a> {
    fn from(reading: &'a Reading) -> Self {
        Self {
            key: &reading.key,
            name: &reading.name,
            kind: &reading.kind,
            source: &reading.source,
            via: &reading.via,
            connection: &reading.connection,
            serial: reading.serial.as_deref(),
            container: reading.container.as_deref(),
            capability: &PollingCapability::ReadWrite,
        }
    }
}
fn current_device_state<'a>(
    batteries: &'a [DeviceView],
    configuration: &'a [ConfigurationDevice],
    selected: Option<&str>,
) -> Option<(DeviceDescriptorRef<'a>, Option<bool>)> {
    let (configured, battery) = selected_device_sources(batteries, configuration, selected);
    let descriptor = configured
        .map(DeviceDescriptorRef::from)
        .or_else(|| battery.map(|battery| DeviceDescriptorRef::from(&battery.reading)))?;
    Some((
        descriptor,
        battery.and_then(|battery| battery.reading.charging),
    ))
}
struct State {
    context: *const UiContext,
    runtime: Runtime,
    settings: Settings,
    dir: PathBuf,
    monitor: HWND,
    dashboard: Option<HWND>,
    controls: BTreeMap<u16, HWND>,
    trays: BTreeMap<String, Tray>,
    snapshot: Snapshot,
    diagnostics: BTreeMap<String, Vec<String>>,
    page: u16,
    selected: usize,
    selected_device: Option<String>,
    device_form_key: Option<String>,
    configuration_devices: Vec<ConfigurationDevice>,
    configuration_generation: u64,
    configuration_failure: Option<String>,
    chart: Option<Chart>,
    theme: Option<Rc<DashboardTheme>>,
    series: HistorySeries,
    history: HistorySelection,
    history_resize: HistoryResize,
    request: u64,
    taskbar: u32,
    notify: Option<HDEVNOTIFY>,
    animating: bool,
    tray_theme: TrayThemeCache,
    error: String,
    font: HFONT,
    heading_font: HFONT,
    font_dpi: u32,
    page_host: Option<HWND>,
    page_scroll: i32,
    wheel_remainder: i32,
    page_height: i32,
    more_options: bool,
    polling: PollingUi,
    tray_polling: PollingUi,
    insights: InsightsUi,
    sleep: SleepUi,
    dpi: DpiUi,
    polling_intents: BTreeMap<String, (u64, PollingRate)>,
    tooltip_rates: BTreeMap<String, PollingObservation>,
}
#[derive(Default)]
struct DpiUi {
    next: u64,
    pending: Option<(u64, String)>,
    attempted: Option<(String, u64)>,
    shown_key: Option<String>,
    outcome: Option<DpiOutcome>,
    previous: Option<(String, u64, DpiState)>,
}
// Exists only during a visible-page rebuild, never as a background draft cache.
#[derive(Default)]
struct FormDraft {
    text: Vec<(u16, String)>,
    checks: Vec<(u16, bool)>,
    choices: Vec<(u16, String)>,
}
#[derive(Default)]
struct SleepUi {
    saver_key: Option<String>,
    next: u64,
    shown_key: Option<String>,
    pending: Option<(u64, String)>,
    outcome: Option<SleepOutcome>,
}
/// Resize painting can use the existing series. Reload its downsampled data
/// only after the native move/size loop finishes, rather than once per pixel.
#[derive(Default)]
struct HistoryResize {
    active: bool,
    pending: bool,
}
impl HistoryResize {
    fn begin(&mut self) {
        self.active = true;
        self.pending = false;
    }
    fn changed(&mut self) -> bool {
        if self.active {
            self.pending = true;
            false
        } else {
            true
        }
    }
    fn finish(&mut self) -> bool {
        self.active = false;
        std::mem::take(&mut self.pending)
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn copy(dst: &mut [u16], s: &str) {
    dst.fill(0);
    let limit = dst.len().saturating_sub(1);
    let mut position = 0;
    for character in s.chars() {
        let mut encoded = [0; 2];
        let units = character.encode_utf16(&mut encoded);
        if position + units.len() > limit {
            break;
        }
        dst[position..position + units.len()].copy_from_slice(units);
        position += units.len();
    }
}
fn cache_tooltip_rate(
    cache: &mut BTreeMap<String, PollingObservation>,
    outcome: &ControlOutcome,
    generation: u64,
    enabled: bool,
) {
    cache.remove(&outcome.key);
    if enabled
        && outcome.failure.is_none()
        && let Some(observation) = &outcome.observation
        && observation.target.generation == generation
        && observation.target.device.key == outcome.key
        && observation.rate.is_some()
    {
        if cache.len() >= 512 {
            cache.pop_first();
        }
        cache.insert(outcome.key.clone(), observation.clone());
    }
}
/// Fixed Shell buffer; reserve the rate line even for a long renamed device.
/// Hover itself does no allocation, device I/O or timer work.
fn copy_tray_tooltip(
    dst: &mut [u16],
    device: &DeviceView,
    observation: Option<&PollingObservation>,
) {
    let rate = observation
        .filter(|o| {
            device.reading.connection != Connection::Stale
                && o.target.device.matches_reading(&device.reading)
        })
        .and_then(|o| o.rate);
    let suffix = match rate.map(|r| r.hz()) {
        Some(125) => "\nPolling: 125 Hz (last confirmed)",
        Some(250) => "\nPolling: 250 Hz (last confirmed)",
        Some(500) => "\nPolling: 500 Hz (last confirmed)",
        Some(1000) => "\nPolling: 1000 Hz (last confirmed)",
        Some(2000) => "\nPolling: 2000 Hz (last confirmed)",
        Some(4000) => "\nPolling: 4000 Hz (last confirmed)",
        Some(8000) => "\nPolling: 8000 Hz (last confirmed)",
        _ => "",
    };
    dst.fill(0);
    let available = dst.len().saturating_sub(1);
    let suffix_len = suffix.len().min(available);
    let prefix_limit = available - suffix_len;
    let mut position = 0;
    for character in device.text.chars() {
        let mut encoded = [0; 2];
        let units = character.encode_utf16(&mut encoded);
        if position + units.len() > prefix_limit {
            break;
        }
        dst[position..position + units.len()].copy_from_slice(units);
        position += units.len();
    }
    for unit in suffix.encode_utf16().take(suffix_len) {
        dst[position] = unit;
        position += 1;
    }
}

fn err(e: windows::core::Error) -> ProviderError {
    ProviderError::new(e.to_string())
}
pub fn run(
    runtime: Runtime,
    settings: Settings,
    dir: PathBuf,
    background: bool,
    initial_error: Option<String>,
) -> Result<(), ProviderError> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).map_err(err)?;
        let class = w!("HaloBatteryNext.Native");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(err)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(ProviderError::new("Register window class failed"));
        }
        let context = Box::new(UiContext {
            state: RefCell::new(State {
                context: std::ptr::null(),
                runtime,
                settings,
                dir,
                monitor: HWND::default(),
                dashboard: None,
                controls: BTreeMap::new(),
                trays: BTreeMap::new(),
                snapshot: Snapshot::default(),
                diagnostics: BTreeMap::new(),
                page: 1,
                selected: 0,
                selected_device: None,
                device_form_key: None,
                configuration_devices: Vec::new(),
                configuration_generation: 0,
                configuration_failure: None,
                chart: None,
                theme: None,
                series: HistorySeries::default(),
                history: HistorySelection::default(),
                history_resize: HistoryResize::default(),
                request: 0,
                taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
                notify: None,
                animating: false,
                tray_theme: TrayThemeCache::default(),
                error: initial_error.unwrap_or_default(),
                font: HFONT::default(),
                heading_font: HFONT::default(),
                font_dpi: 0,
                page_host: None,
                page_scroll: 0,
                wheel_remainder: 0,
                page_height: 0,
                more_options: false,
                polling: PollingUi::default(),
                tray_polling: PollingUi::default(),
                insights: InsightsUi::default(),
                sleep: SleepUi::default(),
                dpi: DpiUi::default(),
                polling_intents: BTreeMap::new(),
                tooltip_rates: BTreeMap::new(),
            }),
            monitor: Cell::new(HWND::default()),
            paint: RefCell::new(None),
            popup: RefCell::new(None),
        });
        let ptr = &*context as *const UiContext;
        let mut state = context.state.borrow_mut();
        state.context = ptr;
        state.monitor = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Halo Battery Next monitor"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some(ptr.cast()),
        )
        .map_err(err)?;
        context.monitor.set(state.monitor);
        // Consume a launcher's startup ShowWindow flag on the hidden monitor.
        let _ = ShowWindow(state.monitor, SW_HIDE);
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: HidD_GetHidGuid(),
            ..Default::default()
        };
        state.notify = RegisterDeviceNotificationW(
            HANDLE(state.monitor.0),
            (&filter as *const DEV_BROADCAST_DEVICEINTERFACE_W).cast(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .ok();
        state.runtime.attach_window(state.monitor.0 as usize);
        state.sync_trays(TrayUpdate::Changed);
        if !background {
            state.open();
        }
        // Runtime schedules initial discovery itself. Refresh here would invalidate
        // a startup restore that completed while the window was being created.
        drop(state);
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            if result <= 0 {
                break;
            }
            if message.message == WM_SYSCHAR && b"dhsri".contains(&(message.wParam.0 as u8)) {
                DispatchMessageW(&message);
                continue;
            }
            if dashboard_message(&context, &message) {
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let mut state = context.state.borrow_mut();
        state.runtime.attach_window(0);
        state.trays.clear();
        if let Some(h) = state.dashboard.take() {
            let _ = DestroyWindow(h);
        }
        state.chart = None;
        if let Some(n) = state.notify.take() {
            let _ = UnregisterDeviceNotification(n);
        }
        let _ = DestroyWindow(state.monitor);
        let _ = DeleteObject(state.font.into());
        let _ = DeleteObject(state.heading_font.into());
        state.runtime.stop();
        let _ = UnregisterClassW(class, Some(instance.into()));
        Ok(())
    }
}
// Route pointer scrolling before dialog processing: closed combo boxes otherwise
// consume wheel events as value changes. Only keyboard focus navigation may reveal
// an offscreen control; pointer messages must never snap back to the old focus.
fn dashboard_message(context: &UiContext, message: &MSG) -> bool {
    let Some(dashboard) = context.state.borrow().dashboard else {
        return false;
    };
    unsafe {
        if message.message == WM_MOUSEWHEEL
            && (message.hwnd == dashboard || IsChild(dashboard, message.hwnd).as_bool())
        {
            let focus = GetFocus();
            let mut class = [0u16; 16];
            let len = GetClassNameW(focus, &mut class);
            let combo = len == 8 && class[..8] == [67, 111, 109, 98, 111, 66, 111, 120];
            if !combo || send(focus, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0 == 0 {
                send(dashboard, WM_MOUSEWHEEL, message.wParam, message.lParam);
                return true;
            }
        }
        let before = GetFocus();
        if IsDialogMessageW(dashboard, message).as_bool() {
            if matches!(
                message.message,
                WM_KEYDOWN | WM_SYSKEYDOWN | WM_CHAR | WM_SYSCHAR
            ) && GetFocus() != before
            {
                context.state.borrow_mut().keep_focus_visible();
            }
            return true;
        }
    }
    false
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const UiContext;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if let Some(result) = dashboard_paint_message(&*ptr, hwnd, msg, wp, lp) {
            return result;
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            // Destruction is synchronous, including inside native modal loops.
            // Detach immediately, then release dashboard resources once the
            // outer State borrow ends. The hidden monitor outlives dashboards.
            if hwnd != (*ptr).monitor.get() {
                if let Ok(mut state) = (*ptr).state.try_borrow_mut() {
                    state.dashboard_destroyed(hwnd);
                } else {
                    let _ = PostMessageW(
                        Some((*ptr).monitor.get()),
                        WM_APP + 11,
                        WPARAM(hwnd.0 as usize),
                        LPARAM(0),
                    );
                }
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Ok(mut guard) = (*ptr).state.try_borrow_mut() else {
            // DefWindowProc handles WM_CLOSE by destroying the window. Never
            // let that bypass State cleanup while a native callback reenters.
            let deferred = match msg {
                WM_CLOSE => Some((WM_APP + 9, WPARAM(hwnd.0 as usize))),
                m if m == WM_APP + 8 || m == WM_APP + 9 || m == WM_APP + 11 => Some((m, wp)),
                _ => None,
            };
            if let Some((message, param)) = deferred {
                let _ = PostMessageW(Some((*ptr).monitor.get()), message, param, lp);
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let s = &mut *guard;
        if let Some(update) = tray_message_update(msg, s.taskbar) {
            if msg == WM_SETTINGCHANGE {
                s.refresh_theme();
            }
            s.sync_trays(update);
            return LRESULT(0);
        }
        match msg {
            WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
                s.refresh_theme();
                s.sync_trays(TrayUpdate::Redraw);
                LRESULT(0)
            }
            m if m == WM_APP + 7 => {
                s.drain();
                LRESULT(0)
            }
            m if m == WM_APP + 8 => {
                s.open();
                LRESULT(0)
            }
            m if m == WM_APP + 9 => {
                if let Some(h) = s.dashboard
                    && (wp.0 == 0 || wp.0 == h.0 as usize)
                {
                    s.close_dashboard(h);
                }
                LRESULT(0)
            }
            m if m == WM_APP + 11 => {
                let destroyed = HWND(wp.0 as *mut _);
                // A recreated dashboard may reuse the old numeric HWND before
                // this deferred notification runs; only retire a stale owner.
                if !s.owns_dashboard(destroyed) {
                    s.dashboard_destroyed(destroyed);
                }
                LRESULT(0)
            }
            m if m == WM_APP + 10 => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    s.runtime.stop();
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wp.0 == 2 {
                    s.animate()
                } else {
                    s.drain()
                }
                LRESULT(0)
            }
            WM_DEVICECHANGE => {
                s.runtime.send(Command::DeviceChanged {
                    arrival_or_removal: matches!(
                        wp.0 as u32,
                        DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE
                    ),
                });
                LRESULT(1)
            }
            WM_POWERBROADCAST => {
                match wp.0 as u32 {
                    PBT_APMSUSPEND => s.runtime.send(Command::Suspend),
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                        s.runtime.send(Command::Resume)
                    }
                    _ => {}
                }
                LRESULT(1)
            }
            TRAY => {
                let event = (lp.0 as u32) & 0xffff;
                let opens_dashboard = matches!(event, WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT)
                    || event == (NIN_SELECT | 1);
                let opens_menu = matches!(event, WM_CONTEXTMENU | WM_RBUTTONUP);
                // Hover/tooltip and button-down callbacks are passive. Avoid
                // selecting a device, cloning its key or querying history here.
                if !opens_dashboard && !opens_menu {
                    return LRESULT(0);
                }
                let key = s
                    .trays
                    .iter()
                    .find(|(_, t)| t.data.uID == wp.0 as u32)
                    .map(|(key, _)| key.clone());
                let mut selection_changed = false;
                if let Some(key) = &key
                    && let Some(i) = s
                        .snapshot
                        .devices
                        .iter()
                        .position(|d| &d.reading.key == key)
                {
                    selection_changed =
                        s.selected != i || s.selected_device.as_deref() != Some(key.as_str());
                    s.selected_device = Some(key.clone());
                    s.selected = i;
                    if selection_changed {
                        s.polling.abandon();
                        s.insights.abandon();
                        if s.dashboard.is_some() && matches!(s.page, 1 | 2 | 6) {
                            s.page_scroll = 0;
                            s.build();
                            s.query_insights();
                        }
                    }
                }
                if opens_menu {
                    s.menu(key.clone())
                } else if opens_dashboard {
                    let was_open = s.dashboard.is_some();
                    s.open();
                    if selection_changed && was_open {
                        s.read_polling();
                    }
                }
                LRESULT(0)
            }
            WM_SYSCHAR => {
                match (wp.0 as u8).to_ascii_lowercase() {
                    b'd' => s.command(1, 0),
                    b'h' => s.command(2, 0),
                    b's' => s.command(3, 0),
                    b'i' => s.command(6, 0),
                    b'r' => s.command(4, 0),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_VSCROLL => {
                let host = s.page_host;
                drop(guard);
                if let Some(host) = host {
                    return send(host, msg, wp, lp);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                s.command((wp.0 & 0xffff) as u16, ((wp.0 >> 16) & 0xffff) as u16);
                LRESULT(0)
            }
            WM_CLOSE => {
                if Some(hwnd) == s.dashboard {
                    s.close_dashboard(hwnd);
                }
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let m = &mut *(lp.0 as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                m.ptMinTrackSize = POINT {
                    x: 840 * dpi / 96,
                    y: 420 * dpi / 96,
                };
                LRESULT(0)
            }
            WM_ENTERSIZEMOVE => {
                if Some(hwnd) == s.dashboard {
                    s.history_resize.begin();
                }
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                if Some(hwnd) == s.dashboard && s.history_resize.finish() && s.page == 2 {
                    s.query();
                }
                LRESULT(0)
            }
            WM_SIZE => {
                if Some(hwnd) == s.dashboard {
                    s.configuration_visibility();
                }
                if Some(hwnd) == s.dashboard && wp.0 as u32 != SIZE_MINIMIZED {
                    s.layout_page();
                }
                if Some(hwnd) == s.dashboard && s.page == 2 && wp.0 as u32 != SIZE_MINIMIZED {
                    if s.history_resize.changed() {
                        s.query();
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_SHOWWINDOW if Some(hwnd) == s.dashboard => {
                // WM_SHOWWINDOW precedes the native visibility flag update.
                // Defer reading that state until ShowWindow has completed.
                let _ = PostMessageW(Some(s.monitor), WM_APP + 12, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            m if m == WM_APP + 12 => {
                s.configuration_visibility();
                LRESULT(0)
            }
            WM_MOUSEWHEEL if Some(hwnd) == s.dashboard && s.page_host.is_some() => {
                let delta = ((wp.0 >> 16) as u16 as i16) as i32;
                s.wheel_remainder += delta * 48;
                let movement = s.wheel_remainder / 120;
                s.wheel_remainder %= 120;
                s.scroll_page(s.page_scroll.saturating_sub(movement));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                s.build();
                s.sync_trays(TrayUpdate::Redraw);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                // BeginPaint can synchronously request background erasure while
                // State is borrowed. Paint the palette ourselves rather than
                // leaving the class's default system brush on screen.
                if Some(hwnd) == s.dashboard
                    && let Some(theme) = &s.theme
                {
                    FillRect(hdc, &ps.rcPaint, theme.background_brush());
                }
                if Some(hwnd) == s.dashboard && s.page == 2 {
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    if s.chart.is_none() {
                        s.chart = Chart::new(hwnd, r.right as u32, r.bottom as u32).ok()
                    }
                    if let Some(c) = &s.chart
                        && s.theme.as_ref().is_some_and(|theme| {
                            c.paint_with_palette(
                                r.right as u32,
                                r.bottom as u32,
                                &s.series,
                                &theme.palette,
                            )
                            .is_err()
                        })
                    {
                        s.chart = None;
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => {
                // Native default processing can synchronously send messages
                // back to this window (SC_CLOSE, WM_PRINT, activation). Release
                // State before handing control back to Windows.
                drop(guard);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
    }))
    .unwrap_or_else(|_| {
        unsafe {
            PostQuitMessage(1);
        }
        LRESULT(0)
    })
}
unsafe fn dashboard_paint_message(
    context: &UiContext,
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> Option<LRESULT> {
    if hwnd == context.monitor.get() && matches!(msg, WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        // TrackPopupMenu reenters while State is borrowed; retain only the
        // immutable menu snapshot and release the context borrow before GDI.
        let appearance = context.popup.borrow().clone();
        if let Some(appearance) = appearance {
            return appearance.message(msg, wp, lp);
        }
    }
    if !matches!(
        msg,
        WM_DRAWITEM
            | WM_CTLCOLOREDIT
            | WM_CTLCOLORLISTBOX
            | WM_CTLCOLORSTATIC
            | WM_CTLCOLORBTN
            | WM_ERASEBKGND
            | WM_PRINTCLIENT
    ) {
        return None;
    }
    // Drop the RefCell borrow before calling Windows; owner drawing can itself
    // send synchronous native messages. Rc retains the brushes through a call.
    let (owner, theme) = context.paint.borrow().as_ref()?.clone();
    if hwnd != owner {
        return None;
    }
    if let Some(result) = theme.control_colors(msg, HDC(wp.0 as *mut _), HWND(lp.0 as *mut _)) {
        return Some(result);
    }
    if msg == WM_DRAWITEM {
        return theme.draw_item(lp);
    }
    if matches!(msg, WM_ERASEBKGND | WM_PRINTCLIENT) {
        unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            FillRect(HDC(wp.0 as *mut _), &rect, theme.background_brush());
        }
        return Some(LRESULT(i32::from(msg == WM_ERASEBKGND) as isize));
    }
    None
}

/// Hide intermediate child teardown/creation from the display. An initially
/// hidden dashboard must remain hidden until open() explicitly shows it.
struct DashboardRedraw {
    hwnd: HWND,
    paused: bool,
}
fn is_fixed_control(id: u16) -> bool {
    matches!(id, 1..=7 | 78 | 95 | 97 | 210)
}
// One ordinary child window clips scrolling controls below the navigation.
// It exists only with a dashboard; it owns no images, fonts or timers.
unsafe extern "system" fn page_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const UiContext;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if matches!(
            msg,
            WM_COMMAND
                | WM_NOTIFY
                | WM_DRAWITEM
                | WM_MEASUREITEM
                | WM_SYSCHAR
                | WM_CTLCOLORSTATIC
                | WM_CTLCOLOREDIT
                | WM_CTLCOLORLISTBOX
                | WM_CTLCOLORBTN
        ) && let Ok(parent) = GetParent(hwnd)
        {
            return send(parent, msg, wp, lp);
        }
        if matches!(msg, WM_PAINT | WM_ERASEBKGND | WM_PRINTCLIENT) {
            let theme = (*ptr)
                .paint
                .borrow()
                .as_ref()
                .map(|(_, theme)| theme.clone());
            if let Some(theme) = theme {
                let mut paint = PAINTSTRUCT::default();
                let dc = if msg == WM_PAINT {
                    BeginPaint(hwnd, &mut paint)
                } else {
                    HDC(wp.0 as _)
                };
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(dc, &rect, theme.background_brush());
                if msg == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(i32::from(msg == WM_ERASEBKGND) as isize);
            }
        }
        if msg == WM_VSCROLL {
            if let Ok(mut state) = (*ptr).state.try_borrow_mut() {
                let mut info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_ALL,
                    ..Default::default()
                };
                let _ = GetScrollInfo(state.controls[&7], SB_CTL, &mut info);
                let page = (info.nPage as i32 - 32).max(32);
                let position = match wp.0 as u16 {
                    n if n == SB_LINEUP.0 as u16 => state.page_scroll - 32,
                    n if n == SB_LINEDOWN.0 as u16 => state.page_scroll + 32,
                    n if n == SB_PAGEUP.0 as u16 => state.page_scroll - page,
                    n if n == SB_PAGEDOWN.0 as u16 => state.page_scroll + page,
                    n if n == SB_TOP.0 as u16 => 0,
                    n if n == SB_BOTTOM.0 as u16 => i32::MAX,
                    n if n == SB_THUMBTRACK.0 as u16 || n == SB_THUMBPOSITION.0 as u16 => {
                        // The page range fits the standard 16-bit WM_VSCROLL
                        // position for both native and themed scrollbar input.
                        ((wp.0 >> 16) & 0xffff) as i32
                    }
                    _ => state.page_scroll,
                };
                state.scroll_page(position);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }))
    .unwrap_or(LRESULT(0))
}
impl DashboardRedraw {
    fn new(hwnd: HWND) -> Self {
        let paused = unsafe { IsWindowVisible(hwnd).as_bool() };
        if paused {
            unsafe {
                send(hwnd, WM_SETREDRAW, WPARAM(0), LPARAM(0));
            }
        }
        Self { hwnd, paused }
    }
}
impl Drop for DashboardRedraw {
    fn drop(&mut self) {
        unsafe {
            if self.paused {
                send(self.hwnd, WM_SETREDRAW, WPARAM(1), LPARAM(0));
            }
            let _ = RedrawWindow(
                Some(self.hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
            );
        }
    }
}
impl State {
    fn heading(&mut self, id: u16, text: &str, x: i32, y: i32, width: i32) {
        self.widget(id, text, x, y, width, 28);
        if let Some(h) = self.controls.get(&id) {
            unsafe {
                send(
                    *h,
                    WM_SETFONT,
                    WPARAM(self.heading_font.0 as usize),
                    LPARAM(0),
                );
            }
        }
    }
    fn prepare_page(&mut self) {
        if self.page == 2 {
            self.page_height = 0;
            return;
        }
        static CLASS: std::sync::Once = std::sync::Once::new();
        unsafe {
            CLASS.call_once(|| {
                let _ = RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(page_proc),
                    lpszClassName: w!("HaloBatteryNext.Page"),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    ..Default::default()
                });
            });
            self.page_host = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                w!("HaloBatteryNext.Page"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                0,
                0,
                self.dashboard,
                None,
                None,
                Some(self.context.cast()),
            )
            .ok();
        }
        self.control(
            7,
            w!("SCROLLBAR"),
            "",
            WINDOW_STYLE(SBS_VERT as u32) | WS_CLIPSIBLINGS,
            800,
            PAGE_TOP,
            14,
            100,
        );
        self.page_height = match self.page {
            3 if self.more_options => 1264,
            3 => 724,
            6 => 1430,
            _ => 708,
        } - PAGE_TOP;
        self.layout_page();
    }
    fn position_control(&self, id: u16, x: i32, y: i32, width: i32, height: i32) {
        if let Some(h) = self.controls.get(&id) {
            let dpi = unsafe { GetDpiForWindow(*h).max(96) } as i32;
            let f = |n| n * dpi / 96;
            unsafe {
                let _ = SetWindowPos(
                    *h,
                    None,
                    f(x),
                    f(y),
                    f(width),
                    f(height),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
    fn update_more_options(&mut self) {
        for id in [109, 110, 207, 208, 213, 217, 218, 219]
            .into_iter()
            .chain((0..providers().len()).map(|index| 300 + index as u16))
        {
            if let Some(h) = self.controls.get(&id) {
                unsafe {
                    let _ = ShowWindow(*h, if self.more_options { SW_SHOW } else { SW_HIDE });
                }
            }
        }
        for id in [111, 209] {
            if let Some(control) = self.controls.get(&id) {
                unsafe {
                    let _ = ShowWindow(*control, SW_HIDE);
                }
            }
        }
        self.page_height = if self.more_options { 1264 } else { 724 } - PAGE_TOP;
        self.set_control_text(
            212,
            if self.more_options {
                "Hide advanced options"
            } else {
                "Advanced options"
            },
        );
        self.layout_page();
    }
    fn layout_page(&mut self) {
        let Some(hwnd) = self.dashboard else {
            return;
        };
        let dpi = unsafe { GetDpiForWindow(hwnd).max(96) } as i32;
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rect);
        }
        let height = rect.bottom * 96 / dpi;
        if let Some(host) = self.page_host {
            unsafe {
                let _ = SetWindowPos(
                    host,
                    None,
                    0,
                    PAGE_TOP * dpi / 96,
                    ((rect.right * 96 / dpi - 16) * dpi / 96).max(1),
                    (height - PAGE_TOP - PAGE_FOOTER).max(1) * dpi / 96,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            self.position_control(
                7,
                rect.right * 96 / dpi - 16,
                PAGE_TOP,
                14,
                (height - PAGE_TOP - PAGE_FOOTER).max(1),
            );
            self.scroll_page(self.page_scroll);
        }
        match self.page {
            1 => {
                self.position_control(5, 20, height - 46, 205, 30);
                self.position_control(95, 245, height - 49, 535, 46);
            }
            3 => {
                self.position_control(210, 20, height - 46, 160, 30);
                self.position_control(5, 194, height - 46, 205, 30);
                self.position_control(95, 415, height - 49, 365, 46);
            }
            2 => self.position_control(97, 20, height - 36, 760, 24),
            6 => self.position_control(78, 20, height - 52, 760, 46),
            _ => {}
        }
    }
    fn scroll_page(&mut self, position: i32) {
        let Some(host) = self.page_host else {
            return;
        };
        let dpi = unsafe { GetDpiForWindow(host).max(96) } as i32;
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(host, &mut rect);
        }
        let viewport = (rect.bottom * 96 / dpi).max(1);
        let max = (self.page_height - viewport).max(0);
        let position = position.clamp(0, max);
        let delta = self.page_scroll * dpi / 96 - position * dpi / 96;
        self.page_scroll = position;
        unsafe {
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: self.page_height - 1,
                nPage: viewport as u32,
                nPos: position,
                ..Default::default()
            };
            if let Some(scrollbar) = self.controls.get(&7) {
                let mut previous = SCROLLINFO { ..info };
                let changed = GetScrollInfo(*scrollbar, SB_CTL, &mut previous).is_err()
                    || previous.nMin != info.nMin
                    || previous.nMax != info.nMax
                    || previous.nPage != info.nPage
                    || previous.nPos != info.nPos;
                if changed {
                    // A native redraw here briefly exposes the Windows scrollbar
                    // underneath our themed paint. Schedule only the latter.
                    SetScrollInfo(*scrollbar, SB_CTL, &info, false);
                    let _ = InvalidateRect(Some(*scrollbar), None, false);
                }
                let visible = GetWindowLongW(*scrollbar, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0;
                if visible != (max > 0) {
                    let _ = ShowWindow(*scrollbar, if max > 0 { SW_SHOW } else { SW_HIDE });
                }
            }
            if delta != 0 {
                // Move even controls outside the viewport. ScrollWindowEx with
                // SW_SCROLLCHILDREN cannot reliably update partially clipped
                // children. These transient positions exist only during input.
                let mut batch = BeginDeferWindowPos(self.controls.len() as i32).ok();
                for (id, child) in &self.controls {
                    if is_fixed_control(*id) {
                        continue;
                    }
                    let mut rect = RECT::default();
                    if GetWindowRect(*child, &mut rect).is_err() {
                        continue;
                    }
                    let mut point = [POINT {
                        x: rect.left,
                        y: rect.top,
                    }];
                    MapWindowPoints(None, Some(host), &mut point);
                    let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW;
                    if let Some(defer) = batch {
                        batch = DeferWindowPos(
                            defer,
                            *child,
                            None,
                            point[0].x,
                            point[0].y + delta,
                            0,
                            0,
                            flags,
                        )
                        .ok();
                        if batch.is_some() {
                            continue;
                        }
                        // The failed batch applies nothing. Rebuild restores
                        // every control from its logical position below.
                        self.build();
                        return;
                    }
                    let _ = SetWindowPos(*child, None, point[0].x, point[0].y + delta, 0, 0, flags);
                }
                if let Some(batch) = batch
                    && EndDeferWindowPos(batch).is_err()
                {
                    self.build();
                    return;
                }
                let _ = RedrawWindow(
                    Some(host),
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
            }
        }
    }
    fn keep_focus_visible(&mut self) {
        let Some(host) = self.page_host else {
            return;
        };
        unsafe {
            let focus = GetFocus();
            if !IsChild(host, focus).as_bool() {
                return;
            }
            let mut child = RECT::default();
            let mut page = RECT::default();
            let _ = GetWindowRect(focus, &mut child);
            let _ = GetWindowRect(host, &mut page);
            let dpi = GetDpiForWindow(host).max(96) as i32;
            let delta = if child.top < page.top {
                child.top - page.top - 8
            } else if child.bottom > page.bottom {
                child.bottom - page.bottom + 8
            } else {
                0
            };
            self.scroll_page(self.page_scroll + delta * 96 / dpi);
        }
    }
    fn refresh_theme(&mut self) {
        if self.dashboard.is_none() {
            return;
        }
        let dark = hb_windows::system::dashboard_dark_theme();
        let high_contrast = hb_windows::system::high_contrast();
        let palette = crate::dashboard_theme::Palette::new(dark, high_contrast);
        if self
            .theme
            .as_ref()
            .is_some_and(|theme| theme.palette == palette)
        {
            return;
        }
        self.apply_theme(DashboardTheme::new(dark, high_contrast));
    }
    fn apply_theme(&mut self, theme: DashboardTheme) {
        let Some(hwnd) = self.dashboard else {
            return;
        };
        let _redraw = DashboardRedraw::new(hwnd);
        let theme = Rc::new(theme);
        unsafe {
            (*self.context).paint.replace(Some((hwnd, theme.clone())));
        }
        self.theme = Some(theme.clone());
        theme.apply_window(hwnd);
        for control in self.controls.values() {
            theme.apply_control(*control);
        }
    }
    fn owns_dashboard(&self, hwnd: HWND) -> bool {
        unsafe {
            IsWindow(Some(hwnd)).as_bool()
                && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == self.context as isize
        }
    }
    fn dashboard_destroyed(&mut self, hwnd: HWND) {
        if self.dashboard != Some(hwnd) {
            return;
        }
        self.dashboard = None;
        self.page_host = None;
        self.page_scroll = 0;
        self.wheel_remainder = 0;
        self.page_height = 0;
        self.history_resize = HistoryResize::default();
        self.configuration_visibility();
        self.chart = None;
        self.release_history();
        self.theme = None;
        unsafe {
            (*self.context).paint.take();
        }
        self.polling.abandon();
        self.sleep = SleepUi {
            next: self.sleep.next,
            ..SleepUi::default()
        };
        self.dpi = DpiUi {
            next: self.dpi.next,
            ..DpiUi::default()
        };
        self.insights.abandon();
        self.insights.data = None;
        self.insights.key = None;
        self.insights.status = String::new();
        self.controls.clear();
        self.device_form_key = None;
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.heading_font.into());
        }
        self.font = HFONT::default();
        self.heading_font = HFONT::default();
        self.font_dpi = 0;
    }
    fn close_dashboard(&mut self, hwnd: HWND) {
        if self.dashboard != Some(hwnd) {
            return;
        }
        // Keep GDI/control resources alive until native children are destroyed.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        self.dashboard_destroyed(hwnd);
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the native control creation fields.
    fn control(
        &mut self,
        id: u16,
        class: PCWSTR,
        text: &str,
        style: WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> HWND {
        unsafe {
            let text = wide(text);
            let scale = GetDpiForWindow(self.dashboard.unwrap()) as i32;
            let f = |n: i32| n * scale / 96;
            let h = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                f(x),
                f((if !is_fixed_control(id) && self.page_host.is_some() {
                    y - PAGE_TOP - self.page_scroll
                } else {
                    y
                }) + if self.page == 3 && !is_fixed_control(id) && y >= 252 {
                    24
                } else {
                    0
                }),
                f(width),
                f(height),
                if is_fixed_control(id) {
                    self.dashboard
                } else {
                    self.page_host.or(self.dashboard)
                },
                Some(HMENU(id as usize as *mut _)),
                None,
                None,
            )
            .unwrap_or_default();
            send(h, WM_SETFONT, WPARAM(self.font.0 as usize), LPARAM(1));
            if let Some(theme) = &self.theme {
                theme.apply_control(h);
            }
            if class.as_wide() == w!("EDIT").as_wide() {
                let margin = (8 * scale / 96) as isize;
                send(
                    h,
                    EM_SETMARGINS,
                    WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize),
                    LPARAM(margin | (margin << 16)),
                );
            }
            self.controls.insert(id, h);
            h
        }
    }
    fn widget(&mut self, id: u16, text: &str, x: i32, y: i32, width: i32, height: i32) {
        self.control(
            id,
            w!("STATIC"),
            text,
            WINDOW_STYLE(13), // SS_OWNERDRAW: native static with a readable caption
            x,
            y,
            width,
            height,
        );
    }
    fn label(&mut self, id: u16, text: &str, x: i32, y: i32, width: i32) {
        self.control(
            id,
            w!("STATIC"),
            text,
            WINDOW_STYLE::default(),
            x,
            y,
            width,
            24,
        );
    }
    fn button(&mut self, id: u16, text: &str, x: i32, y: i32, width: i32) {
        self.control(id, w!("BUTTON"), text, WS_TABSTOP, x, y, width, 34);
    }
    fn edit(&mut self, id: u16, text: &str, x: i32, y: i32, width: i32) {
        self.control(
            id,
            w!("EDIT"),
            text,
            WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            x,
            y,
            width,
            26,
        );
    }
    fn check(&mut self, id: u16, text: &str, value: bool, x: i32, y: i32, width: i32) {
        let h = self.control(
            id,
            w!("BUTTON"),
            text,
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            x,
            y,
            width,
            26,
        );
        unsafe {
            send(h, BM_SETCHECK, WPARAM(value as usize), LPARAM(0));
        }
    }
    fn combo(&mut self, id: u16, items: &[String], selected: usize, x: i32, y: i32, width: i32) {
        let h = self.control(
            id,
            w!("COMBOBOX"),
            "",
            WS_TABSTOP
                | WS_VSCROLL
                | WINDOW_STYLE((CBS_DROPDOWNLIST | CBS_OWNERDRAWFIXED | CBS_HASSTRINGS) as u32),
            x,
            y,
            width,
            280,
        );
        unsafe {
            if items.is_empty() {
                let text = wide(if id == 10 && matches!(self.page, 2 | 6) {
                    "No battery-powered devices"
                } else if id == 10 {
                    "No devices connected"
                } else {
                    "No options available"
                });
                send(h, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
                let _ = EnableWindow(h, false);
            }
            for t in items {
                let text = wide(t);
                send(h, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
            }
            send(h, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
        }
    }
    fn text(&self, id: u16) -> String {
        unsafe {
            let Some(h) = self.controls.get(&id) else {
                return String::new();
            };
            let n = GetWindowTextLengthW(*h) as usize;
            let mut b = vec![0; n + 1];
            GetWindowTextW(*h, &mut b);
            String::from_utf16_lossy(&b[..n])
        }
    }
    fn checked(&self, id: u16) -> bool {
        self.controls
            .get(&id)
            .is_some_and(|h| unsafe { send(*h, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1 })
    }
    fn choice(&self, id: u16) -> usize {
        self.controls.get(&id).map_or(0, |h| unsafe {
            send(*h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0.max(0) as usize
        })
    }
    fn capture_form(&self, same_device: bool) -> FormDraft {
        let (text, checks, choices): (&[u16], Vec<u16>, &[u16]) = match self.page {
            1 if same_device => (
                &[11, 13, 442, 451],
                vec![12, 49, 440],
                &[14, 43, 50, 431, 444, 445],
            ),
            3 if self.checked(3) => (
                &[201, 203, 209, 215],
                (0..CHECKS.len())
                    .map(setting_check_id)
                    .chain([112, 113, 206])
                    .chain((0..providers().len()).map(|i| 300 + i as u16))
                    .collect(),
                &[205],
            ),
            _ => return FormDraft::default(),
        };
        FormDraft {
            text: text
                .iter()
                .filter(|id| self.controls.contains_key(id))
                .map(|id| (*id, self.text(*id)))
                .collect(),
            checks: checks
                .into_iter()
                .filter(|id| self.controls.contains_key(id))
                .map(|id| (id, self.checked(id)))
                .collect(),
            choices: choices
                .iter()
                .filter(|id| self.controls.contains_key(id))
                .map(|id| (*id, self.text(*id)))
                .collect(),
        }
    }
    fn restore_form(&self, draft: FormDraft) {
        for (id, text) in draft.text {
            self.set_control_text(id, &text);
        }
        unsafe {
            for (id, checked) in draft.checks {
                if let Some(h) = self.controls.get(&id) {
                    send(*h, BM_SETCHECK, WPARAM(usize::from(checked)), LPARAM(0));
                }
            }
            for (id, text) in draft.choices {
                if let Some(h) = self.controls.get(&id) {
                    // Choices can change after readback; never restore by index
                    // into a different list of supported hardware values.
                    let text = wide(&text);
                    let index = send(
                        *h,
                        CB_FINDSTRINGEXACT,
                        WPARAM(usize::MAX),
                        LPARAM(text.as_ptr() as isize),
                    )
                    .0;
                    if index >= 0 {
                        send(*h, CB_SETCURSEL, WPARAM(index as usize), LPARAM(0));
                    }
                }
            }
        }
    }
    fn open(&mut self) {
        unsafe {
            if let Some(h) = self.dashboard {
                if self.owns_dashboard(h) {
                    let _ = ShowWindow(h, SW_RESTORE);
                    self.configuration_visibility();
                    let _ = SetForegroundWindow(h);
                    return;
                }
                self.dashboard_destroyed(h);
            }
            let ptr = self.context;
            let dpi = GetDpiForWindow(self.monitor).max(96) as i32;
            let title = wide(concat!("Halo Continued ", env!("CARGO_PKG_VERSION")));
            match CreateWindowExW(
                WS_EX_CONTROLPARENT,
                w!("HaloBatteryNext.Native"),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                840 * dpi / 96,
                780 * dpi / 96,
                None,
                None,
                None,
                Some(ptr.cast()),
            ) {
                Ok(h) => {
                    self.dashboard = Some(h);
                    let mut info = MONITORINFO {
                        cbSize: size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    let mut bounds = RECT::default();
                    if GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut info)
                        .as_bool()
                        && GetWindowRect(h, &mut bounds).is_ok()
                    {
                        let work = info.rcWork;
                        let width = (bounds.right - bounds.left).min(work.right - work.left);
                        let height = (bounds.bottom - bounds.top).min(work.bottom - work.top);
                        if width > 0 && height > 0 {
                            let _ = SetWindowPos(
                                h,
                                None,
                                bounds.left.clamp(work.left, work.right - width),
                                bounds.top.clamp(work.top, work.bottom - height),
                                width,
                                height,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    }
                    // Prepare the requested Devices page and its initial read
                    // before showing the fully built window (avoids a flash).
                    self.runtime
                        .send(Command::ConfigurationVisible(self.page == 1));
                    self.refresh_theme();
                    self.polling.abandon();
                    self.build();
                    let _ = ShowWindow(h, SW_SHOW);
                    self.configuration_visibility();
                    self.read_polling();
                    self.query_insights();
                    let _ = SetForegroundWindow(h);
                }
                Err(e) => self.error = e.to_string(),
            }
        }
    }
    fn build(&mut self) {
        let Some(hwnd) = self.dashboard else {
            return;
        };
        let focused_window = unsafe { GetFocus() };
        let focused = self
            .controls
            .iter()
            .find_map(|(id, h)| (*h == focused_window).then_some(*id));
        let current_key = (self.page == 1)
            .then(|| self.current_device().map(|(d, _)| d.key))
            .flatten();
        let draft = self.capture_form(
            current_key.is_some()
                && current_key == self.device_form_key
                && self.controls.contains_key(&11),
        );
        let caret = focused
            .filter(|id| draft.text.iter().any(|(edit, _)| edit == id))
            .and_then(|id| self.controls.get(&id))
            .map(|h| unsafe {
                send(
                    *h,
                    windows::Win32::UI::Controls::EM_GETSEL,
                    WPARAM(0),
                    LPARAM(0),
                )
                .0
            });
        self.device_form_key = current_key;
        let _redraw = DashboardRedraw::new(hwnd);
        self.chart = None;
        if self.page != 2 {
            self.release_history();
        }
        let controls = std::mem::take(&mut self.controls);
        unsafe {
            if let Some(host) = self.page_host.take() {
                let _ = DestroyWindow(host);
            }
            for h in controls.values() {
                let _ = DestroyWindow(*h);
            }
        }
        unsafe {
            let dpi = GetDpiForWindow(self.dashboard.unwrap());
            if self.font_dpi != dpi || self.font.is_invalid() {
                let _ = DeleteObject(self.font.into());
                let _ = DeleteObject(self.heading_font.into());
                let make_font = |height: i32, weight| {
                    CreateFontW(
                        -height * dpi as i32 / 96,
                        0,
                        0,
                        0,
                        weight,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET,
                        OUT_DEFAULT_PRECIS,
                        CLIP_DEFAULT_PRECIS,
                        CLEARTYPE_QUALITY,
                        DEFAULT_PITCH.0 as u32,
                        w!("Segoe UI"),
                    )
                };
                self.font = make_font(15, 400);
                self.heading_font = make_font(20, 600);
                self.font_dpi = dpi;
            }
        }
        for (i, (id, title)) in [
            (1, "&Devices"),
            (2, "&History"),
            (6, "&Insights"),
            (3, "&Settings"),
        ]
        .iter()
        .enumerate()
        {
            let h = self.control(
                *id,
                w!("BUTTON"),
                title,
                WS_TABSTOP
                    | WINDOW_STYLE((BS_AUTORADIOBUTTON | BS_PUSHLIKE) as u32)
                    | if i == 0 {
                        WS_GROUP
                    } else {
                        WINDOW_STYLE::default()
                    },
                20 + i as i32 * 116,
                16,
                108,
                32,
            );
            unsafe {
                send(
                    h,
                    BM_SETCHECK,
                    WPARAM(usize::from(self.page == *id)),
                    LPARAM(0),
                );
            }
        }
        self.button(4, "&Refresh", 660, 16, 120);
        self.prepare_page();
        let names: Vec<_> = self
            .snapshot
            .devices
            .iter()
            .map(|d| d.text.clone())
            .collect();
        self.selected = self.selected.min(names.len().saturating_sub(1));
        match self.page {
            1 => {
                self.heading(90, "Your devices", 20, 66, 740);
                let rows = self.device_rows();
                let index = self
                    .selected_device
                    .as_ref()
                    .and_then(|key| rows.iter().position(|(d, _)| &d.key == key))
                    .unwrap_or(0);
                self.selected_device = rows.get(index).map(|(d, _)| d.key.clone());
                let device_names = rows
                    .iter()
                    .map(|(d, battery)| {
                        battery.as_ref().map_or_else(
                            || {
                                format!(
                                    "{} · {}",
                                    self.configuration_name(d),
                                    if d.kind == "keyboard" {
                                        "Wired keyboard · No battery"
                                    } else {
                                        "Mouse · Battery not monitored"
                                    }
                                )
                            },
                            |b| b.text.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                self.combo(10, &device_names, index, 20, 96, 760);
                if let Some((device, battery)) = self.current_device() {
                    if let Some(d) = battery {
                        self.heading(88, &battery_summary(&d), 20, 140, 740);
                        self.label(91, &device_detail(&d), 20, 170, 740);
                        self.heading(89, "Device preferences", 20, 212, 740);
                        self.label(92, "Display &name", 20, 255, 120);
                        self.edit(11, &d.name, 150, 251, 360);
                        self.check(12, "&Hide tray icon", d.hidden, 540, 251, 240);
                        self.label(93, "Low battery alert (%)", 400, 298, 190);
                        let low = self
                            .settings
                            .device(&d.reading.key)
                            .and_then(|p| p.low)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        self.edit(13, &low, 610, 294, 80);
                        let alert = device_alert_summary(&self.settings, &d.reading.key);
                        self.control(
                            96,
                            w!("STATIC"),
                            &alert,
                            WINDOW_STYLE::default(),
                            20,
                            332,
                            760,
                            36,
                        );
                        self.label(94, "Tray &icon", 20, 298, 120);
                        let kinds: Vec<_> = [
                            "automatic",
                            "mouse",
                            "keyboard",
                            "headset",
                            "gamepad",
                            "bluetooth",
                            "dualshock",
                            "dualsense",
                        ]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                        let chosen = self
                            .settings
                            .device(&d.reading.key)
                            .and_then(|p| p.icon.as_ref())
                            .and_then(|icon| kinds.iter().position(|s| s == icon))
                            .unwrap_or(0);
                        let labels = ICON_CHOICES
                            .iter()
                            .map(|s| (*s).to_owned())
                            .collect::<Vec<_>>();
                        self.combo(14, &labels, chosen, 150, 294, 215);
                        self.button(15, "&Save changes", 20, 374, 160);
                        self.button(16, "Reset preferences", 192, 374, 190);
                    } else {
                        self.heading(
                            88,
                            if device.kind == "keyboard" {
                                "Wired keyboard"
                            } else {
                                "Mouse settings"
                            },
                            20,
                            140,
                            740,
                        );
                        self.label(
                            91,
                            if device.kind == "keyboard" {
                                "No battery · Device settings only"
                            } else {
                                "Battery not monitored · Device settings only"
                            },
                            20,
                            170,
                            740,
                        );
                        self.heading(89, "Device preferences", 20, 212, 740);
                        self.label(92, "Display &name", 20, 255, 120);
                        self.edit(11, &self.configuration_name(&device), 150, 251, 360);
                        self.button(15, "&Save changes", 20, 374, 160);
                        self.button(16, "Reset name", 192, 374, 172);
                    }
                } else {
                    self.label(
                        91,
                        "No devices found. Connect or wake your device, then select Refresh.",
                        20,
                        150,
                        740,
                    );
                    self.page_height = 220 - PAGE_TOP;
                }
                self.polling_controls();
                let footer_y = if self.controls.contains_key(&49) {
                    910
                } else {
                    680
                };
                let message = user_message(&self.error).to_owned();
                self.control(
                    95,
                    w!("STATIC"),
                    &message,
                    WINDOW_STYLE::default(),
                    245,
                    footer_y,
                    535,
                    46,
                );
                self.button(5, "Export support report", 20, footer_y + 45, 205);
            }
            2 => {
                self.heading(90, "Battery history", 20, 68, 740);
                self.label(92, "Device", 20, 106, 360);
                self.label(93, "Time scale", 420, 106, 160);
                self.label(94, "Period", 600, 106, 160);
                self.combo(10, &names, self.selected, 20, 132, 380);
                self.combo(
                    21,
                    &["Time used".into(), "Calendar time".into()],
                    usize::from(self.history.axis == HistoryAxis::Calendar),
                    420,
                    132,
                    160,
                );
                self.combo(
                    20,
                    &self.history.labels(),
                    self.history.index(),
                    600,
                    132,
                    180,
                );
                self.label(96, self.history.description(), 20, 176, 750);
                self.label(
                    97,
                    "History is saved for 30 days. Mouse and keyboard activity is not tracked.",
                    20,
                    735,
                    760,
                );
                self.query();
            }
            3 => {
                // Leave a dedicated Appearance row for disconnected icons.
                let lower = |y| y + 34;
                let value = serde_json::to_value(&self.settings).unwrap_or_default();
                self.heading(90, "Alerts and battery", 20, 70, 740);
                self.heading(91, "Appearance", 20, 252, 740);
                self.heading(92, "Battery checks", 20, lower(412), 740);
                self.heading(93, "Device controls", 20, lower(492), 740);
                for (i, x, y, width) in [
                    (0, 20, 104, 360),
                    (1, 400, 104, 380),
                    (6, 20, 138, 360),
                    (8, 400, 138, 380),
                    (12, 20, 170, 760),
                    (3, 20, 288, 360),
                    (4, 400, 288, 380),
                    (7, 20, 322, 370),
                    (13, 20, 392, 760),
                    (5, 400, 322, 380),
                    (2, 400, lower(448), 380),
                    (9, 20, lower(938), 760),
                    (10, 20, lower(972), 760),
                    (11, 20, lower(1048), 760),
                ] {
                    let (key, label) = CHECKS[i];
                    self.check(
                        setting_check_id(i),
                        label,
                        value[key].as_bool().unwrap_or(false),
                        x,
                        y,
                        width,
                    );
                }
                self.enable_control(111, false);
                self.label(202, "Default low alert (%)", 20, 208, 180);
                self.edit(203, &self.settings.low.to_string(), 210, 204, 80);
                self.label(214, "Orange warning at (%)", 400, 208, 235);
                self.edit(215, &self.settings.warning_level.to_string(), 650, 204, 80);
                self.label(220, "Device-specific alert levels take priority. Check Devices if an alert is missing.",20,236,760);
                self.label(204, "Tray icon colour", 20, 362, 150);
                let themes = THEME_CHOICES
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>();
                let chosen = ["auto", "white", "black", "windows", "topbar"]
                    .iter()
                    .position(|s| *s == self.settings.icon_theme)
                    .unwrap_or(0);
                self.combo(205, &themes, chosen, 170, 358, 215);
                self.check(
                    206,
                    "Start at sign-in",
                    hb_windows::system::is_startup(),
                    400,
                    358,
                    380,
                );
                self.label(200, "Battery check interval (seconds)", 20, lower(452), 240);
                self.edit(
                    201,
                    &self.settings.interval.to_string(),
                    270,
                    lower(448),
                    100,
                );
                self.check(
                    112,
                    "Allow device settings changes",
                    self.settings.polling_controls,
                    20,
                    lower(528),
                    360,
                );
                self.check(
                    113,
                    "Restore saved rates when the app starts",
                    self.settings.restore_polling_on_startup,
                    400,
                    lower(528),
                    380,
                );
                self.control(216, w!("STATIC"),
                    "Enable this to change supported devices' polling rate, sensitivity (DPI) and sleep time on Devices.",
                    WINDOW_STYLE::default(), 20, lower(566), 760, 46);
                self.heading(207, "Device brands to monitor", 20, lower(674), 740);
                for (i, p) in providers().iter().enumerate() {
                    self.check(
                        300 + i as u16,
                        provider_label(p),
                        self.settings.enabled(p),
                        20 + (i as i32 % 4) * 190,
                        lower(708) + (i as i32 / 4) * 28,
                        180,
                    );
                }
                self.button(
                    212,
                    if self.more_options {
                        "Hide advanced options"
                    } else {
                        "Advanced options"
                    },
                    20,
                    lower(622),
                    200,
                );
                self.control(
                    213,
                    w!("STATIC"),
                    "Extra PlayStation checks are optional and may affect other controller apps.",
                    WINDOW_STYLE::default(),
                    20,
                    lower(1004),
                    760,
                    40,
                );
                self.heading(208, "About Halo Continued", 20, lower(1070), 740);
                self.label(
                    218,
                    concat!("Version ", env!("CARGO_PKG_VERSION"), " · Windows 11"),
                    20,
                    lower(1105),
                    440,
                );
                self.button(217, "Open data folder", 570, lower(1100), 210);
                self.edit(
                    209,
                    self.settings
                        .release_repository
                        .clone()
                        .as_deref()
                        .unwrap_or(""),
                    20,
                    lower(1122),
                    740,
                );
                self.label(
                    219,
                    "Closing this window keeps battery monitoring running. Use the tray menu to exit.",
                    20,
                    lower(1156),
                    760,
                );
                self.button(210, "&Save settings", 20, 682, 160);
                self.button(5, "Export support report", 194, 682, 205);
                let message = user_message(&self.error).to_owned();
                self.control(
                    95,
                    w!("STATIC"),
                    &message,
                    WINDOW_STYLE::default(),
                    415,
                    732,
                    365,
                    46,
                );
                self.update_more_options();
            }
            6 => self.build_insights(&names),
            _ => {}
        }
        self.layout_page();
        self.restore_form(draft);
        drop(_redraw);
        if let Some(h) = focused.and_then(|id| self.controls.get(&id))
            && unsafe { IsWindowVisible(*h).as_bool() }
        {
            unsafe {
                let _ = SetFocus(Some(*h));
                if let Some(caret) = caret {
                    send(
                        *h,
                        windows::Win32::UI::Controls::EM_SETSEL,
                        WPARAM(caret as usize & 0xffff),
                        LPARAM((caret >> 16) & 0xffff),
                    );
                }
            }
            // A data/theme rebuild preserves the viewport, even when pointer
            // scrolling deliberately left the focused control outside it.
        }
    }
    fn set_control_text(&self, id: u16, text: &str) {
        if let Some(h) = self.controls.get(&id) {
            if self.text(id) == text {
                return;
            }
            let text = wide(text);
            unsafe {
                let _ = SetWindowTextW(*h, PCWSTR(text.as_ptr()));
            }
        }
    }
    fn build_insights(&mut self, names: &[String]) {
        let key = self
            .snapshot
            .devices
            .get(self.selected)
            .map(|d| d.reading.key.clone());
        self.insights.select(key);
        self.heading(72, "Battery insights", 20, 68, 760);
        self.combo(10, names, self.selected, 20, 104, 760);
        self.widget(
            400,
            "Time left\nLearning\nBased on your usual rate mix",
            20,
            152,
            245,
            126,
        );
        self.widget(
            401,
            "Full charge\nLearning\nEstimated use from 100%",
            278,
            152,
            245,
            126,
        );
        self.widget(
            402,
            "Known polling rate\n—\nOf recorded use in the last 7 days",
            536,
            152,
            244,
            126,
        );
        self.control(
            79,
            w!("STATIC"),
            "",
            WINDOW_STYLE::default(),
            20,
            290,
            760,
            46,
        );
        self.heading(73, "Your polling habits", 20, 348, 760);
        self.widget(
            410,
            "Polling-rate mix · last 7 days\nWaiting for confirmed readings",
            20,
            388,
            370,
            248,
        );
        self.widget(
            411,
            "Automatic boost · last 7 days\nWaiting for recorded changes",
            406,
            388,
            374,
            248,
        );
        self.heading(80, "Battery life by rate", 20, 662, 760);
        self.combo(70, &[], 0, 20, 704, 760);
        self.widget(74, "Learning battery use", 20, 748, 760, 226);
        self.heading(75, "Recent battery sessions", 20, 998, 760);
        self.combo(71, &[], 0, 20, 1040, 760);
        self.widget(76, "No sessions yet", 20, 1084, 760, 260);
        self.control(77,w!("STATIC"),
            "Estimates exclude sleep, charging and gaps. Recorded use is not gaming time.\r\nBattery life varies with how you use the device. No input activity is tracked.",
            WINDOW_STYLE::default(),20,1360,760,48);
        self.control(
            78,
            w!("STATIC"),
            "",
            WINDOW_STYLE::default(),
            20,
            700,
            760,
            60,
        );
        self.render_insights();
    }
    fn query_insights(&mut self) {
        if self.dashboard.is_none() || self.page != 6 {
            return;
        }
        let key = self
            .snapshot
            .devices
            .get(self.selected)
            .map(|d| d.reading.key.clone());
        self.insights.select(key.clone());
        let Some(key) = key else {
            self.insights.status =
                "No device selected. Connect a device and use Refresh under Devices.".into();
            self.render_insights();
            return;
        };
        let Some(request) = self.insights.begin(&key) else {
            return;
        };
        self.insights.status = "Loading battery history…".into();
        self.set_control_text(78, &self.insights.status);
        if self
            .runtime
            .request_insights(key, SystemClock::default().unix(), request)
            .is_err()
        {
            self.insights.abandon();
            self.insights.status = "Battery history is busy. Select Refresh to try again.".into();
            self.set_control_text(78, &self.insights.status);
        }
    }
    fn render_insights(&mut self) {
        if self.dashboard.is_none() || self.page != 6 {
            return;
        }
        let boost = self
            .snapshot
            .devices
            .get(self.selected)
            .and_then(|d| self.settings.device(&d.reading.key))
            .and_then(|p| p.fullscreen_boost_rate)
            .filter(|_| self.settings.polling_controls);
        let _redraw = DashboardRedraw::new(self.dashboard.unwrap());
        self.render_insight_cards(boost);
        let prior_choices = [self.text(70), self.text(71)];
        for id in [70, 71] {
            if let Some(h) = self.controls.get(&id) {
                unsafe {
                    send(*h, CB_RESETCONTENT, WPARAM(0), LPARAM(0));
                }
            }
        }
        if let Some(data) = &self.insights.data {
            for (id, rows) in [
                (
                    70,
                    data.rates.iter().map(rate_insight_row).collect::<Vec<_>>(),
                ),
                (
                    71,
                    data.cycles
                        .iter()
                        .rev()
                        .take(10)
                        .map(charge_cycle_row)
                        .collect(),
                ),
            ] {
                if let Some(h) = self.controls.get(&id) {
                    let prior = prior_choices[usize::from(id == 71)]
                        .split('·')
                        .next()
                        .unwrap_or("")
                        .trim();
                    let selected = rows
                        .iter()
                        .position(|row| row.split('·').next().unwrap_or("").trim() == prior)
                        .unwrap_or(0);
                    for row in rows {
                        let text = wide(&row);
                        unsafe {
                            send(*h, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
                        }
                    }
                    unsafe {
                        send(*h, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
                    }
                }
            }
        }
        for (id, placeholder) in [(70, "Still learning"), (71, "No sessions yet")] {
            if let Some(h) = self.controls.get(&id) {
                let empty = unsafe { send(*h, CB_GETCOUNT, WPARAM(0), LPARAM(0)).0 == 0 };
                self.enable_control(id, !empty);
                if empty {
                    let text = wide(placeholder);
                    unsafe {
                        send(*h, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
                        send(*h, CB_SETCURSEL, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
        self.insights_details();
        self.set_control_text(78, &self.insights.status);
    }
    fn insights_details(&mut self) {
        if self.dashboard.is_none() || self.page != 6 {
            return;
        }
        let _redraw = DashboardRedraw::new(self.dashboard.unwrap());
        let selected = |id| {
            self.controls.get(&id).map_or(0, |h| unsafe {
                send(*h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0.max(0) as usize
            })
        };
        let rate = self
            .insights
            .data
            .as_ref()
            .and_then(|data| data.rates.get(selected(70)));
        let details = rate.map(rate_insight_text).unwrap_or_else(|| {
            let supported = self.snapshot.devices.get(self.selected).is_some_and(|d| {
                hb_providers::controls::polling_menu_candidate(&ConfigurationDevice::from_reading(
                    &d.reading,
                ))
            });
            format!(
                "Battery life by rate\n{}",
                insight_empty_text(self.insights.data.as_ref(), supported)
            )
        });
        self.set_control_text(74, &details);
        let cycle = self
            .insights
            .data
            .as_ref()
            .and_then(|data| data.cycles.iter().rev().nth(selected(71)));
        let text = cycle
            .map(|c| {
                format!(
                    "Battery session\n{}",
                    charge_cycle_text(c)
                        .lines()
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .unwrap_or_else(|| {
                "Recent battery sessions\nNo sessions yet. Keep using your device on battery."
                    .into()
            });
        self.set_control_text(76, &text);
        self.layout_insight_content();
    }
    fn insight_polling_supported(&self) -> bool {
        self.snapshot.devices.get(self.selected).is_some_and(|d| {
            self.has_polling_controls(&ConfigurationDevice::from_reading(&d.reading))
        })
    }
    fn layout_insight_content(&mut self) {
        if self.page != 6 || self.page_host.is_none() {
            return;
        }
        let polling = self.insight_polling_supported();
        let position = |state: &Self, id, x, y, width, height| {
            state.position_control(id, x, y - PAGE_TOP - state.page_scroll, width, height);
        };
        let height = |id, width, minimum| {
            self.controls.get(&id).map_or(minimum, |h| {
                crate::dashboard_widgets::panel_height(*h, &self.text(id), width, id == 410)
                    .max(minimum)
            })
        };
        for id in [73, 410, 411, 80, 70, 74] {
            if let Some(control) = self.controls.get(&id) {
                unsafe {
                    let _ = ShowWindow(*control, if polling { SW_SHOW } else { SW_HIDE });
                }
            }
        }
        let session_y = if polling {
            // Missing-rate guidance wraps to three lines. Keep the next
            // section below it at every supported dashboard DPI.
            let prediction = self
                .insights
                .data
                .as_ref()
                .map(|d| d.adaptive_prediction(None, None))
                .unwrap_or_default();
            let explanation_height =
                if insight_missing_rate_data(self.insights.data.as_ref(), &prediction) {
                    72
                } else {
                    46
                };
            position(self, 79, 20, 290, 760, explanation_height);
            let habits_y = 388 + explanation_height - 46;
            position(self, 73, 20, habits_y - 40, 760, 28);
            let habits_height = height(410, 370, 172).max(height(411, 374, 172));
            position(self, 410, 20, habits_y, 370, habits_height);
            position(self, 411, 406, habits_y, 374, habits_height);
            let rate_y = habits_y + habits_height + 26;
            position(self, 80, 20, rate_y, 760, 28);
            position(self, 70, 20, rate_y + 42, 760, 280);
            let rate_height = height(74, 760, 160);
            position(self, 74, 20, rate_y + 86, 760, rate_height);
            rate_y + 86 + rate_height + 24
        } else {
            348
        };
        position(self, 75, 20, session_y, 760, 28);
        position(self, 71, 20, session_y + 42, 760, 280);
        let session_height = height(76, 760, 170);
        position(self, 76, 20, session_y + 86, 760, session_height);
        let note_y = session_y + 86 + session_height + 16;
        position(self, 77, 20, note_y, 760, 48);
        self.page_height = note_y + 64 - PAGE_TOP;
        self.layout_page();
    }
    fn render_insight_cards(&self, boost: Option<PollingRate>) {
        let data = self.insights.data.as_ref();
        if !self.insight_polling_supported() {
            let device = self.snapshot.devices.get(self.selected);
            let remaining = device
                .filter(|d| d.reading.online() && d.reading.charging != Some(true))
                .and_then(|d| d.seconds_left);
            let estimate = remaining
                .map(|s| insight_estimate(Some(s as f64 / 3600.0)))
                .unwrap_or_else(|| {
                    if device.is_some() {
                        "Not available".into()
                    } else {
                        "—".into()
                    }
                });
            self.set_control_text(
                400,
                &format!("Time left\n{estimate}\nBased on recorded battery use"),
            );
            self.set_control_text(
                401,
                &format!(
                    "Recorded use\n{}\nOver the last 30 days",
                    data.map(|d| insight_hours(d.coverage.awake_seconds))
                        .unwrap_or_else(|| "—".into())
                ),
            );
            self.set_control_text(
                402,
                &format!(
                    "Battery level\n{}\nLatest available reading",
                    device
                        .and_then(|d| d.reading.level)
                        .map(|p| format!("{p}%"))
                        .unwrap_or_else(|| "—".into())
                ),
            );
            self.set_control_text(79, if device.is_some() {
                "Polling-rate comparisons are unavailable for this device. Its recorded battery use and sessions appear below."
            } else { "Connect or wake a battery-powered device, then select Refresh to get started." });
            return;
        }
        let prediction = data
            .map(|d| d.adaptive_prediction(boost, None))
            .unwrap_or_default();
        let estimate = |hours: Option<f64>| {
            hours.map(|h| insight_estimate(Some(h))).unwrap_or_else(|| {
                if insight_missing_rate_data(data, &prediction) {
                    "Missing rate data".into()
                } else {
                    "Learning".into()
                }
            })
        };
        self.set_control_text(
            400,
            &format!(
                "Time left\n{}\nWith your recent polling-rate mix",
                estimate(prediction.remaining_hours)
            ),
        );
        self.set_control_text(
            401,
            &format!(
                "Full charge\n{}\nEstimated use from 100%",
                estimate(prediction.full_hours)
            ),
        );
        self.set_control_text(
            402,
            &format!(
                "Known polling rate\n{}\nOf recorded use in the last 7 days",
                data.filter(|d| d.recent.total_seconds > 0)
                    .map(|_| format!("{:.0}%", prediction.coverage_percent))
                    .unwrap_or_else(|| "—".into())
            ),
        );
        let explanation =
            insight_prediction_explanation(data, &prediction, self.settings.polling_controls);
        self.set_control_text(79, &explanation);
        let Some(data) = data else {
            self.set_control_text(
                410,
                "Polling-rate mix · last 7 days\nNo history loaded. Select Refresh to try again.",
            );
            self.set_control_text(411, "Automatic boost · last 7 days\nNo history loaded.");
            return;
        };
        let recent = &data.recent;
        let confirmed: u64 = recent.rates.iter().sum();
        let rows = INSIGHT_RATES
            .iter()
            .zip(recent.rates)
            .filter(|(_, s)| *s > 0)
            .map(|(hz, seconds)| {
                format!(
                    "{hz} Hz · {:.0}%",
                    seconds as f64 / confirmed.max(1) as f64 * 100.0
                )
            })
            .collect::<Vec<_>>();
        self.set_control_text(
            410,
            &format!(
                "Polling-rate mix · last 7 days\n{}",
                if rows.is_empty() {
                    "No confirmed rate use yet".into()
                } else {
                    rows.join("\n")
                }
            ),
        );
        let comparison = match (
            prediction.full_hours,
            prediction.normal_full_hours,
            prediction.normal_rate,
        ) {
            (Some(mix), Some(normal), Some(rate)) => format!(
                "{} Hz only: {:.0} h · Your mix: {:.0} h",
                rate.hz(),
                normal,
                mix
            ),
            _ => "Fixed-rate comparison is still learning".into(),
        };
        self.set_control_text(411,&format!("Automatic boost · last 7 days\nBoosts · {}    Restores · {}\n{} during confirmed boosts\nManual choices · {}\n{}\n{}\nRecorded use is not gaming time",recent.boost_sessions,recent.restores,insight_hours(recent.boost_seconds),recent.manual_changes,
            boost.map(|r|format!("Boost setting · {} Hz",r.hz())).unwrap_or_else(||"Current setting · Off".into()),comparison));
    }
    fn save(&mut self) {
        self.runtime.send(Command::Settings(self.settings.clone()));
        self.sync_trays(TrayUpdate::Redraw);
    }
    fn command(&mut self, id: u16, notification: u16) {
        match id {
            1..=3 | 6 => {
                self.error.clear();
                self.polling.abandon();
                self.insights.abandon();
                self.page = id;
                self.page_scroll = 0;
                self.configuration_visibility();
                self.build();
                self.read_polling();
                self.query_insights()
            }
            4 => {
                if self.page == 6 {
                    self.query_insights();
                    return;
                }
                self.runtime.send(Command::Refresh);
                if self.page == 1 {
                    self.runtime.send(Command::ConfigurationRefresh);
                    self.read_polling();
                }
                if self.page == 2 {
                    self.query()
                }
            }
            5 => {
                let path = self.dir.join("diagnostics.json");
                let readings: BTreeMap<_, _> = self.polling.observations.iter()
                    .map(|(key, reading)| (key, serde_json::json!({"rate":reading.rate,
                        "supported":reading.supported,"timestamp":reading.timestamp,"evidence":reading.evidence})))
                    .collect();
                let value = serde_json::json!({"data_folder":self.dir,"snapshot":self.snapshot,"providers":self.diagnostics,"settings":self.settings,"application_error":self.error,
                    "polling_messages":self.polling.status,"tray_polling_messages":self.tray_polling.status,
                    "polling_readings":readings,"dpi_debug":format!("{:?}",self.dpi.outcome),"sleep_debug":format!("{:?}",self.sleep.outcome),"insights_debug":format!("{:?}",self.insights.data)});
                self.error = match hb_storage::atomic_write(
                    &path,
                    &serde_json::to_vec_pretty(&value).unwrap(),
                ) {
                    Ok(()) => format!("Exported {}", path.display()),
                    Err(e) => e.to_string(),
                };
                self.set_control_text(95, user_message(&self.error));
            }
            10 if notification == CBN_SELCHANGE as u16 => {
                self.polling.abandon();
                self.insights.abandon();
                if self.page == 1 {
                    self.selected_device = self
                        .device_rows()
                        .get(self.choice(10))
                        .map(|(d, _)| d.key.clone());
                    if let Some(index) = self
                        .snapshot
                        .devices
                        .iter()
                        .position(|d| Some(&d.reading.key) == self.selected_device.as_ref())
                    {
                        self.selected = index;
                    }
                } else {
                    self.selected = self.choice(10);
                }
                self.page_scroll = 0;
                self.build();
                self.read_polling();
                self.query_insights()
            }
            70 | 71 if notification == CBN_SELCHANGE as u16 => self.insights_details(),
            432 => self.request_sleep(false),
            433 => self.request_sleep(true),
            452 => {
                self.queue_dpi(None);
                self.polling_controls();
            }
            453 => match dpi_input(&self.text(451), self.dpi_limits().as_ref()) {
                Ok(value) => {
                    self.queue_dpi(Some(value));
                    self.polling_controls();
                }
                Err(message) => self.set_control_text(455, &message),
            },
            454 => {
                if let Some(previous) = self.restorable_dpi() {
                    self.queue_dpi(Some(previous));
                    self.polling_controls();
                }
            }
            446 => self.save_battery_saver(),
            51 => {
                if let Some(device) = self.visible_polling_device() {
                    let rate = if self.checked(49) {
                        self.polling.observations.get(&device.key).and_then(|o| {
                            o.supported
                                .iter()
                                .filter(|r| r.hz() > 1000)
                                .nth(self.choice(50))
                                .copied()
                        })
                    } else {
                        None
                    };
                    if self.checked(49) && rate.is_none() {
                        return;
                    }
                    self.settings.device_mut(&device.key).fullscreen_boost_rate = rate;
                    self.save();
                    self.error = "Automatic boost settings saved".into();
                    self.set_control_text(95, user_message(&self.error));
                }
            }
            53 => {
                self.command(3, 0);
                if let Some(control) = self.controls.get(&112) {
                    unsafe {
                        let _ = SetFocus(Some(*control));
                    }
                    self.keep_focus_visible();
                }
            }
            40 => self.apply_polling(false),
            41 => self.read_polling(),
            42 => self.apply_polling(true),
            20 if notification == CBN_SELCHANGE as u16 => {
                self.history.set_index(self.choice(20));
                self.build()
            }
            21 if notification == CBN_SELCHANGE as u16 => {
                self.history.axis = if self.choice(21) == 0 {
                    HistoryAxis::Usage
                } else {
                    HistoryAxis::Calendar
                };
                self.build()
            }
            15 => {
                if let Some((device, battery)) = self.current_device()
                    && battery.is_none()
                {
                    let name = self.text(11);
                    self.settings.device_mut(&device.key).name =
                        hb_core::settings::device_name(&name);
                    self.save();
                    self.error = "Device settings saved".into();
                    self.set_control_text(95, user_message(&self.error));
                    self.build();
                    return;
                }
                if let Some(d) = self.current_device().and_then(|(_, battery)| battery) {
                    let key = d.reading.key.clone();
                    let name = self.text(11);
                    let hidden = self.checked(12);
                    let low = match device_threshold(&self.text(13)) {
                        Ok(low) => low,
                        Err(e) => {
                            self.invalid_field(13, e);
                            return;
                        }
                    };
                    let choice = self.choice(14);
                    let icon = if choice == 0 {
                        None
                    } else {
                        Some(
                            [
                                "mouse",
                                "keyboard",
                                "headset",
                                "gamepad",
                                "bluetooth",
                                "dualshock",
                                "dualsense",
                            ][(choice - 1).min(6)]
                            .to_string(),
                        )
                    };
                    self.settings.set_device(
                        &key,
                        DevicePreferences {
                            low_battery_mode: self
                                .settings
                                .device(&key)
                                .and_then(|p| p.low_battery_mode),
                            name: hb_core::settings::device_name(&name),
                            hidden,
                            icon,
                            low,
                            fullscreen_boost_rate: self
                                .settings
                                .device(&key)
                                .and_then(|p| p.fullscreen_boost_rate),
                            requested_polling_rate: self
                                .settings
                                .device(&key)
                                .and_then(|p| p.requested_polling_rate),
                        },
                    );
                    self.save();
                    self.error = "Device settings saved".into();
                    self.set_control_text(96, &device_alert_summary(&self.settings, &key));
                    self.set_control_text(95, user_message(&self.error));
                }
            }
            16 => {
                if let Some((device, battery)) = self.current_device() {
                    let mut draft = self.capture_form(true);
                    draft.text.retain(|(id, _)| !matches!(id, 11 | 13));
                    draft.checks.retain(|(id, _)| *id != 12);
                    draft.choices.retain(|(id, _)| *id != 14);
                    if battery.is_some() {
                        let preferences = self.settings.device_mut(&device.key);
                        preferences.name = None;
                        preferences.hidden = false;
                        preferences.icon = None;
                        preferences.low = None;
                    } else {
                        self.settings.device_mut(&device.key).name = None;
                    }
                    self.save();
                    self.device_form_key = None;
                    self.error = "Device preferences reset".into();
                    self.build();
                    self.restore_form(draft);
                }
            }
            210 => {
                let mut value = serde_json::to_value(&self.settings).unwrap();
                for (i, (key, _)) in CHECKS.iter().enumerate() {
                    value[*key] = self.checked(setting_check_id(i)).into()
                }
                let Some(interval) = settings_number(&self.text(201), 5, 3600) else {
                    self.invalid_field(
                        201,
                        "Enter a battery check interval from 5 to 3600 seconds.",
                    );
                    return;
                };
                let Some(low) = settings_number(&self.text(203), 0, 100) else {
                    self.invalid_field(
                        203,
                        "Enter a default low-battery alert from 0 to 100% (0 turns it off).",
                    );
                    return;
                };
                let Some(warning) = settings_number(&self.text(215), 0, 100) else {
                    self.invalid_field(215, "Enter an orange warning level from 0 to 100%.");
                    return;
                };
                value["polling_controls"] = self.checked(112).into();
                value["restore_polling_on_startup"] = self.checked(113).into();
                value["interval"] = interval.into();
                value["low"] = low.into();
                value["warning_level"] = warning.into();
                value["icon_theme"] =
                    ["auto", "white", "black", "windows", "topbar"][self.choice(205).min(4)].into();
                let repo = self.text(209);
                if !repo.is_empty() && !hb_core::settings::valid_repository(&repo) {
                    self.error = "Enter the GitHub account and project as account/project.".into();
                    self.set_control_text(95, user_message(&self.error));
                    return;
                }
                value["release_repository"] = if repo.is_empty() {
                    serde_json::Value::Null
                } else {
                    repo.into()
                };
                self.settings = Settings::from_value(value).unwrap();
                self.settings.disabled_providers = providers()
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !self.checked(300 + *i as u16))
                    .map(|(_, p)| p.to_string())
                    .collect();
                self.error = match hb_windows::system::startup(self.checked(206)) {
                    Ok(()) => "Settings saved".into(),
                    Err(e) => e.to_string(),
                };
                self.save();
                self.polling.abandon();
                if !self.settings.polling_controls {
                    self.tooltip_rates.clear();
                    self.polling.observations.clear();
                    self.tray_polling.invalidate(self.tray_polling.generation);
                }
                self.build()
            }
            217 => {
                let folder = wide(&self.dir.to_string_lossy());
                unsafe {
                    ShellExecuteW(
                        self.dashboard,
                        w!("open"),
                        PCWSTR(folder.as_ptr()),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
            }
            212 => {
                self.more_options = !self.more_options;
                self.update_more_options();
                self.keep_focus_visible();
            }
            503 => {
                self.page = 1;
                self.configuration_visibility();
                self.open();
                self.build();
            }
            506 => {
                self.page = 6;
                self.page_scroll = 0;
                self.configuration_visibility();
                self.open();
                self.build();
                self.query_insights();
            }
            504 => {
                self.page = 2;
                self.configuration_visibility();
                self.open();
                self.build();
            }
            505 => {
                if let Some(d) = self.snapshot.devices.get(self.selected) {
                    self.settings.device_mut(&d.reading.key).hidden = true;
                    self.save();
                }
            }
            600..=999 => {
                self.selected = (id - 600) as usize;
                self.selected_device = self
                    .snapshot
                    .devices
                    .get(self.selected)
                    .map(|d| d.reading.key.clone());
                self.page = 1;
                self.configuration_visibility();
                self.open();
                self.build();
            }
            500 => self.open(),
            501 => self.runtime.send(Command::Refresh),
            502 => unsafe { PostQuitMessage(0) },
            _ => {}
        }
    }
    fn configuration_inventory(
        &mut self,
        generation: u64,
        devices: Vec<ConfigurationDevice>,
        failure: Option<String>,
    ) {
        if generation < self.configuration_generation {
            return;
        }
        for device in &devices {
            self.settings.link_device(device);
        }
        self.configuration_generation = generation;
        let failure_changed = failure != self.configuration_failure;
        if let Some(error) = &failure {
            self.error = error.clone();
        } else if self
            .configuration_failure
            .as_ref()
            .is_some_and(|previous| &self.error == previous)
        {
            self.error.clear();
        }
        self.configuration_failure = failure;
        if devices == self.configuration_devices {
            if failure_changed && self.dashboard.is_some() && self.page == 1 {
                self.set_control_text(95, user_message(&self.error));
            }
            return;
        }
        let previous = self.current_device().map(|(d, _)| d);
        self.configuration_devices = devices;
        let current = self.current_device().map(|(d, _)| d);
        self.selected_device = current.as_ref().map(|d| d.key.clone());
        let selection_changed = previous != current;
        if selection_changed {
            self.polling.abandon();
            if previous
                .as_ref()
                .zip(current.as_ref())
                .is_some_and(|(before, after)| {
                    !DeviceDescriptorRef::from(before)
                        .same_identity(DeviceDescriptorRef::from(after))
                })
            {
                self.invalidate_mouse_readbacks();
            }
        }
        self.polling
            .retain_inventory(&self.snapshot.devices, &self.configuration_devices);
        if self.dashboard.is_some() && self.page == 1 {
            self.build();
            if selection_changed {
                self.read_polling();
            }
        }
    }
    fn configuration_visibility(&self) {
        self.runtime.send(Command::ConfigurationVisible(
            self.dashboard_visible() && self.page == 1,
        ));
    }
    fn invalidate_mouse_readbacks(&mut self) {
        // Keep request counters monotonic so late results cannot match a new
        // device's request. Preserve DPI backups on disk for the original unit.
        self.dpi = DpiUi {
            next: self.dpi.next,
            ..DpiUi::default()
        };
        self.sleep = SleepUi {
            next: self.sleep.next,
            ..SleepUi::default()
        };
        self.device_form_key = None;
    }
    fn dashboard_visible(&self) -> bool {
        self.dashboard.is_some_and(|hwnd| unsafe {
            IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
        })
    }
    fn device_rows(&self) -> Vec<(ConfigurationDevice, Option<DeviceView>)> {
        merged_device_rows(&self.snapshot.devices, &self.configuration_devices)
    }
    fn current_device(&self) -> Option<(ConfigurationDevice, Option<DeviceView>)> {
        current_device_row(
            &self.snapshot.devices,
            &self.configuration_devices,
            self.selected_device.as_deref(),
        )
    }
    fn configuration_name(&self, device: &ConfigurationDevice) -> String {
        self.settings
            .device(&device.key)
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| device.name.clone())
    }
    fn has_polling_controls(&self, device: &ConfigurationDevice) -> bool {
        // A supported model with an unavailable connection still needs its recovery message.
        // This is a visibility hint only; the original capability guards every request.
        let mut model = device.clone();
        model.capability = PollingCapability::ReadWrite;
        hb_providers::controls::polling_menu_candidate(&model)
            || (device.kind == "keyboard" && self.configuration_devices.iter().any(|d| d == device))
    }
    fn visible_polling_device(&self) -> Option<ConfigurationDevice> {
        (self.dashboard.is_some() && self.page == 1 && self.settings.polling_controls)
            .then(|| self.current_device().map(|(d, _)| d))
            .flatten()
            .filter(|device| self.has_polling_controls(device))
    }
    fn invalid_field(&mut self, id: u16, message: &str) {
        self.error = message.into();
        self.set_control_text(95, message);
        if let Some(control) = self.controls.get(&id) {
            unsafe {
                let _ = SetFocus(Some(*control));
                send(
                    *control,
                    windows::Win32::UI::Controls::EM_SETSEL,
                    WPARAM(0),
                    LPARAM(-1),
                );
            }
            self.keep_focus_visible();
        }
    }
    fn enable_control(&self, id: u16, enabled: bool) {
        if let Some(h) = self.controls.get(&id) {
            unsafe {
                let _ = EnableWindow(*h, enabled);
            }
        }
    }
    fn polling_controls(&mut self) {
        if self.dashboard.is_none() || self.page != 1 {
            return;
        }
        let _redraw = DashboardRedraw::new(self.dashboard.unwrap());
        let focused = unsafe { GetFocus() };
        let focus_id = self
            .controls
            .iter()
            .find_map(|(id, h)| (*h == focused).then_some(*id));
        let caret = matches!(focus_id, Some(442 | 451)).then(|| unsafe {
            send(
                focused,
                windows::Win32::UI::Controls::EM_GETSEL,
                WPARAM(0),
                LPARAM(0),
            )
            .0
        });
        self.page_height = if self.current_device().is_some() {
            426
        } else {
            220
        } - PAGE_TOP;
        self.build_polling_controls();
        self.build_dpi_controls();
        self.build_sleep_controls();
        self.build_battery_saver();
        self.layout_page();
        drop(_redraw);
        if let Some(control) = focus_id.and_then(|id| self.controls.get(&id)) {
            unsafe {
                if IsWindowVisible(*control).as_bool()
                    && windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(*control)
                        .as_bool()
                {
                    let _ = SetFocus(Some(*control));
                    if let Some(caret) = caret {
                        send(
                            *control,
                            windows::Win32::UI::Controls::EM_SETSEL,
                            WPARAM((caret as usize) & 0xffff),
                            LPARAM((caret >> 16) & 0xffff),
                        );
                    }
                }
            }
        }
    }
    fn build_polling_controls(&mut self) {
        let rate_draft = self.controls.contains_key(&43).then(|| self.text(43));
        let boost_draft = self
            .controls
            .contains_key(&49)
            .then(|| (self.checked(49), self.text(50)));
        // Update only this group: an asynchronous hardware reply must not discard unsaved device edits.
        for id in (40..=53).chain([98, 99]) {
            if let Some(h) = self.controls.remove(&id) {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
        }
        if !self
            .current_device()
            .is_some_and(|(d, _)| self.has_polling_controls(&d))
        {
            return;
        }
        self.page_height = 708 - PAGE_TOP;
        self.heading(98, "Polling rate", 20, 434, 750);
        self.control(
            99,
            w!("STATIC"),
            "How often your device sends updates. Higher rates can use more battery.\r\nClose games before changing the rate.",
            WINDOW_STYLE::default(),
            20,
            468,
            750,
            38,
        );
        if !self.settings.polling_controls {
            self.control(
                44,
                w!("STATIC"),
                &self
                    .current_device()
                    .and_then(|(d, _)| match &d.capability {
                        PollingCapability::Unavailable(_) => {
                            Some(polling_unavailable(&d).to_owned())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        "Turn on Allow device settings changes in Settings to use these controls."
                            .into()
                    }),
                WINDOW_STYLE::default(),
                20,
                510,
                750,
                48,
            );
            if self
                .current_device()
                .is_some_and(|(d, _)| matches!(d.capability, PollingCapability::ReadWrite))
            {
                self.button(53, "Open Settings", 20, 572, 175);
            }
            self.page_height = 626 - PAGE_TOP;
            return;
        }
        let Some(reading) = self.visible_polling_device() else {
            self.control(
                44,
                w!("STATIC"),
                "Choose a connected device to check its polling rate.",
                WINDOW_STYLE::default(),
                20,
                510,
                750,
                48,
            );
            return;
        };
        let available =
            reading.online() && matches!(reading.capability, PollingCapability::ReadWrite);
        let unavailable = match &reading.capability {
            PollingCapability::Unavailable(_) => Some(polling_unavailable(&reading).to_owned()),
            _ if !reading.online() => {
                Some("Wake or reconnect the device to check its rate.".into())
            }
            _ => None,
        };
        let observation = self.polling.observations.get(&reading.key).cloned();
        let boost_supported = boost_controls_visible(
            &reading,
            observation.as_ref(),
            self.settings
                .device(&reading.key)
                .and_then(|p| p.fullscreen_boost_rate),
        );
        let key = reading.key;
        let current = observation.as_ref().and_then(|o| o.rate);
        self.label(
            44,
            &format!(
                "Current rate · {}",
                current.map_or_else(|| "not checked yet".into(), |r| format!("{} Hz", r.hz()))
            ),
            20,
            510,
            750,
        );
        let evidence = observation.as_ref().map_or_else(
            || {
                unavailable
                    .clone()
                    .unwrap_or_else(|| "Use Refresh rate to read this device.".into())
            },
            |o| {
                format!(
                    "Checked {} · Refresh after changing the rate in another app.",
                    polling_timestamp(o.timestamp),
                )
            },
        );
        self.control(
            45,
            w!("STATIC"),
            &evidence,
            WINDOW_STYLE::default(),
            20,
            542,
            750,
            40,
        );
        let requested = self
            .settings
            .device(&key)
            .and_then(|p| p.requested_polling_rate);
        self.label(
            46,
            &format!(
                "Startup choice · {} (when restore at startup is enabled).",
                requested.map_or_else(|| "none".into(), |r| format!("{} Hz", r.hz()))
            ),
            20,
            584,
            750,
        );
        let supported = observation
            .as_ref()
            .map_or_else(Vec::new, |o| o.supported.clone());
        let chosen = requested
            .or(current)
            .and_then(|r| supported.iter().position(|v| *v == r))
            .unwrap_or(0);
        let options = supported
            .iter()
            .map(|r| format!("{} Hz", r.hz()))
            .collect::<Vec<_>>();
        let chosen = rate_draft
            .as_ref()
            .and_then(|draft| options.iter().position(|option| option == draft))
            .unwrap_or(chosen);
        self.combo(43, &options, chosen, 20, 616, 230);
        self.button(40, "&Apply rate", 270, 616, 120);
        self.button(41, "Refresh rate", 402, 616, 140);
        self.button(42, "Restore previous rate", 554, 616, 226);
        let pending = self.polling.pending.is_some() || !available;
        let verified = observation
            .as_ref()
            .is_some_and(|o| o.target.generation == self.polling.generation && o.rate.is_some());
        self.enable_control(43, !pending && verified && !supported.is_empty());
        self.enable_control(40, !pending && verified && !supported.is_empty());
        self.enable_control(41, !pending);
        let restore = self
            .polling
            .previous
            .get(&key)
            .is_some_and(|r| supported.contains(r) && current != Some(*r));
        self.enable_control(42, !pending && verified && restore);
        let status = self.polling.status.get(&key).cloned().unwrap_or_else(|| {
            if verified {
                "Rate checked. Select Apply to change it.".into()
            } else {
                "Hardware rate has not been read in this session.".into()
            }
        });
        self.control(
            47,
            w!("STATIC"),
            polling_message(&status),
            WINDOW_STYLE::default(),
            20,
            660,
            750,
            28,
        );
        if boost_supported {
            self.page_height = 920 - PAGE_TOP;
            self.heading(48, "Automatic fullscreen boost", 20, 708, 750);
            let saved = self
                .settings
                .device(&key)
                .and_then(|p| p.fullscreen_boost_rate);
            self.check(
                49,
                "Boost this mouse in fullscreen games",
                boost_draft.as_ref().map_or(saved.is_some(), |d| d.0),
                20,
                750,
                750,
            );
            let rates: Vec<_> = supported
                .iter()
                .filter(|r| r.hz() > 1000)
                .copied()
                .collect();
            let labels: Vec<_> = rates.iter().map(|r| format!("{} Hz", r.hz())).collect();
            let selected = boost_draft
                .as_ref()
                .and_then(|d| labels.iter().position(|label| label == &d.1))
                .or_else(|| saved.and_then(|r| rates.iter().position(|v| *v == r)))
                .unwrap_or(0);
            self.combo(50, &labels, selected, 20, 792, 230);
            self.button(51, "Save boost settings", 270, 792, 210);
            self.enable_control(49, !pending);
            self.enable_control(50, !pending && !rates.is_empty());
            self.enable_control(51, !pending && (!rates.is_empty() || saved.is_some()));
            self.control(52, w!("STATIC"),
                "Uses Windows fullscreen gaming detection; some borderless games are missed.\r\nBoosts after 10 seconds; restores after 15 seconds back on the desktop.\r\nKeep this app running for restoration. Anti-cheat compatibility is not guaranteed.",
                WINDOW_STYLE::default(), 20, 838, 750, 62);
        }
    }
    fn restorable_dpi(&self) -> Option<DpiValue> {
        let (device, _) = self.current_device()?;
        let (key, generation, previous) = self.dpi.previous.as_ref()?;
        let outcome = self.dpi.outcome.as_ref()?;
        let current = outcome.current.as_ref()?;
        (*key == device.key
            && *generation == self.polling.generation
            && outcome.key == device.key
            && outcome.generation == *generation
            && outcome.failure.is_none()
            && current.profile == previous.profile
            && current.stage == previous.stage)
            .then_some(previous.value)
    }
    fn dpi_limits(&self) -> Option<DpiLimits> {
        let (device, _) = self.current_device()?;
        self.dpi
            .outcome
            .as_ref()
            .filter(|o| o.key == device.key && o.generation == self.polling.generation)
            .and_then(|o| o.limits.clone())
            .or_else(|| hb_providers::dpi::model_limits(&device))
    }
    fn queue_dpi(&mut self, value: Option<DpiValue>) {
        if !self.runtime.configuration_visible() {
            return;
        }
        let Some((device, _)) = self.current_device() else {
            return;
        };
        if self.dpi.pending.is_some()
            || !device.online()
            || !self.settings.polling_controls
            || !hb_providers::dpi::candidate(&device)
        {
            return;
        }
        let expected = self
            .dpi
            .outcome
            .as_ref()
            .filter(|o| {
                o.key == device.key
                    && o.generation == self.polling.generation
                    && o.failure.is_none()
            })
            .and_then(|o| o.current.clone());
        if value.is_some() && expected.is_none() {
            return;
        }
        self.dpi.next = self.dpi.next.wrapping_add(1);
        self.dpi.attempted = Some((device.key.clone(), self.polling.generation));
        let request = DpiRequest {
            request: self.dpi.next,
            target: ControlTarget {
                device: device.clone(),
                generation: self.polling.generation,
                connection_stamp: None,
            },
            value,
            expected,
        };
        match self.runtime.submit_dpi(request.clone()) {
            Ok(()) => {
                self.dpi.pending = Some((request.request, device.key));
                self.dpi.outcome = None;
            }
            Err(e) => self.dpi.outcome = Some(DpiOutcome::failed(&request, e.message)),
        }
    }
    fn build_dpi_controls(&mut self) {
        let draft = self.controls.contains_key(&451).then(|| self.text(451));
        for id in 450..=458 {
            if let Some(h) = self.controls.remove(&id) {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
        }
        let Some((device, _)) = self.current_device() else {
            return;
        };
        if device.kind != "mouse" || !hb_providers::dpi::candidate(&device) {
            return;
        }
        if self.dpi.attempted.as_ref() != Some(&(device.key.clone(), self.polling.generation))
            && self.dpi.pending.is_none()
        {
            self.queue_dpi(None);
        }
        let y = self.page_height + PAGE_TOP + 12;
        self.page_height = y + 276 - PAGE_TOP;
        self.heading(450, "Sensitivity (DPI)", 20, y, 750);
        let observed = self
            .dpi
            .outcome
            .as_ref()
            .filter(|o| o.key == device.key && o.generation == self.polling.generation)
            .cloned();
        let current = observed.as_ref().and_then(|o| o.current.as_ref());
        let display = current
            .map(|s| {
                if s.value.x == s.value.y {
                    format!("Current DPI: {}", s.value.x)
                } else {
                    format!(
                        "Current DPI: {} horizontal / {} vertical",
                        s.value.x, s.value.y
                    )
                }
            })
            .unwrap_or_else(|| "Current DPI: Not read yet".into());
        self.label(457, &display, 20, y + 38, 750);
        let limits = observed
            .as_ref()
            .and_then(|o| o.limits.clone())
            .or_else(|| hb_providers::dpi::model_limits(&device));
        self.control(
            458,
            w!("STATIC"),
            &dpi_limits_text(limits.as_ref()),
            WINDOW_STYLE::default(),
            20,
            y + 67,
            750,
            48,
        );
        let default = current.map(|s| s.value.x.to_string()).unwrap_or_default();
        let text = if self.dpi.shown_key.as_ref() == Some(&device.key) {
            draft.unwrap_or(default)
        } else {
            default
        };
        self.dpi.shown_key = Some(device.key.clone());
        self.edit(451, &text, 20, y + 126, 150);
        self.button(453, "Apply DPI", 190, y + 126, 150);
        self.button(452, "Read current", 355, y + 126, 150);
        self.button(454, "Restore previous DPI", 520, y + 126, 240);
        let ready = self.settings.polling_controls && device.online() && self.dpi.pending.is_none();
        self.enable_control(451, ready);
        self.enable_control(452, ready);
        self.enable_control(453, ready && current.is_some());
        let restore = self.restorable_dpi().is_some();
        self.enable_control(454, ready && restore);
        let status = if self.dpi.pending.is_some() {
            "Contacting the mouse…".into()
        } else if !self.settings.polling_controls {
            "Allow device settings changes in Settings first.".into()
        } else if let Some(o) = &observed {
            if let Some(e) = &o.failure {
                device_control_error(e, o.may_have_changed, "DPI", true)
            } else if o.may_have_changed {
                "DPI updated and checked. The original was saved in the app's dpi-backups folder."
                    .into()
            } else {
                "DPI checked. Enter a value and select Apply DPI to change it.".into()
            }
        } else {
            "Wake the mouse and select Read current.".into()
        };
        self.control(
            455,
            w!("STATIC"),
            &status,
            WINDOW_STYLE::default(),
            20,
            y + 174,
            750,
            48,
        );
        self.control(456,w!("STATIC"),"Higher DPI moves the pointer farther. Apply changes both directions together.\r\nClose games first. Your original sensitivity is saved before each change.",WINDOW_STYLE::default(),20,y+226,750,48);
    }
    fn build_sleep_controls(&mut self) {
        let draft = self.controls.contains_key(&431).then(|| self.choice(431));
        for id in 430..=435 {
            if let Some(h) = self.controls.remove(&id) {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
        }
        let Some((device, _)) = self.current_device() else {
            return;
        };
        if device.kind != "mouse" {
            return;
        }
        let y = self.page_height + PAGE_TOP + 12;
        self.page_height = y + 214 - PAGE_TOP;
        self.heading(430, "Sleep when idle", 20, y, 750);
        let Some(model) = hb_providers::sleep_controls::model(&device) else {
            self.page_height = y + 120 - PAGE_TOP;
            self.control(
                434,
                w!("STATIC"),
                hb_providers::sleep_controls::unavailable(&device),
                WINDOW_STYLE::default(),
                20,
                y + 42,
                750,
                60,
            );
            return;
        };
        let choices = model.choices();
        let labels: Vec<_> = choices
            .iter()
            .map(|seconds| {
                let m = seconds / 60;
                if m == 0 {
                    "Never".into()
                } else {
                    format!("{m} min")
                }
            })
            .collect();
        let observed = self
            .sleep
            .outcome
            .as_ref()
            .filter(|o| o.key == device.key && o.generation == self.polling.generation)
            .cloned();
        let selected = observed
            .as_ref()
            .and_then(|o| o.seconds)
            .and_then(|s| choices.iter().position(|c| *c == s))
            .unwrap_or_else(|| choices.iter().position(|c| *c == 300).unwrap_or(0));
        let selected = if self.sleep.shown_key.as_ref() == Some(&device.key) {
            draft.unwrap_or(selected)
        } else {
            selected
        };
        self.sleep.shown_key = Some(device.key.clone());
        self.combo(
            431,
            &labels,
            selected.min(labels.len() - 1),
            20,
            y + 46,
            230,
        );
        self.button(433, "Apply sleep time", 270, y + 46, 180);
        self.button(432, "Read current", 470, y + 46, 180);
        let can = self.settings.polling_controls && device.online() && self.sleep.pending.is_none();
        self.enable_control(431, can);
        self.enable_control(433, can);
        self.enable_control(432, can && model.readable());
        let status = if self
            .sleep
            .pending
            .as_ref()
            .is_some_and(|(_, k)| *k == device.key)
        {
            "Contacting the mouse…".into()
        } else if !self.settings.polling_controls {
            "Allow device settings changes in Settings first.".into()
        } else if let Some(o) = observed {
            if let Some(error) = &o.failure {
                device_control_error(error, o.may_have_changed, "sleep time", model.readable())
            } else if let Some(seconds) = o.seconds {
                if seconds == 0 {
                    "Current sleep time: Never.".into()
                } else if seconds % 60 == 0 {
                    format!("Current sleep time: {} min.", seconds / 60)
                } else {
                    format!("Current sleep time: {seconds} seconds.")
                }
            } else {
                "Sleep time sent. This mouse cannot confirm the value; check its sleep behaviour."
                    .into()
            }
        } else {
            "Choose how long the mouse waits before sleeping. No change is made until you select Apply.".into()
        };
        self.control(
            434,
            w!("STATIC"),
            &status,
            WINDOW_STYLE::default(),
            20,
            y + 96,
            750,
            54,
        );
        self.control(435,w!("STATIC"),"The mouse handles sleep itself; Halo does not need to stay open.\r\nMay reset after disconnecting. DeathAdder V4 Pro sleep is user-tested; other models remain unverified.",WINDOW_STYLE::default(),20,y+154,750,50);
    }
    fn battery_saver_rates(&self, device: &ConfigurationDevice) -> Vec<u32> {
        if let Some(o) = self.polling.observations.get(&device.key)
            && o.target.generation == self.polling.generation
        {
            return o.supported.iter().map(|r| r.hz()).collect();
        }
        hb_providers::controls::polling_menu_rates(device).to_vec()
    }
    fn build_battery_saver(&mut self) {
        let draft = self.controls.contains_key(&440).then(|| {
            (
                self.checked(440),
                self.text(442),
                self.text(444),
                self.choice(445),
            )
        });
        for id in 439..=449 {
            if let Some(h) = self.controls.remove(&id) {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
        }
        let Some((device, Some(battery))) = self.current_device() else {
            return;
        };
        if device.kind != "mouse" {
            return;
        }
        let polling = hb_providers::controls::polling_menu_candidate(&device);
        let sleep = hb_providers::sleep_controls::model(&device).filter(|m| m.readable());
        if !polling && sleep.is_none() {
            return;
        }
        if device.wired_mouse() || battery.reading.charging == Some(true) {
            let y = self.page_height + PAGE_TOP + 12;
            self.page_height = y + 112 - PAGE_TOP;
            self.heading(439, "Low-battery mode", 20, y, 750);
            self.control(447, w!("STATIC"), "Inactive while plugged in. Your saved power-saving settings are kept for battery use.", WINDOW_STYLE::default(), 20, y + 42, 750, 54);
            return;
        }
        let saved = self
            .settings
            .device(&device.key)
            .and_then(|p| p.low_battery_mode);
        let rates = self.battery_saver_rates(&device);
        let times: Vec<u16> = sleep
            .map(|m| m.choices().into_iter().filter(|s| *s != 0).collect())
            .unwrap_or_default();
        let rate_index = saved
            .and_then(|s| s.polling_rate)
            .and_then(|r| rates.iter().position(|hz| *hz == r.hz()))
            .map(|i| i + 1)
            .unwrap_or(0);
        let sleep_index = saved
            .and_then(|s| s.sleep_seconds)
            .and_then(|s| times.iter().position(|v| *v == s))
            .map(|i| i + 1)
            .unwrap_or(0);
        let values = if self.sleep.saver_key.as_ref() == Some(&device.key) {
            draft
        } else {
            None
        }
        .unwrap_or((
            saved.is_some(),
            saved.map(|s| s.threshold).unwrap_or(20).to_string(),
            rate_index
                .checked_sub(1)
                .and_then(|i| rates.get(i))
                .map(|r| format!("{r} Hz"))
                .unwrap_or_else(|| "Keep polling rate".into()),
            sleep_index,
        ));
        self.sleep.saver_key = Some(device.key);
        let y = self.page_height + PAGE_TOP + 12;
        self.page_height = y + 320 - PAGE_TOP;
        self.heading(439, "Low-battery mode", 20, y, 750);
        self.check(
            440,
            "Save power when this mouse runs low",
            values.0,
            20,
            y + 42,
            700,
        );
        self.label(441, "Start at or below (%)", 20, y + 86, 230);
        self.edit(442, &values.1, 270, y + 82, 90);
        self.label(443, "Polling rate / sleep after", 20, y + 134, 240);
        let mut labels = vec!["Keep polling rate".to_owned()];
        if polling {
            labels.extend(rates.iter().map(|r| format!("{r} Hz")));
        }
        self.combo(
            444,
            &labels,
            labels
                .iter()
                .position(|label| label == &values.2)
                .unwrap_or(0),
            270,
            y + 130,
            220,
        );
        let mut labels = vec!["Keep sleep time".to_owned()];
        labels.extend(times.iter().map(|s| format!("{} min", s / 60)));
        self.combo(
            445,
            &labels,
            values.3.min(labels.len() - 1),
            510,
            y + 130,
            220,
        );
        self.button(446, "Save low-battery settings", 20, y + 180, 280);
        self.control(447, w!("STATIC"), "Gaming boost takes priority. On the desktop, only lower rates and shorter sleep times are applied.\r\nPrevious settings return when charging or 5% above your threshold. Keep Halo running to restore them.\r\nManual changes take priority. Enable device changes in Settings to use this mode.", WINDOW_STYLE::default(), 20, y + 230, 750, 78);
        for id in [440, 442, 444, 445, 446] {
            self.enable_control(id, self.settings.polling_controls);
        }
        self.enable_control(444, self.settings.polling_controls && polling);
        self.enable_control(445, self.settings.polling_controls && sleep.is_some());
    }
    fn save_battery_saver(&mut self) {
        let Some((device, Some(battery))) = self.current_device() else {
            return;
        };
        if device.wired_mouse() || battery.reading.charging == Some(true) {
            return;
        }
        if !self.settings.polling_controls {
            return;
        }
        let choices = if self.checked(440) {
            let threshold = self
                .text(442)
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|v| (1..=95).contains(v));
            let rate = self
                .choice(444)
                .checked_sub(1)
                .and_then(|i| self.battery_saver_rates(&device).get(i).copied())
                .and_then(|v| PollingRate::try_from(v).ok());
            let seconds = hb_providers::sleep_controls::model(&device)
                .filter(|m| m.readable())
                .and_then(|m| {
                    self.choice(445)
                        .checked_sub(1)
                        .and_then(|i| m.choices().into_iter().filter(|s| *s != 0).nth(i))
                });
            if threshold.is_none() || (rate.is_none() && seconds.is_none()) {
                self.error =
                    "Enter a threshold from 1 to 95 and choose a polling rate or sleep time."
                        .into();
                self.set_control_text(95, &self.error);
                return;
            }
            Some(hb_core::settings::LowBatteryMode {
                threshold: threshold.unwrap(),
                polling_rate: rate,
                sleep_seconds: seconds,
            })
        } else {
            None
        };
        self.settings.device_mut(&device.key).low_battery_mode = choices;
        self.save();
        self.error = "Low-battery settings saved".into();
        self.set_control_text(95, &self.error);
    }
    fn request_sleep(&mut self, apply: bool) {
        let Some((device, _)) = self.current_device() else {
            return;
        };
        let Some(model) = hb_providers::sleep_controls::model(&device) else {
            return;
        };
        if self.sleep.pending.is_some() || !device.online() || !self.settings.polling_controls {
            return;
        }
        let seconds = if apply {
            model.choices().get(self.choice(431)).copied()
        } else {
            None
        };
        if apply && seconds.is_none() {
            return;
        }
        self.sleep.next = self.sleep.next.wrapping_add(1);
        let request = SleepRequest {
            request: self.sleep.next,
            target: ControlTarget {
                device: device.clone(),
                generation: self.polling.generation,
                connection_stamp: None,
            },
            seconds,
            expected: None,
        };
        match self.runtime.submit_sleep(request.clone()) {
            Ok(()) => {
                self.sleep.pending = Some((request.request, device.key));
                self.sleep.outcome = None;
            }
            Err(e) => self.sleep.outcome = Some(SleepOutcome::failed(&request, e.message)),
        }
        self.polling_controls();
    }
    fn read_polling(&mut self) {
        if self.dashboard_visible()
            && let Some(reading) = self.visible_polling_device()
            && reading.online()
            && matches!(reading.capability, PollingCapability::ReadWrite)
        {
            self.request_polling(reading, PollingIntent::Read);
        }
    }
    fn apply_polling(&mut self, restore: bool) {
        let Some(reading) = self.visible_polling_device() else {
            return;
        };
        let rate = if restore {
            self.polling.previous.get(&reading.key).copied()
        } else {
            self.polling
                .observations
                .get(&reading.key)
                .and_then(|o| o.supported.get(self.choice(43)))
                .copied()
        };
        let Some(rate) = rate else {
            self.polling.status.insert(
                reading.key,
                "Refresh the hardware rate and select a supported value.".into(),
            );
            self.polling_controls();
            return;
        };
        self.request_polling(reading, PollingIntent::Apply { rate, restore });
    }
    fn request_polling(&mut self, reading: ConfigurationDevice, intent: PollingIntent) {
        if self.tray_polling.pending.is_some() || !self.polling_intents.is_empty() {
            self.polling.status.insert(
                reading.key,
                "A hardware configuration request is already pending".into(),
            );
            self.polling_controls();
            return;
        }
        self.polling.sequence = self.polling.sequence.max(self.tray_polling.sequence);
        let key = reading.key.clone();
        match self.polling.begin(reading, intent) {
            Ok(request) => match self.runtime.submit_control(request.clone()) {
                Ok(()) => {
                    if let PollingIntent::Apply { rate, .. } = intent {
                        self.record_polling_intent(&key, request.request, rate);
                    }
                }
                Err(error) => {
                    self.polling.abandon();
                    self.polling.status.insert(key, error.to_string());
                }
            },
            Err(error) => {
                self.polling.status.insert(key, error.into());
            }
        }
        self.polling_controls();
    }
    fn record_polling_intent(&mut self, key: &str, request: u64, rate: PollingRate) {
        self.polling_intents.insert(key.into(), (request, rate));
        // A page close can abandon its UI request while a submitted write still
        // completes. Revoke evidence in both surfaces before the worker replies.
        self.polling.revoke_observation(key);
        self.tray_polling.revoke_observation(key);
        self.tooltip_rates.remove(key);
        self.sync_trays(TrayUpdate::Changed);
    }
    fn polling_outcome(&mut self, outcome: ControlOutcome) {
        cache_tooltip_rate(
            &mut self.tooltip_rates,
            &outcome,
            self.polling.generation.max(self.tray_polling.generation),
            self.settings.polling_controls,
        );
        self.sync_trays(TrayUpdate::Changed);
        if outcome.request & crate::runtime::STARTUP_POLLING_REQUEST_BIT != 0 {
            self.polling.observe_background(&outcome);
            self.tray_polling.observe_background(&outcome);
        }
        if outcome.request & crate::battery_saver::REQUEST_BIT != 0 {
            let message = outcome
                .failure
                .as_ref()
                .map(|e| format!("Low-battery mode: {e}"))
                .unwrap_or_else(|| "Low-battery mode: rate checked and verified.".into());
            for polling in [&mut self.polling, &mut self.tray_polling] {
                polling.status.insert(outcome.key.clone(), message.clone());
            }
            self.polling_controls();
            return;
        }
        if outcome.request & crate::boost::REQUEST_BIT != 0 {
            let message = outcome
                .failure
                .as_ref()
                .map(|e| format!("Automatic boost stopped: {e}"))
                .unwrap_or_else(|| "Automatic fullscreen rate verified.".into());
            for polling in [&mut self.polling, &mut self.tray_polling] {
                polling.status.insert(outcome.key.clone(), message.clone());
            }
            self.polling_controls();
            return;
        }
        if outcome.request & crate::runtime::STARTUP_POLLING_REQUEST_BIT != 0 {
            let initial_read = outcome.request & crate::runtime::STARTUP_POLLING_READ_BIT != 0;
            if let Some(observation) = self.tooltip_rates.get(&outcome.key) {
                self.tray_polling
                    .observations
                    .insert(outcome.key.clone(), observation.clone());
                self.tray_polling.status.insert(
                    outcome.key.clone(),
                    if initial_read {
                        "Startup rate read."
                    } else {
                        "Saved startup rate verified."
                    }
                    .into(),
                );
                if !initial_read && let Some(previous) = outcome.previous {
                    self.tray_polling
                        .previous
                        .insert(outcome.key.clone(), previous);
                }
            } else if let Some(failure) = &outcome.failure {
                self.tray_polling.status.insert(
                    outcome.key.clone(),
                    format!(
                        "Startup {}: {failure}",
                        if initial_read { "rate read" } else { "restore" }
                    ),
                );
                if !initial_read {
                    self.tray_polling_feedback(&outcome.key);
                }
            }
            self.polling_controls();
            return;
        }
        // Preserve explicit intent even when the user has left the page; never use an observation as a setting.
        if let Some(&(request, rate)) = self.polling_intents.get(&outcome.key)
            && request == outcome.request
        {
            self.polling_intents.remove(&outcome.key);
            self.polling.revoke_observation(&outcome.key);
            self.tray_polling.revoke_observation(&outcome.key);
            self.settings
                .device_mut(&outcome.key)
                .requested_polling_rate = Some(rate);
            self.save();
        }
        let tray_key = self
            .tray_polling
            .pending
            .as_ref()
            .filter(|p| p.request == outcome.request && p.key == outcome.key)
            .map(|p| (p.key.clone(), p.intent));
        if let Some((key, intent)) = tray_key {
            if self.tray_polling.accept(&outcome, Some(&key)) {
                if intent == PollingIntent::Read
                    && outcome.failure.is_none()
                    && self
                        .tray_polling
                        .observations
                        .get(&key)
                        .is_some_and(|o| o.rate.is_some())
                {
                    self.tray_polling.status.insert(
                        key.clone(),
                        "Hardware rate read. Select a rate in the polling menu to apply.".into(),
                    );
                }
                // A tray operation invalidates the dashboard's cached read so it
                // cannot present the previous rate as the newly confirmed value.
                self.polling.observations.remove(&key);
                self.tray_polling_feedback(&key);
                self.polling_controls();
            }
            return;
        }
        let key = self.visible_polling_device().map(|r| r.key);
        if self.polling.accept(&outcome, key.as_deref()) {
            self.tray_polling.observations.remove(&outcome.key);
            self.polling_controls();
        }
    }
    fn release_history(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.series = HistorySeries {
            axis: self.history.axis,
            ..Default::default()
        };
    }
    fn history_outcome(&mut self, id: u64, result: Result<HistorySeries, ProviderError>) {
        if self.dashboard.is_none() || self.page != 2 || id != self.request {
            return;
        }
        match result {
            Ok(series) => {
                self.series = series;
                self.set_control_text(
                    97,
                    "History is saved for 30 days. Mouse and keyboard activity is not tracked.",
                );
            }
            Err(e) => {
                self.error = e.to_string();
                self.set_control_text(
                    97,
                    "Couldn't load battery history. Select Refresh to try again.",
                );
            }
        }
        unsafe {
            if let Some(h) = self.dashboard {
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
    }
    fn query(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.series = HistorySeries {
            axis: self.history.axis,
            ..Default::default()
        };
        let Some(d) = self.snapshot.devices.get(self.selected) else {
            self.set_control_text(97, "Connect a battery-powered device to see its history.");
            return;
        };
        self.set_control_text(97, "Loading battery history…");
        let until = SystemClock::default().unix();
        let mut r = RECT::default();
        unsafe {
            if let Some(h) = self.dashboard {
                let _ = GetClientRect(h, &mut r);
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
        let result = self.runtime.request_history(self.history.command(
            d.reading.key.clone(),
            until,
            (r.right - 80).max(20) as usize,
            self.request,
        ));
        if let Err(error) = result {
            self.history_outcome(self.request, Err(error));
        }
    }
    fn drain(&mut self) {
        let mut latest = None;
        let mut heartbeat = None;
        let mut polling_outcomes = Vec::new();
        let mut polling_invalidated = false;
        let mut polling_selection_changed = false;
        while let Ok(e) = self.runtime.events.try_recv() {
            match e {
                Event::Snapshot(s) => {
                    latest = Some(s);
                    heartbeat = None;
                }
                Event::Heartbeat(timestamp) => {
                    if let Some(snapshot) = latest.as_mut() {
                        snapshot.timestamp = timestamp;
                    } else {
                        heartbeat = Some(timestamp);
                    }
                }
                Event::ConfigurationInventory {
                    generation,
                    devices,
                    failure,
                } => {
                    self.configuration_inventory(generation, devices, failure);
                }
                Event::Sleep(outcome) => {
                    if outcome.request & crate::battery_saver::REQUEST_BIT != 0 {
                        if self.dashboard.is_some() {
                            self.sleep.outcome = Some(*outcome);
                            self.polling_controls();
                        }
                        continue;
                    }
                    if self
                        .sleep
                        .pending
                        .as_ref()
                        .is_some_and(|(id, key)| *id == outcome.request && *key == outcome.key)
                    {
                        self.sleep.pending = None;
                        self.sleep.shown_key = None;
                        self.sleep.outcome = Some(*outcome);
                        self.polling_controls();
                    }
                }
                Event::Dpi(outcome) => {
                    if self
                        .dpi
                        .pending
                        .as_ref()
                        .is_some_and(|(id, key)| *id == outcome.request && *key == outcome.key)
                    {
                        self.dpi.pending = None;
                        if self.dashboard.is_some()
                            && self.page == 1
                            && outcome.generation == self.polling.generation
                        {
                            if let Some(previous) = &outcome.previous {
                                self.dpi.previous = Some((
                                    outcome.key.clone(),
                                    outcome.generation,
                                    previous.clone(),
                                ));
                            }
                            self.dpi.shown_key = None;
                            self.dpi.outcome = Some(*outcome);
                        }
                        self.polling_controls();
                    }
                }
                Event::Polling(outcome) => polling_outcomes.push(*outcome),
                Event::PollingInvalidated(generation) => {
                    // Epoch changes revoke actionable evidence, not the labelled
                    // last-confirmed tooltip for a still-matching physical device.
                    // Snapshots remove missing/stale/replaced identities separately.
                    self.sleep.outcome = None;
                    self.dpi.outcome = None;
                    self.dpi.previous = None;
                    self.polling.invalidate(generation);
                    self.tray_polling.invalidate(generation);
                    polling_invalidated = true;
                }
                Event::Diagnostics(d) => self.diagnostics = d,
                Event::Error(e) => {
                    self.error = e;
                    self.set_control_text(95, user_message(&self.error));
                }
                Event::Alert(n) => {
                    let result = self
                        .trays
                        .get_mut(&n.key)
                        .ok_or_else(|| ProviderError::new("Tray icon not available"))
                        .and_then(|t| t.deliver(&n));
                    if result.is_err() {
                        self.runtime.send(Command::NotificationFailed(n));
                    }
                }
                Event::Insights(id, result) if self.dashboard.is_some() && self.page == 6 => {
                    let key = self
                        .snapshot
                        .devices
                        .get(self.selected)
                        .map(|d| d.reading.key.as_str());
                    if self.insights.accept(id, key, result.map(|data| *data)) {
                        self.render_insights();
                    }
                }
                Event::History(id, result) => self.history_outcome(id, result),
                _ => {}
            }
        }
        if let Some(timestamp) = heartbeat {
            self.snapshot.timestamp = timestamp;
            self.refresh_device_details();
            self.sync_trays(TrayUpdate::Changed);
        }
        if let Some(s) = latest {
            for view in &s.devices {
                self.settings.link_reading(&view.reading);
            }
            let changed_labels: Vec<_> = s
                .devices
                .iter()
                .enumerate()
                .filter_map(|(index, next)| {
                    (self.controls.contains_key(&10)
                        && self
                            .snapshot
                            .devices
                            .get(index)
                            .map(|previous| &previous.text)
                            != Some(&next.text))
                    .then_some(index)
                })
                .collect();
            let identity_changed = self
                .snapshot
                .devices
                .iter()
                .map(|d| &d.reading.key)
                .ne(s.devices.iter().map(|d| &d.reading.key));
            let previous_key = self
                .snapshot
                .devices
                .get(self.selected)
                .map(|d| d.reading.key.as_str());
            let selected = previous_key
                .and_then(|key| s.devices.iter().position(|d| d.reading.key == key))
                .unwrap_or_else(|| self.selected.min(s.devices.len().saturating_sub(1)));
            let battery_selection_changed =
                previous_key != s.devices.get(selected).map(|d| d.reading.key.as_str());
            let previous_device = current_device_state(
                &self.snapshot.devices,
                &self.configuration_devices,
                self.selected_device.as_deref(),
            );
            let current_device = current_device_state(
                &s.devices,
                &self.configuration_devices,
                self.selected_device.as_deref(),
            );
            let charging_changed = previous_device.and_then(|(_, charging)| charging)
                != current_device.and_then(|(_, charging)| charging);
            let device_changed = previous_device.map(|(device, _)| device)
                != current_device.map(|(device, _)| device);
            let replaced_device = device_changed
                && previous_device
                    .zip(current_device)
                    .is_some_and(|((before, _), (after, _))| !before.same_identity(after));
            let current_key = current_device.map(|(device, _)| device.key);
            if self.selected_device.as_deref() != current_key {
                self.selected_device = current_key.map(str::to_owned);
            }
            self.snapshot = s;
            self.tooltip_rates.retain(|key, observation| {
                self.snapshot.devices.iter().any(|d| {
                    &d.reading.key == key
                        && d.reading.connection != Connection::Stale
                        && observation.target.device.matches_reading(&d.reading)
                })
            });
            self.tray_polling
                .retain_inventory(&self.snapshot.devices, &[]);
            if device_changed {
                self.polling.abandon();
                polling_selection_changed = true;
                if replaced_device {
                    self.invalidate_mouse_readbacks();
                }
            }
            self.polling
                .retain_inventory(&self.snapshot.devices, &self.configuration_devices);
            self.selected = selected;
            let selected_key = self
                .snapshot
                .devices
                .get(self.selected)
                .map(|d| &d.reading.key);
            if battery_selection_changed {
                self.polling.abandon();
                self.insights.select(selected_key.cloned());
                polling_selection_changed = true;
            }
            self.refresh_device_details();
            self.sync_trays(TrayUpdate::Changed);
            if (identity_changed && self.page != 3)
                || ((charging_changed || replaced_device) && self.page == 1)
            {
                self.build()
            } else if let Some(h) = self.controls.get(&10) {
                for i in changed_labels {
                    let d = &self.snapshot.devices[i];
                    let text = wide(&d.text);
                    unsafe {
                        send(*h, CB_DELETESTRING, WPARAM(i), LPARAM(0));
                        send(
                            *h,
                            CB_INSERTSTRING,
                            WPARAM(i),
                            LPARAM(text.as_ptr() as isize),
                        );
                    }
                }
                unsafe {
                    let selection = if self.page == 1 {
                        self.selected_device
                            .as_ref()
                            .and_then(|key| {
                                device_row_index(
                                    &self.snapshot.devices,
                                    &self.configuration_devices,
                                    key,
                                )
                            })
                            .unwrap_or(0)
                    } else {
                        self.selected
                    };
                    if send(*h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 != selection as isize {
                        send(*h, CB_SETCURSEL, WPARAM(selection), LPARAM(0));
                    }
                }
            }
        }
        // Startup readback can arrive before its first battery snapshot. Apply
        // the inventory first; generation checks still reject invalidated replies.
        for outcome in polling_outcomes {
            self.polling_outcome(outcome);
        }
        if polling_invalidated || polling_selection_changed {
            self.polling_controls();
            self.read_polling();
        }
    }
    fn refresh_device_details(&self) {
        if self.dashboard_visible()
            && self.page == 1
            && self.controls.contains_key(&91)
            && let Some((_, Some(d))) = self.current_device()
        {
            self.set_control_text(91, &device_detail(&d));
            self.set_control_text(88, &battery_summary(&d));
        }
    }
    fn sync_trays(&mut self, update: TrayUpdate) {
        let dark = self.tray_theme.read(
            Instant::now(),
            &self.settings.icon_theme,
            update != TrayUpdate::Changed,
            || tray_dark(&self.settings, update != TrayUpdate::Changed),
        );
        let settings = &self.settings;
        let devices = tray_devices(&self.snapshot, settings);
        self.trays.retain(|key, _| {
            devices.iter().any(|d| {
                &d.reading.key == key
                    && !settings.device(key).is_some_and(|p| p.hidden)
                    && !d.hidden
            })
        });
        for d in devices
            .iter()
            .filter(|d| !d.hidden && !settings.device(&d.reading.key).is_some_and(|p| p.hidden))
        {
            let existing = self.trays.get_mut(&d.reading.key);
            if let Some(t) = existing {
                let mut changed = false;
                if !t.signature.matches(d, settings, dark)
                    && let Ok(frames) = icons::frames(d, settings, dark, 32)
                {
                    t.frames = frames;
                    t.signature = icon_signature(d, settings, dark);
                    t.frame = 0;
                    changed = true
                }
                let old = t.data.szTip;
                copy_tray_tooltip(
                    &mut t.data.szTip,
                    d,
                    self.tooltip_rates
                        .get(&d.reading.key)
                        .filter(|_| settings.polling_controls),
                );
                changed |= old != t.data.szTip;
                t.data.hIcon = t.frames[t.frame].0;
                t.registration
                    .update_with(&t.data, update, changed, &mut shell_notify);
            } else if let Ok(frames) = icons::frames(d, settings, dark, 32) {
                let mut data = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: self.monitor,
                    uID: stable_guid(&d.reading.key) as u32,
                    uFlags: NIF_GUID | NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    uCallbackMessage: TRAY,
                    hIcon: frames[0].0,
                    guidItem: GUID::from_u128(stable_guid(&d.reading.key)),
                    ..Default::default()
                };
                copy_tray_tooltip(
                    &mut data.szTip,
                    d,
                    self.tooltip_rates
                        .get(&d.reading.key)
                        .filter(|_| settings.polling_controls),
                );
                let mut registration = TrayRegistration::default();
                registration.update_with(&data, update, true, &mut shell_notify);
                self.trays.insert(
                    d.reading.key.clone(),
                    Tray {
                        registration,
                        data,
                        frames,
                        signature: icon_signature(d, settings, dark),
                        frame: 0,
                    },
                );
            }
        }
        let animate = self.trays.values().any(|t| t.frames.len() > 1);
        if animate != self.animating {
            unsafe {
                if animate {
                    SetTimer(Some(self.monitor), 2, 100, None);
                } else {
                    let _ = KillTimer(Some(self.monitor), 2);
                }
            }
            self.animating = animate;
        }
    }
    fn animate(&mut self) {
        for t in self.trays.values_mut().filter(|t| t.frames.len() > 1) {
            t.frame = (t.frame + 1) % t.frames.len();
            let mut data = t.data;
            data.uFlags = NIF_GUID | NIF_ICON;
            data.hIcon = t.frames[t.frame].0;
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
            }
        }
    }
    fn menu(&mut self, key: Option<String>) {
        unsafe {
            let Ok(menu) = popup_menu(&self.snapshot, &self.settings, key.as_deref()) else {
                return;
            };
            // Build from cached metadata only; opening/hovering never performs HID I/O.
            let choices = key
                .as_deref()
                .and_then(|key| self.tray_polling_device(key))
                .map(|device| {
                    tray_polling_choices(
                        &device,
                        &self.settings,
                        &self.tray_polling,
                        self.polling.pending.is_some() || !self.polling_intents.is_empty(),
                    )
                })
                .unwrap_or_default();
            if !choices.is_empty() && append_polling_menu(menu.0, &choices).is_err() {
                return;
            }
            let palette = Palette::new(
                hb_windows::system::dashboard_dark_theme(),
                hb_windows::system::high_contrast(),
            );
            let appearance = PopupAppearance::new(menu.0, self.monitor, palette);
            (*self.context).popup.replace(appearance);
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(self.monitor);
            let chosen = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                Some(0),
                self.monitor,
                None,
            );
            // Destroy the native menu before releasing the brush it references.
            drop(menu);
            (*self.context).popup.take();
            if let Some(choice) = choices
                .iter()
                .find(|choice| choice.id == chosen.0 as u16 && choice.enabled && choice.id != 0)
            {
                if let Some(key) = key.as_deref() {
                    self.tray_polling_command(key, choice.action);
                }
            } else if chosen.0 != 0 {
                self.command(chosen.0 as u16, 0)
            }
            let _ = PostMessageW(Some(self.monitor), WM_APP + 7, WPARAM(0), LPARAM(0));
        }
    }
    fn tray_polling_device(&self, key: &str) -> Option<ConfigurationDevice> {
        let reading = &self
            .snapshot
            .devices
            .iter()
            .find(|d| d.reading.key == key && !d.hidden)?
            .reading;
        let device = ConfigurationDevice::from_reading(reading);
        hb_providers::controls::polling_menu_candidate(&device).then_some(device)
    }
    fn tray_polling_command(&mut self, key: &str, action: TrayPollingAction) {
        if action == TrayPollingAction::Settings {
            self.page = 3;
            self.configuration_visibility();
            self.open();
            self.build();
            return;
        }
        let Some(device) = self.tray_polling_device(key) else {
            return;
        };
        if !self.settings.polling_controls
            || self.polling.pending.is_some()
            || !self.polling_intents.is_empty()
        {
            return;
        }
        let intent = match action {
            TrayPollingAction::Read => PollingIntent::Read,
            TrayPollingAction::Apply(rate) => PollingIntent::Apply {
                rate,
                restore: false,
            },
            TrayPollingAction::Restore => {
                let Some(&rate) = self.tray_polling.previous.get(key) else {
                    return;
                };
                PollingIntent::Apply {
                    rate,
                    restore: true,
                }
            }
            TrayPollingAction::Settings | TrayPollingAction::None => return,
        };
        self.tray_polling.sequence = self.tray_polling.sequence.max(self.polling.sequence);
        match self.tray_polling.begin_tray(device, intent) {
            Ok(request) => match self.runtime.submit_control(request.clone()) {
                Ok(()) => {
                    if let PollingIntent::Apply { rate, .. } = intent {
                        self.record_polling_intent(key, request.request, rate);
                    }
                }
                Err(error) => {
                    self.tray_polling.abandon();
                    self.tray_polling
                        .status
                        .insert(key.into(), error.to_string());
                    self.tray_polling_feedback(key);
                }
            },
            Err(error) => {
                self.tray_polling.status.insert(key.into(), error.into());
                self.tray_polling_feedback(key);
            }
        }
    }
    fn tray_polling_feedback(&mut self, key: &str) {
        if let Some(message) = self.tray_polling.status.get(key)
            && let Some(tray) = self.trays.get_mut(key)
        {
            let _ = tray.show_message("Polling rate", polling_message(message));
        }
    }
}

unsafe fn send(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { SendMessageW(h, m, Some(w), Some(l)) }
}

#[derive(Debug, PartialEq, Eq)]
struct IconSignature {
    level: Option<u8>,
    precision: Precision,
    charging: Option<bool>,
    connection: Connection,
    icon: String,
    low_alert_at: u8,
    dark: bool,
    icon_theme: String,
    animation: bool,
    percent_in_icon: bool,
    badges: bool,
    warning_level: u8,
}
impl IconSignature {
    fn matches(&self, d: &DeviceView, settings: &Settings, dark: bool) -> bool {
        self.level == d.reading.level
            && self.precision == d.reading.precision
            && self.charging == d.reading.charging
            && self.connection == d.reading.connection
            && self.icon == d.icon
            && self.low_alert_at == d.low_alert_at
            && self.dark == dark
            && self.icon_theme == settings.icon_theme
            && self.animation == settings.animation
            && self.percent_in_icon == settings.percent_in_icon
            && self.badges == settings.badges
            && self.warning_level == settings.warning_level
    }
}
fn icon_signature(d: &DeviceView, settings: &Settings, dark: bool) -> IconSignature {
    IconSignature {
        level: d.reading.level,
        precision: d.reading.precision.clone(),
        charging: d.reading.charging,
        connection: d.reading.connection.clone(),
        icon: d.icon.clone(),
        low_alert_at: d.low_alert_at,
        dark,
        icon_theme: settings.icon_theme.clone(),
        animation: settings.animation,
        percent_in_icon: settings.percent_in_icon,
        badges: settings.badges,
        warning_level: settings.warning_level,
    }
}

fn tray_dark(settings: &Settings, force: bool) -> bool {
    let fallback = hb_windows::system::dark_theme();
    if !["auto", "topbar"].contains(&settings.icon_theme.as_str())
        || !hb_windows::system::mydockfinder_running(force)
    {
        return fallback;
    }
    unsafe {
        let dc = GetDC(None);
        if dc.0.is_null() {
            return fallback;
        }
        let width = GetSystemMetrics(SM_CXSCREEN);
        let mut luminance = 0.;
        let mut count = 0;
        for i in 1..20 {
            let c = GetPixel(dc, width * i / 20, 8).0;
            if c != 0xffffffff {
                luminance += 0.2126 * (c & 255) as f64
                    + 0.7152 * ((c >> 8) & 255) as f64
                    + 0.0722 * ((c >> 16) & 255) as f64;
                count += 1
            }
        }
        ReleaseDC(None, dc);
        if count == 0 {
            fallback
        } else {
            luminance / (count as f64) < 123.
        }
    }
}

impl NotificationSink for Tray {
    fn deliver(&mut self, n: &Notification) -> Result<(), ProviderError> {
        self.show_message(&n.title, &n.text)
    }
}
impl Tray {
    fn show_message(&mut self, title: &str, text: &str) -> Result<(), ProviderError> {
        let mut data = self.data;
        data.uFlags = NIF_GUID | NIF_INFO;
        copy(&mut data.szInfoTitle, title);
        copy(&mut data.szInfo, text);
        data.dwInfoFlags = NIIF_INFO;
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            Ok(())
        } else {
            Err(ProviderError::new(
                "Windows could not deliver the notification",
            ))
        }
    }
}

#[cfg(test)]
pub(crate) static NATIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device_alert_summary(settings: &Settings, key: &str) -> String {
    if settings.device(key).is_some_and(|d| d.hidden) {
        return "The tray icon and battery alerts are hidden for this device.".into();
    }
    if !settings.notify && !settings.low_sound {
        return "Notifications and low-battery sounds are both off in Settings.".into();
    }
    let level = settings.low_for(key);
    let origin = if settings.device(key).and_then(|d| d.low).is_some() {
        format!("device override; default {}%", settings.low)
    } else {
        "uses Settings".into()
    };
    if level == 0 {
        "Alerts are off for this device. Leave the field blank to use Settings.".into()
    } else {
        format!(
            "Active alert: {level}% ({origin}). Leave blank to use Settings; 0 turns alerts off."
        )
    }
}

fn battery_summary(d: &DeviceView) -> String {
    match d.reading.level {
        Some(level) if d.reading.online() => format!("{level}% battery"),
        Some(level) => format!("{level}% battery · Last known"),
        None => "Battery level unavailable".into(),
    }
}
fn device_detail(d: &DeviceView) -> String {
    let age = SystemClock::default()
        .unix()
        .saturating_sub(d.reading.timestamp)
        .max(0);
    let fresh = if age < 5 {
        "just now".into()
    } else if age < 60 {
        format!("{age} seconds ago")
    } else if age < 3600 {
        format!("{} min ago", age / 60)
    } else if age < 86400 {
        format!(
            "{} {} ago",
            age / 3600,
            if age / 3600 == 1 { "hour" } else { "hours" }
        )
    } else {
        format!(
            "{} {} ago",
            age / 86400,
            if age / 86400 == 1 { "day" } else { "days" }
        )
    };
    let charging = match d.reading.charging {
        Some(true) => {
            if d.reading.charging_inferred {
                "appears to be charging"
            } else {
                "charging"
            }
        }
        Some(false) => "not charging",
        None => "charging status unavailable",
    };
    let connection = match d.reading.connection {
        Connection::Online => "Connected",
        Connection::Sleeping => "Sleeping",
        Connection::Stale => "Waiting for an update",
    };
    format!(
        "{} · {} · {} · updated {}",
        provider_label(&d.reading.source),
        connection,
        charging,
        fresh
    )
}
fn provider_label(id: &str) -> &str {
    match id {
        "razer" => "Razer",
        "nzxt" => "NZXT Lift Elite",
        "steam" => "Steam Controller",
        "audeze" => "Audeze Maxwell",
        "wlmouse" => "WLmouse",
        "mchose" => "MCHOSE",
        "hyperx_alpha2" => "HyperX Alpha 2",
        "hyperx_cloud3" => "HyperX III / II Core",
        "hyperx_cloud3s" => "HyperX Cloud III S",
        "hyperx" => "HyperX Cloud II",
        "keychron" => "Keychron",
        "pulsar" => "Pulsar / ATK / VXE",
        "jbl" => "JBL Quantum",
        "logitech" => "Logitech",
        "logitech_centurion" => "Logitech PRO X 2",
        "steelseries" => "SteelSeries",
        "steelseries_elite" => "SteelSeries Nova Elite",
        "playstation" => "PlayStation",
        "eightbitdo" => "8BitDo",
        "barracuda" => "Barracuda Pro",
        "nintendo" => "Nintendo Switch",
        "asus" => "ASUS ROG / TUF",
        "gwolves" => "G-Wolves",
        "lofree" => "Lofree",
        "astro" => "Astro A50",
        "corsair" => "Corsair",
        "lamzu" => "LAMZU",
        "am_infinity" => "AM Infinity 8K",
        "bluetooth" => "Bluetooth",
        "xinput" => "Xbox controllers",
        other => other,
    }
}
fn device_threshold(text: &str) -> Result<Option<u8>, &'static str> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    text.trim()
        .parse::<u8>()
        .ok()
        .filter(|v| *v <= 100)
        .map(Some)
        .ok_or(
            "Enter a battery percentage from 0 to 100, or leave blank to use the general setting.",
        )
}

fn tray_devices<'a>(snapshot: &'a Snapshot, settings: &Settings) -> Vec<Cow<'a, DeviceView>> {
    let mut devices: Vec<_> = snapshot
        .devices
        .iter()
        .filter(|d| !d.hidden && !settings.device(&d.reading.key).is_some_and(|p| p.hidden))
        .map(Cow::Borrowed)
        .collect();
    if devices.is_empty() {
        devices.push(Cow::Owned(DeviceView {
            reading: Reading::new("application", "Halo Continued", "app", 0),
            name: "Halo Continued".into(),
            icon: "mouse".into(),
            low_alert_at: 0,
            seconds_left: None,
            text: "Halo Continued — no visible devices".into(),
            hidden: false,
        }));
    }
    devices
}

fn boost_controls_visible(
    device: &ConfigurationDevice,
    observation: Option<&PollingObservation>,
    saved: Option<PollingRate>,
) -> bool {
    device.kind == "mouse"
        && matches!(device.capability, PollingCapability::ReadWrite)
        && (saved.is_some()
            || observation.map_or_else(
                || {
                    hb_providers::controls::polling_menu_rates(device)
                        .iter()
                        .any(|hz| *hz > 1000)
                },
                |o| o.supported.iter().any(|rate| rate.hz() > 1000),
            ))
}

#[cfg(test)]
mod behaviour_tests {
    #[test]
    fn boost_visibility_respects_model_and_readback_but_allows_disabling_saved_boost() {
        use super::*;
        let mut reading = Reading::new("razer:00b7:UNIT1234", "Mouse", "razer", 0);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        let mut device = ConfigurationDevice::from_reading(&reading);
        assert!(!boost_controls_visible(&device, None, None));
        device.key = "razer:00bf:UNIT1234".into();
        assert!(boost_controls_visible(&device, None, None));
        let mut observation = PollingObservation {
            target: ControlTarget {
                device: device.clone(),
                generation: 1,
                connection_stamp: None,
            },
            supported: vec![PollingRate::try_from(1000).unwrap()],
            rate: PollingRate::try_from(1000).ok(),
            timestamp: 0,
            evidence: String::new(),
        };
        assert!(!boost_controls_visible(&device, Some(&observation), None));
        assert!(boost_controls_visible(
            &device,
            Some(&observation),
            PollingRate::try_from(2000).ok()
        ));
        observation
            .supported
            .push(PollingRate::try_from(2000).unwrap());
        assert!(boost_controls_visible(&device, Some(&observation), None));
        device.kind = "keyboard".into();
        assert!(!boost_controls_visible(
            &device,
            Some(&observation),
            PollingRate::try_from(2000).ok()
        ));
    }
    #[test]
    fn dpi_ranges_show_steps_axes_and_discrete_choices_without_filling_gaps() {
        use super::*;
        let mut limits = DpiLimits {
            x: vec![(100, 100, 1), (100, 10000, 50)],
            y: vec![(100, 100, 1), (100, 10000, 50)],
            from_model: false,
        };
        assert_eq!(
            dpi_limits_text(Some(&limits)),
            "Supported DPI: 100–10,000 (50 DPI steps)"
        );
        limits.y = vec![(200, 6400, 100)];
        assert_eq!(
            dpi_limits_text(Some(&limits)),
            "Supported DPI: Horizontal 100–10,000 (50 DPI steps)\r\nVertical 200–6,400 (100 DPI steps)"
        );
        limits.x = vec![(400, 400, 1), (800, 800, 1), (1600, 1600, 1)];
        limits.y = limits.x.clone();
        assert_eq!(
            dpi_limits_text(Some(&limits)),
            "Supported DPI: 400; 800; 1,600"
        );
        limits.x = vec![(50, 900, 75)];
        limits.y = limits.x.clone();
        assert_eq!(
            dpi_limits_text(Some(&limits)),
            "Supported DPI: 125–875 (75 DPI steps)"
        );
        assert!(dpi_limits_text(None).contains("Read current"));
    }

    #[test]
    fn dpi_input_checks_both_axes_and_steps_before_requesting_hardware() {
        let limits = DpiLimits {
            x: vec![(100, 32000, 50)],
            y: vec![(200, 16000, 100)],
            from_model: false,
        };
        assert_eq!(
            dpi_input(" 800 ", Some(&limits)).unwrap(),
            DpiValue::uniform(800)
        );
        for invalid in [
            "", "abc", "800.5", "99", "150", "850", "16001", "32000", "65536",
        ] {
            assert!(dpi_input(invalid, Some(&limits)).is_err(), "{invalid}");
        }
        assert_eq!(dpi_input("45000", None).unwrap(), DpiValue::uniform(45000));
    }
    use super::*;
    #[test]
    fn icon_changes_on_precision_transition_but_not_timestamp_refresh() {
        let settings = Settings::default();
        let mut d = tray_devices(&Snapshot::default(), &settings)
            .remove(0)
            .into_owned();
        d.reading.level = Some(50);
        let exact = icon_signature(&d, &settings, false);
        d.reading.timestamp += 60;
        assert_eq!(exact, icon_signature(&d, &settings, false));
        d.reading.precision = Precision::Coarse;
        assert_ne!(exact, icon_signature(&d, &settings, false));
    }
    #[test]
    fn tray_cache_signature_covers_visual_changes_but_ignores_text_and_freshness() {
        let settings = Settings::default();
        let device = tray_devices(&Snapshot::default(), &settings)
            .remove(0)
            .into_owned();
        let signature = icon_signature(&device, &settings, false);
        type Edit = fn(&mut DeviceView, &mut Settings);
        let edits: [Edit; 11] = [
            |d, _| d.reading.level = Some(50),
            |d, _| d.reading.precision = Precision::Coarse,
            |d, _| d.reading.charging = Some(true),
            |d, _| d.reading.connection = Connection::Sleeping,
            |d, _| d.icon = "keyboard".into(),
            |d, _| d.low_alert_at += 1,
            |_, s| s.icon_theme = "black".into(),
            |_, s| s.animation = !s.animation,
            |_, s| s.percent_in_icon = !s.percent_in_icon,
            |_, s| s.badges = !s.badges,
            |_, s| s.warning_level += 1,
        ];
        for edit in edits {
            let mut d = device.clone();
            let mut s = settings.clone();
            edit(&mut d, &mut s);
            assert!(!signature.matches(&d, &s, false));
        }
        assert!(!signature.matches(&device, &settings, true));
        let mut text_only = device;
        text_only.reading.timestamp += 60;
        text_only.reading.charging_inferred = true;
        text_only.name = "Renamed".into();
        text_only.text = "Fresh tooltip".into();
        assert!(signature.matches(&text_only, &settings, false));
    }
    #[test]
    fn fixed_utf16_buffers_remain_terminated_and_clear_previous_content() {
        let mut buffer = [99; 6];
        copy(&mut buffer, "A😀BCDE");
        assert_eq!(buffer, [65, 0xd83d, 0xde00, 66, 67, 0]);
        copy(&mut buffer, "X");
        assert_eq!(buffer, [88, 0, 0, 0, 0, 0]);
        copy(&mut [], "anything");
        let mut one = [99];
        copy(&mut one, "anything");
        assert_eq!(one, [0]);
    }
    #[test]
    fn placeholder_exists_only_without_visible_devices() {
        let mut settings = Settings::default();
        let mut snapshot = Snapshot::default();
        let empty = tray_devices(&snapshot, &settings)
            .into_iter()
            .map(Cow::into_owned)
            .collect::<Vec<_>>();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].reading.key, "application");
        let mut reading = Reading::new("mouse", "Mouse", "razer", SystemClock::default().unix());
        reading.level = Some(50);
        snapshot.devices.push(DeviceView {
            reading,
            name: "Mouse".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Mouse: 50%".into(),
            hidden: false,
        });
        let visible = tray_devices(&snapshot, &settings);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].reading.key, "mouse");
        settings.device_mut("mouse").hidden = true;
        let hidden = tray_devices(&snapshot, &settings);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].reading.key, "application");
        assert_eq!(
            stable_guid(&empty[0].reading.key),
            stable_guid(&hidden[0].reading.key)
        );
        settings.devices.clear();
        snapshot.devices[0].hidden = true;
        assert_eq!(
            tray_devices(&snapshot, &settings)[0].reading.key,
            "application"
        );
    }
    #[test]
    fn every_provider_has_a_unique_readable_settings_label() {
        let ids = providers();
        assert_eq!(ids.len(), 30);
        let labels = ids
            .iter()
            .map(|id| provider_label(id))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels.len(), ids.len());
        assert_eq!(provider_label("razer"), "Razer");
        assert_eq!(provider_label("playstation"), "PlayStation");
        assert!(ids.iter().all(|id| !provider_label(id).contains('_')));
    }
    #[test]
    fn clearing_device_threshold_restores_default_without_losing_other_preferences() {
        assert_eq!(device_threshold(" "), Ok(None));
        assert_eq!(device_threshold("30"), Ok(Some(30)));
        assert_eq!(device_threshold("0"), Ok(Some(0)));
        assert!(device_threshold("101").is_err());
        assert!(device_threshold("-1").is_err());
        assert!(device_threshold("bad").is_err());
        let mut settings = Settings::default();
        let mut preference = DevicePreferences {
            name: Some("Custom mouse".into()),
            low: device_threshold("30").unwrap(),
            ..Default::default()
        };
        settings.devices.insert("mouse".into(), preference.clone());
        assert_eq!(settings.low_for("mouse"), 30);
        preference.low = device_threshold("").unwrap();
        settings.devices.insert("mouse".into(), preference);
        settings.low = 15;
        assert_eq!(settings.low_for("mouse"), 15);
        assert_eq!(
            settings.devices["mouse"].name.as_deref(),
            Some("Custom mouse")
        );
    }
    #[test]
    fn device_details_show_freshness_and_charging_uncertainty() {
        let mut reading = Reading::new(
            "mouse",
            "Mouse",
            "razer",
            SystemClock::default().unix() - 120,
        );
        reading.charging = Some(false);
        reading.connection = Connection::Stale;
        let mut d = DeviceView {
            reading,
            name: "Mouse".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Mouse".into(),
            hidden: false,
        };
        let text = device_detail(&d);
        assert!(text.contains("Waiting for an update"));
        assert!(text.contains("not charging"));
        assert!(text.contains("2 min ago"));
        d.reading.charging = None;
        assert!(device_detail(&d).contains("charging status unavailable"));
        d.reading.charging = Some(true);
        d.reading.charging_inferred = true;
        assert!(device_detail(&d).contains("appears to be charging"));
    }
    #[test]
    fn user_messages_keep_recovery_actions_and_hide_protocol_details() {
        assert_eq!(
            user_message("Automatic boost settings saved"),
            "Automatic boost settings saved"
        );
        assert_eq!(
            polling_message("Automatic fullscreen rate verified."),
            "Automatic fullscreen rate verified."
        );
        let raw = "Hardware may have changed. HID interface 3 report 0x1F timed out after SET.";
        let message = polling_message(raw);
        assert!(message.starts_with("Refresh rate"));
        assert!(message.contains("may have applied") && message.contains("will not be retried"));
        assert!(message.len() <= 90);
        assert!(!message.contains("HID") && !message.contains("0x1F"));
        assert!(polling_message("Access denied by HID driver").contains("Wake the device"));
        assert!(polling_message("Cannot change rate during game").contains("Close the game"));
        assert!(user_message("Storage: SQLITE_CORRUPT").contains("Couldn't save"));
        assert!(user_message("History: SQLITE_BUSY").contains("Refresh"));
        assert!(!user_message("Exported private data-folder path").contains("private"));
        assert_eq!(
            user_message("Enter a valid percentage."),
            "Enter a valid percentage."
        );
    }
    #[test]
    fn device_feedback_hides_protocol_errors_and_preserves_write_uncertainty() {
        for raw in [
            "HID error: 0x1f invalid report",
            "Unknown DPI feature 2202",
            "profile CRC mismatch",
        ] {
            let message = device_control_error(raw, false, "DPI", true);
            assert!(
                !message.contains("HID") && !message.contains("CRC") && !message.contains("2202")
            );
        }
        let message = device_control_error("Unsupported report", true, "DPI", true);
        assert!(message.contains("may have changed") && message.contains("Read current"));
        let message = device_control_error("HID timeout", true, "sleep time", false);
        assert!(message.contains("may have changed") && !message.contains("Read current"));
        assert!(
            device_control_error("backup failed", false, "DPI", true).contains("original settings")
        );
        assert!(device_control_error("Mouse busy", false, "DPI", true).contains("Wait a moment"));
        assert_eq!(settings_number(" 60 ", 5, 3600), Some(60));
        for invalid in ["4", "3601", "-1", "1.5", ""] {
            assert_eq!(settings_number(invalid, 5, 3600), None);
        }
    }
    #[test]
    fn unavailable_controls_explain_the_actual_constraint() {
        let mut device =
            ConfigurationDevice::from_reading(&Reading::new("test", "Keyboard", "razer", 0));
        device.capability = PollingCapability::Unavailable("multiple collections".into());
        assert!(polling_unavailable(&device).contains("More than one connection"));
        device.capability = PollingCapability::Unavailable("missing interface".into());
        assert!(polling_unavailable(&device).contains("Reconnect"));
        assert!(!polling_unavailable(&device).contains("software running"));
        device.source = "corsair".into();
        assert!(polling_unavailable(&device).contains("software running continuously"));
    }
}

/// Documented owner drawing keeps native popup navigation, command IDs, item
/// strings and accessibility. High contrast retains the system's native menu.
struct PopupItem {
    menu: HMENU,
    position: u32,
    submenu: bool,
    native_type: MENU_ITEM_TYPE,
    native_data: usize,
    text: Vec<u16>,
    separator: bool,
    enabled: bool,
}
struct PopupAppearance {
    menu: HMENU,
    menus: Vec<(HMENU, HBRUSH)>,
    palette: Palette,
    items: Vec<PopupItem>,
    font: HFONT,
    background: HBRUSH,
    width: u32,
    row_height: u32,
    padding: i32,
}
impl Drop for PopupAppearance {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.background.into());
        }
    }
}
fn menu_mnemonic(text: &[u16]) -> Option<char> {
    let mut characters = String::from_utf16_lossy(text)
        .chars()
        .collect::<Vec<_>>()
        .into_iter();
    while let Some(character) = characters.next() {
        if character == '&' {
            let next = characters.next()?;
            if next != '&' {
                return Some(next.to_ascii_lowercase());
            }
        }
    }
    None
}
impl PopupAppearance {
    fn new(menu: HMENU, owner: HWND, palette: Palette) -> Option<Rc<Self>> {
        if palette.high_contrast {
            return None;
        }
        unsafe {
            let dpi = GetDpiForWindow(owner).max(96);
            let padding = (18 * dpi / 96) as i32;
            let font = CreateFontW(
                -((16 * dpi / 96) as i32),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Segoe UI"),
            );
            let mut appearance = Self {
                menu,
                menus: Vec::new(),
                palette,
                items: Vec::new(),
                font,
                background: CreateSolidBrush(palette.surface),
                width: 0,
                row_height: 34 * dpi / 96,
                padding,
            };
            if appearance.font.is_invalid() || appearance.background.is_invalid() {
                return None;
            }
            let hdc = GetDC(Some(owner));
            if hdc.is_invalid() {
                return None;
            }
            let saved = SaveDC(hdc);
            if saved == 0 {
                ReleaseDC(Some(owner), hdc);
                return None;
            }
            SelectObject(hdc, font.into());
            let mut pending_menus = vec![menu];
            let mut next_menu = 0;
            while next_menu < pending_menus.len() {
                let menu = pending_menus[next_menu];
                next_menu += 1;
                let mut native_menu = MENUINFO {
                    cbSize: size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND,
                    ..Default::default()
                };
                if GetMenuInfo(menu, &mut native_menu).is_err() {
                    let _ = RestoreDC(hdc, saved);
                    ReleaseDC(Some(owner), hdc);
                    return None;
                }
                appearance.menus.push((menu, native_menu.hbrBack));
                for index in 0..GetMenuItemCount(Some(menu)).max(0) as u32 {
                    let mut info = MENUITEMINFOW {
                        cbSize: size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_FTYPE | MIIM_STATE | MIIM_SUBMENU | MIIM_DATA,
                        ..Default::default()
                    };
                    if GetMenuItemInfoW(menu, index, true, &mut info).is_err() {
                        let _ = RestoreDC(hdc, saved);
                        ReleaseDC(Some(owner), hdc);
                        return None;
                    }
                    let mut text = [0u16; 2048];
                    let length =
                        GetMenuStringW(menu, index, Some(&mut text), MF_BYPOSITION).max(0) as usize;
                    let text = text[..length].to_vec();
                    let mut rect = RECT::default();
                    let mut measure = text.clone();
                    if !measure.is_empty() {
                        DrawTextW(hdc, &mut measure, &mut rect, DT_SINGLELINE | DT_CALCRECT);
                    }
                    appearance.width = appearance
                        .width
                        .max((rect.right - rect.left).max(0) as u32 + (padding * 2) as u32);
                    appearance.items.push(PopupItem {
                        menu,
                        position: index,
                        submenu: !info.hSubMenu.is_invalid(),
                        native_type: info.fType,
                        native_data: info.dwItemData,
                        text,
                        separator: info.fType.0 & MFT_SEPARATOR.0 != 0,
                        enabled: info.fState.0 & (MFS_DISABLED.0 | MFS_GRAYED.0) == 0,
                    });
                    if !info.hSubMenu.is_invalid() && !pending_menus.contains(&info.hSubMenu) {
                        pending_menus.push(info.hSubMenu);
                    }
                }
            }
            let _ = RestoreDC(hdc, saved);
            ReleaseDC(Some(owner), hdc);
            // Strings remain MIIM_STRING data for accessibility and inspection.
            for (index, item) in appearance.items.iter().enumerate() {
                let info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_DATA,
                    fType: MFT_OWNERDRAW
                        | if item.separator {
                            MFT_SEPARATOR
                        } else {
                            MFT_STRING
                        },
                    dwItemData: index + 1,
                    ..Default::default()
                };
                if SetMenuItemInfoW(item.menu, item.position, true, &info).is_err() {
                    appearance.restore_native();
                    return None;
                }
            }
            let info = MENUINFO {
                cbSize: size_of::<MENUINFO>() as u32,
                fMask: MIM_BACKGROUND,
                hbrBack: appearance.background,
                ..Default::default()
            };
            for (menu, _) in &appearance.menus {
                if SetMenuInfo(*menu, &info).is_err() {
                    appearance.restore_native();
                    return None;
                }
            }
            Some(Rc::new(appearance))
        }
    }
    fn restore_native(&self) {
        unsafe {
            for item in &self.items {
                let info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_DATA,
                    fType: item.native_type,
                    dwItemData: item.native_data,
                    ..Default::default()
                };
                let _ = SetMenuItemInfoW(item.menu, item.position, true, &info);
            }
            // Original brushes belong to the menus' caller; never delete them.
            for (menu, background) in &self.menus {
                let info = MENUINFO {
                    cbSize: size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND,
                    hbrBack: *background,
                    ..Default::default()
                };
                let _ = SetMenuInfo(*menu, &info);
            }
        }
    }
    fn mnemonic(&self, character: char) -> LRESULT {
        self.mnemonic_in(self.menu, character)
    }
    fn mnemonic_in(&self, menu: HMENU, character: char) -> LRESULT {
        let matches = self
            .items
            .iter()
            .filter(|item| {
                item.menu == menu
                    && item.enabled
                    && menu_mnemonic(&item.text) == Some(character.to_ascii_lowercase())
            })
            .map(|item| item.position)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return LRESULT((MNC_IGNORE << 16) as isize);
        }
        let selected = matches.iter().position(|index| unsafe {
            GetMenuState(menu, *index, MF_BYPOSITION) & MF_HILITE.0 != 0
        });
        let index = matches[selected.map_or(0, |selected| (selected + 1) % matches.len())];
        let action = if matches.len() == 1 {
            MNC_EXECUTE
        } else {
            MNC_SELECT
        };
        LRESULT((index | action << 16) as isize)
    }
    fn message(&self, message: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if !matches!(message, WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
            return None;
        }
        unsafe {
            if message == WM_MENUCHAR {
                let menu = HMENU(lp.0 as *mut _);
                if !self.menus.iter().any(|(registered, _)| *registered == menu) {
                    return None;
                }
                let character = char::from_u32((wp.0 & 0xffff) as u32)?;
                return Some(if menu == self.menu {
                    self.mnemonic(character)
                } else {
                    self.mnemonic_in(menu, character)
                });
            }
            if lp.0 == 0 {
                return None;
            }
            if message == WM_MEASUREITEM {
                let item = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
                if item.CtlType != ODT_MENU {
                    return None;
                }
                let entry = self.items.get(item.itemData.checked_sub(1)?)?;
                item.itemWidth = self.width;
                item.itemHeight = if entry.separator {
                    (self.row_height / 3).max(1)
                } else {
                    self.row_height
                };
                return Some(LRESULT(1));
            }
            let item = &*(lp.0 as *const DRAWITEMSTRUCT);
            if item.CtlType != ODT_MENU {
                return None;
            }
            let entry = self.items.get(item.itemData.checked_sub(1)?)?;
            if item.hwndItem.0 != entry.menu.0 {
                return None;
            }
            let saved = SaveDC(item.hDC);
            SelectObject(item.hDC, self.font.into());
            let selected = item.itemState.0 & ODS_SELECTED.0 != 0;
            let disabled = item.itemState.0 & (ODS_DISABLED.0 | ODS_GRAYED.0) != 0;
            FillRect(
                item.hDC,
                &item.rcItem,
                color_brush(item.hDC, self.palette.surface),
            );
            if selected && !disabled {
                let mut highlight = item.rcItem;
                let inset = (self.row_height / 12).max(1) as i32;
                highlight.left += inset;
                highlight.right -= inset;
                highlight.top += inset;
                highlight.bottom -= inset;
                rounded_surface(
                    item.hDC,
                    &highlight,
                    self.palette.selection,
                    self.palette.selection,
                    inset * 3,
                );
            }
            let mut rect = item.rcItem;
            rect.left += self.padding;
            rect.right -= self.padding;
            if entry.separator {
                rect.top = (rect.top + rect.bottom) / 2;
                rect.bottom = rect.top + 1;
                FillRect(item.hDC, &rect, color_brush(item.hDC, self.palette.border));
            } else {
                SetBkMode(item.hDC, TRANSPARENT);
                SetTextColor(
                    item.hDC,
                    if disabled {
                        self.palette.disabled
                    } else if selected {
                        self.palette.selection_text
                    } else {
                        self.palette.text
                    },
                );
                let mut text = entry.text.clone();
                let mut flags = DT_SINGLELINE | DT_VCENTER;
                if item.itemState.0 & ODS_NOACCEL.0 != 0 {
                    flags |= DT_HIDEPREFIX;
                }
                if !text.is_empty() {
                    DrawTextW(item.hDC, &mut text, &mut rect, flags);
                }
                if item.itemState.0 & ODS_CHECKED.0 != 0 {
                    let mut check_rect = item.rcItem;
                    check_rect.right = check_rect.left + self.padding;
                    let mut check = ['✓' as u16];
                    DrawTextW(
                        item.hDC,
                        &mut check,
                        &mut check_rect,
                        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                    );
                }
                if entry.submenu {
                    let mut arrow_rect = item.rcItem;
                    arrow_rect.left = arrow_rect.right - self.padding;
                    let mut arrow = ['›' as u16];
                    DrawTextW(
                        item.hDC,
                        &mut arrow,
                        &mut arrow_rect,
                        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                    );
                }
            }
            let _ = RestoreDC(item.hDC, saved);
            Some(LRESULT(1))
        }
    }
}
struct Popup(HMENU);
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayPollingAction {
    None,
    Settings,
    Read,
    Apply(PollingRate),
    Restore,
}
struct TrayPollingChoice {
    id: u16,
    text: String,
    enabled: bool,
    checked: bool,
    action: TrayPollingAction,
}
fn tray_polling_choices(
    device: &ConfigurationDevice,
    settings: &Settings,
    polling: &PollingUi,
    dashboard_pending: bool,
) -> Vec<TrayPollingChoice> {
    let choice = |id, text: String, enabled, action| TrayPollingChoice {
        id,
        text,
        enabled,
        checked: false,
        action,
    };
    if !settings.polling_controls {
        return vec![choice(
            512,
            "Enable device controls in &Settings…".into(),
            true,
            TrayPollingAction::Settings,
        )];
    }
    let available = device.online() && polling.pending.is_none() && !dashboard_pending;
    let observed = polling
        .observations
        .get(&device.key)
        .filter(|o| o.target.device == *device && o.target.generation == polling.generation);
    let current = observed.and_then(|o| o.rate);
    let mut choices = vec![
        choice(
            0,
            if !device.online() {
                "Device is sleeping or unavailable".into()
            } else if polling.pending.is_some() || dashboard_pending {
                "Device request in progress…".into()
            } else {
                current.map_or_else(
                    || "Choose a rate; the device is checked before changing".into(),
                    |r| format!("Last confirmed: {} Hz", r.hz()),
                )
            },
            false,
            TrayPollingAction::None,
        ),
        choice(
            510,
            "&Refresh rate".into(),
            available,
            TrayPollingAction::Read,
        ),
    ];
    let rates = observed.map_or_else(
        || {
            hb_providers::controls::polling_menu_rates(device)
                .iter()
                .filter_map(|rate| PollingRate::try_from(*rate).ok())
                .collect::<Vec<_>>()
        },
        |o| o.supported.clone(),
    );
    for (index, rate) in rates.iter().take(7).enumerate() {
        let mut row = choice(
            520 + index as u16,
            format!("{} Hz", rate.hz()),
            available,
            TrayPollingAction::Apply(*rate),
        );
        row.checked = current == Some(*rate);
        choices.push(row);
    }
    if let Some(observation) = observed
        && let Some(previous) = polling.previous.get(&device.key)
        && observation.supported.contains(previous)
    {
        choices.push(choice(
            511,
            format!("Restore &previous ({} Hz)", previous.hz()),
            available && current.is_some() && current != Some(*previous),
            TrayPollingAction::Restore,
        ));
    }
    if let Some(status) = polling.status.get(&device.key) {
        let status = polling_message(status);
        let mut text: String = status.chars().take(90).collect();
        if status.chars().count() > 90 {
            text.push('…');
        }
        // Status text is data, never a keyboard mnemonic.
        choices.push(choice(
            0,
            text.replace('&', "&&"),
            false,
            TrayPollingAction::None,
        ));
    }
    choices
}
fn append_polling_menu(menu: HMENU, choices: &[TrayPollingChoice]) -> windows::core::Result<()> {
    unsafe {
        let submenu = Popup(CreatePopupMenu()?);
        for choice in choices {
            let text = wide(&choice.text);
            let flags = MF_STRING
                | if choice.enabled {
                    MF_ENABLED
                } else {
                    MF_GRAYED
                }
                | if choice.checked {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
            AppendMenuW(submenu.0, flags, choice.id as usize, PCWSTR(text.as_ptr()))?;
        }
        InsertMenuW(
            menu,
            4,
            MF_BYPOSITION | MF_POPUP,
            submenu.0.0 as usize,
            w!("&Polling rate"),
        )?;
        // DestroyMenu on the parent now owns the entire submenu tree.
        std::mem::forget(submenu);
        Ok(())
    }
}
fn popup_menu(
    snapshot: &Snapshot,
    settings: &Settings,
    key: Option<&str>,
) -> windows::core::Result<Popup> {
    unsafe {
        let menu = Popup(CreatePopupMenu()?);
        if let Some(d) = key.and_then(|key| snapshot.devices.iter().find(|d| d.reading.key == key))
        {
            let text = wide(&d.text);
            let _ = AppendMenuW(menu.0, MF_STRING | MF_DISABLED, 0, PCWSTR(text.as_ptr()));
            let _ = AppendMenuW(menu.0, MF_STRING, 503, w!("Device &settings"));
            let _ = AppendMenuW(menu.0, MF_STRING, 504, w!("Battery &history"));
            let _ = AppendMenuW(menu.0, MF_STRING, 505, w!("&Hide tray icon"));
            let _ = AppendMenuW(menu.0, MF_STRING, 506, w!("Battery &insights"));
            let _ = AppendMenuW(menu.0, MF_SEPARATOR, 0, None);
        }
        if settings.fluent_menu {
            let mut others = false;
            for (i, d) in snapshot
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| !d.hidden && Some(d.reading.key.as_str()) != key)
            {
                others = true;
                let text = wide(&d.text);
                let _ = AppendMenuW(menu.0, MF_STRING, 600 + i, PCWSTR(text.as_ptr()));
            }
            if others {
                let _ = AppendMenuW(menu.0, MF_SEPARATOR, 0, None);
            }
        }
        let _ = AppendMenuW(menu.0, MF_STRING, 500, w!("Open &dashboard"));
        let _ = AppendMenuW(menu.0, MF_STRING, 501, w!("&Refresh"));
        let _ = AppendMenuW(menu.0, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu.0, MF_STRING, 502, w!("E&xit Halo Continued"));

        Ok(menu)
    }
}

#[cfg(test)]
mod popup_tests {
    use super::*;
    fn polling_fixture() -> (ConfigurationDevice, Settings, PollingUi) {
        let mut reading = Reading::new("simulated:mouse", "Simulated mouse", "simulation", 0);
        reading.kind = "mouse".into();
        let device = ConfigurationDevice::from_reading(&reading);
        let settings = Settings {
            polling_controls: true,
            ..Default::default()
        };
        (device, settings, PollingUi::default())
    }
    #[test]
    fn tray_rate_hints_are_immediate_but_only_readback_is_checked() {
        let (device, settings, mut polling) = polling_fixture();
        let choices = tray_polling_choices(&device, &settings, &polling, false);
        assert!(
            choices
                .iter()
                .any(|c| c.enabled && c.action == TrayPollingAction::Read)
        );
        assert!(
            choices
                .iter()
                .any(|c| c.enabled && matches!(c.action, TrayPollingAction::Apply(_)))
        );
        assert!(choices.iter().all(|c| !c.checked));
        let requested = PollingRate::try_from(2000).unwrap();
        let direct = polling
            .begin_tray(
                device.clone(),
                PollingIntent::Apply {
                    rate: requested,
                    restore: false,
                },
            )
            .unwrap();
        assert_eq!(direct.action, ControlAction::Apply(requested));
        assert_eq!(direct.target.generation, polling.generation);
        assert!(polling.observations.is_empty());
        polling.abandon();
        let request = polling.begin(device.clone(), PollingIntent::Read).unwrap();
        let rate = PollingRate::try_from(1000).unwrap();
        let outcome = ControlOutcome {
            request: request.request,
            key: device.key.clone(),
            observation: Some(PollingObservation {
                target: ControlTarget {
                    device: device.clone(),
                    generation: 3,
                    connection_stamp: None,
                },
                supported: vec![rate, PollingRate::try_from(8000).unwrap()],
                rate: Some(rate),
                timestamp: 5,
                evidence: "Invented readback".into(),
            }),
            previous: None,
            may_have_changed: false,
            failure: None,
        };
        assert!(polling.accept(&outcome, Some(&device.key)));
        let choices = tray_polling_choices(&device, &settings, &polling, false);
        assert_eq!(
            choices
                .iter()
                .filter(|c| matches!(c.action, TrayPollingAction::Apply(_)))
                .count(),
            2
        );
        assert!(
            choices
                .iter()
                .any(|c| c.checked && c.action == TrayPollingAction::Apply(rate))
        );
        assert!(
            !tray_polling_choices(&device, &settings, &polling, true)
                .iter()
                .any(|c| c.enabled)
        );
        let mut sleeping = device.clone();
        sleeping.connection = Connection::Sleeping;
        assert!(
            !tray_polling_choices(&sleeping, &settings, &polling, false)
                .iter()
                .any(|c| c.enabled)
        );
        let mut replaced = device.clone();
        replaced.serial = Some("invented-replacement".into());
        assert!(
            !tray_polling_choices(&replaced, &settings, &polling, false)
                .iter()
                .any(|c| c.checked)
        );
        polling
            .previous
            .insert(device.key.clone(), PollingRate::try_from(8000).unwrap());
        assert!(
            tray_polling_choices(&device, &settings, &polling, false)
                .iter()
                .any(|c| c.enabled && c.action == TrayPollingAction::Restore)
        );
        polling.invalidate(4);
        assert!(
            !tray_polling_choices(&device, &settings, &polling, false)
                .iter()
                .any(|c| matches!(c.action, TrayPollingAction::Restore))
        );
        let disabled = tray_polling_choices(&device, &Settings::default(), &polling, false);
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].action, TrayPollingAction::Settings);
    }
    #[test]
    fn revoked_polling_evidence_cannot_survive_abandoned_ui_write() {
        let (device, settings, mut polling) = polling_fixture();
        let rate = PollingRate::try_from(1000).unwrap();
        polling.observations.insert(
            device.key.clone(),
            PollingObservation {
                target: ControlTarget {
                    device: device.clone(),
                    generation: 0,
                    connection_stamp: None,
                },
                supported: vec![rate],
                rate: Some(rate),
                timestamp: 0,
                evidence: "Invented".into(),
            },
        );
        polling
            .begin(
                device.clone(),
                PollingIntent::Apply {
                    rate,
                    restore: false,
                },
            )
            .unwrap();
        polling.revoke_observation(&device.key);
        assert!(polling.pending.is_some());
        polling.abandon();
        assert!(
            !tray_polling_choices(&device, &settings, &polling, false)
                .iter()
                .any(|c| c.checked)
        );
        assert!(
            polling
                .begin(
                    device,
                    PollingIntent::Apply {
                        rate,
                        restore: false
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn native_polling_submenu_keeps_ids_disabled_states_and_checkmarks() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let menu = popup_menu(&Snapshot::default(), &Settings::default(), None).unwrap();
        let rate = PollingRate::try_from(8000).unwrap();
        let choices = vec![
            TrayPollingChoice {
                id: 0,
                text: "Read first".into(),
                enabled: false,
                checked: false,
                action: TrayPollingAction::None,
            },
            TrayPollingChoice {
                id: 510,
                text: "&Refresh".into(),
                enabled: true,
                checked: false,
                action: TrayPollingAction::Read,
            },
            TrayPollingChoice {
                id: 520,
                text: "8000 Hz".into(),
                enabled: true,
                checked: true,
                action: TrayPollingAction::Apply(rate),
            },
        ];
        append_polling_menu(menu.0, &choices).unwrap();
        unsafe {
            let child = GetSubMenu(menu.0, GetMenuItemCount(Some(menu.0)) - 1);
            assert!(!child.is_invalid());
            assert_eq!(GetMenuItemCount(Some(child)), 3);
            assert_ne!(GetMenuState(child, 0, MF_BYPOSITION) & MF_GRAYED.0, 0);
            assert_eq!(GetMenuItemID(child, 1), 510);
            assert_ne!(GetMenuState(child, 520, MF_BYCOMMAND) & MF_CHECKED.0, 0);
        }
    }
    fn menu_text(menu: HMENU, index: u32) -> String {
        unsafe {
            let mut text = [0u16; 256];
            let n = GetMenuStringW(menu, index, Some(&mut text), MF_BYPOSITION);
            String::from_utf16_lossy(&text[..n as usize])
        }
    }
    #[test]
    fn selected_device_popup_reads_current_summary_and_exposes_accessible_actions() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let settings = Settings::default();
        let mut snapshot = Snapshot::default();
        let reading = Reading::new("mouse", "Mouse", "test", 0);
        snapshot.devices.push(DeviceView {
            reading,
            name: "Mouse".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Mouse: 70% · charging".into(),
            hidden: false,
        });
        let menu = popup_menu(&snapshot, &settings, Some("mouse")).unwrap();
        assert_eq!(menu_text(menu.0, 0), "Mouse: 70% · charging");
        unsafe {
            assert_ne!(GetMenuState(menu.0, 0, MF_BYPOSITION) & MF_DISABLED.0, 0);
            assert_eq!(GetMenuItemID(menu.0, 1), 503);
            assert!(menu_text(menu.0, 1).contains('&'));
            assert_eq!(GetMenuItemID(menu.0, 2), 504);
            assert_eq!(GetMenuItemID(menu.0, 3), 505);
        }
        drop(menu);
        snapshot.devices[0].text = "Renamed mouse: sleeping".into();
        let renamed = popup_menu(&snapshot, &settings, Some("mouse")).unwrap();
        assert_eq!(menu_text(renamed.0, 0), "Renamed mouse: sleeping");
        assert!(!menu_text(renamed.0, 0).contains('\n'));
    }
    #[test]
    fn placeholder_popup_has_no_device_controls_or_icon_picker() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let menu = popup_menu(
            &Snapshot::default(),
            &Settings::default(),
            Some("application"),
        )
        .unwrap();
        unsafe {
            for id in [503, 504, 505, 506] {
                assert_eq!(GetMenuState(menu.0, id, MF_BYCOMMAND), u32::MAX);
            }
            assert!(GetMenuItemCount(Some(menu.0)) >= 3);
        }
    }
}

#[cfg(test)]
mod popup_theme_tests {
    use super::*;
    use windows::Win32::{
        System::Threading::{GR_GDIOBJECTS, GetGuiResources},
        UI::Controls::ODS_FLAGS,
    };
    #[test]
    fn nested_popups_register_unique_items_and_restore_native_metadata() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let menu = Popup(CreatePopupMenu().unwrap());
            let submenu = CreatePopupMenu().unwrap();
            let nested = CreatePopupMenu().unwrap();
            AppendMenuW(nested, MF_STRING, 703, w!("&Refresh")).unwrap();
            AppendMenuW(submenu, MF_STRING, 702, w!("&Rate")).unwrap();
            AppendMenuW(submenu, MF_POPUP, nested.0 as usize, w!("&More")).unwrap();
            AppendMenuW(menu.0, MF_STRING, 700, w!("&Root")).unwrap();
            AppendMenuW(menu.0, MF_POPUP, submenu.0 as usize, w!("&Polling rate")).unwrap();
            let native = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_DATA,
                dwItemData: 77,
                ..Default::default()
            };
            SetMenuItemInfoW(submenu, 0, true, &native).unwrap();
            let appearance =
                PopupAppearance::new(menu.0, HWND::default(), Palette::new(true, false)).unwrap();
            assert_eq!(appearance.menus.len(), 3);
            assert_eq!(appearance.items.len(), 5);
            let mut data = std::collections::BTreeSet::new();
            for (handle, _) in &appearance.menus {
                let mut background = MENUINFO {
                    cbSize: size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND,
                    ..Default::default()
                };
                GetMenuInfo(*handle, &mut background).unwrap();
                assert_eq!(background.hbrBack, appearance.background);
                for position in 0..GetMenuItemCount(Some(*handle)) as u32 {
                    let mut info = MENUITEMINFOW {
                        cbSize: size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_FTYPE | MIIM_DATA,
                        ..Default::default()
                    };
                    GetMenuItemInfoW(*handle, position, true, &mut info).unwrap();
                    assert_ne!(info.fType.0 & MFT_OWNERDRAW.0, 0);
                    assert!(data.insert(info.dwItemData));
                }
            }
            for handle in [menu.0, submenu, nested] {
                let result = appearance
                    .message(WM_MENUCHAR, WPARAM('r' as usize), LPARAM(handle.0 as isize))
                    .unwrap()
                    .0 as u32;
                assert_eq!(result >> 16, MNC_EXECUTE);
                assert_eq!(result & 0xffff, 0);
            }
            assert_eq!(appearance.mnemonic('m').0 as u32 >> 16, MNC_IGNORE);
            appearance.restore_native();
            let mut restored = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_DATA,
                ..Default::default()
            };
            GetMenuItemInfoW(submenu, 0, true, &mut restored).unwrap();
            assert_eq!(restored.dwItemData, 77);
            assert_eq!(restored.fType.0 & MFT_OWNERDRAW.0, 0);
            for (handle, original) in &appearance.menus {
                let mut background = MENUINFO {
                    cbSize: size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND,
                    ..Default::default()
                };
                GetMenuInfo(*handle, &mut background).unwrap();
                assert_eq!(background.hbrBack, *original);
            }
            drop(menu);
            drop(appearance);
        }
    }
    #[test]
    fn menu_mnemonics_skip_escaped_ampersands() {
        assert_eq!(
            menu_mnemonic(&"Open &dashboard".encode_utf16().collect::<Vec<_>>()),
            Some('d')
        );
        assert_eq!(
            menu_mnemonic(&"Mouse && keyboard".encode_utf16().collect::<Vec<_>>()),
            None
        );
        assert_eq!(
            menu_mnemonic(&"Mouse && &keyboard".encode_utf16().collect::<Vec<_>>()),
            Some('k')
        );
    }
    #[test]
    fn native_popup_palette_pixels_mnemonics_and_resources_follow_theme() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            for dark in [false, true] {
                let menu = popup_menu(
                    &Snapshot::default(),
                    &Settings {
                        fluent_menu: false,
                        ..Settings::default()
                    },
                    None,
                )
                .unwrap();
                let palette = Palette::new(dark, false);
                let appearance = PopupAppearance::new(menu.0, HWND::default(), palette).unwrap();
                assert_eq!(appearance.palette, palette);
                assert!(appearance.width > 100);
                assert_eq!(
                    appearance.items[0].text,
                    "Open &dashboard".encode_utf16().collect::<Vec<_>>()
                );
                let mut info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_DATA,
                    ..Default::default()
                };
                GetMenuItemInfoW(menu.0, 0, true, &mut info).unwrap();
                assert_ne!(info.fType.0 & MFT_OWNERDRAW.0, 0);
                assert_eq!(info.dwItemData, 1);
                let result = appearance
                    .message(WM_MENUCHAR, WPARAM('d' as usize), LPARAM(menu.0.0 as isize))
                    .unwrap()
                    .0 as u32;
                assert_eq!(result >> 16, MNC_EXECUTE);
                assert_eq!(GetMenuItemID(menu.0, (result & 0xffff) as i32), 500);
                let source = GetDC(None);
                let hdc = CreateCompatibleDC(Some(source));
                let bitmap = CreateCompatibleBitmap(
                    source,
                    appearance.width as i32,
                    appearance.row_height as i32,
                );
                let old = SelectObject(hdc, bitmap.into());
                let mut item = DRAWITEMSTRUCT {
                    CtlType: ODT_MENU,
                    hwndItem: HWND(menu.0.0),
                    hDC: hdc,
                    rcItem: RECT {
                        left: 0,
                        top: 0,
                        right: appearance.width as i32,
                        bottom: appearance.row_height as i32,
                    },
                    itemData: 1,
                    ..Default::default()
                };
                for (state, expected) in [
                    (Default::default(), palette.surface),
                    (ODS_SELECTED, palette.selection),
                    (ODS_FLAGS(ODS_SELECTED.0 | ODS_DISABLED.0), palette.surface),
                ] {
                    item.itemState = state;
                    assert_eq!(
                        appearance.message(
                            WM_DRAWITEM,
                            WPARAM(0),
                            LPARAM(&item as *const _ as isize)
                        ),
                        Some(LRESULT(1))
                    );
                    assert_eq!(GetPixel(hdc, 8, appearance.row_height as i32 / 2), expected);
                }
                SelectObject(hdc, old);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(hdc);
                ReleaseDC(None, source);
                let font = appearance.font;
                let brush = appearance.background;
                assert!(GetObjectW(font.into(), 0, None) > 0);
                assert!(GetObjectW(brush.into(), 0, None) > 0);
                drop(menu);
                drop(appearance);
            }
            let menu = popup_menu(
                &Snapshot::default(),
                &Settings {
                    fluent_menu: false,
                    ..Settings::default()
                },
                None,
            )
            .unwrap();
            assert!(
                PopupAppearance::new(menu.0, HWND::default(), Palette::new(true, true)).is_none()
            );
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(menu.0, 0, true, &mut info).unwrap();
            assert_eq!(info.fType.0 & MFT_OWNERDRAW.0, 0);
        }
    }
    #[test]
    fn repeated_popup_lifecycle_keeps_gdi_resources_bounded() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let process = windows::Win32::System::Threading::GetCurrentProcess();
            let before = GetGuiResources(process, GR_GDIOBJECTS);
            for _ in 0..40 {
                let menu = popup_menu(&Snapshot::default(), &Settings::default(), None).unwrap();
                let appearance =
                    PopupAppearance::new(menu.0, HWND::default(), Palette::new(true, false))
                        .unwrap();
                assert_eq!(Rc::strong_count(&appearance), 1);
                drop(menu);
                drop(appearance);
            }
            let after = GetGuiResources(process, GR_GDIOBJECTS);
            assert!(
                after <= before + 2,
                "popup GDI resources grew: {before} -> {after}"
            );
        }
    }
    #[test]
    fn native_owner_draw_mnemonics_skip_disabled_and_cycle_duplicates() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let menu = Popup(CreatePopupMenu().unwrap());
            AppendMenuW(menu.0, MF_STRING | MF_DISABLED, 1, w!("&Disabled")).unwrap();
            AppendMenuW(menu.0, MF_STRING, 2, w!("&Dashboard")).unwrap();
            AppendMenuW(menu.0, MF_STRING, 3, w!("&Devices")).unwrap();
            let appearance =
                PopupAppearance::new(menu.0, HWND::default(), Palette::new(true, false)).unwrap();
            let first = appearance.mnemonic('d').0 as u32;
            assert_eq!(first >> 16, MNC_SELECT);
            assert_eq!(first & 0xffff, 1);
            let highlight = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STATE,
                fState: MFS_HILITE,
                ..Default::default()
            };
            SetMenuItemInfoW(menu.0, 1, true, &highlight).unwrap();
            let next = appearance.mnemonic('d').0 as u32;
            assert_eq!(next & 0xffff, 2);
            assert_eq!(appearance.mnemonic('z').0 as u32 >> 16, MNC_IGNORE);
            drop(menu);
            drop(appearance);
        }
    }
}

#[cfg(test)]
mod polling_tests {
    use super::*;
    fn rate(hz: u32) -> PollingRate {
        PollingRate::try_from(hz).unwrap()
    }
    fn reading() -> ConfigurationDevice {
        ConfigurationDevice::from_reading(&Reading::new("mouse", "Mouse", "simulation", 0))
    }
    fn outcome(request: u64, generation: u64, hz: Option<u32>) -> ControlOutcome {
        ControlOutcome {
            request,
            key: "mouse".into(),
            observation: Some(PollingObservation {
                target: ControlTarget {
                    device: reading(),
                    generation,
                    connection_stamp: None,
                },
                supported: vec![rate(1000), rate(4000), rate(8000)],
                rate: hz.map(rate),
                timestamp: 0,
                evidence: "verified simulation".into(),
            }),
            previous: None,
            may_have_changed: false,
            failure: None,
        }
    }
    fn read(ui: &mut PollingUi, generation: u64, hz: u32) {
        let request = ui.begin(reading(), PollingIntent::Read).unwrap();
        assert_eq!(request.action, ControlAction::Read);
        assert_eq!(request.target.generation, 0);
        assert!(ui.accept(
            &outcome(request.request, generation, Some(hz)),
            Some("mouse")
        ));
    }
    #[test]
    fn automatic_failures_revoke_old_rates_and_restore_choices_on_both_surfaces() {
        for reason in 0..4 {
            let mut dashboard = PollingUi::default();
            let mut tray = PollingUi::default();
            read(&mut dashboard, 7, 1000);
            read(&mut tray, 7, 1000);
            dashboard.previous.insert("mouse".into(), rate(4000));
            tray.previous.insert("mouse".into(), rate(4000));
            let mut reply = outcome(1 << 63, 7, Some(8000));
            match reason {
                0 => reply.failure = Some("Uncertain automatic write".into()),
                1 => reply.observation = None,
                2 => reply.observation.as_mut().unwrap().rate = None,
                _ => reply.observation.as_mut().unwrap().target.generation = 6,
            }
            for ui in [&mut dashboard, &mut tray] {
                ui.observe_background(&reply);
                assert!(!ui.observations.contains_key("mouse"));
                assert!(!ui.previous.contains_key("mouse"));
                assert!(
                    ui.begin(
                        reading(),
                        PollingIntent::Apply {
                            rate: rate(4000),
                            restore: true,
                        }
                    )
                    .is_err()
                );
            }
        }
    }
    #[test]
    fn verified_background_read_updates_an_already_open_dashboard() {
        let mut ui = PollingUi::default();
        read(&mut ui, 7, 1000);
        let mut reply = outcome(1 << 63, 7, Some(4000));
        reply.previous = Some(rate(1000));
        reply.may_have_changed = true;
        ui.observe_background(&reply);
        assert_eq!(ui.observations["mouse"].rate, Some(rate(4000)));
        assert_eq!(ui.previous["mouse"], rate(1000));
    }
    #[test]
    fn polling_requires_verified_read_and_supported_rate() {
        assert!(!Settings::default().polling_controls);
        let mut ui = PollingUi::default();
        let intent = PollingIntent::Apply {
            rate: rate(8000),
            restore: false,
        };
        assert!(ui.begin(reading(), intent).is_err());
        read(&mut ui, 7, 1000);
        assert!(ui.pending.is_none()); // A read never schedules an Apply.
        assert!(
            ui.begin(
                reading(),
                PollingIntent::Apply {
                    rate: rate(125),
                    restore: false
                }
            )
            .is_err()
        );
        let request = ui.begin(reading(), intent).unwrap();
        assert_eq!(request.target.generation, 7);
        assert_eq!(request.action, ControlAction::Apply(rate(8000)));
        assert!(ui.begin(reading(), PollingIntent::Read).is_err());
    }
    #[test]
    fn unknown_rate_and_stale_replies_cannot_enable_apply() {
        let mut ui = PollingUi::default();
        let request = ui.begin(reading(), PollingIntent::Read).unwrap();
        assert!(!ui.accept(&outcome(request.request + 1, 3, Some(1000)), Some("mouse")));
        assert!(!ui.accept(&outcome(request.request, 3, Some(1000)), Some("other")));
        assert!(ui.accept(&outcome(request.request, 3, None), Some("mouse")));
        assert!(
            ui.begin(
                reading(),
                PollingIntent::Apply {
                    rate: rate(8000),
                    restore: false
                }
            )
            .is_err()
        );
        ui.invalidate(4);
        assert!(!ui.accept(&outcome(request.request, 3, Some(1000)), Some("mouse")));
        let request = ui.begin(reading(), PollingIntent::Read).unwrap();
        assert!(ui.accept(&outcome(request.request, 3, Some(1000)), Some("mouse")));
        assert!(ui.observations.is_empty());
    }
    #[test]
    fn restore_uses_latest_verified_before_value_and_survives_partial_failure() {
        let mut ui = PollingUi::default();
        read(&mut ui, 2, 1000);
        for (before, after) in [(1000, 8000), (8000, 4000)] {
            let req = ui
                .begin(
                    reading(),
                    PollingIntent::Apply {
                        rate: rate(after),
                        restore: false,
                    },
                )
                .unwrap();
            let mut result = outcome(req.request, 2, Some(after));
            result.previous = Some(rate(before));
            assert!(ui.accept(&result, Some("mouse")));
            assert_eq!(ui.previous["mouse"], rate(before));
        }
        let req = ui
            .begin(
                reading(),
                PollingIntent::Apply {
                    rate: rate(8000),
                    restore: false,
                },
            )
            .unwrap();
        let mut result = outcome(req.request, 2, None);
        result.previous = Some(rate(4000));
        result.failure = Some("Readback failed".into());
        result.may_have_changed = true;
        assert!(ui.accept(&result, Some("mouse")));
        assert_eq!(ui.previous["mouse"], rate(4000));
        assert!(ui.status["mouse"].contains("Hardware may have changed"));
        assert!(!ui.status["mouse"].contains("Confirmed"));
        read(&mut ui, 2, 8000);
        let req = ui
            .begin(
                reading(),
                PollingIntent::Apply {
                    rate: ui.previous["mouse"],
                    restore: true,
                },
            )
            .unwrap();
        assert_eq!(req.action, ControlAction::Apply(rate(4000)));
        let mut result = outcome(req.request, 2, Some(4000));
        result.previous = Some(rate(8000));
        ui.accept(&result, Some("mouse"));
        assert_eq!(ui.previous["mouse"], rate(4000));
        ui.invalidate(3);
        assert!(
            ui.previous.is_empty()
                && ui.status.is_empty()
                && ui.observations.is_empty()
                && ui.pending.is_none()
        );
        ui.invalidate(2);
        assert_eq!(ui.generation, 3);
    }
    #[test]
    fn closed_page_ignores_completion_and_removed_devices_release_observations() {
        let mut ui = PollingUi::default();
        read(&mut ui, 1, 1000);
        let req = ui.begin(reading(), PollingIntent::Read).unwrap();
        ui.abandon();
        assert!(!ui.accept(&outcome(req.request, 1, Some(8000)), Some("mouse")));
        assert_eq!(ui.observations["mouse"].rate, Some(rate(1000)));
        ui.retain_devices(&[]);
        assert!(ui.observations.is_empty() && ui.status.is_empty());
    }
    #[test]
    fn local_read_timestamp_handles_epoch_leap_day_and_invalid_dates() {
        for (timestamp, expected) in [
            (0, (1970, 1, 1, 0, 0, 0)),
            (-1, (1969, 12, 31, 23, 59, 59)),
            (1709210096, (2024, 2, 29, 12, 34, 56)),
        ] {
            let utc = timestamp_utc(timestamp).unwrap();
            assert_eq!(
                (
                    utc.wYear,
                    utc.wMonth,
                    utc.wDay,
                    utc.wHour,
                    utc.wMinute,
                    utc.wSecond
                ),
                expected
            );
            let local = timestamp_local(timestamp).unwrap();
            assert_eq!(
                polling_timestamp(timestamp),
                format!(
                    "{} {} {}, {:02}:{:02}",
                    local.wDay,
                    month_name(local.wMonth),
                    local.wYear,
                    local.wHour,
                    local.wMinute
                )
            );
        }
        for timestamp in [i64::MIN, i64::MAX] {
            assert!(timestamp_utc(timestamp).is_none());
            assert_eq!(polling_timestamp(timestamp), "Date unavailable");
        }
    }
}

#[cfg(test)]
mod configuration_device_ui_tests {
    use super::*;
    fn keyboard(key: &str) -> ConfigurationDevice {
        let mut d = ConfigurationDevice::from_reading(&Reading::new(
            key,
            "Wired keyboard",
            "simulation",
            0,
        ));
        d.kind = "keyboard".into();
        d
    }
    #[test]
    fn keyboard_inventory_has_no_battery_or_tray_view_and_deduplicates_keys() {
        let keyboard = keyboard("keyboard-only");
        let rows = merged_device_rows(&[], &[keyboard.clone(), keyboard]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.is_none());
        assert!(!Settings::default().polling_controls);
    }
    #[test]
    fn sleep_only_mice_have_no_battery_view_and_preserve_fallback_icon() {
        let mut mouse = keyboard("razer:00bf:TEST-MOUSE");
        mouse.source = "razer".into();
        mouse.via = "usb".into();
        mouse.kind = "mouse".into();
        let rows = merged_device_rows(&[], &[mouse.clone(), mouse.clone()]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.is_none());
        let snapshot = Snapshot::default();
        let icons = tray_devices(&snapshot, &Settings::default());
        assert!(icons.iter().all(|icon| icon.reading.key != mouse.key));
        assert_eq!(
            current_device_row(&[], &[mouse.clone()], Some(&mouse.key))
                .unwrap()
                .0,
            mouse
        );
    }
    fn battery(key: &str) -> DeviceView {
        DeviceView {
            reading: Reading::new(key, "Mouse", "simulation", 70),
            name: "Mouse".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Mouse: 70%".into(),
            hidden: false,
        }
    }
    #[test]
    fn mixed_inventory_preserves_batteries_and_replaces_matching_configuration_descriptor() {
        let mouse = battery("mouse");
        let mut matched = ConfigurationDevice::from_reading(&mouse.reading);
        matched.capability = PollingCapability::Unavailable("Read unavailable".into());
        let rows = merged_device_rows(&[mouse], &[matched.clone(), keyboard("keyboard-only")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, matched);
        assert!(rows[0].1.is_some());
        assert!(rows[1].1.is_none());
    }
    #[test]
    fn keyboard_inventory_never_changes_battery_tray_fallback() {
        let snapshot = Snapshot::default();
        let settings = Settings::default();
        let before = tray_devices(&snapshot, &settings);
        let rows = merged_device_rows(&snapshot.devices, &[keyboard("keyboard-only")]);
        assert_eq!(rows.len(), 1);
        let after = tray_devices(&snapshot, &settings);
        assert_eq!(before.len(), after.len());
        assert_eq!(
            before.first().map(|d| &d.reading.key),
            after.first().map(|d| &d.reading.key)
        );
    }
    #[test]
    fn borrowed_device_state_matches_owned_selection_without_inventory_copies() {
        let mut batteries = vec![battery("mouse"), battery("second")];
        for device in &mut batteries {
            device.reading.charging = Some(false);
            device.reading.kind = "mouse".into();
            device.reading.via = "usb".into();
            device.reading.serial = Some("SYNTHETIC-SERIAL".into());
            device.reading.container = Some("SYNTHETIC-CONTAINER".into());
        }
        let mut matched = ConfigurationDevice::from_reading(&batteries[0].reading);
        matched.name = "Configured name".into();
        matched.capability =
            PollingCapability::Unavailable("Synthetic unavailable collection".into());
        let configured = vec![matched, keyboard("keyboard")];
        for (batteries, configuration) in [
            (&batteries[..], &[][..]),
            (&[][..], &configured[..]),
            (&batteries[..], &configured[..]),
            (&[][..], &[][..]),
        ] {
            for selected in [
                None,
                Some("missing"),
                Some("mouse"),
                Some("second"),
                Some("keyboard"),
            ] {
                let owned = current_device_row(batteries, configuration, selected);
                let borrowed = current_device_state(batteries, configuration, selected);
                assert_eq!(
                    borrowed,
                    owned.as_ref().map(|(device, battery)| (
                        DeviceDescriptorRef::from(device),
                        battery.as_ref().and_then(|b| b.reading.charging)
                    )),
                );
            }
        }
        let (_, before) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                std::hint::black_box(current_device_row(&batteries, &configured, Some("mouse")));
                std::hint::black_box(current_device_row(&batteries, &configured, Some("mouse")));
            }
        });
        let (_, after) = crate::allocation_counter::measure(|| {
            for _ in 0..1000 {
                std::hint::black_box(current_device_state(&batteries, &configured, Some("mouse")));
                std::hint::black_box(current_device_state(&batteries, &configured, Some("mouse")));
            }
        });
        println!("1,000 snapshot selection comparisons: owned={before:?}, borrowed={after:?}");
        assert!(before.calls > 0);
        assert_eq!(after.calls, 0);
        assert_eq!(after.bytes, 0);
    }
    #[test]
    fn borrowed_device_descriptor_detects_each_configuration_field_change() {
        let mut original = ConfigurationDevice::from_reading(&battery("mouse").reading);
        original.serial = Some("SERIAL".into());
        original.container = Some("CONTAINER".into());
        for field in 0..9 {
            let mut changed = original.clone();
            match field {
                0 => changed.key.push_str("-new"),
                1 => changed.name.push_str(" renamed"),
                2 => changed.kind.push_str("keyboard"),
                3 => changed.source.push_str("-new"),
                4 => changed.via.push_str("bluetooth"),
                5 => changed.connection = Connection::Stale,
                6 => changed.serial = None,
                7 => changed.container = None,
                8 => changed.capability = PollingCapability::Unavailable("Unavailable".into()),
                _ => unreachable!(),
            }
            assert_ne!(
                DeviceDescriptorRef::from(&original),
                DeviceDescriptorRef::from(&changed)
            );
            assert_eq!(
                DeviceDescriptorRef::from(&original)
                    .same_identity(DeviceDescriptorRef::from(&changed)),
                matches!(field, 1 | 5 | 8),
                "Only name, availability and capability changes preserve physical identity"
            );
        }
    }
    #[test]
    fn direct_selection_and_row_indices_match_mixed_deduplicated_inventory() {
        let batteries = vec![battery("mouse"), battery("second")];
        let matched = ConfigurationDevice::from_reading(&batteries[0].reading);
        let configured = vec![
            matched,
            keyboard("keyboard"),
            keyboard("keyboard"),
            keyboard("other"),
        ];
        let rows = merged_device_rows(&batteries, &configured);
        for selected in [
            None,
            Some("missing"),
            Some("mouse"),
            Some("second"),
            Some("keyboard"),
            Some("other"),
        ] {
            let expected = selected
                .and_then(|key| rows.iter().find(|(d, _)| d.key == key))
                .unwrap_or(&rows[0]);
            let actual = current_device_row(&batteries, &configured, selected).unwrap();
            assert_eq!(actual.0, expected.0);
            assert_eq!(
                actual.1.as_ref().map(|d| &d.reading.key),
                expected.1.as_ref().map(|d| &d.reading.key)
            );
            if let Some(key) = selected {
                assert_eq!(
                    device_row_index(&batteries, &configured, key),
                    rows.iter().position(|(d, _)| d.key == key)
                );
            }
        }
        let views = tray_devices(
            &Snapshot {
                devices: batteries.clone(),
                ..Snapshot::default()
            },
            &Settings::default(),
        )
        .into_iter()
        .map(Cow::into_owned)
        .collect::<Vec<_>>();
        assert_eq!(views.len(), 2);
        let snapshot = Snapshot {
            devices: batteries,
            ..Snapshot::default()
        };
        let views = tray_devices(&snapshot, &Settings::default());
        assert!(matches!(&views[0], Cow::Borrowed(d) if std::ptr::eq(*d, &snapshot.devices[0])));
    }
    #[test]
    fn retention_uses_matching_descriptor_and_rejects_replaced_battery_identity() {
        let mut batteries = vec![battery("mouse")];
        let device = ConfigurationDevice::from_reading(&batteries[0].reading);
        let mut ui = PollingUi::default();
        ui.begin(device, PollingIntent::Read).unwrap();
        ui.retain_inventory(&batteries, &[]);
        assert!(ui.pending.is_some());
        batteries[0].reading.serial = Some("replacement".into());
        ui.retain_inventory(&batteries, &[]);
        assert!(ui.pending.is_none());
    }
    #[test]
    fn unavailable_keyboard_never_submits_hardware_request() {
        let mut device = keyboard("corsair");
        device.capability = PollingCapability::Unavailable("Unsupported protocol".into());
        let mut ui = PollingUi::default();
        assert!(ui.begin(device, PollingIntent::Read).is_err());
        assert!(ui.pending.is_none());
    }
    #[test]
    fn configuration_removal_and_identity_replacement_abandon_pending_read() {
        let device = keyboard("keyboard-only");
        let mut ui = PollingUi::default();
        ui.begin(device.clone(), PollingIntent::Read).unwrap();
        ui.retain_devices(std::slice::from_ref(&device));
        assert!(ui.pending.is_some());
        let mut replacement = device;
        replacement.serial = Some("replacement-unit".into());
        ui.retain_devices(&[replacement]);
        assert!(ui.pending.is_none());
    }
    #[test]
    fn response_from_replaced_identity_cannot_confirm_rate() {
        let mut ui = PollingUi::default();
        let request = ui
            .begin(
                ConfigurationDevice::from_reading(&Reading::new("mouse", "Mouse", "simulation", 0)),
                PollingIntent::Read,
            )
            .unwrap();
        let mut outcome = ControlOutcome {
            request: request.request,
            key: "mouse".into(),
            observation: Some(PollingObservation {
                target: request.target,
                rate: Some(PollingRate::try_from(1000).unwrap()),
                supported: vec![],
                timestamp: 0,
                evidence: "test".into(),
            }),
            previous: None,
            may_have_changed: false,
            failure: None,
        };
        outcome.observation.as_mut().unwrap().target.device.serial =
            Some("replacement-unit".into());
        assert!(ui.accept(&outcome, Some("mouse")));
        assert!(ui.observations.is_empty());
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    #[test]
    fn interactive_resize_defers_queries_until_finished() {
        let mut resize = HistoryResize::default();
        assert!(
            resize.changed(),
            "Maximize/restore resizes reload immediately"
        );
        resize.begin();
        for _ in 0..500 {
            assert!(
                !resize.changed(),
                "Dragging must not enqueue queries per pixel"
            );
        }
        assert!(resize.finish(), "One query is needed for the final width");
        assert!(!resize.finish(), "No duplicate query after finishing");
        resize.begin();
        assert!(!resize.finish(), "Moving without resizing needs no query");
    }
    #[test]
    fn time_used_is_default_and_routes_24_hours_to_usage_query() {
        let selection = HistorySelection::default();
        assert_eq!(selection.axis, HistoryAxis::Usage);
        assert_eq!(selection.index(), 2);
        assert_eq!(selection.labels()[2], "24 hours used");
        let Command::UsageHistory {
            key,
            seconds,
            until,
            width,
            request,
        } = selection.command("mouse".into(), 123456, 640, 7)
        else {
            panic!("Usage history must not use calendar timestamps")
        };
        assert_eq!(
            (key.as_str(), seconds, until, width, request),
            ("mouse", 86400, 123456, 640, 7)
        );
        assert!(
            selection
                .description()
                .contains("Pauses during sleep, charging or disconnection")
        );
    }
    #[test]
    fn calendar_and_usage_keep_independent_ranges_and_route_the_requested_width() {
        let mut selection = HistorySelection::default();
        selection.set_index(0);
        selection.axis = HistoryAxis::Calendar;
        assert_eq!(selection.index(), 0);
        selection.set_index(1);
        let Command::History {
            since,
            until,
            width,
            request,
            ..
        } = selection.command("mouse".into(), 1_000_000, 333, 8)
        else {
            panic!("Calendar history must use timestamps")
        };
        assert_eq!((since, until, width, request), (395200, 1_000_000, 333, 8));
        assert!(
            selection
                .description()
                .contains("last known battery level between readings")
        );
        selection.axis = HistoryAxis::Usage;
        assert_eq!(selection.index(), 0);
        assert_eq!(selection.labels()[0], "2 hours used");
        selection.set_index(99);
        assert_eq!(selection.index(), 2);
        selection.axis = HistoryAxis::Calendar;
        assert_eq!(selection.index(), 1);
    }
}

#[cfg(test)]
mod tray_registration_tests {
    use super::*;
    fn data() -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            guidItem: GUID::from_u128(stable_guid("razer:receiver")),
            uID: 7,
            uFlags: NIF_GUID | NIF_ICON | NIF_MESSAGE | NIF_TIP,
            ..Default::default()
        }
    }
    #[derive(Default)]
    struct Recorder {
        calls: Vec<(NOTIFY_ICON_MESSAGE, GUID, u32)>,
        fail_modify: bool,
        fail_add: bool,
    }
    impl Recorder {
        fn notify(&mut self, command: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
            self.calls.push((command, data.guidItem, data.uID));
            !(command == NIM_MODIFY && self.fail_modify || command == NIM_ADD && self.fail_add)
        }
        fn commands(&self) -> Vec<NOTIFY_ICON_MESSAGE> {
            self.calls.iter().map(|c| c.0).collect()
        }
    }
    #[test]
    fn sleep_wake_settings_and_theme_modify_same_guid_without_recreating_icon() {
        let mut registration = TrayRegistration::default();
        let data = data();
        let mut shell = Recorder::default();
        registration.update_with(&data, TrayUpdate::Changed, true, &mut |m, d| {
            shell.notify(m, d)
        });
        // Sleep and wake change icon/tooltip, but not the Shell registration.
        for _ in 0..2 {
            registration.update_with(&data, TrayUpdate::Changed, true, &mut |m, d| {
                shell.notify(m, d)
            });
        }
        let mode = tray_message_update(WM_SETTINGCHANGE, 0xC123).unwrap();
        assert_eq!(mode, TrayUpdate::Redraw);
        registration.update_with(&data, mode, false, &mut |m, d| shell.notify(m, d));
        registration.update_with(&data, TrayUpdate::Redraw, false, &mut |m, d| {
            shell.notify(m, d)
        });
        registration.update_with(&data, TrayUpdate::Changed, false, &mut |m, d| {
            shell.notify(m, d)
        });
        assert_eq!(
            shell.commands(),
            [NIM_ADD, NIM_MODIFY, NIM_MODIFY, NIM_MODIFY, NIM_MODIFY]
        );
        assert!(
            shell
                .calls
                .iter()
                .all(|(_, guid, id)| *guid == data.guidItem && *id == 7)
        );
        registration.remove_with(&data, &mut |m, d| shell.notify(m, d));
        registration.remove_with(&data, &mut |m, d| shell.notify(m, d));
        assert_eq!(shell.commands().last(), Some(&NIM_DELETE));
        assert_eq!(
            shell
                .commands()
                .iter()
                .filter(|c| **c == NIM_DELETE)
                .count(),
            1
        );
    }
    #[test]
    fn explorer_recovery_modifies_healthy_icon_and_adds_only_after_missing_icon() {
        let mut registration = TrayRegistration::default();
        let data = data();
        let mut shell = Recorder::default();
        registration.update_with(&data, TrayUpdate::Changed, true, &mut |m, d| {
            shell.notify(m, d)
        });
        let mode = tray_message_update(0xC123, 0xC123).unwrap();
        assert_eq!(mode, TrayUpdate::ExplorerRecovery);
        registration.update_with(&data, mode, false, &mut |m, d| shell.notify(m, d));
        assert_eq!(shell.commands(), [NIM_ADD, NIM_MODIFY]);
        shell.fail_modify = true;
        registration.update_with(&data, mode, false, &mut |m, d| shell.notify(m, d));
        assert_eq!(shell.commands(), [NIM_ADD, NIM_MODIFY, NIM_MODIFY, NIM_ADD]);
        assert!(registration.registered);
        assert!(!shell.commands().contains(&NIM_DELETE));
        assert!(
            shell
                .calls
                .iter()
                .all(|(_, guid, _)| *guid == data.guidItem)
        );
    }
    #[test]
    fn ordinary_modify_failure_preserves_registration_and_failed_add_can_retry() {
        let mut registration = TrayRegistration::default();
        let data = data();
        let mut shell = Recorder {
            fail_add: true,
            ..Default::default()
        };
        registration.update_with(&data, TrayUpdate::Changed, true, &mut |m, d| {
            shell.notify(m, d)
        });
        assert!(!registration.registered);
        shell.fail_add = false;
        registration.update_with(&data, TrayUpdate::Changed, false, &mut |m, d| {
            shell.notify(m, d)
        });
        assert!(registration.registered);
        shell.fail_modify = true;
        registration.update_with(&data, TrayUpdate::Redraw, false, &mut |m, d| {
            shell.notify(m, d)
        });
        registration.update_with(&data, TrayUpdate::Changed, false, &mut |m, d| {
            shell.notify(m, d)
        });
        assert_eq!(shell.commands(), [NIM_ADD, NIM_ADD, NIM_MODIFY]);
        assert!(registration.registered);
        assert_eq!(tray_message_update(WM_NULL, 0), None);
    }
}

#[cfg(test)]
mod tooltip_polling_tests {
    use super::*;
    #[test]
    fn notification_text_truncation_keeps_complete_unicode_characters() {
        let mut notification = [0u16; 256];
        let message = format!("{}🖱 battery is low", "a".repeat(254));
        copy(&mut notification, &message);
        assert_eq!(text(&notification), "a".repeat(254));
        assert_eq!(notification[255], 0);
        copy(&mut [], &message);
        let mut tiny = [7u16; 2];
        copy(&mut tiny, "🖱");
        assert_eq!(tiny, [0, 0]);
    }
    fn fixture() -> (DeviceView, ControlOutcome) {
        let mut reading = Reading::new("synthetic", "Test mouse", "simulation", 10);
        reading.kind = "mouse".into();
        reading.level = Some(100);
        let device = DeviceView {
            reading: reading.clone(),
            name: reading.name.clone(),
            icon: "mouse".into(),
            low_alert_at: 15,
            seconds_left: None,
            text: "Test mouse: 100%".into(),
            hidden: false,
        };
        let observation = PollingObservation {
            target: ControlTarget {
                device: ConfigurationDevice::from_reading(&reading),
                generation: 3,
                connection_stamp: None,
            },
            supported: vec![PollingRate::try_from(2000).unwrap()],
            rate: Some(PollingRate::try_from(2000).unwrap()),
            timestamp: 10,
            evidence: "Simulation".into(),
        };
        (
            device,
            ControlOutcome {
                request: 1,
                key: reading.key,
                observation: Some(observation),
                previous: None,
                may_have_changed: false,
                failure: None,
            },
        )
    }
    fn text(buffer: &[u16]) -> String {
        String::from_utf16(buffer.split(|u| *u == 0).next().unwrap()).unwrap()
    }
    #[test]
    fn tooltip_uses_confirmed_rate_and_drops_failed_or_revoked_evidence() {
        let (device, mut outcome) = fixture();
        let mut cache = BTreeMap::new();
        let mut tip = [0u16; 128];
        cache_tooltip_rate(&mut cache, &outcome, 3, true);
        copy_tray_tooltip(&mut tip, &device, cache.get("synthetic"));
        assert_eq!(
            text(&tip),
            "Test mouse: 100%\nPolling: 2000 Hz (last confirmed)"
        );
        outcome.failure = Some("Readback failed".into());
        cache_tooltip_rate(&mut cache, &outcome, 3, true);
        copy_tray_tooltip(&mut tip, &device, cache.get("synthetic"));
        assert_eq!(text(&tip), device.text);
        outcome.failure = None;
        for (generation, enabled) in [(4, true), (3, false)] {
            cache_tooltip_rate(&mut cache, &outcome, generation, enabled);
            assert!(cache.is_empty());
        }
    }
    #[test]
    fn tooltip_reserves_rate_line_without_splitting_unicode_and_matches_identity() {
        let (mut device, outcome) = fixture();
        device.text = "🖱".repeat(128);
        let mut tip = [0u16; 128];
        copy_tray_tooltip(&mut tip, &device, outcome.observation.as_ref());
        assert_eq!(tip[127], 0);
        assert!(text(&tip).ends_with("Polling: 2000 Hz (last confirmed)"));
        device.reading.serial = Some("other-synthetic-unit".into());
        copy_tray_tooltip(&mut tip, &device, outcome.observation.as_ref());
        assert!(!text(&tip).contains("Polling:"));
        device.reading.serial = None;
        device.reading.connection = Connection::Stale;
        copy_tray_tooltip(&mut tip, &device, outcome.observation.as_ref());
        assert!(!text(&tip).contains("Polling:"));
    }
}

#[cfg(test)]
mod insights_tests {
    use super::*;
    #[test]
    fn adaptive_cards_explain_missing_rate_data_instead_of_restarting_learning() {
        let mut data = BatteryInsights::default();
        data.coverage.observation_count = 1000;
        data.recent.total_seconds = 227418;
        data.recent.rates = [0, 0, 0, 76541, 0, 28978, 0];
        data.rates = [1000, 4000]
            .into_iter()
            .map(|hz| RateInsight {
                hz,
                projection_seconds: 7200,
                projection_consumed_percent: 10,
                projected_full_charge_hours: Some(60.0),
                ..Default::default()
            })
            .collect();
        let prediction = data.adaptive_prediction(PollingRate::try_from(4000).ok(), None);
        assert!(prediction.missing_rates.is_empty());
        assert!(prediction.full_hours.is_none());
        assert!(insight_missing_rate_data(Some(&data), &prediction));
        let text = insight_prediction_explanation(Some(&data), &prediction, true);
        assert!(text.contains("46%") && text.contains("80% needed"));
        assert!(text.contains("when your mouse wakes"));
        assert!(text.contains("Older unknown periods stay excluded"));
        assert!(!text.contains("Still learning"));
        assert!(
            insight_prediction_explanation(Some(&data), &prediction, false)
                .contains("Turn on Allow device settings changes")
        );
        // Missing drain evidence is a separate reason when rate coverage is good.
        data.recent.total_seconds = 105519;
        data.rates.clear();
        let prediction = data.adaptive_prediction(None, None);
        assert!(!insight_missing_rate_data(Some(&data), &prediction));
        let text = insight_prediction_explanation(Some(&data), &prediction, true);
        assert!(text.contains("Still learning at 1000 Hz, 4000 Hz"));
        data.recent.total_seconds = 1200;
        data.recent.rates = [0, 0, 0, 1200, 0, 0, 0];
        data.rates.push(RateInsight {
            hz: 1000,
            projection_seconds: 7200,
            projection_consumed_percent: 10,
            projected_full_charge_hours: Some(60.0),
            ..Default::default()
        });
        let prediction = data.adaptive_prediction(None, None);
        assert!(
            insight_prediction_explanation(Some(&data), &prediction, true).contains("30 minutes")
        );
    }
    #[test]
    fn alert_summary_distinguishes_default_override_and_disabled_delivery() {
        let mut settings = Settings {
            low: 20,
            ..Default::default()
        };
        assert!(device_alert_summary(&settings, "synthetic").contains("20% (uses Settings)"));
        settings.device_mut("synthetic").low = Some(15);
        assert!(
            device_alert_summary(&settings, "synthetic")
                .contains("15% (device override; default 20%)")
        );
        settings.notify = false;
        settings.low_sound = false;
        assert!(device_alert_summary(&settings, "synthetic").contains("both off"));
    }
    #[test]
    fn formatting_exposes_evidence_and_estimation_limits() {
        let rate = RateInsight {
            hz: 1000,
            awake_seconds: 7200,
            projection_seconds: 7200,
            projection_consumed_percent: 20,
            projection_drop_count: 4,
            consumed_percent: 20,
            sample_count: 15,
            drop_count: 4,
            confidence: InsightConfidence::Low,
            projected_full_charge_hours: Some(10.0),
            remaining_hours: None,
        };
        let text = rate_insight_text(&rate);
        for expected in [
            "1000 Hz",
            "Early estimate",
            "Estimate based on · 2.0 hours of battery use",
            "Battery used in that time · 20%",
            "Full battery · about 10 hours",
            "At this rate, time left · Waiting for a fresh reading",
            "Rate changes and gaps are excluded.",
        ] {
            assert!(text.contains(expected), "Missing {expected}: {text}");
        }
        assert_eq!(
            insight_estimate(Some(f64::NAN)),
            "Keep using the device to build an estimate"
        );
        assert_eq!(
            insight_estimate(Some(-1.0)),
            "Keep using the device to build an estimate"
        );
        assert!(INSIGHTS_EMPTY.contains("tray menu"));
        assert!(INSIGHTS_EMPTY.contains("Only rates confirmed by the device are included"));
        assert!(!text.contains("samples") && !text.contains("confidence"));
        assert!(!text.contains("/ 30 min"));
        let mut learning = rate;
        learning.projected_full_charge_hours = None;
        learning.projection_seconds = 600;
        learning.projection_consumed_percent = 1;
        let text = rate_insight_text(&learning);
        assert!(text.contains("Learning time · 10 / 30 min"));
        assert!(text.contains("Battery used · 1 / 3 percentage points"));
    }
    #[test]
    fn insight_coverage_explains_empty_rate_evidence_and_corruption() {
        let mut data = BatteryInsights::default();
        assert!(insight_empty_text(Some(&data), true).contains("No battery history yet"));
        data.coverage.observation_count = 12;
        assert!(insight_empty_text(Some(&data), true).contains("More time on battery is needed"));
        data.coverage.discharge_sample_count = 5;
        data.coverage.awake_seconds = 3600;
        data.coverage.excluded_interval_count = 6;
        data.coverage.unreadable_row_count = 1;
        data.coverage.last_reading_timestamp = Some(0);
        assert!(insight_empty_text(Some(&data), true).contains("Check the device's current rate"));
        let text = insight_coverage_text(&data);
        for expected in [
            "1.0 hours of estimated use in the last 30 days",
            "Some saved readings could not be used",
        ] {
            assert!(text.contains(expected), "{text}");
        }
    }
    #[test]
    fn charge_summary_distinguishes_partial_and_inferred_evidence() {
        let mut cycle = ChargeCycle {
            start_timestamp: 0,
            end_timestamp: 3600,
            awake_seconds: 1800,
            start_percent: 70,
            end_percent: 65,
            consumed_percent: 5,
            evidence: CycleEvidence::Partial,
            current: true,
        };
        let text = charge_cycle_text(&cycle);
        assert!(text.contains("Latest session"));
        assert!(text.contains("Charging was not recorded at the start"));
        assert!(text.contains("70% to 65%"));
        assert!(text.contains("Estimated use: 30 min"));
        assert!(charge_cycle_row(&cycle).ends_with(" · 30 min"));
        assert!(text.contains("Average battery use: 10.0% per hour"));
        cycle.start_percent = 100;
        cycle.end_percent = 19;
        cycle.consumed_percent = 70;
        assert!(
            charge_cycle_text(&cycle)
                .contains("70 percentage points · 11 outside counted intervals")
        );
        cycle.awake_seconds = 0;
        assert!(
            charge_cycle_text(&cycle).contains("Average battery use: Not enough recorded use yet")
        );
        cycle.evidence = CycleEvidence::InferredCharge;
        assert!(
            charge_cycle_text(&cycle)
                .contains("Possible charging detected from a rising battery level")
        );
        cycle.evidence = CycleEvidence::ObservedCharge;
        assert!(charge_cycle_text(&cycle).contains("may cover only part of a charge"));
    }
    #[test]
    fn insights_coalesce_pending_refreshes_and_clear_failed_results() {
        let mut ui = InsightsUi::default();
        ui.select(Some("device".into()));
        let first = ui.begin("device").unwrap();
        for _ in 0..20 {
            assert_eq!(ui.begin("device"), None);
        }
        assert!(ui.accept(first, Some("device"), Ok(BatteryInsights::default())));
        let second = ui.begin("device").unwrap();
        assert_ne!(first, second);
        assert!(ui.accept(
            second,
            Some("device"),
            Err(ProviderError::new("Test error"))
        ));
        assert!(ui.data.is_none());
        assert!(ui.status.contains("Couldn't load"));
        ui.begin("device").unwrap();
        ui.select(Some("other".into()));
        assert!(ui.begin("other").is_some());
    }
    #[test]
    fn insights_explain_projection_evidence_and_unsupported_devices() {
        let mut rate = RateInsight {
            hz: 1000,
            awake_seconds: 12 * 3600,
            projection_seconds: 2 * 3600,
            drop_count: 4,
            projected_full_charge_hours: Some(0.4),
            ..RateInsight::default()
        };
        let text = rate_insight_text(&rate);
        assert!(text.contains("Estimate based on · 2.0 hours of battery use"));
        assert!(text.contains("Battery used in that time · 0%"));
        assert!(rate_insight_row(&rate).contains("less than 1 hour"));
        rate.projected_full_charge_hours = Some(f64::NAN);
        assert!(rate_insight_row(&rate).contains("Still learning"));
        let text = insight_empty_text(None, false);
        assert!(text.contains("not available for this device"));
        assert!(!text.contains("allow polling") && !text.contains("Refresh rate"));
        let data = BatteryInsights {
            coverage: InsightCoverage {
                last_reading_timestamp: Some(1_700_000_000),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(insight_coverage_text(&data).contains(&polling_timestamp(1_700_000_000)));
    }
    #[test]
    fn closed_or_switched_selection_ignores_insight_replies() {
        let mut ui = InsightsUi::default();
        ui.select(Some("simulation".into()));
        ui.pending = Some((1, "simulation".into()));
        assert!(!ui.accept(2, Some("simulation"), Ok(BatteryInsights::default())));
        assert!(!ui.accept(1, Some("other"), Ok(BatteryInsights::default())));
        assert!(ui.accept(1, Some("simulation"), Ok(BatteryInsights::default())));
        ui.pending = Some((2, "simulation".into()));
        ui.abandon();
        assert!(!ui.accept(2, Some("simulation"), Ok(BatteryInsights::default())));
        ui.select(Some("other".into()));
        assert!(ui.data.is_none());
    }
}

#[cfg(test)]
mod dashboard_lifecycle_tests {
    use super::*;

    // Opt-in resource measurement of the optimized native test fixture. Never touches real hardware.
    fn sample_closed_dashboard(context: &UiContext, phase: &str) {
        let Some(requested) = std::env::var_os("HALO_MEASURE_UI_RESOURCES") else {
            return;
        };
        // "1" measures both lifecycle phases; a phase name selects one
        // three-minute window for a matched before/after code comparison.
        if requested != "1" && requested != phase {
            return;
        }
        use windows::Win32::System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{GetCurrentProcess, GetProcessTimes},
        };
        unsafe {
            let process = GetCurrentProcess();
            let cpu = || {
                let (mut created, mut exited, mut kernel, mut user) = (
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                );
                GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)
                    .unwrap();
                let ticks =
                    |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
                ticks(kernel) + ticks(user)
            };
            let monitor = context.monitor.get();
            context
                .state
                .borrow()
                .runtime
                .attach_window(monitor.0 as usize);
            context.state.borrow_mut().drain();
            let start = Instant::now();
            let start_cpu = cpu();
            let (mut total, mut count, mut peak, mut last) = (0u64, 0u64, 0usize, 0usize);
            eprintln!("Starting three-minute UI resource sample: {phase}");
            while start.elapsed() < Duration::from_secs(180) {
                MsgWaitForMultipleObjectsEx(None, 1000, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                // The fixture also owns native framework windows. Drain the whole
                // thread queue so an unrelated paint cannot leave the wait signalled.
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                let mut memory = PROCESS_MEMORY_COUNTERS_EX {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ..Default::default()
                };
                assert!(
                    K32GetProcessMemoryInfo(
                        process,
                        (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                        memory.cb
                    )
                    .as_bool()
                );
                last = memory.PrivateUsage;
                total += last as u64;
                count += 1;
                peak = peak.max(last);
                assert!(context.state.borrow().dashboard.is_none());
            }
            let elapsed = start.elapsed().as_secs_f64();
            let report = serde_json::json!({"phase": phase, "seconds": elapsed,
                "cpu_percent_one_core": (cpu() - start_cpu) as f64 / 10_000_000. / elapsed * 100.,
                "private_mean_mib": total as f64 / count as f64 / 1048576.,
                "private_peak_mib": peak as f64 / 1048576., "private_end_mib": last as f64 / 1048576.,
                "workload": "Optimized native test fixture, simulated mouse, dashboard closed, no charging animation"});
            let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../validation-local/ui-modern-resources");
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(
                output.join(format!("{phase}.json")),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            eprintln!("{report}");
        }
    }

    unsafe fn dispatch_monitor(monitor: HWND) {
        let mut message = MSG::default();
        unsafe {
            // Scope pumping to this test's monitor. Do not consume another
            // process/window's commands or depend on timer timing.
            while PeekMessageW(
                &mut message,
                Some(monitor),
                WM_APP + 8,
                WM_APP + 12,
                PM_REMOVE,
            )
            .as_bool()
            {
                DispatchMessageW(&message);
            }
        }
    }

    #[test]
    fn native_dashboard_reopens_after_nested_close_and_external_destruction() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        for restore in [false, true] {
            exercise_native_dashboard(restore);
        }
    }
    unsafe fn exercise_tray_selection(context: &UiContext) {
        fn battery(key: &str, name: &str) -> DeviceView {
            let mut reading = Reading::new(key, name, "simulation", 120);
            reading.level = Some(50);
            reading.charging = Some(false);
            reading.kind = "mouse".into();
            DeviceView {
                reading,
                name: name.into(),
                icon: "mouse".into(),
                low_alert_at: 20,
                seconds_left: None,
                text: format!("{name}: 50%"),
                hidden: false,
            }
        }
        for page in [1, 2, 6] {
            let (second_icon, history_request, insights_sequence, name_control) = {
                let mut state = context.state.borrow_mut();
                state.settings.polling_controls = false;
                state.snapshot.devices = vec![
                    battery("synthetic:first", "First mouse"),
                    battery("synthetic:second", "Second mouse"),
                ];
                state.selected = 0;
                state.selected_device = Some("synthetic:first".into());
                state.page = page;
                state.build();
                state.sync_trays(TrayUpdate::Changed);
                if page == 1 {
                    state.set_control_text(11, "Unsaved first mouse");
                }
                (
                    state.trays["synthetic:second"].data.uID,
                    state.request,
                    state.insights.sequence,
                    state.controls.get(&11).copied(),
                )
            };
            for event in [
                WM_MOUSEMOVE,
                WM_LBUTTONDOWN,
                WM_RBUTTONDOWN,
                NIN_POPUPOPEN,
                NIN_POPUPCLOSE,
            ] {
                unsafe {
                    send(
                        context.monitor.get(),
                        TRAY,
                        WPARAM(second_icon as usize),
                        LPARAM(event as isize),
                    );
                }
                let state = context.state.borrow();
                assert_eq!(
                    state.selected, 0,
                    "Passive tray event changed selection on page {page}"
                );
                assert_eq!(state.selected_device.as_deref(), Some("synthetic:first"));
                assert_eq!(state.request, history_request);
                assert_eq!(state.insights.sequence, insights_sequence);
                if page == 1 {
                    assert_eq!(state.controls.get(&11).copied(), name_control);
                    assert_eq!(state.text(11), "Unsaved first mouse");
                }
            }
            let (_, hover_allocations) = crate::allocation_counter::measure(|| {
                for _ in 0..1000 {
                    unsafe {
                        send(
                            context.monitor.get(),
                            TRAY,
                            WPARAM(second_icon as usize),
                            LPARAM(WM_MOUSEMOVE as isize),
                        );
                    }
                }
            });
            assert_eq!(hover_allocations.calls, 0);
            assert_eq!(hover_allocations.bytes, 0);
            unsafe {
                send(
                    context.monitor.get(),
                    TRAY,
                    WPARAM(second_icon as usize),
                    LPARAM(WM_LBUTTONUP as isize),
                );
            }
            let state = context.state.borrow();
            assert_eq!(state.selected, 1);
            assert_eq!(state.selected_device.as_deref(), Some("synthetic:second"));
            assert_eq!(
                state.choice(10),
                1,
                "Clicked tray device must match the visible form"
            );
            match page {
                1 => assert_eq!(state.text(11), "Second mouse"),
                2 => assert_ne!(state.request, history_request),
                6 => assert_eq!(state.insights.key.as_deref(), Some("synthetic:second")),
                _ => unreachable!(),
            }
        }
        let mut state = context.state.borrow_mut();
        state.snapshot = Snapshot::default();
        state.selected = 0;
        state.selected_device = None;
        state.page = 1;
        state.insights.select(None);
        state.sync_trays(TrayUpdate::Changed);
        state.build();
    }
    fn exercise_native_dashboard(restore: bool) {
        let mut settings = Settings {
            polling_controls: true,
            restore_polling_on_startup: restore,
            ..Settings::default()
        };
        settings
            .device_mut("simulated:mouse")
            .requested_polling_rate = Some(PollingRate::try_from(2000).unwrap());
        let test_directory = tempfile::tempdir().unwrap();
        let dir = test_directory.path().to_path_buf();
        let runtime = Runtime::start(dir.clone(), settings.clone(), true).unwrap();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = w!("HaloBatteryNext.Native");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let context = Box::new(UiContext {
                state: RefCell::new(State {
                    context: std::ptr::null(),
                    runtime,
                    settings,
                    dir,
                    monitor: HWND::default(),
                    dashboard: None,
                    controls: BTreeMap::new(),
                    trays: BTreeMap::new(),
                    snapshot: Snapshot::default(),
                    diagnostics: BTreeMap::new(),
                    page: 1,
                    selected: 0,
                    selected_device: None,
                    device_form_key: None,
                    configuration_devices: Vec::new(),
                    configuration_generation: 0,
                    configuration_failure: None,
                    chart: None,
                    theme: None,
                    series: HistorySeries::default(),
                    history: HistorySelection::default(),
                    history_resize: HistoryResize::default(),
                    request: 0,
                    taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
                    notify: None,
                    animating: false,
                    tray_theme: TrayThemeCache::default(),
                    error: String::new(),
                    font: HFONT::default(),
                    heading_font: HFONT::default(),
                    font_dpi: 0,
                    page_host: None,
                    page_scroll: 0,
                    wheel_remainder: 0,
                    page_height: 0,
                    more_options: false,
                    polling: PollingUi::default(),
                    tray_polling: PollingUi::default(),
                    insights: InsightsUi::default(),
                    sleep: SleepUi::default(),
                    dpi: DpiUi::default(),
                    polling_intents: BTreeMap::new(),
                    tooltip_rates: BTreeMap::new(),
                }),
                monitor: Cell::new(HWND::default()),
                paint: RefCell::new(None),
                popup: RefCell::new(None),
            });
            let ptr = &*context as *const UiContext;
            {
                let mut state = context.state.borrow_mut();
                state.context = ptr;
                state.monitor = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    w!("Test monitor"),
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance.into()),
                    Some(ptr.cast()),
                )
                .unwrap();
                context.monitor.set(state.monitor);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !state.tooltip_rates.contains_key("simulated:mouse")
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                    state.drain();
                }
                assert!(state.tooltip_rates.contains_key("simulated:mouse"));
                assert!(state.dashboard.is_none());
                assert!(state.controls.is_empty());
                let hz = if restore { 2000 } else { 1000 };
                let expected_tip = format!("Polling: {hz} Hz (last confirmed)");
                assert_eq!(
                    state.tray_polling.previous.contains_key("simulated:mouse"),
                    restore
                );
                let tray = state.trays.get("simulated:mouse").unwrap();
                let text =
                    String::from_utf16(tray.data.szTip.split(|u| *u == 0).next().unwrap()).unwrap();
                assert!(text.contains(&expected_tip), "{text}");
                let previous_generation = state.tray_polling.generation;
                // Exercise the real dispatcher for startup, boost and saver
                // completions: an uncertain write must clear both UI caches.
                let observed = state.tooltip_rates["simulated:mouse"].clone();
                for bit in [
                    0,
                    crate::boost::REQUEST_BIT,
                    crate::battery_saver::REQUEST_BIT,
                ] {
                    let mut reply = ControlOutcome {
                        request: crate::runtime::STARTUP_POLLING_REQUEST_BIT | bit,
                        key: "simulated:mouse".into(),
                        observation: Some(observed.clone()),
                        previous: None,
                        may_have_changed: true,
                        failure: Some("Synthetic uncertain rate change".into()),
                    };
                    state.polling_outcome(reply.clone());
                    assert!(state.polling.observations.is_empty());
                    assert!(state.tray_polling.observations.is_empty());
                    assert!(state.tooltip_rates.is_empty());
                    reply.failure = None;
                    state.polling_outcome(reply);
                    assert_eq!(
                        state.polling.observations["simulated:mouse"].rate,
                        observed.rate
                    );
                    assert_eq!(
                        state.tray_polling.observations["simulated:mouse"].rate,
                        observed.rate
                    );
                }
                state.runtime.send(Command::Refresh);
                let deadline = Instant::now() + Duration::from_secs(5);
                while state.tray_polling.generation == previous_generation
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                    state.drain();
                }
                assert!(state.tray_polling.generation > previous_generation);
                assert!(state.tray_polling.observations.is_empty());
                let tray = state.trays.get("simulated:mouse").unwrap();
                let text =
                    String::from_utf16(tray.data.szTip.split(|u| *u == 0).next().unwrap()).unwrap();
                assert!(text.contains(&expected_tip), "{text}");
                state.snapshot.devices[0].reading.connection = Connection::Sleeping;
                state.snapshot.devices[0].text = "Simulated mouse: sleeping".into();
                state.sync_trays(TrayUpdate::Changed);
                let tray = state.trays.get("simulated:mouse").unwrap();
                let text =
                    String::from_utf16(tray.data.szTip.split(|u| *u == 0).next().unwrap()).unwrap();
                assert!(text.contains(&expected_tip), "{text}");
                // Restore the empty-inventory baseline for the lifecycle scenarios below.
                state.settings = Settings::default();
                state.polling = PollingUi::default();
                state.tray_polling = PollingUi::default();
                state.snapshot = Snapshot::default();
                state.tooltip_rates.clear();
                state.sync_trays(TrayUpdate::Changed);
                state.open();
                let dashboard = state.dashboard.unwrap();
                let popup = popup_menu(&Snapshot::default(), &Settings::default(), None).unwrap();
                let appearance =
                    PopupAppearance::new(popup.0, state.monitor, Palette::new(true, false))
                        .unwrap();
                context.popup.replace(Some(appearance.clone()));
                let mut measure = MEASUREITEMSTRUCT {
                    CtlType: ODT_MENU,
                    itemData: 1,
                    ..Default::default()
                };
                assert_eq!(
                    send(
                        state.monitor,
                        WM_MEASUREITEM,
                        WPARAM(0),
                        LPARAM(&mut measure as *mut _ as isize)
                    ),
                    LRESULT(1)
                );
                assert_eq!(measure.itemWidth, appearance.width);
                let mnemonic = send(
                    state.monitor,
                    WM_MENUCHAR,
                    WPARAM('d' as usize),
                    LPARAM(popup.0.0 as isize),
                );
                assert_eq!(mnemonic.0 as u32 >> 16, MNC_EXECUTE);
                drop(popup);
                context.popup.take();
                drop(appearance);
                state.series =
                    HistorySeries::calendar(vec![Reading::new("test", "Test", "test", 0)], 0, 10);
                assert!(!state.series.samples.is_empty());
                state.sleep.next = 77;
                state.sleep.pending = Some((77, "test".into()));
                state.sleep.shown_key = Some("test".into());
                state.sleep.saver_key = Some("test".into());
                state.sleep.outcome = Some(SleepOutcome {
                    connection_stamp: None,
                    request: 76,
                    key: "test".into(),
                    generation: 0,
                    seconds: Some(300),
                    may_have_changed: false,
                    failure: None,
                });
                // Synchronous close enters proc while State is borrowed: it
                // must defer rather than silently use DefWindowProc's destroy.
                send(dashboard, WM_CLOSE, WPARAM(0), LPARAM(0));
                assert!(IsWindow(Some(dashboard)).as_bool());
            }
            let monitor = context.monitor.get();
            dispatch_monitor(monitor);
            assert!(context.state.borrow().dashboard.is_none());
            assert!(context.state.borrow().controls.is_empty());
            assert!(context.state.borrow().theme.is_none());
            assert!(context.paint.borrow().is_none());
            assert!(context.state.borrow().font.is_invalid());
            {
                let state = context.state.borrow();
                assert_eq!(state.sleep.next, 77);
                assert!(state.sleep.pending.is_none());
                assert!(state.sleep.shown_key.is_none());
                assert!(state.sleep.saver_key.is_none());
                assert!(state.sleep.outcome.is_none());
            }
            {
                let mut state = context.state.borrow_mut();
                assert!(state.series.samples.is_empty());
                let request = state.request;
                state.history_outcome(
                    request,
                    Ok(HistorySeries::calendar(
                        vec![Reading::new("test", "Test", "test", 0)],
                        0,
                        10,
                    )),
                );
                assert!(state.series.samples.is_empty());
            }
            send(monitor, WM_APP + 8, WPARAM(0), LPARAM(0));
            let first = context.state.borrow().dashboard.unwrap();
            assert!(IsWindowVisible(first).as_bool());
            exercise_tray_selection(&context);
            {
                let mut state = context.state.borrow_mut();
                state.command(2, 0);
                let request = state.request;
                let populated = || {
                    HistorySeries::calendar(vec![Reading::new("test", "Test", "test", 0)], 0, 10)
                };
                state.history_outcome(request.wrapping_sub(1), Ok(populated()));
                assert!(state.series.samples.is_empty());
                state.history_outcome(request, Ok(populated()));
                assert!(!state.series.samples.is_empty());
                state.command(1, 0);
                assert!(state.series.samples.is_empty());
                state.history_outcome(request, Ok(populated()));
                assert!(state.series.samples.is_empty());
                let current = state.request;
                state.history_outcome(current, Err(ProviderError::new("Inactive history error")));
                assert_ne!(state.error, "Inactive history error");
                state.command(2, 0);
                let reopened_request = state.request;
                state.history_outcome(reopened_request, Ok(populated()));
                assert!(!state.series.samples.is_empty());
                state.history_outcome(
                    reopened_request,
                    Err(ProviderError::new("Active history error")),
                );
                assert_eq!(state.error, "Active history error");
                assert!(state.text(97).contains("Couldn't load battery history"));
                state.command(1, 0);
            }
            {
                let mut state = context.state.borrow_mut();
                state.command(2, 0);
                state.series =
                    HistorySeries::calendar(vec![Reading::new("test", "Test", "test", 0)], 0, 10);
            }
            let resize_request = context.state.borrow().request;
            send(first, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            for _ in 0..500 {
                send(first, WM_SIZE, WPARAM(SIZE_RESTORED as usize), LPARAM(0));
            }
            assert_eq!(context.state.borrow().request, resize_request);
            assert!(!context.state.borrow().series.samples.is_empty());
            send(first, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
            assert_eq!(
                context.state.borrow().request,
                resize_request.wrapping_add(1)
            );
            send(first, WM_SIZE, WPARAM(SIZE_MINIMIZED as usize), LPARAM(0));
            assert_eq!(
                context.state.borrow().request,
                resize_request.wrapping_add(1)
            );
            context.state.borrow_mut().command(1, 0);
            {
                let mut state = context.state.borrow_mut();
                let mut keyboard = ConfigurationDevice::from_reading(&Reading::new(
                    "keyboard-only",
                    "Test keyboard",
                    "simulation",
                    0,
                ));
                keyboard.kind = "keyboard".into();
                keyboard.capability =
                    PollingCapability::Unavailable("Unsupported keyboard protocol".into());
                state.configuration_devices = vec![keyboard.clone(), keyboard];
                state.selected_device = Some("keyboard-only".into());
                state.build();
                assert_eq!(state.device_rows().len(), 1);
                assert!(state.controls.contains_key(&11));
                assert!(!state.controls.contains_key(&12));
                assert!(!state.controls.contains_key(&13));
                assert!(!state.controls.contains_key(&14));
                assert!(state.visible_polling_device().is_none());
                state.set_control_text(11, "Unsaved keyboard edit");
                assert!(!state.controls.contains_key(&49));
                let name_control = state.controls[&11];
                let save_control = state.controls[&15];
                let mut pending_device = state.configuration_devices[0].clone();
                pending_device.capability = PollingCapability::ReadWrite;
                state
                    .polling
                    .begin(pending_device, PollingIntent::Read)
                    .unwrap();
                let request = state.polling.pending.as_ref().unwrap().request;
                let inventory = state.configuration_devices.clone();
                state.configuration_inventory(0, inventory.clone(), None);
                assert_eq!(state.controls[&11], name_control);
                assert_eq!(state.controls[&15], save_control);
                assert_eq!(state.text(11), "Unsaved keyboard edit");
                assert_eq!(state.polling.pending.as_ref().unwrap().request, request);
                state.polling.abandon();
                state.configuration_inventory(
                    0,
                    inventory.clone(),
                    Some("Inventory failed".into()),
                );
                assert_eq!(state.error, "Inventory failed");
                state.error = "Other error".into();
                state.configuration_inventory(0, inventory.clone(), None);
                assert_eq!(state.error, "Other error");
                state.configuration_inventory(
                    0,
                    inventory.clone(),
                    Some("Inventory failed".into()),
                );
                state.configuration_inventory(0, inventory.clone(), None);
                assert!(state.error.is_empty());
                state.set_control_text(11, "Renamed keyboard");
                state.command(15, 0);
                assert_eq!(
                    state.settings.devices["keyboard-only"].name.as_deref(),
                    Some("Renamed keyboard")
                );
                assert_eq!(state.selected_device.as_deref(), Some("keyboard-only"));
                state.settings.polling_controls = true;
                state.build();
                for id in [40, 41, 42, 43] {
                    assert_ne!(
                        GetWindowLongPtrW(state.controls[&id], GWL_STYLE) as u32 & WS_DISABLED.0,
                        0
                    );
                }
                assert!(state.text(45).contains("rate controls are unavailable"));
                state.command(2, 0);
                assert_eq!(
                    send(state.controls[&10], CB_GETCOUNT, WPARAM(0), LPARAM(0)).0,
                    1
                );
                assert!(
                    !windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(
                        state.controls[&10]
                    )
                    .as_bool()
                );
                assert_eq!(state.text(10), "No battery-powered devices");
                assert!(state.visible_polling_device().is_none());
                state.command(6, 0);
                assert_eq!(
                    send(state.controls[&10], CB_GETCOUNT, WPARAM(0), LPARAM(0)).0,
                    1
                );
                assert!(state.visible_polling_device().is_none());
                state.command(1, 0);
                assert_eq!(state.selected_device.as_deref(), Some("keyboard-only"));
                state.command(16, 0);
                assert!(state.settings.devices["keyboard-only"].name.is_none());
                state.configuration_devices.clear();
                state.selected_device = None;
                state.settings.polling_controls = false;
                state.build();
            }
            for mode in [SW_MINIMIZE, SW_HIDE] {
                let _ = ShowWindow(first, mode);
                dispatch_monitor(monitor);
                assert!(!context.state.borrow().dashboard_visible());
                assert!(!context.state.borrow().runtime.configuration_visible());
                send(monitor, WM_APP + 8, WPARAM(0), LPARAM(0));
                assert_eq!(context.state.borrow().dashboard, Some(first));
                assert!(IsWindowVisible(first).as_bool());
                assert!(!IsIconic(first).as_bool());
                assert!(context.state.borrow().dashboard_visible());
                assert!(context.state.borrow().runtime.configuration_visible());
            }
            {
                let mut state = context.state.borrow_mut();
                DestroyWindow(first).unwrap();
                assert_eq!(state.dashboard, Some(first)); // Deferred NCDESTROY.
                assert!(!IsWindow(Some(first)).as_bool());
                state.open(); // Must recover even before queued cleanup runs.
                assert!(state.owns_dashboard(state.dashboard.unwrap()));
            }
            dispatch_monitor(monitor);
            let reopened = context.state.borrow().dashboard.unwrap();
            assert!(context.state.borrow().owns_dashboard(reopened));
            // Native double-click and menu action must be idempotent opens.
            for _ in 0..3 {
                send(monitor, TRAY, WPARAM(0), LPARAM(WM_LBUTTONDBLCLK as isize));
                send(monitor, WM_COMMAND, WPARAM(500), LPARAM(0));
                assert_eq!(context.state.borrow().dashboard, Some(reopened));
            }
            {
                let mut state = context.state.borrow_mut();
                let mut reading = Reading::new(
                    "synthetic-mouse",
                    "Test mouse",
                    "simulation",
                    SystemClock::default().unix(),
                );
                reading.level = Some(50);
                reading.charging = Some(false);
                reading.kind = "mouse".into();
                state.snapshot.devices = vec![DeviceView {
                    reading,
                    name: "Test mouse".into(),
                    icon: "mouse".into(),
                    low_alert_at: 20,
                    seconds_left: None,
                    text: "Test mouse: 50%".into(),
                    hidden: false,
                }];
                state.error.clear();
                state.settings.polling_controls = true;
                state
                    .settings
                    .device_mut("synthetic-mouse")
                    .requested_polling_rate = Some(PollingRate::try_from(8000).unwrap());
                let target = ControlTarget {
                    device: ConfigurationDevice::from_reading(&state.snapshot.devices[0].reading),
                    generation: state.polling.generation,
                    connection_stamp: None,
                };
                state.polling.observations.insert(
                    "synthetic-mouse".into(),
                    PollingObservation {
                        target,
                        rate: Some(PollingRate::try_from(1000).unwrap()),
                        supported: [125, 250, 500, 1000, 2000, 4000, 8000]
                            .into_iter()
                            .map(|hz| PollingRate::try_from(hz).unwrap())
                            .collect(),
                        timestamp: SystemClock::default().unix(),
                        evidence: "Synthetic UI fixture".into(),
                    },
                );
                let now = SystemClock::default().unix();
                let insights = BatteryInsights {
                    recent: RecentUse {
                        total_seconds: 7200,
                        rates: [0, 0, 0, 5400, 1800, 0, 0],
                        boost_seconds: 1800,
                        boost_sessions: 3,
                        restores: 3,
                        manual_changes: 1,
                        normal_rate: PollingRate::try_from(1000).ok(),
                        current_level: Some(50),
                        last_confirmed_timestamp: Some(now),
                        ..Default::default()
                    },
                    rates: vec![
                        RateInsight {
                            hz: 1000,
                            awake_seconds: 7200,
                            projection_seconds: 7200,
                            projection_consumed_percent: 20,
                            projection_drop_count: 4,
                            consumed_percent: 20,
                            sample_count: 15,
                            drop_count: 4,
                            confidence: InsightConfidence::Low,
                            projected_full_charge_hours: Some(10.0),
                            remaining_hours: Some(5.0),
                        },
                        RateInsight {
                            hz: 2000,
                            awake_seconds: 1800,
                            projection_seconds: 1800,
                            projection_consumed_percent: 5,
                            drop_count: 2,
                            confidence: InsightConfidence::Low,
                            projected_full_charge_hours: Some(8.0),
                            remaining_hours: Some(4.0),
                            ..Default::default()
                        },
                    ],
                    cycles: vec![ChargeCycle {
                        start_timestamp: now - 7200,
                        end_timestamp: now,
                        awake_seconds: 7200,
                        start_percent: 70,
                        end_percent: 50,
                        consumed_percent: 20,
                        evidence: CycleEvidence::ObservedCharge,
                        current: true,
                    }],
                    coverage: InsightCoverage {
                        observation_count: 15,
                        discharge_sample_count: 15,
                        awake_seconds: 7200,
                        last_reading_timestamp: Some(now),
                        ..Default::default()
                    },
                };
                state.insights.status = insight_coverage_text(&insights);
                state.insights.data = Some(insights);
                state.insights.key = Some("synthetic-mouse".into());
            }
            {
                let mut state = context.state.borrow_mut();
                let saved = state.insights.data.clone();
                state.insights.data.as_mut().unwrap().recent.total_seconds = 15652;
                state.page = 6;
                state.build();
                assert!(state.text(400).contains("Missing rate data"));
                assert!(state.text(79).contains("46%"));
                let label = state.controls[&79];
                let mut bounds = RECT::default();
                GetClientRect(label, &mut bounds).unwrap();
                let available = bounds.bottom;
                let dc = GetDC(Some(label));
                let saved_dc = SaveDC(dc);
                let font = HFONT(send(label, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as _);
                SelectObject(dc, font.into());
                let mut text = wide(&state.text(79));
                DrawTextW(
                    dc,
                    &mut text,
                    &mut bounds,
                    DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
                );
                assert!(
                    bounds.bottom <= available,
                    "missing-rate guidance must fit without clipping"
                );
                let _ = RestoreDC(dc, saved_dc);
                ReleaseDC(Some(label), dc);
                let mut note = RECT::default();
                let mut heading = RECT::default();
                GetWindowRect(label, &mut note).unwrap();
                GetWindowRect(state.controls[&73], &mut heading).unwrap();
                assert!(note.bottom < heading.top);
                state.insights.data = saved;
            }
            {
                let mut state = context.state.borrow_mut();
                let saved = state.snapshot.devices[0].clone();
                let saved_data = state.insights.data.clone();
                state.snapshot.devices[0].reading.source = "bluetooth".into();
                state.snapshot.devices[0].reading.kind = "headset".into();
                state.snapshot.devices[0].seconds_left = Some(7200);
                state.page = 1;
                state.build();
                assert!(
                    !state.controls.contains_key(&98),
                    "battery-only devices must not offer polling controls"
                );
                assert!(state.visible_polling_device().is_none());
                state.page = 6;
                state.build();
                assert!(state.text(400).contains("about 2 hours"));
                assert!(state.text(401).contains("Recorded use"));
                assert!(state.text(402).contains("50%"));
                assert!(!IsWindowVisible(state.controls[&410]).as_bool());
                assert!(!IsWindowVisible(state.controls[&70]).as_bool());
                assert!(IsWindowVisible(state.controls[&71]).as_bool());
                state.snapshot.devices[0] = saved;
                state.insights.data = saved_data;
                state.page = 1;
                state.build();
                let permission = state.settings.polling_controls;
                state.settings.polling_controls = false;
                state.build();
                assert!(state.controls.contains_key(&53));
                state.command(53, 0);
                assert_eq!(state.page, 3);
                assert_eq!(GetFocus(), state.controls[&112]);
                assert!(
                    !state.settings.polling_controls,
                    "navigation must not grant hardware permission"
                );
                state.settings.polling_controls = permission;
                state.page = 1;
                state.build();
            }
            // DPI UI uses invented state only; this never opens physical hardware.
            {
                let mut state = context.state.borrow_mut();
                let saved = state.snapshot.devices[0].reading.clone();
                let key = "razer:00bf:SYNTHETIC-DPI".to_string();
                state.snapshot.devices[0].reading.key = key.clone();
                state.snapshot.devices[0].reading.source = "razer".into();
                state.snapshot.devices[0].reading.via = "usb".into();
                state.selected_device = Some(key.clone());
                let generation = state.polling.generation;
                let original = DpiState {
                    value: DpiValue { x: 800, y: 1200 },
                    profile: None,
                    stage: Some(1),
                };
                state.dpi.attempted = Some((key.clone(), generation));
                state.dpi.outcome = Some(DpiOutcome {
                    limits: None,
                    request: 1,
                    key: key.clone(),
                    generation,
                    current: Some(original.clone()),
                    previous: None,
                    may_have_changed: false,
                    failure: None,
                });
                state.dpi.previous = Some((key.clone(), generation, original.clone()));
                state.build();
                assert_eq!(state.text(451), "800");
                assert_eq!(state.text(458), "Supported DPI: 100–45,000 (model range)");
                assert!(state.text(457).contains("800 horizontal / 1200 vertical"));
                assert_eq!(state.restorable_dpi(), Some(original.value));
                state.set_control_text(451, "1600");
                state.polling_controls();
                assert_eq!(
                    state.text(451),
                    "1600",
                    "hardware replies must retain the DPI draft"
                );
                state.set_control_text(442, "17");
                send(state.controls[&49], BM_SETCHECK, WPARAM(1), LPARAM(0));
                send(state.controls[&50], CB_SETCURSEL, WPARAM(1), LPARAM(0));
                send(state.controls[&431], CB_SETCURSEL, WPARAM(1), LPARAM(0));
                let boost_draft = state.text(50);
                let sleep_draft = state.text(431);
                let _ = SetFocus(Some(state.controls[&451]));
                send(
                    state.controls[&451],
                    windows::Win32::UI::Controls::EM_SETSEL,
                    WPARAM(1),
                    LPARAM(3),
                );
                state.build();
                assert_eq!(
                    state.text(451),
                    "1600",
                    "a DPI/connection layout rebuild must preserve unfinished sensitivity edits"
                );
                assert_eq!(state.text(442), "17");
                assert!(state.checked(49));
                assert_eq!(state.text(50), boost_draft);
                assert_eq!(state.text(431), sleep_draft);
                assert_eq!(GetFocus(), state.controls[&451]);
                assert_eq!(
                    send(
                        state.controls[&451],
                        windows::Win32::UI::Controls::EM_GETSEL,
                        WPARAM(0),
                        LPARAM(0)
                    )
                    .0,
                    1 | (3 << 16)
                );
                let next = state.dpi.next;
                state.set_control_text(451, "45001");
                state.command(453, 0);
                assert_eq!(
                    state.dpi.next, next,
                    "out-of-range input must not queue a hardware request"
                );
                assert!(state.text(455).contains("supported range"));
                assert!(state.dpi.outcome.as_ref().unwrap().current.is_some());
                state.dpi.previous.as_mut().unwrap().0 = "different-device".into();
                assert!(state.restorable_dpi().is_none());
                state.dpi.previous.as_mut().unwrap().0 = key;
                state.dpi.previous.as_mut().unwrap().2.stage = Some(2);
                assert!(state.restorable_dpi().is_none());
                state.dpi.previous.as_mut().unwrap().2.stage = Some(1);
                let saved_polling = std::mem::take(&mut state.polling);
                state.polling.generation = generation;
                let saved_tray_polling = std::mem::take(&mut state.tray_polling);
                state.tray_polling.generation = generation;
                let saved_tooltip_rates = std::mem::take(&mut state.tooltip_rates);
                let saved_sleep = std::mem::take(&mut state.sleep);
                for configuration_only in [false, true] {
                    let before_snapshot = state.snapshot.clone();
                    let before_configuration = state.configuration_devices.clone();
                    let mut device =
                        ConfigurationDevice::from_reading(&state.snapshot.devices[0].reading);
                    let device_key = device.key.clone();
                    if configuration_only {
                        state.snapshot.devices.clear();
                        state.configuration_devices = vec![device.clone()];
                    }
                    state.dpi.outcome.as_mut().unwrap().current = Some(original.clone());
                    state.dpi.previous = Some((device.key.clone(), generation, original.clone()));
                    state.sleep.outcome = Some(SleepOutcome {
                        connection_stamp: ConnectionStamp::new(1),
                        request: 88,
                        key: device.key.clone(),
                        generation,
                        seconds: Some(300),
                        may_have_changed: false,
                        failure: None,
                    });
                    assert_eq!(state.restorable_dpi(), Some(original.value));
                    device.serial = Some("SYNTHETIC-REPLACEMENT-UNIT".into());
                    let (events, received) = crossbeam_channel::bounded(1);
                    let real_events = std::mem::replace(&mut state.runtime.events, received);
                    if configuration_only {
                        events
                            .send(Event::ConfigurationInventory {
                                generation,
                                devices: vec![device],
                                failure: None,
                            })
                            .unwrap();
                    } else {
                        let mut replacement = state.snapshot.clone();
                        replacement.devices[0].reading.serial = device.serial;
                        events.send(Event::Snapshot(replacement)).unwrap();
                    }
                    state.drain();
                    state.runtime.events = real_events;
                    assert!(
                        state.restorable_dpi().is_none(),
                        "Replacement hardware inherited the old DPI restore value"
                    );
                    assert!(state.dpi.previous.is_none());
                    assert!(state.sleep.outcome.is_none());
                    state.snapshot = before_snapshot;
                    state.configuration_devices = before_configuration;
                    state.dpi.pending = None;
                    state.dpi.outcome = Some(DpiOutcome {
                        limits: None,
                        request: 1,
                        key: device_key,
                        generation,
                        current: Some(original.clone()),
                        previous: None,
                        may_have_changed: false,
                        failure: None,
                    });
                }
                state.polling = saved_polling;
                state.tray_polling = saved_tray_polling;
                state.tooltip_rates = saved_tooltip_rates;
                state.sleep = saved_sleep;
                state.snapshot.devices[0].reading = saved;
                state.selected_device = Some("synthetic-mouse".into());
                state.dpi = DpiUi::default();
                state.build();
                assert!(!state.controls.contains_key(&451));
                state.page = 3;
                state.build();
                state.set_control_text(201, "123");
                state.set_control_text(203, "22");
                send(state.controls[&114], BM_SETCHECK, WPARAM(1), LPARAM(0));
                send(state.controls[&300], BM_SETCHECK, WPARAM(0), LPARAM(0));
                send(state.controls[&205], CB_SETCURSEL, WPARAM(2), LPARAM(0));
                state.build();
                assert_eq!(state.text(201), "123");
                assert_eq!(state.text(203), "22");
                assert!(state.checked(114));
                assert!(!state.checked(300));
                assert_eq!(state.choice(205), 2);
                state.page = 1;
                state.build();
            }
            // Exercise each page against actual native brushes/controls without
            // changing the user's Windows appearance or stored settings.
            for dark in [false, true] {
                for (page, advanced) in [(1, false), (2, false), (3, false), (6, false), (3, true)]
                {
                    let expected;
                    {
                        let mut state = context.state.borrow_mut();
                        state.page = page;
                        state.error.clear();
                        state.more_options = advanced;
                        if page == 2 {
                            state.history.usage_index = 0;
                        }
                        state.page_scroll = 0;
                        state.apply_theme(DashboardTheme::new(dark, false));
                        expected = state.theme.as_ref().unwrap().palette.background;
                        state.build();
                        if page == 3 {
                            let mut boxes = Vec::new();
                            for id in (0..CHECKS.len())
                                .map(setting_check_id)
                                .chain([112, 113, 206, 205])
                            {
                                let hwnd = state.controls[&id];
                                if IsWindowVisible(hwnd).as_bool() {
                                    let mut rect = RECT::default();
                                    GetWindowRect(hwnd, &mut rect).unwrap();
                                    boxes.push((id, rect));
                                }
                            }
                            for (index, (id, a)) in boxes.iter().enumerate() {
                                for (other, b) in &boxes[index + 1..] {
                                    assert!(
                                        a.right <= b.left
                                            || b.right <= a.left
                                            || a.bottom <= b.top
                                            || b.bottom <= a.top,
                                        "Settings controls {id} and {other} overlap"
                                    );
                                }
                            }
                            let retained_id = setting_check_id(
                                CHECKS
                                    .iter()
                                    .position(|(key, _)| *key == "keep_disconnected")
                                    .unwrap(),
                            );
                            for id in [206, retained_id] {
                                let text = state.text(id);
                                let checked = state.checked(id);
                                send(state.controls[&id], WM_MOUSEMOVE, WPARAM(0), LPARAM(0));
                                send(state.controls[&id], WM_PAINT, WPARAM(0), LPARAM(0));
                                send(
                                    state.controls[&id],
                                    windows::Win32::UI::Controls::WM_MOUSELEAVE,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                                assert_eq!(state.text(id), text);
                                assert_eq!(state.checked(id), checked);
                            }
                            assert_eq!(state.text(206), "Start at sign-in");
                            assert_eq!(
                                state.text(retained_id),
                                "Keep disconnected icons until exit"
                            );
                        }
                        if advanced {
                            state.scroll_page(i32::MAX);
                        }
                        if page == 6 {
                            send(state.controls[&70], CB_SETCURSEL, WPARAM(1), LPARAM(0));
                            state.command(70, CBN_SELCHANGE as u16);
                            assert!(state.text(74).starts_with("2000 Hz"));
                            assert!(state.text(74).contains("Estimate based on"));
                            assert!(!state.text(74).contains("/ 30 min"));
                            state.render_insights();
                            assert_eq!(state.choice(70), 1);
                            assert!(state.text(402).contains("100%"));
                            assert!(state.text(410).contains("1000 Hz · 75%"));
                            let original_mix = state.text(410);
                            let original_details = state.text(74);
                            let many_rows = INSIGHT_RATES
                                .iter()
                                .map(|r| format!("{r} Hz · 14%"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            state.set_control_text(
                                410,
                                &format!("Polling-rate mix · last 7 days\n{many_rows}"),
                            );
                            state.set_control_text(74, &format!("Long result\n{}", "An explanation that needs more than one line at the current size. ".repeat(30)));
                            state.layout_insight_content();
                            let mut rate_bounds = RECT::default();
                            let mut next_bounds = RECT::default();
                            GetWindowRect(state.controls[&74], &mut rate_bounds).unwrap();
                            GetWindowRect(state.controls[&75], &mut next_bounds).unwrap();
                            let needed = crate::dashboard_widgets::panel_height(
                                state.controls[&74],
                                &state.text(74),
                                760,
                                false,
                            );
                            let scale = GetDpiForWindow(reopened).max(96) as i32;
                            assert!(rate_bounds.bottom - rate_bounds.top >= needed * scale / 96);
                            assert!(
                                next_bounds.top >= rate_bounds.bottom,
                                "wrapped results must not overlap the next section"
                            );
                            state.set_control_text(410, &original_mix);
                            state.set_control_text(74, &original_details);
                            state.layout_insight_content();
                            let saved = state.insights.data.take();
                            state.render_insights();
                            assert!(state.text(410).contains("No history loaded"));
                            assert!(state.text(70).contains("Still learning"));
                            state.insights.data = saved;
                            state.render_insights();
                            state.scroll_page(i32::MAX);
                            let mut pane = RECT::default();
                            let mut details = RECT::default();
                            GetWindowRect(state.page_host.unwrap(), &mut pane).unwrap();
                            GetWindowRect(state.controls[&77], &mut details).unwrap();
                            assert!(details.top >= pane.top && details.bottom <= pane.bottom);
                            assert!(
                                state.text(410).contains("polling-rate mix")
                                    || state.text(410).contains("Polling-rate mix")
                            );
                            state.scroll_page(0);
                        }
                        if page == 1 {
                            assert!(state.controls.contains_key(&49));
                            // Other supported settings can follow boost; reveal this section itself.
                            state.scroll_page(708 - PAGE_TOP);
                            let mut pane = RECT::default();
                            GetWindowRect(state.page_host.unwrap(), &mut pane).unwrap();
                            for id in [49, 50, 51, 52] {
                                let mut bounds = RECT::default();
                                GetWindowRect(state.controls[&id], &mut bounds).unwrap();
                                assert!(
                                    bounds.top >= pane.top && bounds.bottom <= pane.bottom,
                                    "boost control {id} must be reachable by scrolling"
                                );
                            }
                            // A hardware reply rebuilds this group at the current
                            // scroll position; it must retain the full range.
                            state.polling_controls();
                            assert!(state.page_height >= 920 - PAGE_TOP);
                            state.scroll_page(0);
                            assert!(!state.checked(49));
                            send(state.controls[&49], BM_SETCHECK, WPARAM(1), LPARAM(0));
                            state.command(51, 0);
                            assert_eq!(state.text(95), "Automatic boost settings saved");
                            assert_eq!(
                                state.settings.devices["synthetic-mouse"]
                                    .fullscreen_boost_rate
                                    .unwrap()
                                    .hz(),
                                2000
                            );
                            state.command(15, 0);
                            assert_eq!(
                                state.settings.devices["synthetic-mouse"]
                                    .fullscreen_boost_rate
                                    .unwrap()
                                    .hz(),
                                2000
                            );
                            send(state.controls[&49], BM_SETCHECK, WPARAM(0), LPARAM(0));
                            state.command(51, 0);
                            assert!(
                                state.settings.devices["synthetic-mouse"]
                                    .fullscreen_boost_rate
                                    .is_none()
                            );
                            let saved_rates = state.polling.observations["synthetic-mouse"]
                                .supported
                                .clone();
                            state
                                .polling
                                .observations
                                .get_mut("synthetic-mouse")
                                .unwrap()
                                .supported
                                .retain(|rate| rate.hz() <= 1000);
                            state.polling_controls();
                            assert!(state.controls.contains_key(&43));
                            assert!(
                                !state.controls.contains_key(&49),
                                "A 1000 Hz-only connection must not offer an empty boost selector"
                            );
                            state
                                .polling
                                .observations
                                .get_mut("synthetic-mouse")
                                .unwrap()
                                .supported = saved_rates;
                            state.polling_controls();
                            assert!(state.controls.contains_key(&49));
                            // A combo's selected-item callback and a static's
                            // color callback reenter the parent while State is
                            // held, exactly as during a page rebuild. Print the
                            // real controls, not a direct call to the helper.
                            let palette = state.theme.as_ref().unwrap().palette;
                            for (id, background) in [
                                (10, palette.surface),
                                (91, palette.background),
                                (11, palette.surface),
                            ] {
                                let child = state.controls[&id];
                                let source = GetDC(Some(child));
                                let dc = CreateCompatibleDC(Some(source));
                                let mut rect = RECT::default();
                                GetClientRect(child, &mut rect).unwrap();
                                let bitmap =
                                    CreateCompatibleBitmap(source, rect.right, rect.bottom);
                                let old = SelectObject(dc, bitmap.into());
                                FillRect(
                                    dc,
                                    &rect,
                                    HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
                                );
                                send(
                                    child,
                                    WM_PRINTCLIENT,
                                    WPARAM(dc.0 as usize),
                                    LPARAM(PRF_CLIENT as isize),
                                );
                                assert_eq!(
                                    GetPixel(dc, rect.right - 40, rect.bottom / 2),
                                    background,
                                    "reentrant control {id}, dark={dark}"
                                );
                                assert!(
                                    (3..rect.bottom - 3).any(|y| (8..rect.right / 2 - 4).any(
                                        |x| {
                                            let color = GetPixel(dc, x, y);
                                            let channel = color.0 & 255;
                                            channel.abs_diff(palette.background.0 & 255) > 90
                                        }
                                    )),
                                    "missing reentrant control text {id}, dark={dark}"
                                );
                                SelectObject(dc, old);
                                let _ = DeleteObject(bitmap.into());
                                let _ = DeleteDC(dc);
                                ReleaseDC(Some(child), source);
                            }
                            // Use an offscreen surface: desktop occlusion must not
                            // turn this paint test into GetPixel(CLR_INVALID).
                            let source = GetDC(Some(reopened));
                            let dc = CreateCompatibleDC(Some(source));
                            let bitmap = CreateCompatibleBitmap(source, 8, 8);
                            let old = SelectObject(dc, bitmap.into());
                            send(reopened, WM_ERASEBKGND, WPARAM(dc.0 as usize), LPARAM(0));
                            assert_eq!(
                                GetPixel(dc, 4, 4),
                                palette.background,
                                "reentrant dashboard erase, dark={dark}"
                            );
                            SelectObject(dc, old);
                            let _ = DeleteObject(bitmap.into());
                            let _ = DeleteDC(dc);
                            ReleaseDC(Some(reopened), source);
                        }
                    }
                    let sleep_capture = if page == 1
                        && std::env::var("HALO_CAPTURE_DASHBOARD_TEST")
                            .is_ok_and(|mode| mode == "sleep" || mode == "saver")
                    {
                        let mut state = context.state.borrow_mut();
                        let saved = state.snapshot.devices[0].reading.clone();
                        state.snapshot.devices[0].reading.key = "razer:00bf:SYNTHETIC-SLEEP".into();
                        state.snapshot.devices[0].reading.via = "usb".into();
                        let device =
                            ConfigurationDevice::from_reading(&state.snapshot.devices[0].reading);
                        if let Some(mut observation) =
                            state.polling.observations.get(&saved.key).cloned()
                        {
                            observation.target.device = device.clone();
                            state
                                .polling
                                .observations
                                .insert(device.key.clone(), observation);
                        }
                        state.selected_device = Some(device.key);
                        state.build();
                        state.scroll_page(i32::MAX);
                        Some(saved)
                    } else {
                        None
                    };
                    if std::env::var("HALO_CAPTURE_DASHBOARD_TEST")
                        .is_ok_and(|mode| mode == "details")
                    {
                        context.state.borrow_mut().scroll_page(i32::MAX);
                    }
                    let _ = RedrawWindow(
                        Some(reopened),
                        None,
                        None,
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                    );
                    let source = GetDC(Some(reopened));
                    let dc = CreateCompatibleDC(Some(source));
                    let mut client = RECT::default();
                    GetClientRect(reopened, &mut client).unwrap();
                    let width = client.right;
                    let height = client.bottom;
                    let bitmap = CreateCompatibleBitmap(source, width, height);
                    let old = SelectObject(dc, bitmap.into());
                    send(reopened, WM_ERASEBKGND, WPARAM(dc.0 as usize), LPARAM(0));
                    assert_eq!(
                        GetPixel(dc, width - 8, height - 8),
                        expected,
                        "page {page}, dark={dark}"
                    );
                    // Exercise the real print/client paint route, not only an
                    // explicit erase message with a forced palette.
                    FillRect(dc, &client, HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _));
                    send(reopened, WM_PRINTCLIENT, WPARAM(dc.0 as usize), LPARAM(0));
                    assert_eq!(
                        GetPixel(dc, 0, 0),
                        expected,
                        "client paint page {page}, dark={dark}"
                    );
                    if std::env::var_os("HALO_CAPTURE_DASHBOARD_TEST").is_some() {
                        // WM_PRINT uses window-relative coordinates even without
                        // PRF_NONCLIENT. Compensate for the frame so the capture
                        // contains the entire client, including its fixed footer.
                        let mut origin = POINT::default();
                        let mut window = RECT::default();
                        let _ = ClientToScreen(reopened, &mut origin);
                        GetWindowRect(reopened, &mut window).unwrap();
                        let mut previous_origin = POINT::default();
                        let _ = SetViewportOrgEx(
                            dc,
                            window.left - origin.x,
                            window.top - origin.y,
                            Some(&mut previous_origin),
                        );
                        send(
                            reopened,
                            WM_PRINT,
                            WPARAM(dc.0 as usize),
                            LPARAM((PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize),
                        );
                        let _ = SetViewportOrgEx(dc, previous_origin.x, previous_origin.y, None);
                        if page == 2 {
                            let points = (0..=16)
                                .map(|index| {
                                    let mut reading = Reading::new(
                                        "synthetic-mouse",
                                        "Test mouse",
                                        "simulation",
                                        index * 450,
                                    );
                                    reading.level = Some(66 - index as u8);
                                    reading.connection = Connection::Online;
                                    reading
                                })
                                .collect();
                            let mut series = HistorySeries::calendar(points, 0, 7200);
                            series.axis = HistoryAxis::Usage;
                            let chart = Chart::new(reopened, width as u32, height as u32).unwrap();
                            chart
                                .paint_with_palette(
                                    width as u32,
                                    height as u32,
                                    &series,
                                    &Palette::new(dark, false),
                                )
                                .unwrap();
                            let scale = GetDpiForWindow(reopened).max(96) as i32;
                            let top = 212 * scale / 96;
                            let bottom = height - 60 * scale / 96;
                            BitBlt(
                                dc,
                                0,
                                top,
                                width,
                                bottom - top,
                                Some(source),
                                0,
                                top,
                                SRCCOPY,
                            )
                            .unwrap();
                        }
                        SelectObject(dc, old);
                        let mut info = BITMAPINFO::default();
                        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                        info.bmiHeader.biWidth = width;
                        info.bmiHeader.biHeight = -height;
                        info.bmiHeader.biPlanes = 1;
                        info.bmiHeader.biBitCount = 32;
                        let mut pixels = vec![0u8; width as usize * height as usize * 4];
                        assert_eq!(
                            GetDIBits(
                                dc,
                                bitmap,
                                0,
                                height as u32,
                                Some(pixels.as_mut_ptr().cast()),
                                &mut info,
                                DIB_RGB_COLORS
                            ),
                            height
                        );
                        let mut file = Vec::with_capacity(54 + pixels.len());
                        file.extend_from_slice(b"BM");
                        file.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
                        file.extend_from_slice(&[0; 4]);
                        file.extend_from_slice(&54u32.to_le_bytes());
                        file.extend_from_slice(&40u32.to_le_bytes());
                        file.extend_from_slice(&width.to_le_bytes());
                        file.extend_from_slice(&(-height).to_le_bytes());
                        file.extend_from_slice(&1u16.to_le_bytes());
                        file.extend_from_slice(&32u16.to_le_bytes());
                        file.extend_from_slice(&[0; 24]);
                        file.extend_from_slice(&pixels);
                        let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("../../validation-local/dashboard");
                        std::fs::create_dir_all(&output).unwrap();
                        std::fs::write(
                            output.join(format!(
                                "page-{page}{}{}-{}.bmp",
                                if std::env::var("HALO_CAPTURE_DASHBOARD_TEST")
                                    .is_ok_and(|m| m == "details")
                                {
                                    "-details"
                                } else if std::env::var("HALO_CAPTURE_DASHBOARD_TEST")
                                    .is_ok_and(|m| m == "saver")
                                {
                                    "-saver"
                                } else if sleep_capture.is_some() {
                                    "-sleep"
                                } else {
                                    ""
                                },
                                if advanced { "-advanced" } else { "" },
                                if dark { "dark" } else { "light" }
                            )),
                            file,
                        )
                        .unwrap();
                    } else {
                        SelectObject(dc, old);
                    }
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(dc);
                    ReleaseDC(Some(reopened), source);
                    if let Some(saved) = sleep_capture {
                        let mut state = context.state.borrow_mut();
                        state
                            .polling
                            .observations
                            .remove("razer:00bf:SYNTHETIC-SLEEP");
                        state.selected_device = Some(saved.key.clone());
                        state.snapshot.devices[0].reading = saved;
                        state.sleep.shown_key = None;
                    }
                }
            }
            {
                let mut state = context.state.borrow_mut();
                state.page = 3;
                state.more_options = false;
                state.build();
                let interval = state.controls[&201];
                SetWindowTextW(interval, w!("1234")).unwrap();
                for dark in [false, true, false] {
                    state.apply_theme(DashboardTheme::new(dark, false));
                    assert_eq!(state.controls[&201], interval);
                    assert_eq!(state.text(201), "1234");
                    assert_eq!(state.page, 3);
                    assert_eq!(state.dashboard, Some(reopened));
                }
                let fonts = (state.font, state.heading_font);
                let more = state.controls[&212];
                state.command(212, 0);
                assert!(state.more_options);
                assert_eq!(state.controls[&212], more);
                assert_eq!(state.controls[&201], interval);
                assert_eq!(state.text(201), "1234");
                assert!(IsWindowVisible(state.controls[&217]).as_bool());
                assert!(!IsWindowVisible(state.controls[&209]).as_bool());
                state.command(212, 0);
                assert!(!IsWindowVisible(state.controls[&209]).as_bool());
                assert_eq!(state.text(201), "1234");
                state.set_control_text(201, "invalid");
                state.command(210, 0);
                assert_eq!(state.controls[&201], interval);
                assert_eq!(state.text(201), "invalid");
                assert!(state.text(95).contains("battery check interval"));
                assert_eq!(GetFocus(), state.controls[&201]);
                state.set_control_text(201, " 60 ");
                state.set_control_text(203, "101");
                state.command(210, 0);
                assert!(state.text(95).contains("default low-battery alert"));
                assert_eq!(GetFocus(), state.controls[&203]);
                state.set_control_text(203, "20");
                state.set_control_text(215, "-1");
                state.command(210, 0);
                assert!(state.text(95).contains("orange warning level"));
                assert_eq!(GetFocus(), state.controls[&215]);
                state.set_control_text(215, "30");
                state.set_control_text(201, "60");
                let dpi = GetDpiForWindow(reopened).max(96) as i32;
                SetWindowPos(
                    reopened,
                    None,
                    0,
                    0,
                    840 * dpi / 96,
                    480 * dpi / 96,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                state.layout_page();
                let host = state.page_host.unwrap();
                let save = state.controls[&210];
                let mut footer = RECT::default();
                GetWindowRect(save, &mut footer).unwrap();
                state.scroll_page(i32::MAX);
                assert!(state.page_scroll > 0);
                let mut scroll = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_ALL,
                    ..Default::default()
                };
                GetScrollInfo(state.controls[&7], SB_CTL, &mut scroll).unwrap();
                assert_eq!(state.page_scroll, scroll.nPos);
                let mut current_footer = RECT::default();
                GetWindowRect(save, &mut current_footer).unwrap();
                assert_eq!(footer, current_footer);
                let mut content = RECT::default();
                let mut pane = RECT::default();
                GetWindowRect(state.controls[&212], &mut content).unwrap();
                GetWindowRect(host, &mut pane).unwrap();
                let mut bar_rect = RECT::default();
                GetWindowRect(state.controls[&7], &mut bar_rect).unwrap();
                assert!(
                    pane.right <= bar_rect.left,
                    "page must not paint over scrollbar"
                );
                let _ = ValidateRect(Some(state.controls[&7]), None);
                let unchanged_position = state.page_scroll;
                state.scroll_page(unchanged_position);
                assert!(
                    !GetUpdateRect(state.controls[&7], None, false).as_bool(),
                    "unchanged position/range must not repaint the scrollbar"
                );
                assert!(content.top >= pane.top && content.bottom <= pane.bottom);
                state.scroll_page(0);
                let _ = SetFocus(Some(state.controls[&212]));
                state.keep_focus_visible();
                assert!(state.page_scroll > 0);
                GetWindowRect(state.controls[&212], &mut content).unwrap();
                assert!(content.top >= pane.top && content.bottom <= pane.bottom);
                let _ = SetFocus(Some(state.controls[&201]));
                state.keep_focus_visible();
                state.build();
                assert_eq!(GetFocus(), state.controls[&201]);
                assert_eq!((state.font, state.heading_font), fonts);
                assert!(!IsWindow(Some(host)).as_bool());
                state.command(1, 0);
                assert_eq!(
                    send(state.controls[&1], BM_GETCHECK, WPARAM(0), LPARAM(0)).0,
                    1
                );
                assert_eq!(
                    send(state.controls[&3], BM_GETCHECK, WPARAM(0), LPARAM(0)).0,
                    0
                );
                state.set_control_text(11, "Unsaved mouse name");
                state.set_control_text(13, "invalid");
                let name = state.controls[&11];
                state.command(15, 0);
                assert_eq!(state.controls[&11], name);
                assert_eq!(state.text(11), "Unsaved mouse name");
                assert_eq!(state.text(13), "invalid");
                state.build();
                assert_eq!(state.text(11), "Unsaved mouse name");
                assert_eq!(state.text(13), "invalid");
                let preserved = state.settings.device_mut("synthetic-mouse");
                preserved.fullscreen_boost_rate = Some(PollingRate::try_from(4000).unwrap());
                preserved.low_battery_mode = Some(hb_core::settings::LowBatteryMode {
                    threshold: 20,
                    polling_rate: Some(PollingRate::try_from(500).unwrap()),
                    sleep_seconds: None,
                });
                let startup = preserved.requested_polling_rate;
                state.command(16, 0);
                let preserved = state.settings.device("synthetic-mouse").unwrap();
                assert_eq!(preserved.requested_polling_rate, startup);
                assert_eq!(preserved.fullscreen_boost_rate.unwrap().hz(), 4000);
                assert!(preserved.low_battery_mode.is_some());
                assert_eq!(state.text(95), "Device preferences reset");
                assert_ne!(state.text(11), "Unsaved mouse name");
                assert!(state.text(13).is_empty());
                state.set_control_text(11, "Another unsaved name");
                let name = state.controls[&11];
                state.error = "Storage: native database diagnostic".into();
                state.command(5, 0);
                assert_eq!(state.controls[&11], name);
                assert!(!state.text(95).contains("database"));
                let report: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(state.dir.join("diagnostics.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    report["application_error"],
                    "Storage: native database diagnostic"
                );
            }
            send(reopened, WM_VSCROLL, WPARAM(SB_TOP.0 as usize), LPARAM(0));
            assert_eq!(context.state.borrow().page_scroll, 0);
            send(
                reopened,
                WM_VSCROLL,
                WPARAM(SB_BOTTOM.0 as usize),
                LPARAM(0),
            );
            assert!(context.state.borrow().page_scroll > 0);
            let scrollbar = context.state.borrow().controls[&7];
            // Exercise the actual themed control input, without injecting
            // system mouse/keyboard events or involving connected hardware.
            send(reopened, WM_VSCROLL, WPARAM(SB_TOP.0 as usize), LPARAM(0));
            let mut bounds = RECT::default();
            GetClientRect(scrollbar, &mut bounds).unwrap();
            let point = |y: i32| LPARAM(((y as u32) << 16 | 7) as isize);
            send(
                scrollbar,
                WM_LBUTTONDOWN,
                WPARAM(0),
                point(bounds.bottom - 2),
            );
            assert!(context.state.borrow().page_scroll > 0);
            send(scrollbar, WM_LBUTTONUP, WPARAM(0), point(bounds.bottom - 2));
            send(reopened, WM_VSCROLL, WPARAM(SB_TOP.0 as usize), LPARAM(0));
            let mut bar = SCROLLBARINFO {
                cbSize: size_of::<SCROLLBARINFO>() as u32,
                ..Default::default()
            };
            GetScrollBarInfo(scrollbar, OBJID_CLIENT, &mut bar).unwrap();
            send(
                scrollbar,
                WM_LBUTTONDOWN,
                WPARAM(0),
                point(bar.xyThumbTop + 2),
            );
            send(scrollbar, WM_MOUSEMOVE, WPARAM(0), point(bounds.bottom - 2));
            send(scrollbar, WM_LBUTTONUP, WPARAM(0), point(bounds.bottom - 2));
            let state = context.state.borrow();
            assert!(state.page_scroll > 0);
            let mut info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_ALL,
                ..Default::default()
            };
            GetScrollInfo(scrollbar, SB_CTL, &mut info).unwrap();
            assert_eq!(state.page_scroll, info.nMax - info.nPage as i32 + 1);
            drop(state);
            {
                let mut state = context.state.borrow_mut();
                state.page = 3;
                state.more_options = true;
                state.build();
                state.scroll_page(i32::MAX);
                SetFocus(Some(state.controls[&212])).unwrap();
            }
            let deliver = |hwnd: HWND, msg, wp, lp| {
                let message = MSG {
                    hwnd,
                    message: msg,
                    wParam: wp,
                    lParam: lp,
                    ..Default::default()
                };
                if !dashboard_message(&context, &message) {
                    DispatchMessageW(&message);
                }
            };
            let wheel = |delta: i16| WPARAM((delta as u16 as usize) << 16);
            let bottom = context.state.borrow().page_scroll;
            let focused = GetFocus();
            deliver(focused, WM_MOUSEWHEEL, wheel(120), LPARAM(0));
            assert_eq!(
                context.state.borrow().page_scroll,
                bottom - 48,
                "wheel must not snap to old focus"
            );
            for _ in 0..40 {
                deliver(focused, WM_MOUSEWHEEL, wheel(120), LPARAM(0));
            }
            assert_eq!(
                context.state.borrow().page_scroll,
                0,
                "top must remain reachable with lower control focused"
            );
            context.state.borrow_mut().build();
            assert_eq!(
                context.state.borrow().page_scroll,
                0,
                "background rebuild must preserve viewport"
            );
            let combo = context.state.borrow().controls[&205];
            SetFocus(Some(combo)).unwrap();
            let selected = send(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0));
            deliver(combo, WM_MOUSEWHEEL, wheel(-120), LPARAM(0));
            assert_eq!(context.state.borrow().page_scroll, 48);
            assert_eq!(
                send(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)),
                selected,
                "closed dropdown must not change on page wheel"
            );
            for _ in 0..5 {
                deliver(combo, WM_MOUSEWHEEL, wheel(-1), LPARAM(0));
            }
            assert_eq!(
                context.state.borrow().page_scroll,
                50,
                "small wheel deltas must accumulate"
            );
            {
                let mut state = context.state.borrow_mut();
                state.scroll_page(i32::MAX);
                SetFocus(Some(state.controls[&212])).unwrap();
            }
            let (scrollbar, bottom) = {
                let state = context.state.borrow();
                (state.controls[&7], state.page_scroll)
            };
            deliver(scrollbar, WM_LBUTTONDOWN, WPARAM(0), point(2));
            deliver(scrollbar, WM_LBUTTONUP, WPARAM(0), point(2));
            assert_eq!(
                context.state.borrow().page_scroll,
                bottom - 32,
                "upper arrow must not snap to focused control"
            );
            // Devices has the longest page: reproduce scrolling from sleep/saver
            // controls while a polling update rebuilds the lower sections.
            {
                let mut state = context.state.borrow_mut();
                state.snapshot.devices[0].reading.key = "razer:00bf:SCROLL-TEST".into();
                state.snapshot.devices[0].reading.source = "razer".into();
                state.snapshot.devices[0].reading.via = "usb".into();
                state.selected_device = Some("razer:00bf:SCROLL-TEST".into());
                state.settings.polling_controls = true;
                state.page = 1;
                state.build();
                state.scroll_page(i32::MAX);
                SetFocus(Some(state.controls[&445])).unwrap();
            }
            let bottom = context.state.borrow().page_scroll;
            deliver(GetFocus(), WM_MOUSEWHEEL, wheel(120), LPARAM(0));
            assert_eq!(context.state.borrow().page_scroll, bottom - 48);
            context.state.borrow_mut().polling_controls();
            assert_eq!(
                context.state.borrow().page_scroll,
                bottom - 48,
                "polling update must not undo scrolling"
            );
            let scrollbar = context.state.borrow().controls[&7];
            let mut thumb = SCROLLBARINFO {
                cbSize: size_of::<SCROLLBARINFO>() as u32,
                ..Default::default()
            };
            GetScrollBarInfo(scrollbar, OBJID_CLIENT, &mut thumb).unwrap();
            deliver(
                scrollbar,
                WM_LBUTTONDOWN,
                WPARAM(0),
                point(thumb.xyThumbTop + 2),
            );
            deliver(scrollbar, WM_MOUSEMOVE, WPARAM(0), point(2));
            deliver(scrollbar, WM_LBUTTONUP, WPARAM(0), point(2));
            assert_eq!(
                context.state.borrow().page_scroll,
                0,
                "thumb drag must reach top on Devices"
            );
            // The titlebar X enters DefWindowProc(SC_CLOSE), which synchronously
            // reenters WM_CLOSE. Default processing must release State first.
            send(
                reopened,
                WM_SYSCOMMAND,
                WPARAM(SC_CLOSE as usize),
                LPARAM(0),
            );
            dispatch_monitor(monitor);
            assert!(context.state.borrow().dashboard.is_none());
            if !restore {
                use windows::Win32::System::Threading::{
                    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
                };
                let process = GetCurrentProcess();
                sample_closed_dashboard(&context, "before-40-cycles");
                let mut baseline = None;
                for cycle in 0..40 {
                    let mut state = context.state.borrow_mut();
                    state.open();
                    let window = state.dashboard.unwrap();
                    let mut title = [0u16; 128];
                    let title_length = GetWindowTextW(window, &mut title) as usize;
                    assert_eq!(
                        String::from_utf16_lossy(&title[..title_length]),
                        concat!("Halo Continued ", env!("CARGO_PKG_VERSION"))
                    );
                    // Exercise real sleep controls, in both themes, without touching hardware.
                    state.snapshot.devices[0].reading.key = "razer:00bf:SYNTHETIC-SLEEP".into();
                    state.snapshot.devices[0].reading.source = "razer".into();
                    state.snapshot.devices[0].reading.via = "usb".into();
                    state.selected_device = Some("razer:00bf:SYNTHETIC-SLEEP".into());
                    state.settings.polling_controls = true;
                    state.page = 1;
                    state.build();
                    state.apply_theme(DashboardTheme::new(cycle % 2 == 0, false));
                    assert!(state.controls.contains_key(&433));
                    assert_eq!(
                        GetWindowLongW(state.controls[&433], GWL_STYLE) as u32 & WS_DISABLED.0,
                        0
                    );
                    send(state.controls[&431], CB_SETCURSEL, WPARAM(6), LPARAM(0));
                    state.polling_controls();
                    assert_eq!(
                        state.choice(431),
                        6,
                        "polling replies must preserve the sleep draft"
                    );
                    assert!(state.controls.contains_key(&446));
                    send(state.controls[&444], CB_SETCURSEL, WPARAM(2), LPARAM(0));
                    state.set_control_text(442, "25");
                    SetFocus(Some(state.controls[&442])).unwrap();
                    send(
                        state.controls[&442],
                        windows::Win32::UI::Controls::EM_SETSEL,
                        WPARAM(1),
                        LPARAM(2),
                    );
                    state.polling_controls();
                    assert_eq!(
                        GetFocus(),
                        state.controls[&442],
                        "hardware replies must not steal edit focus"
                    );
                    assert_eq!(
                        send(
                            state.controls[&442],
                            windows::Win32::UI::Controls::EM_GETSEL,
                            WPARAM(0),
                            LPARAM(0)
                        )
                        .0,
                        1 | (2 << 16)
                    );
                    let choices = state.battery_saver_rates(&state.current_device().unwrap().0);
                    assert!(
                        !choices.contains(&250),
                        "DeathAdder does not accept the keyboard-only 250 Hz code"
                    );
                    assert_eq!(
                        state.choice(444),
                        2,
                        "automatic outcomes preserve the battery-saver draft"
                    );
                    assert_eq!(state.text(442), "25");
                    state.scroll_page(i32::MAX);
                    let mut pane = RECT::default();
                    let mut bounds = RECT::default();
                    GetWindowRect(state.page_host.unwrap(), &mut pane).unwrap();
                    GetWindowRect(state.controls[&447], &mut bounds).unwrap();
                    assert!(
                        bounds.top >= pane.top && bounds.bottom <= pane.bottom,
                        "battery saver controls must be reachable"
                    );
                    let battery_reading = state.snapshot.devices[0].reading.clone();
                    state.snapshot.devices[0].reading.key = "razer:00be:SYNTHETIC-SLEEP".into();
                    state.snapshot.devices[0].reading.charging = Some(false);
                    state.selected_device = Some("razer:00be:SYNTHETIC-SLEEP".into());
                    state.build();
                    assert!(!state.controls.contains_key(&440));
                    assert!(state.text(447).contains("Inactive while plugged in"));
                    state.snapshot.devices[0].reading = battery_reading;
                    state.selected_device = Some("razer:00bf:SYNTHETIC-SLEEP".into());
                    state.page = 3;
                    state.build();
                    state.command(212, 0);
                    state.scroll_page(i32::MAX);
                    state.close_dashboard(window);
                    assert!(state.dashboard.is_none() && state.page_host.is_none());
                    assert!(
                        state.controls.is_empty() && state.chart.is_none() && state.theme.is_none()
                    );
                    assert!(state.font.is_invalid() && state.heading_font.is_invalid());
                    let handles = (
                        GetGuiResources(process, GR_GDIOBJECTS),
                        GetGuiResources(process, GR_USEROBJECTS),
                    );
                    if cycle == 0 {
                        baseline = Some(handles);
                    }
                    if cycle == 39 {
                        let first = baseline.unwrap();
                        assert!(
                            handles.0 <= first.0 && handles.1 <= first.1,
                            "closed dashboard native resources grew: {first:?} -> {handles:?}"
                        );
                        if std::env::var_os("HALO_CAPTURE_DASHBOARD_TEST").is_some() {
                            eprintln!(
                                "Closed dashboard native handles, cycles 1 and 40: {first:?} -> {handles:?}"
                            );
                        }
                    }
                }
            }
            if !restore {
                sample_closed_dashboard(&context, "after-40-cycles");
            }
            context.state.borrow_mut().runtime.stop();
            DestroyWindow(monitor).unwrap();
            UnregisterClassW(class, Some(instance.into())).unwrap();
        }
        drop(test_directory);
    }
}
