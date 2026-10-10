use hb_core::*;
use hb_providers::controls::HidDeviceController;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn unix(&self) -> i64 {
        77
    }
    fn monotonic(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
    fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct State {
    epoch: AtomicU64,
    opens: AtomicU64,
    sent: Mutex<Vec<Vec<u8>>>,
    change_on_set: AtomicBool,
    change_on_read: AtomicBool,
    open_failure: AtomicBool,
}
struct Transport {
    devices: Vec<HidInfo>,
    state: Arc<State>,
}
impl HidTransport for Transport {
    fn generation(&self) -> u64 {
        self.state.epoch.load(Ordering::Relaxed)
    }
    fn enumerate(&self, v: u16) -> Result<Vec<HidInfo>, ProviderError> {
        assert_eq!(v, 0x1532);
        Ok(self.devices.clone())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        self.state.opens.fetch_add(1, Ordering::Relaxed);
        if self.state.open_failure.load(Ordering::Relaxed) {
            return Err(ProviderError::new("open refused"));
        }
        Ok(Box::new(Session(self.state.clone())))
    }
}
struct Session(Arc<State>);
impl HidSession for Session {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("output report forbidden")
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        panic!("input read forbidden")
    }
    fn send_feature(&mut self, d: &[u8]) -> Result<(), ProviderError> {
        self.0.sent.lock().unwrap().push(d.to_vec());
        if d[8] == 0x40 && self.0.change_on_set.load(Ordering::Relaxed) {
            self.0.epoch.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        let sent = self.0.sent.lock().unwrap();
        let last = sent.last().unwrap();
        let mut r = vec![0; 91];
        r[1] = 2;
        r[6] = 1;
        r[8] = last[8];
        r[10] = if sent.iter().any(|s| s[8] == 0x40) {
            1
        } else {
            8
        };
        if self.0.change_on_read.load(Ordering::Relaxed) {
            self.0.epoch.fetch_add(1, Ordering::Relaxed);
        }
        Ok(r)
    }
}
fn device() -> HidInfo {
    HidInfo {
        path: "exact path".into(),
        vendor_id: 0x1532,
        product_id: 0x00be,
        interface: 0,
        serial: "Mouse-1".into(),
        container: Some("container-1".into()),
        feature_length: Some(91),
        ..Default::default()
    }
}
fn request(action: ControlAction) -> ControlRequest {
    let mut reading = Reading::new("razer:00be:MOUSE-1", "Unrelated display name", "razer", 0);
    reading.kind = "mouse".into();
    reading.serial = Some("Mouse-1".into());
    reading.container = Some("container-1".into());
    reading.level = Some(84);
    ControlRequest {
        request: 9,
        target: ControlTarget {
            device: ConfigurationDevice::from_reading(&reading),
            generation: 3,
        },
        action,
    }
}
fn transport() -> Transport {
    let state = Arc::new(State::default());
    state.epoch.store(3, Ordering::Relaxed);
    Transport {
        devices: vec![device()],
        state,
    }
}
fn run(request: &ControlRequest, hid: &Transport, cancelled: bool) -> ControlOutcome {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(cancelled);
    let ctx = PollContext {
        clock: &clock,
        cancelled: &cancel,
        deadline: Duration::from_secs(2),
        playstation_full_mode: false,
    };
    HidDeviceController.execute(request, hid, &ctx)
}
fn apply() -> ControlAction {
    ControlAction::Apply(PollingRate::try_from(8000).unwrap())
}
#[test]
fn reads_fresh_epoch_and_ignores_display_name() {
    let hid = transport();
    let mut r = request(ControlAction::Read);
    r.target.generation = 1;
    let o = run(&r, &hid, false);
    assert!(o.failure.is_none());
    let observation = o.observation.unwrap();
    assert_eq!(observation.target.generation, 3);
    assert_eq!(observation.rate.unwrap().hz(), 1000);
    assert!(observation.evidence.contains("unverified locally"));
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 1);
}
#[test]
fn dedicated_high_rate_razer_receivers_use_exact_device_keys() {
    for pid in [0x009f, 0x00c1] {
        let mut hid = transport();
        hid.devices[0].product_id = pid;
        let mut request = request(apply());
        request.target.device.key = format!("razer:{pid:04x}:MOUSE-1");
        let result = run(&request, &hid, false);
        assert!(result.failure.is_none(), "{result:?}");
        assert!(result.confirmed_change());
        let observation = result.observation.unwrap();
        assert_eq!(observation.rate.unwrap().hz(), 8000);
        assert!(observation.evidence.contains("Razer high-rate"));
        assert_eq!(
            hid.state
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|packet| packet[8])
                .collect::<Vec<_>>(),
            [0xc0, 0x40, 0x40, 0xc0]
        );
    }
}
#[test]
fn apply_requires_fresh_epoch_before_open() {
    let hid = transport();
    let mut r = request(apply());
    r.target.generation = 2;
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    assert!(hid.state.sent.lock().unwrap().is_empty());
}
#[test]
fn duplicate_key_is_rejected_even_with_same_serial() {
    let mut hid = transport();
    hid.devices.push(HidInfo {
        path: "other path".into(),
        ..device()
    });
    assert!(run(&request(apply()), &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
}
#[test]
fn wrong_pid_key_container_or_serial_never_opens() {
    for altered in [
        HidInfo {
            product_id: 0x00b3,
            ..device()
        },
        HidInfo {
            container: Some("replacement".into()),
            ..device()
        },
        HidInfo {
            serial: "replacement".into(),
            ..device()
        },
        HidInfo {
            interface: 1,
            ..device()
        },
    ] {
        let mut hid = transport();
        hid.devices = vec![altered];
        assert!(run(&request(apply()), &hid, false).failure.is_some());
        assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    }
    let hid = transport();
    let mut r = request(apply());
    r.target.device.key = "razer:00be:wrong".into();
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
}
#[test]
fn cancellation_unsupported_provider_offline_and_nonmouse_never_open() {
    let hid = transport();
    assert!(run(&request(apply()), &hid, true).failure.is_some());
    for change in [0, 1, 2] {
        let mut r = request(apply());
        match change {
            0 => r.target.device.source = "other".into(),
            1 => r.target.device.connection = Connection::Sleeping,
            _ => r.target.device.kind = "headset".into(),
        };
        assert!(run(&r, &hid, false).failure.is_some());
    }
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    assert!(hid.state.sent.lock().unwrap().is_empty());
}
#[test]
fn open_failure_has_no_hardware_mutation() {
    let hid = transport();
    hid.state.open_failure.store(true, Ordering::Relaxed);
    let o = run(&request(apply()), &hid, false);
    assert!(o.failure.unwrap().contains("open refused"));
    assert!(!o.may_have_changed);
    assert!(hid.state.sent.lock().unwrap().is_empty());
}
#[test]
fn epoch_change_after_first_set_prevents_second_set_and_keeps_battery() {
    let hid = transport();
    hid.state.change_on_set.store(true, Ordering::Relaxed);
    let r = request(apply());
    let original = r.target.device.clone();
    let o = run(&r, &hid, false);
    assert!(o.failure.unwrap().contains("connection changed"));
    assert!(o.may_have_changed);
    assert_eq!(o.previous.unwrap().hz(), 1000);
    assert!(o.observation.is_none());
    let sent = hid.state.sent.lock().unwrap();
    assert_eq!(sent.iter().map(|s| s[8]).collect::<Vec<_>>(), [0xc0, 0x40]);
    assert_eq!(r.target.device, original);
}
#[test]
fn verified_apply_has_previous_and_actual_rate() {
    let hid = transport();
    let o = run(&request(apply()), &hid, false);
    assert!(o.failure.is_none());
    assert_eq!(o.previous.unwrap().hz(), 1000);
    assert_eq!(o.observation.as_ref().unwrap().rate.unwrap().hz(), 8000);
    assert!(o.confirmed_change());
    assert_eq!(hid.state.sent.lock().unwrap().len(), 4);
}

#[test]
fn epoch_changed_during_get_discards_late_observation_and_sends_no_set() {
    let hid = transport();
    hid.state.change_on_read.store(true, Ordering::Relaxed);
    let o = run(&request(apply()), &hid, false);
    assert!(o.failure.unwrap().contains("connection changed"));
    assert!(!o.may_have_changed);
    assert!(o.observation.is_none());
    assert!(o.previous.is_none());
    assert_eq!(
        hid.state
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|r| r[8])
            .collect::<Vec<_>>(),
        [0xc0]
    );
}

#[test]
fn keyboard_controller_uses_single_set_and_rejects_mouse_kind_mismatch() {
    for pid in [0x026b, 0x026c, 0x0287, 0x028d, 0x02a5] {
        let mut hid = transport();
        hid.devices[0].product_id = pid;
        hid.devices[0].interface = 3;
        let mut r = request(apply());
        r.target.device.key = format!("razer:{pid:04x}:MOUSE-1");
        r.target.device.kind = "keyboard".into();
        r.target.device.via = "usb".into();
        let result = run(&r, &hid, false);
        assert!(result.failure.is_none(), "{result:?}");
        assert!(result.confirmed_change());
        assert_eq!(
            hid.state
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|p| p[8])
                .collect::<Vec<_>>(),
            [0xc0, 0x40, 0xc0]
        );
        let mut hid = transport();
        hid.devices[0].product_id = pid;
        hid.devices[0].interface = 3;
        r.target.device.kind = "mouse".into();
        assert!(run(&r, &hid, false).failure.is_some());
        assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    }
    let hid = transport();
    let mut r = request(apply());
    r.target.device.kind = "keyboard".into();
    r.target.device.via = "usb".into();
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
}

#[test]
fn keyboard_revalidates_epoch_and_physical_identity_before_any_control_open() {
    let mut hid = transport();
    hid.devices[0].product_id = 0x026b;
    hid.devices[0].interface = 3;
    let mut r = request(apply());
    r.target.device.key = "razer:026b:MOUSE-1".into();
    r.target.device.kind = "keyboard".into();
    r.target.device.via = "usb".into();
    r.target.generation = 2;
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    r.target.generation = 3;
    let mut other = hid.devices[0].clone();
    other.interface = 0;
    other.container = Some("ANOTHER-SYNTHETIC-CONTAINER".into());
    hid.devices.push(other);
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    hid.devices.pop();
    hid.state.change_on_read.store(true, Ordering::Relaxed);
    let result = run(&r, &hid, false);
    assert!(result.failure.is_some());
    assert!(result.observation.is_none());
    assert!(!result.may_have_changed);
    assert_eq!(
        hid.state
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|p| p[8])
            .collect::<Vec<_>>(),
        [0xc0]
    );
}

#[test]
fn keyboard_controller_rejects_bluetooth_path_and_non_usb_descriptor_before_open() {
    let mut r = request(apply());
    r.target.device.key = "razer:026b:MOUSE-1".into();
    r.target.device.kind = "keyboard".into();
    r.target.device.via = "usb".into();
    let mut hid = transport();
    hid.devices[0].product_id = 0x026b;
    hid.devices[0].interface = 3;
    hid.devices[0].path = "synthetic-bluetooth-vid&1532".into();
    assert!(run(&r, &hid, false).failure.is_some());
    assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    assert!(hid.state.sent.lock().unwrap().is_empty());
    hid.devices[0].path = "synthetic-usb".into();
    for via in ["bluetooth", "", "unknown"] {
        r.target.device.via = via.into();
        assert!(run(&r, &hid, false).failure.is_some());
        assert_eq!(hid.state.opens.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn wireless_deathadder_v4_reports_user_verified_support() {
    let mut hid = transport();
    hid.devices[0].product_id = 0x00bf;
    let mut request = request(ControlAction::Read);
    request.target.device.key = "razer:00bf:MOUSE-1".into();
    let result = run(&request, &hid, false);
    assert!(result.failure.is_none());
    assert!(
        result
            .observation
            .unwrap()
            .evidence
            .contains("Hardware verified (user tested)")
    );
}
