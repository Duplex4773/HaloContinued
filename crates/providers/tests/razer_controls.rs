use hb_core::{Clock, HidInfo, HidSession, PollContext, ProviderError};
use hb_providers::razer_controls::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn unix(&self) -> i64 {
        0
    }
    fn monotonic(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
    fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }
}
struct Session {
    replies: std::collections::VecDeque<Vec<u8>>,
    sent: Mutex<Vec<Vec<u8>>>,
}
impl Session {
    fn new(replies: Vec<Vec<u8>>) -> Self {
        Self {
            replies: replies.into(),
            sent: Mutex::new(vec![]),
        }
    }
}
impl HidSession for Session {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("output forbidden")
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        panic!("input forbidden")
    }
    fn send_feature(&mut self, d: &[u8]) -> Result<(), ProviderError> {
        self.sent.lock().unwrap().push(d.to_vec());
        Ok(())
    }
    fn feature(&mut self, id: u8, n: usize) -> Result<Vec<u8>, ProviderError> {
        assert_eq!((id, n), (0, 91));
        self.replies
            .pop_front()
            .ok_or_else(|| ProviderError::new("empty fake"))
    }
}
fn reply(cmd: u8, status: u8, a0: u8, a1: u8) -> Vec<u8> {
    let mut r = vec![0; 91];
    r[1] = status;
    r[2] = 0x99;
    r[6] = 1;
    r[8] = cmd;
    r[9] = a0;
    r[10] = a1;
    r
}
fn context<'a>(clock: &'a TestClock, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: Duration::from_secs(1),
        playstation_full_mode: false,
    }
}
#[test]
fn conditional_rate_change_refuses_external_changes_before_any_set() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut session = Session::new(vec![reply(0xc0, 2, 0, 2)]); // now 4000 Hz
    let result = execute_rate_checked(
        &mut session,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(1000),
        Some(2000),
    );
    assert_eq!(result.failure, Some(ProtocolFailure::VerificationMismatch));
    assert!(!result.may_have_changed);
    let sent = session.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0][8], 0xc0);
}
#[test]
fn known_collection_only() {
    let i = HidInfo {
        vendor_id: 0x1532,
        product_id: 0x00be,
        interface: 0,
        feature_length: Some(91),
        ..Default::default()
    };
    assert_eq!(protocol(&i), Some(Protocol::Extended));
    for wrong in [
        HidInfo {
            vendor_id: 1,
            ..i.clone()
        },
        HidInfo {
            product_id: 0x00b3,
            ..i.clone()
        },
        HidInfo {
            interface: 1,
            ..i.clone()
        },
        HidInfo {
            feature_length: Some(90),
            ..i.clone()
        },
    ] {
        assert_eq!(protocol(&wrong), None);
    }
}
#[test]
fn dedicated_high_rate_receivers_have_exact_allowlist_and_settle() {
    for pid in [0x009f, 0x00c1] {
        let info = HidInfo {
            vendor_id: 0x1532,
            product_id: pid,
            interface: 0,
            feature_length: Some(91),
            ..Default::default()
        };
        let p = protocol(&info).unwrap();
        assert_eq!(p, Protocol::ExtendedWireless);
        assert_eq!(p.rates(), EXTENDED_RATES);
        for hz in p.rates() {
            let clock = TestClock::default();
            let cancel = AtomicBool::new(false);
            let code = (8000 / hz) as u8;
            let mut s = Session::new(vec![
                reply(0xc0, 2, 0, if *hz == 1000 { 1 } else { 8 }),
                reply(0x40, 2, 0, code),
                reply(0x40, 2, 1, code),
                reply(0xc0, 2, 0, code),
            ]);
            let result = execute_rate(&mut s, &context(&clock, &cancel), p, Some(*hz));
            assert_eq!(result.failure, None);
            assert_eq!(result.observed_hz, Some(*hz));
            let sent = s.sent.lock().unwrap();
            assert_eq!(sent.len(), 4);
            assert!(sent.iter().all(|r| r[2] == 0x1f));
            assert_eq!(
                sent[1],
                request(Protocol::Extended, Some((*hz, 0))).unwrap()
            );
            assert_eq!(
                sent[2],
                request(Protocol::Extended, Some((*hz, 1))).unwrap()
            );
            assert_eq!(clock.monotonic(), Duration::from_millis(240));
        }
        for wrong in [
            HidInfo {
                interface: 1,
                ..info.clone()
            },
            HidInfo {
                feature_length: None,
                ..info.clone()
            },
            HidInfo {
                feature_length: Some(90),
                ..info.clone()
            },
        ] {
            assert_eq!(protocol(&wrong), None);
        }
    }
    // Stock receivers, wired counterparts and pairable accessories do not
    // inherit the dedicated receiver's ceiling from their marketing name.
    for pid in [
        0x009e, 0x00c0, 0x00b3, 0x00a4, 0x00a5, 0x00a6, 0x00aa, 0x00ab, 0x00af, 0x00b0, 0x00b8,
        0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00cc, 0x00cd, 0x00d6, 0x00d7,
    ] {
        assert_eq!(
            protocol(&HidInfo {
                vendor_id: 0x1532,
                product_id: pid,
                interface: 0,
                feature_length: Some(91),
                ..Default::default()
            }),
            None
        );
    }
}

#[test]
fn dedicated_receiver_settle_respects_deadline_before_write() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut c = context(&clock, &cancel);
    c.deadline = Duration::from_millis(40);
    let mut s = Session::new(vec![reply(0xc0, 2, 0, 8)]);
    let result = execute_rate(&mut s, &c, Protocol::ExtendedWireless, Some(8000));
    assert_eq!(result.failure, Some(ProtocolFailure::CancelledOrDeadline));
    assert!(!result.may_have_changed);
    assert_eq!(s.sent.lock().unwrap().len(), 1);
    assert_eq!(s.replies.len(), 1);
}
#[test]
fn golden_extended_reports_and_strict_rate_codes() {
    let get = request(Protocol::Extended, None).unwrap();
    assert_eq!(&get[..11], &[0, 0, 0x1f, 0, 0, 0, 1, 0, 0xc0, 0, 0]);
    assert_eq!(get[89], 0xc1);
    for (hz, code) in [
        (125, 64),
        (500, 16),
        (1000, 8),
        (2000, 4),
        (4000, 2),
        (8000, 1),
    ] {
        for step in [0, 1] {
            let r = request(Protocol::Extended, Some((hz, step))).unwrap();
            assert_eq!(&r[6..11], &[2, 0, 0x40, step, code]);
            assert_eq!(r[89], 2 ^ 0x40 ^ step ^ code);
        }
    }
    for hz in [0, 250, 999, 16000] {
        assert!(request(Protocol::Extended, Some((hz, 0))).is_none());
    }
}
#[test]
fn extended_get_decodes_argument_one_once() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![reply(0xc0, 2, 1, 4)]);
    assert_eq!(
        read_rate(&mut s, &context(&clock, &cancel), Protocol::Extended),
        Ok(2000)
    );
    assert_eq!(s.sent.lock().unwrap().len(), 1);
}
#[test]
fn extended_set_get_before_two_steps_then_verified_get() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x40, 2, 0, 1),
        reply(0x40, 2, 1, 1),
        reply(0xc0, 2, 0, 1),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(8000),
    );
    assert_eq!(
        r,
        PollingResult {
            previous_hz: Some(1000),
            observed_hz: Some(8000),
            may_have_changed: true,
            failure: None
        }
    );
    let sent = s.sent.lock().unwrap();
    assert_eq!(
        sent.iter().map(|r| r[8]).collect::<Vec<_>>(),
        [0xc0, 0x40, 0x40, 0xc0]
    );
    assert_eq!((sent[1][9], sent[2][9]), (0, 1));
}
#[test]
fn busy_is_failure_even_when_partial_write_readback_matches() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x40, 1, 0, 1),
        reply(0xc0, 2, 0, 1),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(8000),
    );
    assert_eq!(r.failure, Some(ProtocolFailure::Busy));
    assert_eq!(r.observed_hz, Some(8000));
    assert!(r.may_have_changed);
    assert_eq!(s.sent.lock().unwrap().len(), 3);
}
#[test]
fn unchanged_mismatch_unknown_and_cancelled() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![reply(0xc0, 2, 0, 8)]);
    assert!(
        !execute_rate(
            &mut s,
            &context(&clock, &cancel),
            Protocol::Extended,
            Some(1000)
        )
        .may_have_changed
    );
    let mut s = Session::new(vec![reply(0xc0, 2, 0, 32)]);
    assert_eq!(
        read_rate(&mut s, &context(&clock, &cancel), Protocol::Extended),
        Err(ProtocolFailure::UnknownRate(32))
    );
    let mut s = Session::new(vec![
        reply(0x85, 2, 1, 0),
        reply(0x05, 2, 8, 0),
        reply(0x85, 2, 1, 0),
    ]);
    assert_eq!(
        execute_rate(
            &mut s,
            &context(&clock, &cancel),
            Protocol::Legacy,
            Some(125)
        )
        .failure,
        Some(ProtocolFailure::VerificationMismatch)
    );
    cancel.store(true, Ordering::Relaxed);
    let mut s = Session::new(vec![]);
    assert_eq!(
        read_rate(&mut s, &context(&clock, &cancel), Protocol::Extended),
        Err(ProtocolFailure::CancelledOrDeadline)
    );
    assert!(s.sent.lock().unwrap().is_empty());
}
#[test]
fn malformed_or_deadline_does_not_write_rate() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    for raw in [
        vec![],
        vec![0; 90],
        reply(0x85, 2, 0, 8),
        reply(0xc0, 5, 0, 8),
    ] {
        let mut s = Session::new(vec![raw]);
        let r = execute_rate(
            &mut s,
            &context(&clock, &cancel),
            Protocol::Extended,
            Some(8000),
        );
        assert!(r.failure.is_some());
        assert!(!r.may_have_changed);
    }
    let mut s = Session::new(vec![]);
    let mut c = context(&clock, &cancel);
    c.deadline = clock.monotonic() + Duration::from_millis(10);
    let r = execute_rate(&mut s, &c, Protocol::Extended, Some(8000));
    assert_eq!(r.failure, Some(ProtocolFailure::CancelledOrDeadline));
    assert!(!r.may_have_changed);
}

#[test]
fn malformed_structure_and_all_non_success_statuses() {
    for (offset, value) in [(0, 1), (3, 1), (4, 1), (6, 0), (6, 81), (7, 7), (8, 0x85)] {
        let clock = TestClock::default();
        let cancel = AtomicBool::new(false);
        let mut raw = reply(0xc0, 2, 0, 8);
        raw[offset] = value;
        let mut s = Session::new(vec![raw]);
        assert_eq!(
            read_rate(&mut s, &context(&clock, &cancel), Protocol::Extended),
            Err(ProtocolFailure::InvalidReply)
        );
    }
    for status in [0, 1, 3, 4, 5, 255] {
        let clock = TestClock::default();
        let cancel = AtomicBool::new(false);
        let mut s = Session::new(vec![reply(0xc0, status, 0, 8)]);
        assert!(read_rate(&mut s, &context(&clock, &cancel), Protocol::Extended).is_err());
    }
}

#[test]
fn second_step_failure_retains_fresh_observation_and_failure() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x40, 2, 0, 1),
        reply(0x40, 3, 1, 1),
        reply(0xc0, 2, 0, 8),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(8000),
    );
    assert_eq!(r.failure, Some(ProtocolFailure::DeviceStatus(3)));
    assert_eq!(r.observed_hz, Some(1000));
    assert_eq!(r.previous_hz, Some(1000));
    assert!(r.may_have_changed);
}

#[test]
fn missing_verification_clears_observation_after_write() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x40, 2, 0, 1),
        reply(0x40, 2, 1, 1),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(8000),
    );
    assert!(matches!(r.failure, Some(ProtocolFailure::Transport(_))));
    assert_eq!(r.observed_hz, None);
    assert_eq!(r.previous_hz, Some(1000));
    assert!(r.may_have_changed);
}

#[test]
fn foreign_write_ack_is_failure_even_if_verification_matches() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x85, 2, 0, 1),
        reply(0xc0, 2, 0, 1),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Extended,
        Some(8000),
    );
    assert_eq!(r.failure, Some(ProtocolFailure::InvalidReply));
    assert_eq!(r.previous_hz, Some(1000));
    assert_eq!(r.observed_hz, Some(8000));
    assert_eq!(s.sent.lock().unwrap().len(), 3);
}

#[test]
fn legacy_reports_and_single_write_verify() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0x85, 2, 1, 0),
        reply(0x05, 2, 8, 0),
        reply(0x85, 2, 8, 0),
    ]);
    let r = execute_rate(
        &mut s,
        &context(&clock, &cancel),
        Protocol::Legacy,
        Some(125),
    );
    assert_eq!(r.failure, None);
    assert_eq!(r.previous_hz, Some(1000));
    assert_eq!(r.observed_hz, Some(125));
    let sent = s.sent.lock().unwrap();
    assert_eq!(
        sent.iter().map(|r| r[8]).collect::<Vec<_>>(),
        [0x85, 0x05, 0x85]
    );
    assert_eq!(&sent[1][6..11], &[1, 0, 5, 8, 0]);
    assert_eq!(sent[1][89], 1 ^ 5 ^ 8);
}

#[test]
fn deadline_during_second_set_does_not_read_late_reply() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut s = Session::new(vec![
        reply(0xc0, 2, 0, 8),
        reply(0x40, 2, 0, 1),
        reply(0x40, 2, 1, 1),
    ]);
    let mut c = context(&clock, &cancel);
    c.deadline = Duration::from_millis(70);
    let r = execute_rate(&mut s, &c, Protocol::Extended, Some(8000));
    assert_eq!(r.failure, Some(ProtocolFailure::CancelledOrDeadline));
    assert_eq!(r.observed_hz, None);
    assert!(r.may_have_changed);
    assert_eq!(s.replies.len(), 1);
    assert_eq!(s.sent.lock().unwrap().len(), 3);
}

#[test]
fn unsupported_rate_is_rejected_before_any_io() {
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    for (protocol, hz) in [(Protocol::Extended, 250), (Protocol::Legacy, 2000)] {
        let mut s = Session::new(vec![]);
        let r = execute_rate(&mut s, &context(&clock, &cancel), protocol, Some(hz));
        assert_eq!(r.failure, Some(ProtocolFailure::Unsupported));
        assert!(s.sent.lock().unwrap().is_empty());
        assert!(!r.may_have_changed);
    }
}

#[test]
fn cancellation_after_first_set_stops_second_set_and_verification() {
    struct CancelClock<'a> {
        time: AtomicU64,
        sleeps: AtomicU64,
        cancel: &'a AtomicBool,
    }
    impl Clock for CancelClock<'_> {
        fn unix(&self) -> i64 {
            0
        }
        fn monotonic(&self) -> Duration {
            Duration::from_millis(self.time.load(Ordering::Relaxed))
        }
        fn sleep(&self, d: Duration) {
            self.time.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
            if self.sleeps.fetch_add(1, Ordering::Relaxed) == 1 {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    for protocol in [Protocol::Extended, Protocol::KeyboardExtended] {
        let cancel = AtomicBool::new(false);
        let clock = CancelClock {
            time: AtomicU64::new(0),
            sleeps: AtomicU64::new(0),
            cancel: &cancel,
        };
        let c = PollContext {
            clock: &clock,
            cancelled: &cancel,
            deadline: Duration::from_secs(1),
            playstation_full_mode: false,
        };
        let mut s = Session::new(vec![reply(0xc0, 2, 0, 8), reply(0x40, 2, 0, 1)]);
        let r = execute_rate(&mut s, &c, protocol, Some(8000));
        assert_eq!(r.failure, Some(ProtocolFailure::CancelledOrDeadline));
        assert_eq!(r.observed_hz, None);
        assert!(r.may_have_changed);
        assert_eq!(s.sent.lock().unwrap().len(), 2);
        assert_eq!(s.replies.len(), 1);
    }
}

#[test]
fn keyboard_all_rates_have_exact_single_set_and_one_millisecond_settle() {
    for pid in [0x026b, 0x026c, 0x0287, 0x028d, 0x02a5] {
        let info = HidInfo {
            vendor_id: 0x1532,
            product_id: pid,
            interface: 3,
            feature_length: Some(91),
            ..Default::default()
        };
        let p = protocol(&info).unwrap();
        assert_eq!(p, Protocol::KeyboardExtended);
        assert_eq!(p.rates(), &[125, 250, 500, 1000, 2000, 4000, 8000]);
        for &hz in p.rates() {
            let clock = TestClock::default();
            let cancel = AtomicBool::new(false);
            let previous = if hz == 1000 { 8000 } else { 1000 };
            let mut session = Session::new(vec![
                reply(0xc0, 2, 0, (8000 / previous) as u8),
                reply(0x40, 2, 0, (8000 / hz) as u8),
                reply(0xc0, 2, 0, (8000 / hz) as u8),
            ]);
            let result = execute_rate(&mut session, &context(&clock, &cancel), p, Some(hz));
            assert_eq!(result.failure, None);
            assert_eq!(result.previous_hz, Some(previous));
            assert_eq!(result.observed_hz, Some(hz));
            let sent = session.sent.lock().unwrap();
            assert_eq!(
                sent.iter().map(|r| r[8]).collect::<Vec<_>>(),
                [0xc0, 0x40, 0xc0]
            );
            let mut golden = vec![0; 91];
            golden[2] = 0x1f;
            golden[6] = 2;
            golden[8] = 0x40;
            golden[10] = (8000 / hz) as u8;
            golden[89] = golden[3..89].iter().fold(0, |a, b| a ^ b);
            assert_eq!(sent[1], golden);
            assert_eq!(clock.monotonic(), Duration::from_millis(3));
            assert!(request(p, Some((hz, 1))).is_none());
        }
        for interface in [0, 1, 2, 4] {
            assert_eq!(
                protocol(&HidInfo {
                    interface,
                    ..info.clone()
                }),
                None
            );
        }
        for feature_length in [None, Some(90), Some(92)] {
            assert_eq!(
                protocol(&HidInfo {
                    feature_length,
                    ..info.clone()
                }),
                None
            );
        }
    }
}
#[test]
fn keyboard_busy_malformed_verification_and_late_reply_never_repeat_set() {
    let p = Protocol::KeyboardExtended;
    for acknowledgement in [reply(0x40, 1, 0, 1), reply(0xc0, 2, 0, 1), vec![0; 90]] {
        let clock = TestClock::default();
        let cancel = AtomicBool::new(false);
        let mut session = Session::new(vec![
            reply(0xc0, 2, 0, 8),
            acknowledgement,
            reply(0xc0, 2, 0, 1),
        ]);
        let result = execute_rate(&mut session, &context(&clock, &cancel), p, Some(8000));
        assert!(result.failure.is_some());
        assert!(result.may_have_changed);
        assert_eq!(result.previous_hz, Some(1000));
        assert_eq!(result.observed_hz, Some(8000));
        assert_eq!(
            session
                .sent
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r[8] == 0x40)
                .count(),
            1
        );
    }
    for last in [reply(0xc0, 2, 0, 8), vec![0; 90]] {
        let clock = TestClock::default();
        let cancel = AtomicBool::new(false);
        let mut session = Session::new(vec![reply(0xc0, 2, 0, 8), reply(0x40, 2, 0, 1), last]);
        let result = execute_rate(&mut session, &context(&clock, &cancel), p, Some(8000));
        assert!(result.failure.is_some());
        assert!(result.may_have_changed);
        assert_eq!(result.previous_hz, Some(1000));
    }
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let mut ctx = context(&clock, &cancel);
    ctx.deadline = Duration::from_millis(2);
    let mut session = Session::new(vec![reply(0xc0, 2, 0, 8), reply(0x40, 2, 0, 1)]);
    let result = execute_rate(&mut session, &ctx, p, Some(8000));
    assert_eq!(result.failure, Some(ProtocolFailure::CancelledOrDeadline));
    assert!(result.observed_hz.is_none());
    assert_eq!(session.sent.lock().unwrap().len(), 2);
    cancel.store(true, Ordering::Relaxed);
    let mut session = Session::new(vec![]);
    let result = execute_rate(&mut session, &context(&clock, &cancel), p, Some(250));
    assert_eq!(result.failure, Some(ProtocolFailure::CancelledOrDeadline));
    assert!(session.sent.lock().unwrap().is_empty());
}
