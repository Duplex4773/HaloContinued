mod common;
use common::*;
use hb_core::*;
use hb_providers::controls::HidDeviceController;
use std::sync::atomic::AtomicBool;

fn receiver() -> HidInfo {
    HidInfo {
        path: "synthetic-receiver".into(),
        vendor_id: 0x3837,
        product_id: 0x100b,
        usage_page: 0xff01,
        usage: 1,
        serial: "Synthetic-1".into(),
        container: Some("synthetic-container".into()),
        feature_length: Some(65),
        ..Default::default()
    }
}
fn request() -> ControlRequest {
    let mut reading = Reading::new("mchose:3837:SYNTHETIC-1", "Renamed device", "mchose", 0);
    reading.kind = "mouse".into();
    reading.serial = Some("Synthetic-1".into());
    reading.container = Some("synthetic-container".into());
    ControlRequest {
        request: 1,
        target: ControlTarget {
            device: ConfigurationDevice::from_reading(&reading),
            generation: 0,
        },
        action: ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
    }
}
fn refused_without_open(info: Vec<HidInfo>, request: &ControlRequest, cancelled: bool) {
    let hid = FakeHid::new(info, vec![]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(cancelled);
    let result = HidDeviceController.execute(request, &hid, &context(&clock, &cancel));
    assert!(result.failure.is_some());
    assert!(!result.may_have_changed);
    assert!(hid.opened.lock().unwrap().is_empty());
    hid.done();
}

#[test]
fn exact_mchose_receiver_route_rejects_ambiguous_or_changed_identity() {
    let request = request();
    refused_without_open(vec![receiver(), receiver()], &request, false);
    for changed in [
        HidInfo {
            product_id: 0x1014,
            ..receiver()
        },
        HidInfo {
            vendor_id: 0x5253,
            ..receiver()
        },
        HidInfo {
            usage_page: 0xff0b,
            usage: 0x104,
            ..receiver()
        },
        HidInfo {
            feature_length: Some(21),
            ..receiver()
        },
        HidInfo {
            serial: "Replacement".into(),
            ..receiver()
        },
        HidInfo {
            container: Some("replacement-container".into()),
            ..receiver()
        },
    ] {
        refused_without_open(vec![changed], &request, false);
    }
}

#[test]
fn mchose_session_requires_online_mouse_current_epoch_and_permission() {
    let base = request();
    refused_without_open(vec![receiver()], &base, true);
    for changed in 0..4 {
        let mut request = base.clone();
        match changed {
            0 => request.target.generation = 1,
            1 => request.target.device.connection = Connection::Sleeping,
            2 => request.target.device.kind = "keyboard".into(),
            _ => request.target.device.key = "mchose:3837:other".into(),
        }
        refused_without_open(vec![receiver()], &request, false);
    }
}

fn command(id: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0; 65];
    packet[0] = id;
    packet[1..=if id == 0x11 { 20 } else { 64 }].fill(0xff);
    packet[1] = !command;
    for (byte, value) in packet[2..].iter_mut().zip(payload) {
        *byte = !value;
    }
    packet
}
fn configuration() -> Vec<u8> {
    let mut bytes = vec![0; 63];
    bytes[1] = 0x22;
    bytes[2] = 0x22;
    bytes[16] = 6;
    bytes[18] = 4;
    for pair in bytes[4..16].as_chunks_mut::<2>().0 {
        pair.copy_from_slice(&800u16.to_le_bytes());
    }
    for (index, byte) in bytes[20..].iter_mut().enumerate() {
        *byte = index as u8;
    }
    bytes
}
fn validated_reads(configuration: &[u8]) -> Vec<Step> {
    let mut steps = vec![];
    for (id, cmd, payload) in [
        (0x11, 3, vec![1, 0x37, 0x38, 0x0b, 0x10, 1, 0]),
        (0x11, 6, vec![0x37, 0x38, 0x21, 0x40, 5, 46, 2, 4, 9, 41, 0]),
        (0x12, 0x67, configuration.to_vec()),
    ] {
        for _ in 0..2 {
            steps.push(Step::Send(command(id, cmd, &[])));
            let mut reply = command(id, cmd, &payload);
            if id == 0x11 {
                reply.truncate(21);
            }
            steps.push(Step::Feature(id, 65, Ok(reply)));
        }
    }
    steps
}
#[test]
fn selected_receiver_read_exposes_only_verified_reference_rates() {
    let mut request = request();
    request.action = ControlAction::Read;
    let hid = FakeHid::new(vec![receiver()], validated_reads(&configuration()));
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let result = HidDeviceController.execute(&request, &hid, &context(&clock, &cancel));
    assert_eq!(result.failure, None);
    assert!(!result.may_have_changed);
    assert_eq!(result.previous.unwrap().hz(), 1000);
    let observation = result.observation.unwrap();
    assert_eq!(observation.rate.unwrap().hz(), 1000);
    assert_eq!(observation.target.generation, request.target.generation);
    assert_eq!(observation.target.device.key, request.target.device.key);
    assert_eq!(
        observation.target.device.serial,
        request.target.device.serial
    );
    assert_eq!(
        observation.target.device.container,
        request.target.device.container
    );
    assert_eq!(
        observation
            .supported
            .iter()
            .map(|rate| rate.hz())
            .collect::<Vec<_>>(),
        vec![125, 500, 1000, 2000, 4000, 8000]
    );
    assert!(observation.evidence.contains("hardware unverified locally"));
    assert_eq!(*hid.opened.lock().unwrap(), vec!["synthetic-receiver"]);
    hid.done();
}
#[test]
fn selected_receiver_apply_preserves_blob_and_verifies_observation() {
    let before = configuration();
    let mut after = before.clone();
    after[2] = 0x52;
    let mut steps = validated_reads(&before);
    steps.extend(validated_reads(&before));
    steps.push(Step::Send(command(0x12, 0x57, &after)));
    steps.extend(validated_reads(&after));
    let hid = FakeHid::new(vec![receiver()], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let request = request();
    let result = HidDeviceController.execute(&request, &hid, &context(&clock, &cancel));
    assert_eq!(result.failure, None);
    assert!(result.may_have_changed);
    assert_eq!(result.previous.unwrap().hz(), 1000);
    let observation = result.observation.unwrap();
    assert_eq!(observation.rate.unwrap().hz(), 8000);
    assert_eq!(observation.target.generation, request.target.generation);
    assert_eq!(observation.target.device.key, request.target.device.key);
    assert_eq!(
        observation.target.device.serial,
        request.target.device.serial
    );
    assert_eq!(
        observation.target.device.container,
        request.target.device.container
    );
    assert_eq!(*hid.opened.lock().unwrap(), vec!["synthetic-receiver"]);
    hid.done();
}

struct ChangingTransport {
    inner: FakeHid,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
struct ChangingSession {
    inner: Box<dyn HidSession>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl HidTransport for ChangingTransport {
    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        self.inner.enumerate(vendor)
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        Ok(Box::new(ChangingSession {
            inner: self.inner.open(info)?,
            generation: self.generation.clone(),
        }))
    }
}
impl HidSession for ChangingSession {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("output forbidden")
    }
    fn read(&mut self, _: usize, _: std::time::Duration) -> Result<Vec<u8>, ProviderError> {
        panic!("input forbidden")
    }
    fn send_feature(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        self.inner.send_feature(bytes)?;
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        panic!("epoch change must stop before next hardware operation")
    }
}
#[test]
fn epoch_change_during_feature_call_stops_open_session_before_apply() {
    let hid = ChangingTransport {
        inner: FakeHid::new(vec![receiver()], vec![Step::Send(command(0x11, 3, &[]))]),
        generation: Default::default(),
    };
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let result = HidDeviceController.execute(&request(), &hid, &context(&clock, &cancel));
    assert!(
        result
            .failure
            .unwrap()
            .contains("device connection changed")
    );
    assert!(result.observation.is_none());
    assert_eq!(result.previous, None);
    assert!(!result.may_have_changed);
    assert_eq!(
        *hid.inner.opened.lock().unwrap(),
        vec!["synthetic-receiver"]
    );
    hid.inner.done();
}
