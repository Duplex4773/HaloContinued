use hb_core::{Clock, HidInfo, HidSession, PollContext, ProviderError};
use hb_providers::mchose_controls::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
#[derive(Default)]
struct Session {
    config: Vec<u8>,
    sent: Vec<Vec<u8>>,
    pending: (u8, u8),
    config_reads: usize,
    short21: bool,
    late: bool,
    malformed: bool,
    wrong_id: bool,
    profile_after: bool,
    firmware_wrong: bool,
    identity_changes: bool,
    mouse_reads: usize,
    config_before: bool,
    profile_before: bool,
    tail_after: bool,
    unstable: bool,
    reject_set: bool,
    ignore_set: bool,
}
impl Session {
    fn new() -> Self {
        let mut config = vec![0; 63];
        config[1] = 0x22;
        config[2] = 0x22;
        config[16] = 6;
        config[18] = 4;
        for pair in config[4..16].as_chunks_mut::<2>().0 {
            pair.copy_from_slice(&800u16.to_le_bytes());
        }
        for (i, b) in config[20..].iter_mut().enumerate() {
            *b = i as u8;
        }
        Self {
            config,
            ..Default::default()
        }
    }
    fn sets(&self) -> Vec<&Vec<u8>> {
        self.sent
            .iter()
            .filter(|b| b[0] == 0x12 && b[1] == !0x57)
            .collect()
    }
}
impl HidSession for Session {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("output forbidden")
    }
    fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, ProviderError> {
        panic!("input forbidden")
    }
    fn send_feature(&mut self, b: &[u8]) -> Result<(), ProviderError> {
        assert_eq!(b.len(), 65);
        self.sent.push(b.to_vec());
        self.pending = (b[0], !b[1]);
        if self.pending.1 == 0x57 {
            if !self.ignore_set {
                self.config = b[2..].iter().map(|v| !v).collect();
            }
            if self.reject_set {
                return Err(ProviderError::new("uncertain write"));
            }
        }
        Ok(())
    }
    fn feature(&mut self, id: u8, n: usize) -> Result<Vec<u8>, ProviderError> {
        assert_eq!((id, n), (self.pending.0, 65));
        let cmd = self.pending.1;
        let mut payload = match cmd {
            3 => vec![1, 0x37, 0x38, 0x0b, 0x10, 1, 0],
            6 => vec![0x37, 0x38, 0x21, 0x40, 5, 46, 2, 4, 9, 41, 0],
            0x67 => {
                self.config_reads += 1;
                self.config.clone()
            }
            _ => panic!("unexpected query"),
        };
        if cmd == 6 {
            self.mouse_reads += 1;
        }
        if cmd == 6 && (self.firmware_wrong || (self.identity_changes && self.mouse_reads > 2)) {
            payload[4] = 6;
        }
        if cmd == 0x67 {
            if self.config_before && self.config_reads > 2 {
                payload[30] ^= 1;
            }
            if self.profile_before && self.config_reads > 2 {
                payload[0] = 1;
            }
            if self.profile_after && self.config_reads > 4 {
                payload[0] = 1;
            }
            if self.tail_after && self.config_reads > 4 {
                payload[30] ^= 1;
            }
            if self.unstable {
                payload[30] = self.config_reads as u8;
            }
        }
        let mut raw = vec![0; if id == 0x11 && self.short21 { 21 } else { 65 }];
        raw[0] = id;
        raw[1] = !cmd;
        for (b, v) in raw[2..].iter_mut().zip(payload) {
            *b = !v;
        }
        if self.late {
            self.late = false;
            raw[1] = !0x77;
        }
        if self.wrong_id {
            raw[0] ^= 1;
        }
        if self.malformed {
            raw.pop();
        }
        Ok(raw)
    }
}
fn info() -> HidInfo {
    HidInfo {
        vendor_id: 0x3837,
        product_id: 0x100b,
        usage_page: 0xff01,
        usage: 1,
        feature_length: Some(65),
        ..Default::default()
    }
}
fn run(s: &mut Session, hz: Option<u32>, deadline: u64, cancel: bool) -> PollingResult {
    let clock = TestClock::default();
    let cancelled = AtomicBool::new(cancel);
    let context = PollContext {
        clock: &clock,
        cancelled: &cancelled,
        deadline: Duration::from_millis(deadline),
        playstation_full_mode: false,
    };
    execute_rate(s, &context, &info(), hz)
}
#[test]
fn conditional_restore_refuses_external_rate_changes_without_set() {
    let mut s = Session::new();
    let clock = TestClock::default();
    let cancel = AtomicBool::new(false);
    let context = PollContext {
        clock: &clock,
        cancelled: &cancel,
        deadline: Duration::from_secs(10),
        playstation_full_mode: false,
    };
    let result = execute_rate_checked(&mut s, &context, &info(), Some(1000), Some(8000));
    assert_eq!(result.failure, Some(ProtocolFailure::ConfigurationChanged));
    assert!(!result.may_have_changed);
    assert!(s.sets().is_empty());
}
#[test]
fn exact_candidate_only() {
    let i = info();
    assert!(candidate(&i));
    for wrong in [
        HidInfo {
            product_id: 0x4021,
            ..i.clone()
        },
        HidInfo {
            usage: 2,
            ..i.clone()
        },
        HidInfo {
            feature_length: Some(64),
            ..i.clone()
        },
        HidInfo {
            vendor_id: 1,
            ..i.clone()
        },
    ] {
        assert!(!candidate(&wrong));
    }
}
#[test]
fn stable_read_and_short_transport_lengths() {
    for short21 in [false, true] {
        let mut s = Session::new();
        s.short21 = short21;
        let r = run(&mut s, None, 10000, false);
        assert_eq!(r.failure, None);
        assert_eq!(r.observed_hz, Some(1000));
        assert_eq!(r.supported_hz, RATES);
        assert_eq!(s.sent.len(), 6);
        assert!(s.sets().is_empty());
    }
}
#[test]
fn scalar_change_preserves_every_other_byte() {
    let mut s = Session::new();
    let before = s.config.clone();
    let r = run(&mut s, Some(8000), 10000, false);
    assert_eq!(r.failure, None);
    assert_eq!(r.previous_hz, Some(1000));
    assert_eq!(r.observed_hz, Some(8000));
    assert!(r.may_have_changed);
    assert_eq!(s.sent.len(), 19);
    assert_eq!(s.sets().len(), 1);
    for (i, b) in s.config.iter().enumerate() {
        assert_eq!(*b, if i == 2 { 0x52 } else { before[i] });
    }
}
#[test]
fn unsupported_and_same_rate_do_not_set() {
    for hz in [250, 1000] {
        let mut s = Session::new();
        let r = run(&mut s, Some(hz), 10000, false);
        assert!(s.sets().is_empty());
        assert!(!r.may_have_changed);
        if hz == 250 {
            assert_eq!(r.failure, Some(ProtocolFailure::Unsupported));
            assert!(s.sent.is_empty());
        } else {
            assert_eq!(r.failure, None);
        }
    }
}
#[test]
fn malformed_and_unknown_firmware_never_set() {
    for fw in [false, true] {
        let mut s = Session::new();
        s.firmware_wrong = fw;
        s.malformed = !fw;
        let r = run(&mut s, Some(8000), 10000, false);
        assert_eq!(
            r.failure,
            Some(if fw {
                ProtocolFailure::UnsupportedIdentity
            } else {
                ProtocolFailure::InvalidReply
            })
        );
        assert!(s.sets().is_empty());
    }
}
#[test]
fn late_foreign_echo_requires_two_new_matching_reads() {
    let mut s = Session::new();
    s.late = true;
    let r = run(&mut s, None, 10000, false);
    assert_eq!(r.failure, None);
    assert_eq!(s.sent.len(), 7);
}
#[test]
fn unstable_and_changed_profiles_never_set() {
    for unstable in [true, false] {
        let mut s = Session::new();
        s.unstable = unstable;
        s.profile_before = !unstable;
        let r = run(&mut s, Some(8000), 10000, false);
        assert_eq!(
            r.failure,
            Some(if unstable {
                ProtocolFailure::UnstableReply
            } else {
                ProtocolFailure::ProfileChanged
            })
        );
        assert!(s.sets().is_empty());
        assert_eq!(r.previous_hz, None);
        assert_eq!(r.observed_hz, None);
        assert!(r.supported_hz.is_empty());
    }
}
#[test]
fn unrelated_change_and_missing_effect_fail_verification() {
    for tail in [true, false] {
        let mut s = Session::new();
        s.tail_after = tail;
        s.ignore_set = !tail;
        let r = run(&mut s, Some(8000), 10000, false);
        assert_eq!(
            r.failure,
            Some(if tail {
                ProtocolFailure::UnrelatedConfigurationChanged
            } else {
                ProtocolFailure::VerificationMismatch
            })
        );
        assert!(r.may_have_changed);
        if tail {
            assert_eq!(r.previous_hz, None);
            assert!(r.supported_hz.is_empty());
        }
        assert_eq!(r.observed_hz, if tail { None } else { Some(1000) });
    }
}
#[test]
fn failed_send_remains_failure_even_with_verified_effect() {
    let mut s = Session::new();
    s.reject_set = true;
    let r = run(&mut s, Some(8000), 10000, false);
    assert_eq!(
        r.failure,
        Some(ProtocolFailure::Transport("uncertain write".into()))
    );
    assert!(r.may_have_changed);
    assert_eq!(r.observed_hz, Some(8000));
}
#[test]
fn cancellation_deadline_before_and_after_set() {
    for (deadline, cancel, changed) in [
        (10000, true, false),
        (1080, false, false),
        (1480, false, true),
    ] {
        let mut s = Session::new();
        let r = run(&mut s, Some(8000), deadline, cancel);
        assert_eq!(r.failure, Some(ProtocolFailure::CancelledOrDeadline));
        assert_eq!(r.may_have_changed, changed);
        assert_eq!(!s.sets().is_empty(), changed);
    }
}

#[test]
fn binding_or_configuration_changes_clear_recovery_values() {
    for identity in [true, false] {
        let mut s = Session::new();
        s.identity_changes = identity;
        s.config_before = !identity;
        let r = run(&mut s, Some(8000), 10000, false);
        assert_eq!(
            r.failure,
            Some(if identity {
                ProtocolFailure::UnsupportedIdentity
            } else {
                ProtocolFailure::ConfigurationChanged
            })
        );
        assert_eq!(r.previous_hz, None);
        assert_eq!(r.observed_hz, None);
        assert!(r.supported_hz.is_empty());
        assert!(s.sets().is_empty());
    }
}

#[test]
fn wrong_report_and_post_set_profile_change_fail_closed() {
    for wrong_id in [true, false] {
        let mut s = Session::new();
        s.wrong_id = wrong_id;
        s.profile_after = !wrong_id;
        let r = run(&mut s, Some(8000), 10000, false);
        assert_eq!(
            r.failure,
            Some(if wrong_id {
                ProtocolFailure::InvalidReply
            } else {
                ProtocolFailure::ProfileChanged
            })
        );
        assert_eq!(r.previous_hz, None);
        assert_eq!(r.observed_hz, None);
        assert!(r.supported_hz.is_empty());
        assert_eq!(r.may_have_changed, !wrong_id);
    }
}
