use hb_core::*;
use hb_providers::dpi;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn unix(&self) -> i64 {
        1000
    }
    fn monotonic(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::Relaxed))
    }
    fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
    }
}
fn crc(bytes: &[u8]) -> u16 {
    let mut v = 0xffffu16;
    for b in bytes {
        v ^= u16::from(*b) << 8;
        for _ in 0..8 {
            v = if v & 0x8000 == 0 {
                v << 1
            } else {
                (v << 1) ^ 0x1021
            }
        }
    }
    v
}
fn seal(bytes: &mut [u8]) {
    let n = bytes.len();
    let v = crc(&bytes[..n - 2]);
    bytes[n - 2..].copy_from_slice(&v.to_be_bytes());
}
struct State {
    value: DpiValue,
    sent: Vec<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    last: Vec<u8>,
    status: u8,
    bad_frame: bool,
    mismatch: bool,
    fail_set: bool,
    reconnect_set: bool,
    logitech: bool,
    extended: bool,
    onboard: bool,
    format: u8,
    raw: Vec<u8>,
    writing: Vec<u8>,
    stage: u8,
    unit: [u8; 4],
    model: u16,
    usb: Option<u16>,
    stale: bool,
    fail_function: Option<u8>,
}
impl Default for State {
    fn default() -> Self {
        let mut raw = vec![0x45; 256];
        raw[1] = 1;
        raw[19..23].copy_from_slice(&[0x20, 3, 0xb0, 4]);
        raw[23] = 2;
        seal(&mut raw);
        Self {
            value: DpiValue { x: 800, y: 1200 },
            sent: vec![],
            replies: VecDeque::new(),
            last: vec![],
            status: 2,
            bad_frame: false,
            mismatch: false,
            fail_set: false,
            reconnect_set: false,
            logitech: false,
            extended: true,
            onboard: false,
            format: 7,
            raw,
            writing: vec![],
            stage: 3,
            unit: [1, 2, 3, 4],
            model: 0x40b8,
            usb: Some(0xc0a0),
            stale: false,
            fail_function: None,
        }
    }
}
struct Transport {
    infos: Vec<HidInfo>,
    state: Arc<Mutex<State>>,
    epoch: Arc<AtomicU64>,
}
struct Session {
    state: Arc<Mutex<State>>,
    epoch: Arc<AtomicU64>,
}
impl HidTransport for Transport {
    fn enumerate(&self, v: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .infos
            .iter()
            .filter(|i| i.vendor_id == v)
            .cloned()
            .collect())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        Ok(Box::new(Session {
            state: self.state.clone(),
            epoch: self.epoch.clone(),
        }))
    }
    fn generation(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }
}
impl HidSession for Session {
    fn send_feature(&mut self, p: &[u8]) -> Result<(), ProviderError> {
        let mut s = self.state.lock().unwrap();
        s.sent.push(p.to_vec());
        s.last = p.to_vec();
        if p[8] == 5 {
            if s.reconnect_set {
                self.epoch.fetch_add(1, Ordering::Relaxed);
            }
            if s.fail_set {
                return Err(ProviderError::new("Lost SET acknowledgement"));
            }
            if !s.mismatch {
                s.value = DpiValue {
                    x: u16::from_be_bytes([p[10], p[11]]),
                    y: u16::from_be_bytes([p[12], p[13]]),
                };
            }
        }
        Ok(())
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        let s = self.state.lock().unwrap();
        let mut r = s.last.clone();
        r[1] = s.status;
        if r[8] == 0x85 {
            r[10..12].copy_from_slice(&s.value.x.to_be_bytes());
            r[12..14].copy_from_slice(&s.value.y.to_be_bytes());
        }
        if r[8] == 0x86 {
            r[10] = 1;
            r[11] = 5;
        }
        if s.bad_frame {
            r.truncate(20);
        }
        Ok(r)
    }
    fn write(&mut self, p: &[u8]) -> Result<(), ProviderError> {
        let mut s = self.state.lock().unwrap();
        s.sent.push(p.to_vec());
        let (feature, function) = (p[2], p[3] >> 4);
        let mut r = vec![0; 20];
        r[..4].copy_from_slice(&p[..4]);
        if p[1] != 1 {
            r[2] = 0xff;
            r[3] = feature;
            r[4] = p[3];
            r[5] = 8;
            s.replies.push_back(r);
            return Ok(());
        }
        let params = &p[4..];
        let data = &mut r[4..];
        let setter = (feature == 2 && matches!(function, 3 | 6))
            || (feature == 3 && matches!(function, 6..=8 | 12));
        if setter && s.reconnect_set {
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
        if setter && s.fail_set {
            return Err(ProviderError::new("Lost SET acknowledgement"));
        }
        if feature == 3 && s.fail_function == Some(function) {
            return Err(ProviderError::new("Interrupted profile exchange"));
        }
        match (feature, function) {
            (0, 0) => {
                data[0] = match u16::from_be_bytes([params[0], params[1]]) {
                    3 => 1,
                    0x8100 => 3,
                    0x2202 if s.extended => 2,
                    0x2201 if !s.extended => 2,
                    _ => 0,
                }
            }
            (1, 0) => {
                data[1..5].copy_from_slice(&s.unit);
                data[6] = if s.usb.is_some() { 12 } else { 4 };
                data[7..9].copy_from_slice(&s.model.to_be_bytes());
                if let Some(usb) = s.usb {
                    data[9..11].copy_from_slice(&usb.to_be_bytes());
                }
            }
            (2, 0) => data[0] = 1,
            (2, 1) if s.extended => data[2] = 3,
            (2, 2) if s.extended => {
                data[..3].copy_from_slice(&params[..3]);
                data[3..11].copy_from_slice(&[0, 100, 0xe0, 50, 0x27, 0x10, 0, 0]);
            }
            (2, 1) => data[1..9].copy_from_slice(&[0, 100, 0xe0, 50, 0x27, 0x10, 0, 0]),
            (2, 2 | 5) => {
                data[1..3].copy_from_slice(&s.value.x.to_be_bytes());
                if s.extended {
                    data[5..7].copy_from_slice(&s.value.y.to_be_bytes());
                    data[9] = 2;
                }
            }
            (2, 3 | 6) => {
                if !s.mismatch {
                    s.value = DpiValue {
                        x: u16::from_be_bytes([params[1], params[2]]),
                        y: if s.extended {
                            u16::from_be_bytes([params[3], params[4]])
                        } else {
                            u16::from_be_bytes([params[1], params[2]])
                        },
                    };
                }
            }
            (3, 0) => {
                data[0] = 1;
                data[1] = s.format;
                data[3] = 3;
                data[6] = 4;
                data[7] = 1;
            }
            (3, 2) => data[0] = if s.onboard { 1 } else { 2 },
            (3, 4) => data[1] = 2,
            (3, 11) => data[0] = s.stage,
            (3, 5) => {
                let offset = usize::from(u16::from_be_bytes([params[2], params[3]]));
                data.copy_from_slice(&s.raw[offset..offset + 16]);
            }
            (3, 6) => s.writing.clear(),
            (3, 7) => s.writing.extend_from_slice(params),
            (3, 8) => {
                s.raw = s.writing.clone();
            }
            (3, 12) => {
                if !s.mismatch {
                    assert_eq!(params[0], s.stage);
                    let offset = if s.format == 7 {
                        4 + usize::from(s.stage) * 5
                    } else {
                        3 + usize::from(s.stage) * 2
                    };
                    let x = u16::from_le_bytes([s.raw[offset], s.raw[offset + 1]]);
                    let y = if s.format == 7 {
                        u16::from_le_bytes([s.raw[offset + 2], s.raw[offset + 3]])
                    } else {
                        x
                    };
                    s.value = DpiValue { x, y };
                }
            }
            _ => panic!("unexpected DPI command: {feature}:{function}"),
        }
        if s.bad_frame {
            r.truncate(6);
        }
        if s.stale {
            let mut stale = r.clone();
            stale[3] ^= 0x01;
            s.replies.push_back(stale);
        }
        s.replies.push_back(r);
        Ok(())
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .replies
            .pop_front()
            .unwrap_or_default())
    }
}
fn fixture(logitech: bool) -> Transport {
    Transport {
        infos: vec![HidInfo {
            path: "synthetic-control".into(),
            vendor_id: if logitech { 0x046d } else { 0x1532 },
            product_id: if logitech { 0xc54d } else { 0x00bf },
            interface: 0,
            usage_page: 0xff00,
            usage: 2,
            serial: "TEST-UNIT".into(),
            feature_length: Some(91),
            ..Default::default()
        }],
        state: Arc::new(Mutex::new(State {
            logitech,
            ..State::default()
        })),
        epoch: Arc::new(AtomicU64::new(1)),
    }
}
fn context<'a>(clock: &'a FakeClock, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: Duration::from_secs(20),
        playstation_full_mode: false,
    }
}
fn request(t: &Transport) -> DpiRequest {
    let logitech = t.state.lock().unwrap().logitech;
    DpiRequest {
        request: 1,
        target: ControlTarget {
            generation: 1,
            device: ConfigurationDevice {
                key: if logitech {
                    "logitech:01020304".into()
                } else {
                    format!("razer:{:04x}:TEST-UNIT", t.infos[0].product_id)
                },
                name: "Synthetic mouse".into(),
                kind: "mouse".into(),
                source: if logitech { "logitech" } else { "razer" }.into(),
                via: "usb".into(),
                serial: Some(if logitech { "01020304" } else { "TEST-UNIT" }.into()),
                container: None,
                connection: Connection::Online,
                capability: PollingCapability::Unavailable("Polling unrelated".into()),
            },
        },
        value: None,
        expected: None,
    }
}
fn read(t: &Transport, c: &PollContext<'_>) -> DpiRequest {
    let mut r = request(t);
    let o = dpi::execute(&r, t, c, &mut |_| panic!("read must not save a backup"));
    assert!(o.failure.is_none(), "{:?}", o.failure);
    let limits = o
        .limits
        .as_ref()
        .expect("a successful read must provide DPI limits");
    if t.state.lock().unwrap().logitech {
        assert!(!limits.from_model);
        assert_eq!(limits.x, vec![(100, 100, 1), (100, 10000, 50)]);
        assert_eq!(limits.x, limits.y);
    } else {
        assert!(limits.from_model);
        assert_eq!(
            limits.x,
            vec![(100, dpi::razer_maximum(t.infos[0].product_id).unwrap(), 1)]
        );
    }
    r.expected = o.current;
    r
}
fn setters(s: &State) -> usize {
    s.sent
        .iter()
        .filter(|p| {
            if s.logitech {
                p[2] == 2 && matches!(p[3] >> 4, 3 | 6)
                    || p[2] == 3 && matches!(p[3] >> 4, 6..=8 | 12)
            } else {
                p[8] == 5
            }
        })
        .count()
}

#[test]
fn razer_all_allowlisted_models_read_backup_apply_and_restore_both_axes() {
    for pid in [
        0x0094, 0x00a5, 0x00a6, 0x00a7, 0x00a8, 0x00aa, 0x00ab, 0x00af, 0x00b0, 0x00b6, 0x00b7,
        0x00be, 0x00bf, 0x00c0, 0x00c1, 0x00c2, 0x00c3, 0x00cc, 0x00cd,
    ] {
        let mut t = fixture(false);
        t.infos[0].product_id = pid;
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        let original = r.expected.clone().unwrap();
        r.value = Some(DpiValue::uniform(1600));
        let mut backups = vec![];
        let o = dpi::execute(&r, &t, &c, &mut |b| {
            assert_eq!(setters(&t.state.lock().unwrap()), 0);
            backups.push(b.clone());
            Ok(())
        });
        assert!(o.failure.is_none(), "{pid:04x}: {:?}", o.failure);
        assert_eq!(backups[0].original, original);
        assert_eq!(backups[0].profile_bytes.len(), 38);
        assert_eq!(setters(&t.state.lock().unwrap()), 1);
        r.expected = o.current;
        r.value = Some(original.value);
        let o = dpi::execute(&r, &t, &c, &mut |_| Ok(()));
        assert!(o.failure.is_none(), "{:?}", o.failure);
        assert_eq!(o.current.unwrap(), original);
    }
}
#[test]
fn invalid_original_backup_failure_and_same_value_send_no_sets() {
    for logitech in [false, true] {
        for reason in 0..5 {
            let t = fixture(logitech);
            let clock = FakeClock::default();
            let cancel = AtomicBool::new(false);
            let c = context(&clock, &cancel);
            let mut r = read(&t, &c);
            r.value = Some(DpiValue::uniform(1600));
            match reason {
                0 => r.expected = None,
                1 => r.expected.as_mut().unwrap().value.x = 900,
                2 => r.value = Some(r.expected.as_ref().unwrap().value),
                3 => {}
                _ => {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            let o = dpi::execute(&r, &t, &c, &mut |_| {
                Err(ProviderError::new("Backup cannot be saved"))
            });
            assert_eq!(setters(&t.state.lock().unwrap()), 0);
            assert!(!o.may_have_changed);
            assert_eq!(o.failure.is_none(), reason == 2);
        }
    }
}
#[test]
fn razer_busy_malformed_unsupported_and_reconnect_never_retry() {
    for reason in 0..6 {
        let t = fixture(false);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        r.value = Some(DpiValue::uniform(1600));
        {
            let mut s = t.state.lock().unwrap();
            match reason {
                0 => s.status = 1,
                1 => s.status = 5,
                2 => s.bad_frame = true,
                3 => s.fail_set = true,
                4 => s.reconnect_set = true,
                _ => s.mismatch = true,
            }
        }
        let o = dpi::execute(&r, &t, &c, &mut |_| Ok(()));
        assert!(o.failure.is_some());
        assert!(o.current.is_none());
        assert!(setters(&t.state.lock().unwrap()) <= 1);
        assert_eq!(o.may_have_changed, reason >= 3);
    }
}
#[test]
fn changed_identity_ambiguous_collection_or_generation_is_refused() {
    for reason in 0..4 {
        let mut t = fixture(false);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        r.value = Some(DpiValue::uniform(1600));
        match reason {
            0 => t.infos.push(t.infos[0].clone()),
            1 => t.epoch.store(2, Ordering::Relaxed),
            2 => t.infos[0].feature_length = Some(90),
            _ => r.target.device.serial = Some("REPLACED".into()),
        }
        let o = dpi::execute(&r, &t, &c, &mut |_| panic!("must refuse before backup"));
        assert!(o.failure.is_some());
        assert_eq!(setters(&t.state.lock().unwrap()), 0);
    }
}
#[test]
fn logitech_live_and_onboard_keep_other_profile_settings_and_restore_original() {
    for classic in [false, true] {
        for onboard in [false, true] {
            let t = fixture(true);
            {
                let mut s = t.state.lock().unwrap();
                s.onboard = onboard;
                if classic {
                    s.extended = false;
                    s.model = 0x4093;
                    s.usb = Some(0xc094);
                    s.format = 3;
                    s.value = DpiValue::uniform(800);
                    s.raw[9..11].copy_from_slice(&800u16.to_le_bytes());
                    seal(&mut s.raw);
                }
            }
            let original_bytes = t.state.lock().unwrap().raw.clone();
            let clock = FakeClock::default();
            let cancel = AtomicBool::new(false);
            let c = context(&clock, &cancel);
            let mut r = read(&t, &c);
            let original = r.expected.clone().unwrap();
            r.value = Some(DpiValue::uniform(1600));
            let mut backups = vec![];
            let o = dpi::execute(&r, &t, &c, &mut |b| {
                assert_eq!(setters(&t.state.lock().unwrap()), 0);
                backups.push(b.clone());
                Ok(())
            });
            assert!(o.failure.is_none(), "{classic} {onboard}: {:?}", o.failure);
            assert_eq!(o.current.as_ref().unwrap().value, DpiValue::uniform(1600));
            assert_eq!(backups[0].original, original);
            if onboard {
                assert_eq!(backups[0].profile_bytes, original_bytes);
                let current = t.state.lock().unwrap().raw.clone();
                let offset = if classic { 9 } else { 19 };
                for n in 0..254 {
                    if !(offset..offset + if classic { 2 } else { 4 }).contains(&n) {
                        assert_eq!(current[n], original_bytes[n], "unrelated byte {n}");
                    }
                }
            }
            r.expected = o.current;
            r.value = Some(original.value);
            let o = dpi::execute(&r, &t, &c, &mut |_| Ok(()));
            assert!(o.failure.is_none(), "{:?}", o.failure);
            assert_eq!(o.current.unwrap(), original);
            if onboard {
                assert_eq!(t.state.lock().unwrap().raw, original_bytes);
            }
        }
    }
}
#[test]
fn logitech_range_late_reports_profile_errors_and_uncertain_sets() {
    for reason in 0..8 {
        let t = fixture(true);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        r.value = Some(DpiValue::uniform(1600));
        {
            let mut s = t.state.lock().unwrap();
            match reason {
                0 => s.stale = true,
                1 => r.value = Some(DpiValue::uniform(1633)),
                2 => {
                    s.onboard = true;
                    s.format = 8
                }
                3 => {
                    s.onboard = true;
                    s.raw[200] ^= 1
                }
                4 => s.unit = [5, 6, 7, 8],
                5 => s.fail_set = true,
                6 => s.mismatch = true,
                _ => s.reconnect_set = true,
            }
        }
        let o = dpi::execute(&r, &t, &c, &mut |_| Ok(()));
        assert_eq!(
            o.failure.is_none(),
            reason == 0,
            "{reason}: {:?}",
            o.failure
        );
        if reason != 0 {
            assert!(o.current.is_none());
        }
        assert!(setters(&t.state.lock().unwrap()) <= 1);
    }
}
#[test]
fn cancellation_or_external_change_after_backup_blocks_writes() {
    for logitech in [false, true] {
        for cancel_after in [false, true] {
            let t = fixture(logitech);
            let clock = FakeClock::default();
            let cancel = AtomicBool::new(false);
            let c = context(&clock, &cancel);
            let mut r = read(&t, &c);
            r.value = Some(DpiValue::uniform(1600));
            let o = dpi::execute(&r, &t, &c, &mut |_| {
                if cancel_after {
                    cancel.store(true, Ordering::Relaxed)
                } else {
                    t.state.lock().unwrap().value = DpiValue::uniform(2000)
                }
                Ok(())
            });
            assert!(o.failure.is_some());
            assert!(!o.may_have_changed);
            assert_eq!(setters(&t.state.lock().unwrap()), 0);
        }
    }
}
#[test]
fn razer_packet_encodes_xy_and_checksum_without_stage_write() {
    let p = dpi::razer_packet(5, Some(DpiValue { x: 800, y: 1200 }));
    assert_eq!(&p[6..16], &[7, 4, 5, 1, 3, 32, 4, 176, 0, 0]);
    assert_eq!(p[2], 0x1f);
    assert_eq!(p[89], p[3..89].iter().fold(0, |a, b| a ^ b));
}

#[test]
fn interrupted_onboard_writes_stop_without_retry_and_keep_original_backup() {
    for function in [6, 7, 8, 12] {
        let t = fixture(true);
        t.state.lock().unwrap().onboard = true;
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        let original = t.state.lock().unwrap().raw.clone();
        r.value = Some(DpiValue::uniform(1600));
        t.state.lock().unwrap().fail_function = Some(function);
        let mut backups = vec![];
        let o = dpi::execute(&r, &t, &c, &mut |b| {
            backups.push(b.clone());
            Ok(())
        });
        assert!(o.failure.is_some());
        assert!(o.current.is_none());
        assert!(o.may_have_changed);
        assert_eq!(o.previous, r.expected);
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].profile_bytes, original);
        let s = t.state.lock().unwrap();
        assert_eq!(
            s.sent
                .iter()
                .filter(|p| p[2] == 3 && p[3] >> 4 == function)
                .count(),
            1
        );
        if function < 12 {
            assert_eq!(s.raw, original);
        }
    }
}

#[test]
fn concurrent_profile_changes_and_unknown_models_never_write() {
    for reason in 0..4 {
        let mut t = fixture(true);
        t.state.lock().unwrap().onboard = true;
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = read(&t, &c);
        r.value = Some(DpiValue::uniform(1600));
        if reason == 2 {
            t.state.lock().unwrap().model = 0x1234;
        }
        if reason == 3 {
            t.infos.push(t.infos[0].clone());
        }
        let o = dpi::execute(&r, &t, &c, &mut |_| {
            let mut s = t.state.lock().unwrap();
            if reason == 0 {
                s.stage = 2;
            }
            if reason == 1 {
                s.raw[200] ^= 1;
                seal(&mut s.raw);
            }
            Ok(())
        });
        assert!(o.failure.is_some());
        assert!(!o.may_have_changed);
        assert_eq!(setters(&t.state.lock().unwrap()), 0);
    }
}
