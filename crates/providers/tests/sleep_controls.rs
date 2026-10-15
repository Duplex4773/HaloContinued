use hb_core::*;
use hb_providers::sleep_controls::*;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct ClockFake(AtomicU64);
impl Clock for ClockFake {
    fn unix(&self) -> i64 {
        0
    }
    fn monotonic(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::Relaxed))
    }
    fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct State {
    sent: Vec<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    fail_send: bool,
    reconnect_on_set: bool,
    arrival: u64,
    reconnect_on_open: bool,
}
struct Transport {
    infos: Vec<HidInfo>,
    state: Arc<Mutex<State>>,
    generation: Arc<AtomicU64>,
}
struct Session {
    state: Arc<Mutex<State>>,
    generation: Arc<AtomicU64>,
}
impl HidTransport for Transport {
    fn connection_stamp(&self, _: &HidInfo) -> Option<ConnectionStamp> {
        ConnectionStamp::new(self.state.lock().unwrap().arrival)
    }
    fn enumerate(&self, v: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .infos
            .iter()
            .filter(|i| i.vendor_id == v)
            .cloned()
            .collect())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        let mut state = self.state.lock().unwrap();
        if state.reconnect_on_open {
            state.arrival += 1;
        }
        Ok(Box::new(Session {
            state: self.state.clone(),
            generation: self.generation.clone(),
        }))
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}
impl HidSession for Session {
    fn write(&mut self, r: &[u8]) -> Result<(), ProviderError> {
        self.state.lock().unwrap().sent.push(r.to_vec());
        Ok(())
    }
    fn read(&mut self, _: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        if timeout.is_zero() {
            return Ok(vec![]);
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .replies
            .pop_front()
            .unwrap_or_else(|| vec![0; 64]))
    }
    fn send_feature(&mut self, r: &[u8]) -> Result<(), ProviderError> {
        let mut s = self.state.lock().unwrap();
        s.sent.push(r.to_vec());
        if r[8] == 3 && s.reconnect_on_set {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        if s.fail_send {
            Err(ProviderError::new("disconnected"))
        } else {
            Ok(())
        }
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        self.state
            .lock()
            .unwrap()
            .replies
            .pop_front()
            .ok_or_else(|| ProviderError::new("No reply"))
    }
}
fn reply(seconds: u16, set: bool) -> Vec<u8> {
    let mut r = razer_packet(0x1f, set.then_some(seconds)).to_vec();
    r[1] = 2;
    r[9..11].copy_from_slice(&seconds.to_be_bytes());
    r
}
fn fixture(pid: u16) -> Transport {
    Transport {
        infos: vec![HidInfo {
            path: "test-mouse-control".into(),
            vendor_id: 0x1532,
            product_id: pid,
            interface: 0,
            feature_length: Some(91),
            serial: "TEST-DEVICE".into(),
            ..Default::default()
        }],
        state: Arc::new(Mutex::new(State {
            arrival: 77,
            ..Default::default()
        })),
        generation: Arc::new(AtomicU64::new(1)),
    }
}
fn context<'a>(clock: &'a ClockFake, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: Duration::from_secs(12),
        playstation_full_mode: false,
    }
}
fn request(t: &Transport, c: &PollContext<'_>, seconds: Option<u16>) -> SleepRequest {
    SleepRequest {
        request: 1,
        target: ControlTarget {
            device: discover(t, c).unwrap().remove(0),
            generation: 1,
            connection_stamp: None,
        },
        seconds,
        expected: None,
    }
}
#[test]
fn sleep_restore_uses_connection_proof_independently_of_refresh_generation() {
    for arrival in [77, 78, 0] {
        let t = fixture(0x00bf);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        t.state.lock().unwrap().replies.push_back(reply(300, false));
        let mut r = request(&t, &c, None);
        let baseline = execute(&r, &t, &c);
        assert!(baseline.failure.is_none());
        assert_eq!(baseline.connection_stamp, ConnectionStamp::new(77));
        t.generation.store(2, Ordering::Relaxed);
        {
            let mut state = t.state.lock().unwrap();
            state.arrival = arrival;
            state.sent.clear();
            state.replies = vec![reply(60, false), reply(300, true), reply(300, false)].into();
        }
        r.target.generation = 2;
        r.target.connection_stamp = baseline.connection_stamp;
        r.seconds = Some(300);
        r.expected = Some(60);
        let out = execute(&r, &t, &c);
        assert_eq!(out.failure.is_none(), arrival == 77);
        assert_eq!(out.may_have_changed, arrival == 77);
        if arrival != 77 {
            assert!(t.state.lock().unwrap().sent.is_empty());
        }
    }
}
#[test]
fn sleep_reconnect_during_open_sends_no_reports() {
    let t = fixture(0x00bf);
    let clock = ClockFake::default();
    let cancel = AtomicBool::new(false);
    let c = context(&clock, &cancel);
    t.state.lock().unwrap().reconnect_on_open = true;
    let r = request(&t, &c, Some(60));
    let out = execute(&r, &t, &c);
    assert!(out.failure.is_some());
    assert!(!out.may_have_changed);
    assert!(t.state.lock().unwrap().sent.is_empty());
}
#[test]
fn every_razer_model_uses_reviewed_transaction_id_and_every_minute_roundtrips() {
    for m in MODELS
        .iter()
        .filter(|m| matches!(m.wire, Wire::Razer { .. }))
    {
        for seconds in (60..=900).step_by(60) {
            let t = fixture(m.pid);
            let clock = ClockFake::default();
            let cancel = AtomicBool::new(false);
            let c = context(&clock, &cancel);
            let r = request(&t, &c, Some(seconds));
            let before = if seconds == 300 { 600 } else { 300 };
            t.state.lock().unwrap().replies = vec![
                reply(before, false),
                reply(seconds, true),
                reply(seconds, false),
            ]
            .into();
            let o = execute(&r, &t, &c);
            assert!(o.failure.is_none(), "{:?}", o.failure);
            assert_eq!(o.seconds, Some(seconds));
            let state = t.state.lock().unwrap();
            assert_eq!(state.sent.len(), 3);
            assert_eq!(state.sent.iter().filter(|r| r[8] == 3).count(), 1);
            let Wire::Razer { tid, wait_us } = m.wire else {
                unreachable!()
            };
            assert!(state.sent.iter().all(|r| r[2] == tid
                && r[7] == 7
                && r[89] == r[3..89].iter().fold(0, |a, b| a ^ b)));
            assert_eq!(clock.monotonic(), Duration::from_micros(wait_us * 3));
        }
    }
}
#[test]
fn same_value_skips_set_and_unknown_duration_does_not_write() {
    for before in [300, 0, 901] {
        let t = fixture(0xbf);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let r = request(&t, &c, Some(300));
        t.state
            .lock()
            .unwrap()
            .replies
            .push_back(reply(before, false));
        let o = execute(&r, &t, &c);
        assert_eq!(o.failure.is_none(), before == 300);
        assert!(!o.may_have_changed);
        assert_eq!(t.state.lock().unwrap().sent.len(), 1);
    }
}
#[test]
fn malformed_busy_unsupported_late_and_mismatched_responses_never_retry_set() {
    for kind in 0..7 {
        let t = fixture(0xbf);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let r = request(&t, &c, Some(600));
        let mut ack = reply(600, true);
        match kind {
            0 => ack.truncate(20),
            1 => ack[1] = 1,
            2 => ack[1] = 5,
            3 => ack[8] = 0x83,
            4 => ack[7] = 0,
            5 => ack[6] = 1,
            _ => {}
        }
        t.state.lock().unwrap().replies = vec![reply(300, false), ack, reply(300, false)].into();
        let o = execute(&r, &t, &c);
        assert!(o.failure.is_some());
        assert!(o.may_have_changed);
        assert!(o.seconds.is_none());
        assert_eq!(
            t.state
                .lock()
                .unwrap()
                .sent
                .iter()
                .filter(|r| r[8] == 3)
                .count(),
            1
        );
    }
}
#[test]
fn cancellation_reconnect_identity_and_ambiguity_reject_before_commands() {
    for kind in 0..7 {
        let mut t = fixture(0xbf);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let mut r = request(&t, &c, Some(300));
        match kind {
            0 => cancel.store(true, Ordering::Relaxed),
            1 => r.target.generation = 0,
            2 => t.infos.push(t.infos[0].clone()),
            3 => r.target.device.serial = Some("OTHER".into()),
            4 => t.infos[0].interface = 1,
            5 => t.infos[0].feature_length = Some(90),
            _ => r.seconds = Some(59),
        }
        assert!(execute(&r, &t, &c).failure.is_some());
        assert!(t.state.lock().unwrap().sent.is_empty());
    }
}
#[test]
fn disconnect_after_set_preserves_uncertainty() {
    let t = fixture(0xbf);
    let clock = ClockFake::default();
    let cancel = AtomicBool::new(false);
    let c = context(&clock, &cancel);
    let r = request(&t, &c, Some(600));
    {
        let mut s = t.state.lock().unwrap();
        s.replies.push_back(reply(300, false));
        s.reconnect_on_set = true;
    }
    let o = execute(&r, &t, &c);
    assert!(o.failure.is_some());
    assert!(o.may_have_changed);
    assert!(o.seconds.is_none());
    assert_eq!(t.state.lock().unwrap().sent.len(), 2);
}
#[test]
fn steelseries_packets_have_correct_units_and_are_never_claimed_verified() {
    for m in MODELS.iter().filter(|m| m.source == "steelseries") {
        let mut t = fixture(m.pid);
        t.infos[0].vendor_id = m.vendor;
        t.infos[0].interface = m.interface;
        t.infos[0].usage_page = 0xff00;
        t.infos[0].output_length = Some(65);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        for seconds in (m.minimum..=m.maximum).step_by(60) {
            let r = request(&t, &c, Some(seconds));
            let o = execute(&r, &t, &c);
            assert!(o.failure.is_none(), "{:?}", o.failure);
            assert!(o.seconds.is_none());
            assert!(o.may_have_changed);
            let s = t.state.lock().unwrap();
            let p = s.sent.last().unwrap();
            assert_eq!(p.len(), 65);
            match m.wire {
                Wire::Rival650 => {
                    assert_eq!(&p[1..7], &[0x2b, 1, 1, 0, 0, 0]);
                    assert_eq!(u16::from_le_bytes([p[7], p[8]]), seconds);
                }
                Wire::SteelSeries { wireless } => {
                    assert_eq!(p[1], if wireless { 0x69 } else { 0x29 });
                    assert_eq!(
                        u32::from_le_bytes([p[2], p[3], p[4], 0]),
                        u32::from(seconds) * 1000
                    );
                }
                _ => unreachable!(),
            }
        }
        let count = t.state.lock().unwrap().sent.len();
        let o = execute(&request(&t, &c, None), &t, &c);
        assert!(o.failure.is_some());
        assert_eq!(t.state.lock().unwrap().sent.len(), count);
    }
}
#[test]
fn unsupported_brands_bluetooth_and_unknown_models_send_nothing() {
    let mut t = fixture(0xffff);
    let clock = ClockFake::default();
    let cancel = AtomicBool::new(false);
    let c = context(&clock, &cancel);
    assert!(discover(&t, &c).unwrap().is_empty());
    t.infos[0].product_id = 0xbf;
    t.infos[0].path = "hid#vid&1532".into();
    assert!(discover(&t, &c).unwrap().is_empty());
    t.infos[0].path = "test-mouse-control".into();
    let mut r = request(&t, &c, Some(300));
    for vendor in ["logitech", "corsair", "pulsar", "unknown"] {
        r.target.device.source = vendor.into();
        assert!(execute(&r, &t, &c).failure.is_some());
    }
    assert!(t.state.lock().unwrap().sent.is_empty());
}

fn asus_reply(command: [u8; 2], code: u8, threshold: u8) -> Vec<u8> {
    let mut r = vec![0; 65];
    r[1..3].copy_from_slice(&command);
    r[5] = 70;
    r[6] = code;
    r[7] = threshold;
    r
}
#[test]
fn asus_changes_only_sleep_preserves_low_warning_and_verifies_after_save() {
    for m in MODELS.iter().filter(|m| matches!(m.wire, Wire::Asus)) {
        for (seconds, code) in [(0, 255), (60, 0), (120, 1), (180, 2), (300, 3), (600, 4)] {
            let mut t = fixture(m.pid);
            t.infos[0].vendor_id = m.vendor;
            t.infos[0].usage_page = 0xff00;
            t.infos[0].output_length = Some(65);
            let clock = ClockFake::default();
            let cancel = AtomicBool::new(false);
            let c = context(&clock, &cancel);
            let r = request(&t, &c, Some(seconds));
            let before = if code == 3 { 4 } else { 3 };
            t.state.lock().unwrap().replies = vec![
                asus_reply([0x12, 7], before, 20),
                asus_reply([0x51, 0x37], 0, 0),
                asus_reply([0x50, 3], 0, 0),
                asus_reply([0x12, 7], code, 20),
            ]
            .into();
            let o = execute(&r, &t, &c);
            assert!(o.failure.is_none(), "{:?}", o.failure);
            assert_eq!(o.seconds, Some(seconds));
            let state = t.state.lock().unwrap();
            assert_eq!(state.sent.len(), 4);
            assert_eq!(&state.sent[1][..8], &[0, 0x51, 0x37, 0, 0, code, 0, 20]);
            assert_eq!(&state.sent[2][..3], &[0, 0x50, 3]);
        }
    }
}
#[test]
fn asus_invalid_before_value_and_same_value_never_send_a_set() {
    for code in [3, 6] {
        let mut t = fixture(0x1a72);
        t.infos[0].vendor_id = 0xb05;
        t.infos[0].usage_page = 0xff00;
        t.infos[0].output_length = Some(65);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let r = request(&t, &c, Some(300));
        t.state
            .lock()
            .unwrap()
            .replies
            .push_back(asus_reply([0x12, 7], code, 20));
        let o = execute(&r, &t, &c);
        assert_eq!(o.failure.is_none(), code == 3);
        assert!(!o.may_have_changed);
        assert_eq!(t.state.lock().unwrap().sent.len(), 1);
    }
}
#[test]
fn asus_rejects_unsupported_minutes_and_mismatched_verification_without_retrying() {
    for bad in [true, false] {
        let mut t = fixture(0x1a72);
        t.infos[0].vendor_id = 0xb05;
        t.infos[0].usage_page = 0xff00;
        t.infos[0].output_length = Some(65);
        let clock = ClockFake::default();
        let cancel = AtomicBool::new(false);
        let c = context(&clock, &cancel);
        let r = request(&t, &c, Some(if bad { 240 } else { 600 }));
        t.state.lock().unwrap().replies = vec![
            asus_reply([0x12, 7], 3, 20),
            asus_reply([0x51, 0x37], 0, 0),
            asus_reply([0x50, 3], 0, 0),
            asus_reply([0x12, 7], 4, 30),
        ]
        .into();
        let o = execute(&r, &t, &c);
        assert!(o.failure.is_some());
        assert_eq!(o.may_have_changed, !bad);
        assert!(o.seconds.is_none());
        assert_eq!(
            t.state
                .lock()
                .unwrap()
                .sent
                .iter()
                .filter(|r| r[1] == 0x51)
                .count(),
            usize::from(!bad)
        );
    }
}

#[test]
fn conditional_sleep_change_refuses_external_changes_without_set() {
    let t = fixture(0x00bf);
    let clock = ClockFake::default();
    let cancel = AtomicBool::new(false);
    let c = context(&clock, &cancel);
    t.state.lock().unwrap().replies.push_back(reply(600, false));
    let mut r = request(&t, &c, Some(60));
    r.expected = Some(300);
    let o = execute(&r, &t, &c);
    assert!(o.failure.is_some());
    assert!(!o.may_have_changed);
    assert_eq!(t.state.lock().unwrap().sent.len(), 1);
}

#[test]
fn asus_conditional_sleep_change_preserves_external_value() {
    let mut t = fixture(0x1a72);
    t.infos[0].vendor_id = 0xb05;
    t.infos[0].usage_page = 0xff00;
    t.infos[0].output_length = Some(65);
    let clock = ClockFake::default();
    let cancel = AtomicBool::new(false);
    let c = context(&clock, &cancel);
    t.state
        .lock()
        .unwrap()
        .replies
        .push_back(asus_reply([0x12, 7], 4, 20));
    let mut r = request(&t, &c, Some(60));
    r.expected = Some(300);
    let o = execute(&r, &t, &c);
    assert!(o.failure.is_some());
    assert!(!o.may_have_changed);
    assert_eq!(t.state.lock().unwrap().sent.len(), 1);
}
