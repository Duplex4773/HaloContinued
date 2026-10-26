use hb_core::*;
use hb_providers::device_info::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct TestClock;
impl Clock for TestClock {
    fn unix(&self) -> i64 {
        1234
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) {}
}
struct Shared {
    reports: Mutex<Vec<Vec<u8>>>,
    reply: Mutex<Vec<u8>>,
    opens: AtomicUsize,
    generation: AtomicU64,
    arrival: AtomicU64,
    reconnect_on_open: AtomicBool,
    change_on_get: AtomicBool,
    fail_open: AtomicBool,
}
struct Transport {
    shared: Arc<Shared>,
    devices: Vec<HidInfo>,
}
struct Session(Arc<Shared>);
impl HidSession for Session {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("Information must not send output reports")
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        panic!("Information must not read input reports")
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        assert!(
            is_firmware_query(data),
            "Only the firmware GET is permitted"
        );
        self.0.reports.lock().unwrap().push(data.to_vec());
        Ok(())
    }
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        assert_eq!((id, length), (0, 91));
        if self.0.change_on_get.load(Ordering::Relaxed) {
            self.0.generation.fetch_add(1, Ordering::Relaxed);
        }
        Ok(self.0.reply.lock().unwrap().clone())
    }
}
impl HidTransport for Transport {
    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }
    fn connection_stamp(&self, _: &HidInfo) -> Option<ConnectionStamp> {
        ConnectionStamp::new(self.shared.arrival.load(Ordering::Relaxed))
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        assert_eq!(vendor, 0x1532);
        Ok(self.devices.clone())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        self.shared.opens.fetch_add(1, Ordering::Relaxed);
        if self.shared.fail_open.load(Ordering::Relaxed) {
            return Err(ProviderError::new("device busy"));
        }
        if self.shared.reconnect_on_open.load(Ordering::Relaxed) {
            self.shared.arrival.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Box::new(Session(self.shared.clone())))
    }
}
fn response() -> Vec<u8> {
    let mut report = firmware_query();
    report[1] = 2;
    report[9] = 1;
    report[10] = 21;
    report[89] = report[3..89].iter().fold(0, |a, b| a ^ b);
    report.to_vec()
}
fn fixture() -> (Transport, DeviceInfoRequest) {
    let info = HidInfo {
        path: "test#device#collection".into(),
        vendor_id: 0x1532,
        product_id: 0x00bf,
        interface: 0,
        feature_length: Some(91),
        serial: "TESTUNIT".into(),
        container: Some("CONTAINER-A".into()),
        ..Default::default()
    };
    let mut reading = Reading::new("razer:00bf:TESTUNIT", "Mouse", "razer", 0);
    reading.kind = "mouse".into();
    reading.via = "usb".into();
    reading.serial = Some(info.serial.clone());
    reading.container = info.container.clone();
    let request = DeviceInfoRequest {
        request: 1,
        target: ControlTarget {
            device: ConfigurationDevice::from_reading(&reading),
            generation: 1,
            connection_stamp: None,
        },
    };
    let shared = Arc::new(Shared {
        reports: Mutex::new(Vec::new()),
        reply: Mutex::new(response()),
        opens: AtomicUsize::new(0),
        generation: AtomicU64::new(1),
        arrival: AtomicU64::new(1),
        reconnect_on_open: AtomicBool::new(false),
        change_on_get: AtomicBool::new(false),
        fail_open: AtomicBool::new(false),
    });
    (
        Transport {
            shared,
            devices: vec![info],
        },
        request,
    )
}
fn run(hid: &Transport, request: &DeviceInfoRequest, cancel: bool) -> DeviceInfoOutcome {
    execute(
        request,
        hid,
        &PollContext {
            clock: &TestClock,
            cancelled: &AtomicBool::new(cancel),
            deadline: Duration::from_secs(2),
            playstation_full_mode: false,
        },
    )
}
#[test]
fn query_is_one_read_only_exchange_and_formats_actual_version() {
    let (hid, request) = fixture();
    let result = run(&hid, &request, false);
    assert!(result.failure.is_none());
    assert_eq!(result.firmware.unwrap().to_string(), "1.21");
    assert_eq!(result.checked_at, 1234);
    assert_eq!(
        hid.shared.reports.lock().unwrap().as_slice(),
        [firmware_query().to_vec()]
    );
    assert_eq!(FirmwareVersion { major: 1, minor: 4 }.to_string(), "1.04");
}
#[test]
fn malformed_late_checksum_and_unknown_replies_are_not_versions() {
    for edit in 0..8 {
        let mut report = response();
        match edit {
            0 => {
                report.pop();
            }
            1 => report[0] = 1,
            2 => report[2] = 0xff,
            3 => report[7] = 7,
            4 => report[8] = 0x80,
            5 => report[6] = 1,
            6 => report[89] ^= 1,
            _ => {
                report[9] = 0;
                report[10] = 0;
                report[89] = report[3..89].iter().fold(0, |a, b| a ^ b);
            }
        }
        assert!(parse_firmware(&report).is_err(), "edit {edit}");
    }
    for status in [0, 1, 3, 4, 5, 255] {
        let mut report = response();
        report[1] = status;
        assert!(parse_firmware(&report).is_err());
    }
}
#[test]
fn cancellation_reconnect_and_generation_changes_reject_the_read() {
    for scenario in 0..4 {
        let (hid, mut request) = fixture();
        if scenario == 1 {
            request.target.generation = 0;
        }
        if scenario == 2 {
            hid.shared.reconnect_on_open.store(true, Ordering::Relaxed);
        }
        if scenario == 3 {
            hid.shared.change_on_get.store(true, Ordering::Relaxed);
        }
        let result = run(&hid, &request, scenario == 0);
        assert!(result.firmware.is_none());
        assert!(result.failure.is_some());
        assert_eq!(
            hid.shared.reports.lock().unwrap().len(),
            usize::from(scenario == 3)
        );
    }
}
#[test]
fn unknown_models_other_vendors_and_identity_changes_send_nothing() {
    for scenario in 0..6 {
        let (mut hid, mut request) = fixture();
        match scenario {
            0 => request.target.device.key = "razer:ffff:TESTUNIT".into(),
            1 => request.target.device.source = "corsair".into(),
            2 => request.target.device.serial = Some("OTHER".into()),
            3 => request.target.device.container = Some("OTHER".into()),
            4 => hid.devices[0].feature_length = Some(90),
            _ => request.target.device.connection = Connection::Sleeping,
        }
        assert!(run(&hid, &request, false).firmware.is_none());
        assert!(hid.shared.reports.lock().unwrap().is_empty());
    }
}
#[test]
fn ambiguous_collections_and_duplicate_physical_identities_are_not_opened() {
    for separate in [false, true] {
        let (mut hid, request) = fixture();
        let mut duplicate = hid.devices[0].clone();
        if separate {
            duplicate.container = Some("CONTAINER-B".into());
            duplicate.interface = 1;
        }
        hid.devices.push(duplicate);
        assert!(run(&hid, &request, false).firmware.is_none());
        assert_eq!(hid.shared.opens.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn busy_device_is_a_recoverable_error_with_no_retry() {
    let (hid, request) = fixture();
    hid.shared.fail_open.store(true, Ordering::Relaxed);
    let result = run(&hid, &request, false);
    assert!(result.failure.is_some());
    assert_eq!(hid.shared.opens.load(Ordering::Relaxed), 1);
    assert!(hid.shared.reports.lock().unwrap().is_empty());
}
