use crate::transport::{container, property};
use hb_core::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use windows::{
    Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothDevice, BluetoothLEDevice},
    Foundation::TypedEventHandler,
    Gaming::Input::{Gamepad, RawGameController},
    System::Power::BatteryStatus,
    Win32::{
        Devices::{DeviceAndDriverInstallation::*, Properties::*},
        Foundation::{DEVPROPKEY, ERROR_NO_MORE_ITEMS, GetLastError},
        UI::Input::XboxController::*,
    },
    core::{GUID, PCWSTR},
};

struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
fn wait_bluetooth_query(
    c: &PollContext<'_>,
    mut pending: impl FnMut() -> Result<bool, ProviderError>,
    mut cancel: impl FnMut(),
) -> Result<(), ProviderError> {
    let until = (c.clock.monotonic() + Duration::from_secs(3)).min(c.deadline);
    loop {
        if !c.active() || c.clock.monotonic() >= until {
            cancel();
            return Err(ProviderError::new("Bluetooth connection query timed out"));
        }
        match pending() {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(error) => {
                cancel();
                return Err(error);
            }
        }
        c.sleep(Duration::from_millis(20));
    }
}
fn text(bytes: Vec<u8>) -> String {
    String::from_utf16_lossy(
        &bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|c| *c != 0)
            .collect::<Vec<_>>(),
    )
}
fn battery_percent(bytes: Vec<u8>) -> Option<u8> {
    let value = match bytes.as_slice() {
        [a] => u32::from(*a),
        [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
        _ => return None,
    };
    (value <= 100).then_some(value as u8)
}
fn mac_of(id: &str) -> Option<String> {
    let id = id.to_ascii_uppercase();
    if let Some(at) = id.find("DEV_") {
        let s = &id[at + 4..];
        if s.as_bytes()
            .get(..12)
            .is_some_and(|bytes| bytes.iter().all(u8::is_ascii_hexdigit))
            && !s.as_bytes().get(12).is_some_and(u8::is_ascii_hexdigit)
        {
            return Some(s[..12].into());
        }
    }
    id.split(|c: char| !c.is_ascii_hexdigit())
        .rfind(|s| s.len() == 12)
        .map(str::to_owned)
}
fn kind_from_class(raw: u32, le: bool) -> &'static str {
    if le {
        match (raw >> 6, raw & 63) {
            (15, 1) => "keyboard",
            (15, 2) => "mouse",
            (15, 3 | 4) => "gamepad",
            (37, _) => "headset",
            _ => "",
        }
    } else {
        let minor = (raw >> 2) & 63;
        match (raw >> 8) & 31 {
            4 if matches!(minor, 1 | 2 | 6) => "headset",
            5 if matches!(minor & 15, 1 | 2) => "gamepad",
            5 => match minor >> 4 {
                1 | 3 => "keyboard",
                2 => "mouse",
                _ => "",
            },
            _ => "",
        }
    }
}
#[derive(Default)]
struct Node {
    level: Option<u8>,
    fallback: Option<bool>,
    name: String,
    root_name: String,
    container: Option<String>,
    le: bool,
    audio: bool,
}
enum Link {
    Classic(BluetoothDevice, Option<i64>),
    Le(BluetoothLEDevice, Option<i64>),
}
impl Link {
    fn connected(&self) -> windows::core::Result<bool> {
        match self {
            Self::Classic(d, _) => d.ConnectionStatus(),
            Self::Le(d, _) => d.ConnectionStatus(),
        }
        .map(|s| s == BluetoothConnectionStatus::Connected)
    }
    fn kind(&self) -> &'static str {
        match self {
            Self::Classic(d, _) => d
                .ClassOfDevice()
                .and_then(|c| c.RawValue())
                .map(|v| kind_from_class(v, false)),
            Self::Le(d, _) => d
                .Appearance()
                .and_then(|a| a.RawValue())
                .map(|v| kind_from_class(v.into(), true)),
        }
        .unwrap_or("")
    }
}
impl Drop for Link {
    fn drop(&mut self) {
        match self {
            Self::Classic(d, t) => {
                if let Some(t) = t {
                    let _ = d.RemoveConnectionStatusChanged(*t);
                }
                let _ = d.Close();
            }
            Self::Le(d, t) => {
                if let Some(t) = t {
                    let _ = d.RemoveConnectionStatusChanged(*t);
                }
                let _ = d.Close();
            }
        }
    }
}
#[derive(Default)]
pub struct BluetoothProvider {
    links: BTreeMap<String, Link>,
    link_status: BTreeMap<String, Option<bool>>,
    levels: BTreeMap<String, u8>,
    readings: Vec<Reading>,
    snapshot_error: Option<ProviderError>,
    changed: Arc<AtomicBool>,
    retries: Vec<Duration>,
    next_full: Duration,
    last_poll: Duration,
    diagnostics: Vec<String>,
}
impl BluetoothProvider {
    fn complete_snapshot(&mut self, result: PollResult, now: Duration) {
        match result {
            Ok(readings) => {
                self.readings = readings;
                self.snapshot_error = None;
                self.next_full = now + Duration::from_secs(60);
            }
            Err(error) => {
                // Preserve failure evidence between bounded retries. Returning
                // the old online cache would falsely clear the engine's error.
                self.snapshot_error = Some(error);
                self.next_full = now + Duration::from_secs(15);
            }
        }
    }
    fn retain_inventory(&mut self, nodes: &BTreeMap<String, Node>) {
        // Call only after a complete successful PnP inventory. Disconnected
        // paired devices still have nodes, so their last level survives sleep.
        self.links.retain(|mac, _| nodes.contains_key(mac));
        self.link_status
            .retain(|mac, _| self.links.contains_key(mac));
        self.levels.retain(|mac, _| nodes.contains_key(mac));
    }
    pub fn invalidate(&mut self) {
        self.changed.store(true, Ordering::Relaxed);
    }
    fn refresh_due(&mut self, now: Duration) -> bool {
        self.last_poll = now;
        if self.changed.swap(false, Ordering::Relaxed) {
            self.retries = [0, 3, 8, 15]
                .into_iter()
                .map(|s| now + Duration::from_secs(s))
                .collect();
        }
        let due = self.retries.iter().any(|d| *d <= now);
        self.retries.retain(|d| *d > now);
        due || now >= self.next_full
    }
    fn connect(&mut self, mac: &str, le: bool, c: &PollContext<'_>) -> Result<(), ProviderError> {
        if self.links.contains_key(mac) {
            return Ok(());
        }
        let address =
            u64::from_str_radix(mac, 16).map_err(|e| ProviderError::new(e.to_string()))?;
        macro_rules! device {
            ($ty:ty,$variant:ident) => {{
                let op = <$ty>::FromBluetoothAddressAsync(address)
                    .map_err(|e| ProviderError::new(e.to_string()))?;
                wait_bluetooth_query(
                    c,
                    || {
                        op.Status()
                            .map(|s| s.0 == 0)
                            .map_err(|e| ProviderError::new(e.to_string()))
                    },
                    || {
                        let _ = op.Cancel();
                    },
                )?;
                let d = op
                    .GetResults()
                    .map_err(|e| ProviderError::new(e.to_string()))?;
                let changed = self.changed.clone();
                let token = d
                    .ConnectionStatusChanged(&TypedEventHandler::new(move |_, _| {
                        changed.store(true, Ordering::Relaxed);
                        Ok(())
                    }))
                    .ok();
                self.links.insert(mac.into(), Link::$variant(d, token));
            }};
        }
        if le {
            device!(BluetoothLEDevice, Le)
        } else {
            device!(BluetoothDevice, Classic)
        }
        Ok(())
    }
    fn snapshot(&mut self, c: &PollContext<'_>) -> PollResult {
        let set = DeviceSet(
            unsafe {
                SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT)
            }
            .map_err(|e| ProviderError::new(e.to_string()))?,
        );
        let battery = DEVPROPKEY {
            fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
            pid: 2,
        };
        let connection = DEVPROPKEY {
            fmtid: GUID::from_u128(0x83da6326_97a6_4088_9453_a1923f573b29),
            pid: 15,
        };
        let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
        for index in 0..65536 {
            if !c.active() {
                return Err(ProviderError::new(
                    "Bluetooth snapshot cancelled or deadline exceeded",
                ));
            }
            let mut info = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) }.is_err() {
                if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
                    break;
                }
                return Err(ProviderError::new("Bluetooth PnP enumeration failed"));
            }
            let mut id = [0; 1024];
            if unsafe { SetupDiGetDeviceInstanceIdW(set.0, &info, Some(&mut id), None) }.is_err() {
                continue;
            }
            let id = String::from_utf16_lossy(
                &id[..id.iter().position(|x| *x == 0).unwrap_or(id.len())],
            )
            .to_ascii_uppercase();
            if !id.starts_with("BTH") {
                continue;
            }
            let Some(mac) = mac_of(&id) else { continue };
            let node = nodes.entry(mac).or_default();
            node.le |= id.starts_with("BTHLE");
            node.audio |= ["0000110B-", "0000111E-", "00001108-", "00001131-"]
                .iter()
                .any(|u| id.contains(u));
            let name = property(info.DevInst, &DEVPKEY_Device_FriendlyName)
                .or_else(|| property(info.DevInst, &DEVPKEY_Device_DeviceDesc))
                .map(text)
                .unwrap_or_default();
            if id.starts_with("BTHENUM\\DEV_") || id.starts_with("BTHLE\\DEV_") {
                node.root_name = name.clone()
            }
            if node.name.is_empty() {
                node.name = name
            }
            if node.container.is_none() {
                node.container = container(info.DevInst)
            }
            if let Some(level) = property(info.DevInst, &battery).and_then(battery_percent) {
                node.level = Some(level)
            }
            if let Some(value) =
                property(info.DevInst, &connection).and_then(|b| b.first().copied())
            {
                node.fallback = Some(node.fallback.unwrap_or(false) || value != 0)
            }
        }
        self.retain_inventory(&nodes);
        let mut out = Vec::new();
        self.diagnostics.clear();
        for (mac, node) in nodes {
            if self.links.get(&mac).is_some_and(|d| d.connected().is_err()) {
                self.links.remove(&mac);
            }
            if let Some(level) = node.level {
                self.levels.insert(mac.clone(), level);
            }
            if let Err(e) = self.connect(&mac, node.le, c) {
                self.diagnostics
                    .push(format!("{mac}: {e}; using PnP connection fallback"));
            }
            let connected = self
                .links
                .get(&mac)
                .and_then(|d| d.connected().ok())
                .or(node.fallback)
                .unwrap_or(false);
            if !connected {
                continue;
            }
            let Some(level) = self.levels.get(&mac).copied() else {
                continue;
            };
            let name = if !node.root_name.is_empty() {
                node.root_name
            } else if !node.name.is_empty() {
                node.name
            } else {
                format!("Bluetooth {mac}")
            };
            let mut r = Reading::new(
                format!("bluetooth:{mac}"),
                name,
                "bluetooth",
                c.clock.unix(),
            );
            r.level = Some(level);
            r.serial = Some(mac.clone());
            r.container = node.container;
            r.via = "bluetooth".into();
            r.kind = self
                .links
                .get(&mac)
                .map(Link::kind)
                .filter(|s| !s.is_empty())
                .unwrap_or(if node.audio { "headset" } else { "" })
                .into();
            if node.level.is_none() {
                r.connection = Connection::Stale;
                r.approx = Some("Last battery level reported by Windows".into())
            }
            out.push(r);
        }
        Ok(out)
    }
}
impl BatteryProvider for BluetoothProvider {
    fn id(&self) -> &'static str {
        "bluetooth"
    }
    fn invalidate(&mut self) {
        BluetoothProvider::invalidate(self);
    }
    fn next_poll_delay(&self) -> Option<Duration> {
        Some(
            self.retries
                .iter()
                .copied()
                .min()
                .unwrap_or(self.next_full)
                .min(self.next_full)
                .saturating_sub(self.last_poll)
                .min(Duration::from_secs(2)),
        )
    }
    fn diagnostics(&self) -> Vec<String> {
        self.diagnostics.clone()
    }
    fn poll(&mut self, _: &dyn HidTransport, c: &PollContext<'_>) -> PollResult {
        let now = c.clock.monotonic();
        for (mac, link) in &self.links {
            let status = link.connected().ok();
            let changed = if let Some(previous) = self.link_status.get_mut(mac) {
                let changed = *previous != status;
                *previous = status;
                changed
            } else {
                self.link_status.insert(mac.clone(), status);
                false
            };
            if changed {
                self.changed.store(true, Ordering::Relaxed);
            }
        }
        if self.refresh_due(now) {
            let fresh = self.snapshot(c);
            self.complete_snapshot(fresh, c.clock.monotonic());
        }
        if let Some(error) = &self.snapshot_error {
            return Err(error.clone());
        }
        Ok(self.readings.clone())
    }
}
pub struct ControllerProvider;
fn xbox_bluetooth(vid: u16, pid: u16) -> bool {
    vid == 0x045e
        && matches!(
            pid,
            0x02e0 | 0x02fd | 0x0b05 | 0x0b0c | 0x0b13 | 0x0b20 | 0x0b21 | 0x0b22
        )
}
fn coarse(battery: &XINPUT_BATTERY_INFORMATION) -> Option<(u8, &'static str)> {
    if battery.BatteryType != BATTERY_TYPE_ALKALINE && battery.BatteryType != BATTERY_TYPE_NIMH {
        return None;
    }
    match battery.BatteryLevel.0 {
        0 => Some((5, "Empty")),
        1 => Some((20, "Low")),
        2 => Some((55, "Medium")),
        3 => Some((100, "Full")),
        _ => None,
    }
}
fn wgi_percentage(vid: u16, pid: u16, remaining: Option<i32>, full: Option<i32>) -> Option<u8> {
    if !matches!(vid, 0x045e | 0x3537) || xbox_bluetooth(vid, pid) {
        return None;
    }
    remaining
        .zip(full)
        .filter(|(a, b)| *a >= 0 && *b > 0)
        .map(|(a, b)| (i64::from(a) * 100 / i64::from(b)).clamp(0, 100) as u8)
}
fn wgi_charging(status: Option<BatteryStatus>) -> Option<bool> {
    status.and_then(|s| {
        if s == BatteryStatus::Charging {
            Some(true)
        } else if s == BatteryStatus::Discharging || s == BatteryStatus::Idle {
            Some(false)
        } else {
            None
        }
    })
}
fn xinput_reading(
    slot: u32,
    battery: Option<&XINPUT_BATTERY_INFORMATION>,
    timestamp: i64,
) -> Reading {
    let mut r = Reading::new(
        format!("xinput:{slot}"),
        format!("Xbox controller {}", slot + 1),
        "xinput",
        timestamp,
    );
    r.kind = "gamepad".into();
    r.precision = Precision::Coarse;
    if let Some(battery) = battery {
        if let Some((level, label)) = coarse(battery) {
            r.level = Some(level);
            r.approx = Some(format!("About {level}% ({label})"));
            r.charging = Some(false);
            r.via = "wireless".into();
        } else if battery.BatteryType == BATTERY_TYPE_WIRED {
            r.via = "usb".into();
            r.approx = Some("Wired connection; battery not reported".into());
        }
    }
    r
}
impl BatteryProvider for ControllerProvider {
    fn id(&self) -> &'static str {
        "xinput"
    }
    fn diagnostics(&self) -> Vec<String> {
        vec![]
    }
    fn poll(&mut self, _: &dyn HidTransport, c: &PollContext<'_>) -> PollResult {
        let mut out = Vec::new();
        if let Ok(controllers) = RawGameController::RawGameControllers() {
            for raw in controllers {
                if !c.active() {
                    return Err(ProviderError::new("Controller snapshot deadline exceeded"));
                }
                if Gamepad::FromGameController(&raw).is_err() {
                    continue;
                }
                let Ok(id) = raw.NonRoamableId() else {
                    continue;
                };
                if id.is_empty() {
                    continue;
                }
                let mut r = Reading::new(
                    format!("wgi:{id}"),
                    raw.DisplayName()
                        .map(|x| x.to_string())
                        .unwrap_or("Controller".into()),
                    "xinput",
                    c.clock.unix(),
                );
                r.kind = "gamepad".into();
                let bt = xbox_bluetooth(
                    raw.HardwareVendorId().unwrap_or(0),
                    raw.HardwareProductId().unwrap_or(0),
                );
                r.via = if bt { "bluetooth" } else { "wireless" }.into();
                if let Ok(report) = raw.TryGetBatteryReport() {
                    r.level = wgi_percentage(
                        raw.HardwareVendorId().unwrap_or(0),
                        raw.HardwareProductId().unwrap_or(0),
                        report
                            .RemainingCapacityInMilliwattHours()
                            .and_then(|v| v.Value())
                            .ok(),
                        report
                            .FullChargeCapacityInMilliwattHours()
                            .and_then(|v| v.Value())
                            .ok(),
                    );
                    r.charging = wgi_charging(report.Status().ok());
                }
                if bt {
                    r.approx = Some("Battery available through Windows Bluetooth devices".into())
                }
                out.push(r);
            }
        }
        // Windows exposes no dependable WGI-to-XInput slot identity. Keep every slot;
        // a WGI report from one pad must never hide another pad's XInput report.
        for slot in 0..4 {
            let mut state = XINPUT_STATE::default();
            if unsafe { XInputGetState(slot, &mut state) } != 0 {
                continue;
            }
            let mut battery = XINPUT_BATTERY_INFORMATION::default();
            let code =
                unsafe { XInputGetBatteryInformation(slot, BATTERY_DEVTYPE_GAMEPAD, &mut battery) };
            out.push(xinput_reading(
                slot,
                (code == 0).then_some(&battery),
                c.clock.unix(),
            ));
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn bluetooth_level_cache_tracks_inventory_and_retains_sleeping_paired_devices() {
        let mut provider = BluetoothProvider::default();
        provider.levels.insert("sleeping-paired".into(), 75);
        provider.levels.insert("removed-pairing".into(), 40);
        let mut nodes = BTreeMap::new();
        nodes.insert("sleeping-paired".into(), Node::default());
        provider.retain_inventory(&nodes);
        assert_eq!(provider.levels.get("sleeping-paired"), Some(&75));
        assert!(!provider.levels.contains_key("removed-pairing"));
        for index in 0..1000 {
            let mac = format!("new-pairing-{index}");
            provider.levels.insert(mac.clone(), 50);
            nodes.retain(|key, _| key == "sleeping-paired");
            nodes.insert(mac, Node::default());
            provider.retain_inventory(&nodes);
            assert_eq!(provider.levels.len(), 2);
        }
        assert_eq!(provider.levels["sleeping-paired"], 75);
    }
    use super::*;
    #[test]
    fn service_node_mac_and_boundaries() {
        assert_eq!(
            mac_of("BTHENUM\\{UUID}_VID&1234\\8&01&AABBCCDDEEFF_C00000000"),
            Some("AABBCCDDEEFF".into())
        );
        assert_eq!(
            mac_of("BTHLE\\DEV_aabbccddeeff"),
            Some("AABBCCDDEEFF".into())
        );
        assert_eq!(mac_of("BTHLE\\DEV_AABBCCDDEEFF0"), None);
    }
    #[test]
    fn unknown_and_wired_do_not_invent_battery() {
        for kind in [
            BATTERY_TYPE_WIRED,
            BATTERY_TYPE_UNKNOWN,
            BATTERY_TYPE_DISCONNECTED,
        ] {
            assert!(
                coarse(&XINPUT_BATTERY_INFORMATION {
                    BatteryType: kind,
                    BatteryLevel: BATTERY_LEVEL_FULL
                })
                .is_none()
            );
        }
    }
    #[test]
    fn coarse_levels_match_original() {
        assert_eq!(
            coarse(&XINPUT_BATTERY_INFORMATION {
                BatteryType: BATTERY_TYPE_ALKALINE,
                BatteryLevel: BATTERY_LEVEL_MEDIUM
            }),
            Some((55, "Medium"))
        );
    }
    #[test]
    fn bluetooth_classification() {
        assert_eq!(kind_from_class((15 << 6) | 2, true), "mouse");
        assert_eq!(kind_from_class((4 << 8) | (6 << 2), false), "headset");
        assert!(xbox_bluetooth(0x045e, 0x02fd));
        assert!(!xbox_bluetooth(0x045e, 0x02ea));
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    struct FakeClock(Duration);
    impl Clock for FakeClock {
        fn unix(&self) -> i64 {
            100
        }
        fn monotonic(&self) -> Duration {
            self.0
        }
        fn sleep(&self, _: Duration) {
            panic!("cached poll must not sleep")
        }
    }
    struct NoHid;
    impl HidTransport for NoHid {
        fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
            panic!("Bluetooth must not enumerate HID")
        }
        fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            panic!("Bluetooth must not open HID")
        }
    }
    #[test]
    fn arrival_retries_and_minute_refresh() {
        let mut p = BluetoothProvider {
            next_full: Duration::from_secs(60),
            ..Default::default()
        };
        p.invalidate();
        for (seconds, due) in [
            (0, true),
            (2, false),
            (3, true),
            (7, false),
            (8, true),
            (14, false),
            (15, true),
            (59, false),
            (60, true),
        ] {
            assert_eq!(
                p.refresh_due(Duration::from_secs(seconds)),
                due,
                "at {seconds}"
            );
        }
    }
    #[test]
    fn cached_poll_uses_fake_clock_and_never_touches_hardware() {
        let mut p = BluetoothProvider {
            next_full: Duration::from_secs(60),
            ..Default::default()
        };
        p.readings.push(Reading::new(
            "bluetooth:AABBCCDDEEFF",
            "Headset",
            "bluetooth",
            99,
        ));
        let clock = FakeClock(Duration::from_secs(1));
        let cancelled = AtomicBool::new(false);
        let context = PollContext {
            clock: &clock,
            cancelled: &cancelled,
            deadline: Duration::from_secs(10),
            playstation_full_mode: false,
        };
        let result = p.poll(&NoHid, &context).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp, 99);
        assert_eq!(p.next_poll_delay(), Some(Duration::from_secs(2)));
    }
    #[test]
    fn failed_bluetooth_snapshot_stays_failed_until_successful_refresh() {
        let mut provider = BluetoothProvider::default();
        let original = vec![Reading::new("test", "Test headset", "bluetooth", 0)];
        provider.complete_snapshot(Ok(original.clone()), Duration::ZERO);
        provider.complete_snapshot(
            Err(ProviderError::new("PnP unavailable")),
            Duration::from_secs(60),
        );
        let cancelled = AtomicBool::new(false);
        let clock = FakeClock(Duration::from_secs(62));
        let context = PollContext {
            clock: &clock,
            cancelled: &cancelled,
            deadline: Duration::from_secs(100),
            playstation_full_mode: false,
        };
        assert_eq!(
            provider.poll(&NoHid, &context).unwrap_err().message,
            "PnP unavailable"
        );
        assert_eq!(provider.next_full, Duration::from_secs(75));
        provider.complete_snapshot(Ok(original.clone()), Duration::from_secs(75));
        assert_eq!(provider.poll(&NoHid, &context).unwrap(), original);
        assert!(provider.snapshot_error.is_none());
    }
}

#[cfg(test)]
mod battery_property_tests {
    use super::*;
    #[test]
    fn rejects_out_of_range_dword() {
        assert_eq!(battery_percent(300u32.to_le_bytes().to_vec()), None);
        assert_eq!(battery_percent(75u32.to_le_bytes().to_vec()), Some(75));
        assert_eq!(battery_percent(vec![100]), Some(100));
        assert_eq!(battery_percent(vec![1, 0]), None);
    }
}
#[cfg(test)]
mod native_query_tests {
    use super::*;
    use std::{cell::Cell, sync::atomic::AtomicU64};
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn unix(&self) -> i64 {
            100
        }
        fn monotonic(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::Relaxed))
        }
        fn sleep(&self, d: Duration) {
            self.0.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
        }
    }
    fn run(
        pending: impl FnMut() -> Result<bool, ProviderError>,
        deadline: Duration,
        cancelled: bool,
    ) -> (Result<(), ProviderError>, Duration, bool) {
        let clock = FakeClock(AtomicU64::new(0));
        let stop = AtomicBool::new(cancelled);
        let context = PollContext {
            clock: &clock,
            cancelled: &stop,
            deadline,
            playstation_full_mode: false,
        };
        let cancelled = Cell::new(false);
        let result = wait_bluetooth_query(&context, pending, || cancelled.set(true));
        (result, clock.monotonic(), cancelled.get())
    }
    #[test]
    fn native_stalled_query_is_cancelled_within_three_seconds() {
        let (result, elapsed, cancelled) = run(|| Ok(true), Duration::from_secs(60), false);
        assert!(result.is_err());
        assert_eq!(elapsed, Duration::from_secs(3));
        assert!(cancelled);
    }
    #[test]
    fn native_query_honors_shorter_deadline_and_suspend_cancel() {
        let (result, elapsed, cancelled) = run(|| Ok(true), Duration::from_millis(7), false);
        assert!(result.is_err());
        assert_eq!(elapsed, Duration::from_millis(7));
        assert!(cancelled);
        let (result, elapsed, cancelled) = run(
            || panic!("cancelled query must not call WinRT"),
            Duration::from_secs(60),
            true,
        );
        assert!(result.is_err());
        assert_eq!(elapsed, Duration::ZERO);
        assert!(cancelled);
    }
    #[test]
    fn healthy_native_query_completes_without_restart_or_cancel() {
        let mut calls = 0;
        let (result, elapsed, cancelled) = run(
            || {
                calls += 1;
                Ok(calls < 3)
            },
            Duration::from_secs(60),
            false,
        );
        assert!(result.is_ok());
        assert_eq!(elapsed, Duration::from_millis(40));
        assert!(!cancelled);
    }
    #[test]
    fn native_query_failure_is_not_an_empty_success() {
        let (result, _, _) = run(
            || Err(ProviderError::new("WinRT unavailable")),
            Duration::from_secs(60),
            false,
        );
        assert_eq!(result.unwrap_err().message, "WinRT unavailable");
    }
}
#[cfg(test)]
mod controller_tests {
    use super::*;
    #[test]
    fn xbox_bluetooth_capacity_is_not_false_ten_percent() {
        assert_eq!(wgi_percentage(0x045e, 0x02fd, Some(100), Some(1000)), None);
    }
    #[test]
    fn every_xbox_bluetooth_pid_ignores_unreliable_capacity() {
        for pid in [
            0x02e0, 0x02fd, 0x0b05, 0x0b0c, 0x0b13, 0x0b20, 0x0b21, 0x0b22,
        ] {
            assert!(xbox_bluetooth(0x045e, pid));
            assert_eq!(wgi_percentage(0x045e, pid, Some(820), Some(1000)), None);
        }
    }
    #[test]
    fn xbox_usb_and_adapter_capacity_remains_exact() {
        for pid in [0x02ea, 0x0b12, 0x0b00, 0x02ff] {
            assert!(!xbox_bluetooth(0x045e, pid));
            assert_eq!(wgi_percentage(0x045e, pid, Some(820), Some(1000)), Some(82));
        }
    }
    #[test]
    fn gamesir_same_pid_number_is_not_xbox_bluetooth() {
        assert!(!xbox_bluetooth(0x3537, 0x02fd));
        assert_eq!(
            wgi_percentage(0x3537, 0x02fd, Some(640), Some(1000)),
            Some(64)
        );
    }
    #[test]
    fn gamesir_g7_capacity_charging_and_vendor_allowlist() {
        assert_eq!(
            wgi_percentage(0x3537, 0x1010, Some(730), Some(1000)),
            Some(73)
        );
        assert_eq!(wgi_charging(Some(BatteryStatus::Charging)), Some(true));
        for vid in [0x2dc8, 0x054c, 0x057e, 0xffff] {
            assert_eq!(wgi_percentage(vid, 0x1010, Some(1000), Some(1000)), None);
        }
    }
    #[test]
    fn discharging_dongle_cannot_invent_full_or_charging() {
        let battery = XINPUT_BATTERY_INFORMATION {
            BatteryType: BATTERY_TYPE_WIRED,
            BatteryLevel: BATTERY_LEVEL_FULL,
        };
        let r = xinput_reading(0, Some(&battery), 100);
        assert_eq!(r.level, None);
        assert_eq!(r.charging, None);
        assert!(!r.approx.unwrap().contains("charging"));
        assert_eq!(wgi_percentage(0x2dc8, 0x3106, Some(1000), Some(1000)), None);
        assert_eq!(wgi_charging(Some(BatteryStatus::Discharging)), Some(false));
    }
    #[test]
    fn genuine_wgi_wired_charging_uses_reported_percentage() {
        assert_eq!(
            wgi_percentage(0x045e, 0x02ea, Some(800), Some(1000)),
            Some(80)
        );
        assert_eq!(wgi_charging(Some(BatteryStatus::Charging)), Some(true));
    }
    #[test]
    fn wired_without_wgi_does_not_invent_full_or_charging() {
        for level in [BATTERY_LEVEL_EMPTY, BATTERY_LEVEL_FULL] {
            let battery = XINPUT_BATTERY_INFORMATION {
                BatteryType: BATTERY_TYPE_WIRED,
                BatteryLevel: level,
            };
            let r = xinput_reading(0, Some(&battery), 100);
            assert_eq!(r.level, None);
            assert_eq!(r.charging, None);
        }
        let failed = xinput_reading(0, None, 100);
        assert_eq!(failed.level, None);
        assert_eq!(failed.charging, None);
    }
    #[test]
    fn untrusted_vendor_can_report_charging_without_fake_level() {
        assert_eq!(wgi_percentage(0x2dc8, 0x3106, Some(1000), Some(1000)), None);
        assert_eq!(wgi_charging(Some(BatteryStatus::Charging)), Some(true));
    }
    #[test]
    fn missing_wgi_status_does_not_infer_charging() {
        assert_eq!(wgi_charging(None), None);
        assert_eq!(wgi_charging(Some(BatteryStatus::NotPresent)), None);
    }
    #[test]
    fn all_xinput_slots_keep_distinct_keys_and_unknown_reports() {
        let readings = (0..4)
            .map(|slot| xinput_reading(slot, None, 100))
            .collect::<Vec<_>>();
        assert_eq!(
            readings.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["xinput:0", "xinput:1", "xinput:2", "xinput:3"]
        );
        assert!(readings.iter().all(|r| r.level.is_none() && r.online()));
    }
    #[test]
    fn wireless_coarse_steps_and_unknown_types_are_explicit() {
        for kind in [BATTERY_TYPE_NIMH, BATTERY_TYPE_ALKALINE] {
            for (level, expected) in [
                (BATTERY_LEVEL_EMPTY, 5),
                (BATTERY_LEVEL_LOW, 20),
                (BATTERY_LEVEL_MEDIUM, 55),
                (BATTERY_LEVEL_FULL, 100),
            ] {
                let battery = XINPUT_BATTERY_INFORMATION {
                    BatteryType: kind,
                    BatteryLevel: level,
                };
                let r = xinput_reading(0, Some(&battery), 100);
                assert_eq!(r.level, Some(expected));
                assert_eq!(r.precision, Precision::Coarse);
                assert_eq!(r.charging, Some(false));
            }
        }
        for kind in [BATTERY_TYPE_UNKNOWN, BATTERY_TYPE_DISCONNECTED] {
            let r = xinput_reading(
                0,
                Some(&XINPUT_BATTERY_INFORMATION {
                    BatteryType: kind,
                    BatteryLevel: BATTERY_LEVEL_FULL,
                }),
                100,
            );
            assert_eq!(r.level, None);
        }
    }
    #[test]
    fn invalid_capacity_and_overflow_are_bounded() {
        for (a, b) in [
            (None, Some(1000)),
            (Some(100), None),
            (Some(-1), Some(1000)),
            (Some(100), Some(0)),
        ] {
            assert_eq!(wgi_percentage(0x3537, 0x1010, a, b), None);
        }
        assert_eq!(
            wgi_percentage(0x3537, 0x1010, Some(i32::MAX), Some(1)),
            Some(100)
        );
    }
}
