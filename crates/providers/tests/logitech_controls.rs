mod common;
#[path = "../src/logitech_controls.rs"]
mod controls;
use common::*;
use controls::*;
use hb_core::*;
use std::{sync::atomic::AtomicBool, time::Duration};
fn device(pid: u16) -> HidInfo {
    let mut d = info(0x046d, pid, 0xff00);
    d.usage = 2;
    d.path = "trusted-long-collection".into();
    d
}
fn packet(slot: u8, feature: u8, token: u8, data: &[u8]) -> Vec<u8> {
    let mut p = vec![0; 20];
    p[..4].copy_from_slice(&[0x11, slot, feature, token]);
    p[4..4 + data.len()].copy_from_slice(data);
    p
}
fn pair(slot: u8, feature: u8, token: u8, request: &[u8], reply: &[u8]) -> Vec<Step> {
    vec![
        Step::Write(packet(slot, feature, token, request)),
        Step::ReadSized(64, Duration::ZERO, Ok(packet(slot, feature, token, reply))),
    ]
}
fn probe(
    slot: u8,
    unit: [u8; 4],
    wpid: u16,
    usb: u16,
    mask: u16,
    code: u8,
    mode: Option<u8>,
) -> Vec<Step> {
    let mut ids = [0u8; 16];
    ids[1..5].copy_from_slice(&unit);
    ids[6] = 12;
    ids[7..9].copy_from_slice(&wpid.to_be_bytes());
    ids[9..11].copy_from_slice(&usb.to_be_bytes());
    let mut steps = pair(slot, 0, 1, &[0, 3], &[2]);
    steps.extend(pair(slot, 2, 2, &[], &ids));
    steps.extend(pair(slot, 0, 3, &[0x80, 0x61], &[11]));
    steps.extend(pair(slot, 11, 0x14, &[], &mask.to_be_bytes()));
    steps.extend(pair(slot, 11, 0x25, &[], &[code]));
    steps.extend(pair(
        slot,
        0,
        6,
        &[0x81, 0],
        &[if mode.is_some() { 12 } else { 0 }],
    ));
    if let Some(mode) = mode {
        steps.extend(pair(slot, 12, 0x27, &[], &[mode]));
    }
    steps
}
fn normal(slot: u8, code: u8, mode: Option<u8>) -> Vec<Step> {
    probe(slot, [1, 2, 3, 4], 0x40a9, 0xc09b, 0x7f, code, mode)
}
fn discover_fake(h: &FakeHid, d: &HidInfo, slot: u8) -> Result<Capabilities, ProtocolFailure> {
    let c = FakeClock::default();
    let cancel = AtomicBool::new(false);
    discover(
        &mut *h.open(d).unwrap(),
        &mut None,
        &context(&c, &cancel),
        d,
        slot,
        "01020304",
    )
}
fn execute_fake(h: &FakeHid, d: &HidInfo, cap: &Capabilities, hz: Option<u32>) -> PollingResult {
    let c = FakeClock::default();
    let cancel = AtomicBool::new(false);
    execute_rate(
        &mut *h.open(d).unwrap(),
        &mut None,
        &context(&c, &cancel),
        d,
        cap,
        hz,
    )
}
fn capability(d: &HidInfo, slot: u8) -> Capabilities {
    let h = FakeHid::new(vec![d.clone()], normal(slot, 3, Some(2)));
    let cap = discover_fake(&h, d, slot).unwrap();
    h.done();
    cap
}
#[test]
fn conditional_restore_refuses_external_rate_changes_without_set() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    let h = FakeHid::new(vec![d.clone()], normal(1, 3, Some(2)));
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let result = execute_rate_checked(
        &mut *h.open(&d).unwrap(),
        &mut None,
        &context(&clock, &cancel),
        &d,
        &cap,
        Some(1000),
        Some(2000),
    );
    assert_eq!(result.failure, Some(ProtocolFailure::VerificationMismatch));
    assert!(!result.may_have_changed);
    h.done();
}
#[test]
fn models_capability_mask_exact_current_and_wired_limit() {
    for (slot, pid, wpid, usb) in [
        (1, 0xc54d, 0x40a9, 0xc09b),
        (2, 0xc53a, 0x40b8, 0xc0a0),
        (1, 0xc54d, 0x40bd, 0xc0a8),
        (255, 0xc09b, 0x40a9, 0xc09b),
        (255, 0xc0a0, 0x40b8, 0xc0a0),
        (255, 0xc0a8, 0x40bd, 0xc0a8),
    ] {
        let d = device(pid);
        let h = FakeHid::new(
            vec![d.clone()],
            probe(slot, [1, 2, 3, 4], wpid, usb, 0xffff, 3, Some(2)),
        );
        let cap = discover_fake(&h, &d, slot).unwrap();
        assert_eq!(cap.unit, "01020304");
        assert_eq!(
            (
                cap.model_wpid,
                cap.model_usb,
                cap.observed_hz,
                cap.onboard_mode
            ),
            (wpid, usb, 1000, Some(2))
        );
        assert_eq!(
            cap.supported_hz,
            if pid != 0xc54d {
                vec![125, 250, 500, 1000]
            } else {
                vec![125, 250, 500, 1000, 2000, 4000, 8000]
            }
        );
        h.done();
    }
}
#[test]
fn superstrike_receiver_rate_change_uses_only_live_rate_and_mode_getters() {
    let d = device(0xc54d);
    let discovery = probe(2, [1, 2, 3, 4], 0x40bd, 0xc0a8, 0x7f, 3, Some(2));
    let h = FakeHid::new(vec![d.clone()], discovery.clone());
    let cap = discover_fake(&h, &d, 2).unwrap();
    h.done();
    let mut steps = discovery;
    steps.extend(pair(2, 11, 0x39, &[6], &[]));
    steps.extend(pair(2, 11, 0x2a, &[], &[6]));
    steps.extend(pair(2, 12, 0x2b, &[], &[2]));
    let h = FakeHid::new(vec![d.clone()], steps);
    let result = execute_fake(&h, &d, &cap, Some(8000));
    assert_eq!(result.failure, None);
    assert_eq!(result.observed_hz, Some(8000));
    assert_eq!(result.previous_hz, Some(1000));
    assert!(result.software_mode && result.may_have_changed);
    h.done();
}
#[test]
fn older_receiver_never_offers_or_sets_high_rates_from_mouse_mask() {
    let d = device(0xc53a);
    let cap = capability(&d, 1);
    assert_eq!(cap.supported_hz, vec![125, 250, 500, 1000]);
    let h = FakeHid::new(vec![d.clone()], vec![]);
    let result = execute_fake(&h, &d, &cap, Some(8000));
    assert_eq!(result.failure, Some(ProtocolFailure::Unsupported));
    assert!(!result.may_have_changed);
    h.done();
}
#[test]
fn superstrike_unproven_receiver_is_refused_before_rate_queries() {
    let d = device(0xc53a);
    let mut steps = probe(1, [1, 2, 3, 4], 0x40bd, 0xc0a8, 0x7f, 3, Some(2));
    steps.truncate(4); // Only feature lookup and exact model/unit identity.
    let h = FakeHid::new(vec![d.clone()], steps);
    assert_eq!(discover_fake(&h, &d, 1), Err(ProtocolFailure::Unsupported));
    h.done();
}
#[test]
fn unsupported_names_vendor_collections_slots_and_zero_unit_do_not_query() {
    for (mut d, slot, expected) in [
        (device(0xc547), 1, "01020304"),
        (device(0xc54d), 255, "01020304"),
        (device(0xc09b), 1, "01020304"),
        (device(0xc54d), 0, "01020304"),
        (device(0xc54d), 1, "00000000"),
        (device(0xc54d), 1, "receiver-serial"),
    ] {
        d.product = "Logitech PRO X 2 DEX".into();
        let h = FakeHid::new(vec![d.clone()], vec![]);
        let c = FakeClock::default();
        let cancel = AtomicBool::new(false);
        assert!(
            discover(
                &mut *h.open(&d).unwrap(),
                &mut None,
                &context(&c, &cancel),
                &d,
                slot,
                expected
            )
            .is_err()
        );
        h.done();
    }
    let mut d = device(0xc54d);
    d.usage = 1;
    assert!(!candidate(&d));
    d.usage = 2;
    d.vendor_id = 0x1532;
    assert!(!candidate(&d));
}
#[test]
fn paired_unit_and_model_must_match_before_capability_queries() {
    for (unit, wpid, usb, expected_error) in [
        (
            [5, 6, 7, 8],
            0x40a9,
            0xc09b,
            ProtocolFailure::IdentityMismatch,
        ),
        (
            [0, 0, 0, 0],
            0x40a9,
            0xc09b,
            ProtocolFailure::IdentityMismatch,
        ),
        ([1, 2, 3, 4], 0x4093, 0xc094, ProtocolFailure::Unsupported),
    ] {
        let mut steps = probe(1, unit, wpid, usb, 0x7f, 3, Some(2));
        steps.truncate(4);
        let d = device(0xc54d);
        let h = FakeHid::new(vec![d.clone()], steps);
        assert_eq!(discover_fake(&h, &d, 1), Err(expected_error));
        h.done();
    }
}
#[test]
fn missing_rate_feature_and_empty_rate_mask_do_not_offer_controls() {
    let d = device(0xc54d);
    let mut steps = normal(1, 3, Some(2));
    steps.truncate(6);
    steps[5] = Step::ReadSized(64, Duration::ZERO, Ok(packet(1, 0, 3, &[0])));
    let h = FakeHid::new(vec![d.clone()], steps);
    assert_eq!(discover_fake(&h, &d, 1), Err(ProtocolFailure::Unsupported));
    h.done();
    let mut steps = probe(1, [1, 2, 3, 4], 0x40a9, 0xc09b, 0, 3, Some(2));
    steps.truncate(8);
    let h = FakeHid::new(vec![d.clone()], steps);
    assert_eq!(discover_fake(&h, &d, 1), Err(ProtocolFailure::Unsupported));
    h.done();
}
#[test]
fn late_foreign_slot_and_swid_on_both_channels_are_ignored() {
    let d = device(0xc54d);
    let mut steps = normal(1, 3, Some(2));
    let good = steps.remove(1);
    steps.splice(
        1..1,
        [
            Step::ReadSized(64, Duration::ZERO, Ok(packet(2, 0, 1, &[99]))),
            Step::ReadSized(64, Duration::ZERO, Ok(vec![0x10, 1, 0x8f, 0, 9, 9, 0])),
            good,
        ],
    );
    let h = FakeHid::new(vec![d.clone()], steps);
    let c = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut short_session = h.open(&d).unwrap();
    let mut short: Option<&mut dyn HidSession> = Some(&mut *short_session);
    let cap = discover(
        &mut *h.open(&d).unwrap(),
        &mut short,
        &context(&c, &cancel),
        &d,
        1,
        "01020304",
    )
    .unwrap();
    assert_eq!(cap.observed_hz, 1000);
    assert_eq!(c.monotonic(), Duration::from_millis(5));
    h.done();
}
#[test]
fn short_channel_device_error_and_matching_truncated_long_reply_fail() {
    let d = device(0xc54d);
    let steps = vec![
        Step::Write(packet(1, 0, 1, &[0, 3])),
        Step::ReadSized(64, Duration::ZERO, Ok(vec![])),
        Step::ReadSized(64, Duration::ZERO, Ok(vec![0x10, 1, 0x8f, 0, 1, 9, 0])),
    ];
    let h = FakeHid::new(vec![d.clone()], steps);
    let c = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut short_session = h.open(&d).unwrap();
    let mut short: Option<&mut dyn HidSession> = Some(&mut *short_session);
    assert_eq!(
        discover(
            &mut *h.open(&d).unwrap(),
            &mut short,
            &context(&c, &cancel),
            &d,
            1,
            "01020304"
        ),
        Err(ProtocolFailure::DeviceError(9))
    );
    h.done();
    let h = FakeHid::new(
        vec![d.clone()],
        vec![
            Step::Write(packet(1, 0, 1, &[0, 3])),
            Step::ReadSized(64, Duration::ZERO, Ok(vec![0x11, 1, 0, 1, 2])),
        ],
    );
    assert_eq!(discover_fake(&h, &d, 1), Err(ProtocolFailure::InvalidReply));
    h.done();
}
#[test]
fn explicit_set_revalidates_then_reads_back_exact_extended_code() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    let mut steps = normal(1, 3, Some(2));
    steps.extend(pair(1, 11, 0x39, &[6], &[]));
    steps.extend(pair(1, 11, 0x2a, &[], &[6]));
    steps.extend(pair(1, 12, 0x2b, &[], &[2]));
    let h = FakeHid::new(vec![d.clone()], steps);
    let r = execute_fake(&h, &d, &cap, Some(8000));
    assert_eq!(
        r,
        PollingResult {
            observed_hz: Some(8000),
            previous_hz: Some(1000),
            software_mode: true,
            may_have_changed: true,
            failure: None
        }
    );
    h.done();
}
#[test]
fn replaced_pairing_slot_never_gets_a_set() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    let mut steps = probe(1, [5, 6, 7, 8], 0x40a9, 0xc09b, 0x7f, 3, Some(2));
    steps.truncate(4);
    let h = FakeHid::new(vec![d.clone()], steps);
    let r = execute_fake(&h, &d, &cap, Some(8000));
    assert_eq!(r.failure, Some(ProtocolFailure::IdentityMismatch));
    assert!(!r.may_have_changed);
    h.done();
}
#[test]
fn onboard_and_unknown_mode_refuse_setting_without_changing_profiles() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for (mode, error) in [
        (Some(1), ProtocolFailure::OnboardMode),
        (Some(0), ProtocolFailure::UnknownMode),
        (None, ProtocolFailure::UnknownMode),
    ] {
        let h = FakeHid::new(vec![d.clone()], normal(1, 3, mode));
        let r = execute_fake(&h, &d, &cap, Some(8000));
        assert_eq!(r.failure, Some(error));
        assert_eq!(r.observed_hz, Some(1000));
        assert!(!r.may_have_changed);
        h.done();
    }
}
#[test]
fn no_change_and_read_only_do_not_send_set_and_disallowed_rate_never_queries() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for hz in [None, Some(1000)] {
        let h = FakeHid::new(vec![d.clone()], normal(1, 3, Some(1)));
        let r = execute_fake(&h, &d, &cap, hz);
        assert_eq!(r.observed_hz, Some(1000));
        assert!(!r.may_have_changed);
        assert_eq!(r.failure, None);
        h.done();
    }
    let h = FakeHid::new(vec![d.clone()], vec![]);
    assert_eq!(
        execute_fake(&h, &d, &cap, Some(16000)).failure,
        Some(ProtocolFailure::Unsupported)
    );
    h.done();
}
#[test]
fn failed_set_ack_keeps_failure_even_when_readback_matches_and_tracks_partial() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for failed_send in [false, true] {
        let mut steps = normal(1, 3, Some(2));
        if failed_send {
            steps.push(Step::WriteError(
                packet(1, 11, 0x39, &[6]),
                "write uncertain",
            ));
        } else {
            steps.extend([
                Step::Write(packet(1, 11, 0x39, &[6])),
                Step::ReadSized(64, Duration::ZERO, Ok(vec![0x11, 1, 0xff, 11, 0x39, 1, 0])),
            ]);
        }
        steps.extend(pair(1, 11, 0x2a, &[], &[6]));
        steps.extend(pair(1, 12, 0x2b, &[], &[2]));
        let h = FakeHid::new(vec![d.clone()], steps);
        let r = execute_fake(&h, &d, &cap, Some(8000));
        assert_eq!(r.observed_hz, Some(8000));
        assert!(r.may_have_changed);
        assert!(r.failure.is_some());
        h.done();
    }
}
#[test]
fn readback_mismatch_unknown_code_and_changed_capability_are_not_success() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for (code, error) in [
        (3, ProtocolFailure::VerificationMismatch),
        (7, ProtocolFailure::UnknownRate(7)),
    ] {
        let mut steps = normal(1, 3, Some(2));
        steps.extend(pair(1, 11, 0x39, &[6], &[]));
        steps.extend(pair(1, 11, 0x2a, &[], &[code]));
        steps.extend(pair(1, 12, 0x2b, &[], &[2]));
        let h = FakeHid::new(vec![d.clone()], steps);
        assert_eq!(execute_fake(&h, &d, &cap, Some(8000)).failure, Some(error));
        h.done();
    }
    let h = FakeHid::new(
        vec![d.clone()],
        probe(1, [1, 2, 3, 4], 0x40a9, 0xc09b, 0x0f, 3, Some(2)),
    );
    let r = execute_fake(&h, &d, &cap, Some(8000));
    assert_eq!(r.failure, Some(ProtocolFailure::Unsupported));
    assert!(!r.may_have_changed);
    h.done();
}
#[test]
fn cancelled_and_silent_calls_are_bounded_and_never_set() {
    let d = device(0xc54d);
    let h = FakeHid::new(vec![d.clone()], vec![]);
    let c = FakeClock::default();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        discover(
            &mut *h.open(&d).unwrap(),
            &mut None,
            &context(&c, &cancel),
            &d,
            1,
            "01020304"
        ),
        Err(ProtocolFailure::CancelledOrDeadline)
    );
    h.done();
    let mut steps = vec![Step::Write(packet(1, 0, 1, &[0, 3]))];
    steps.extend((0..120).map(|_| Step::ReadSized(64, Duration::ZERO, Ok(vec![]))));
    let h = FakeHid::new(vec![d.clone()], steps);
    let c = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        discover(
            &mut *h.open(&d).unwrap(),
            &mut None,
            &context(&c, &cancel),
            &d,
            1,
            "01020304"
        ),
        Err(ProtocolFailure::Timeout)
    );
    assert_eq!(c.monotonic(), Duration::from_millis(600));
    h.done();
}
fn control_request(action: ControlAction) -> ControlRequest {
    let mut reading = Reading::new("logitech:01020304", "PRO X 2", "logitech", 1);
    reading.kind = "mouse".into();
    reading.serial = Some("01020304".into());
    ControlRequest {
        request: 7,
        target: ControlTarget {
            device: ConfigurationDevice::from_reading(&reading),
            generation: 0,
        },
        action,
    }
}
fn route_scan(target_slot: u8, mode: Option<u8>) -> Vec<Step> {
    let mut steps = vec![];
    for slot in 1..=6 {
        if slot == target_slot {
            steps.extend(normal(slot, 3, mode));
        } else {
            steps.extend([
                Step::Write(packet(slot, 0, 1, &[0, 3])),
                Step::ReadSized(64, Duration::ZERO, Ok(vec![0x10, slot, 0x8f, 0, 1, 8, 0])),
            ]);
        }
    }
    steps
}
fn adapter(request: &ControlRequest, h: &dyn HidTransport, c: &FakeClock) -> ControlOutcome {
    use hb_providers::controls::HidDeviceController;
    HidDeviceController.execute(request, h, &context(c, &AtomicBool::new(false)))
}
#[test]
fn adapter_resolves_nonfirst_paired_slot_by_unit_and_revalidates_before_setting() {
    let d = device(0xc54d);
    let mut steps = route_scan(2, Some(2));
    steps.extend(normal(2, 3, Some(2)));
    steps.extend(pair(2, 11, 0x39, &[5], &[]));
    steps.extend(pair(2, 11, 0x2a, &[], &[5]));
    steps.extend(pair(2, 12, 0x2b, &[], &[2]));
    let h = FakeHid::new(vec![d], steps);
    let request = control_request(ControlAction::Apply(PollingRate::try_from(4000).unwrap()));
    let r = adapter(&request, &h, &FakeClock::default());
    assert_eq!(r.failure, None);
    assert_eq!(r.previous.unwrap().hz(), 1000);
    assert_eq!(r.observation.unwrap().rate.unwrap().hz(), 4000);
    assert!(r.may_have_changed);
    h.done();
}
#[test]
fn adapter_resolves_superstrike_direct_collection_and_keeps_wired_ceiling() {
    let d = device(0xc0a8);
    let discovery = probe(255, [1, 2, 3, 4], 0x40bd, 0xc0a8, 0x7f, 3, Some(2));
    let mut steps = discovery.clone();
    steps.extend(discovery);
    steps.extend(pair(255, 11, 0x39, &[2], &[]));
    steps.extend(pair(255, 11, 0x2a, &[], &[2]));
    steps.extend(pair(255, 12, 0x2b, &[], &[2]));
    let h = FakeHid::new(vec![d], steps);
    let result = adapter(
        &control_request(ControlAction::Apply(PollingRate::try_from(500).unwrap())),
        &h,
        &FakeClock::default(),
    );
    assert_eq!(result.failure, None);
    assert_eq!(result.previous.unwrap().hz(), 1000);
    let observation = result.observation.unwrap();
    assert_eq!(observation.rate.unwrap().hz(), 500);
    assert_eq!(
        observation
            .supported
            .iter()
            .map(|r| r.hz())
            .collect::<Vec<_>>(),
        vec![125, 250, 500, 1000]
    );
    assert!(result.may_have_changed);
    h.done();
}
#[test]
fn adapter_duplicate_receiver_routes_are_ambiguous_and_never_set() {
    let mut a = device(0xc54d);
    a.path = "receiver-a".into();
    let mut b = a.clone();
    b.path = "receiver-b".into();
    let mut steps = route_scan(1, Some(2));
    steps.extend(route_scan(2, Some(2)));
    let h = FakeHid::new(vec![a, b], steps);
    let r = adapter(
        &control_request(ControlAction::Apply(PollingRate::try_from(4000).unwrap())),
        &h,
        &FakeClock::default(),
    );
    assert!(r.failure.unwrap().contains("multiple receiver routes"));
    assert!(!r.may_have_changed);
    h.done();
}
#[test]
fn adapter_read_in_onboard_mode_exposes_caps_but_never_claims_active_rate() {
    let d = device(0xc54d);
    let h = FakeHid::new(vec![d], route_scan(1, Some(1)));
    let r = adapter(
        &control_request(ControlAction::Read),
        &h,
        &FakeClock::default(),
    );
    assert!(!r.may_have_changed);
    assert_eq!(r.previous, None);
    assert!(
        r.failure
            .unwrap()
            .contains("software control mode in G HUB")
    );
    let o = r.observation.unwrap();
    assert_eq!(o.rate, None);
    assert_eq!(o.supported.len(), 7);
    assert!(o.evidence.contains("effective polling rate unavailable"));
    assert!(o.evidence.contains("1000 Hz"));
    h.done();
}
#[test]
fn adapter_exact_container_and_verified_unit_key_and_epoch_gate_before_gets() {
    let mut d = device(0xc54d);
    d.container = Some("container-new".into());
    for variation in 0..4 {
        let h = FakeHid::new(vec![d.clone()], vec![]);
        let mut request =
            control_request(ControlAction::Apply(PollingRate::try_from(4000).unwrap()));
        match variation {
            0 => request.target.device.container = Some("container-old".into()),
            1 => request.target.device.key = "logitech:c54d:1".into(),
            2 => request.target.device.serial = Some("00000000".into()),
            _ => request.target.generation = 9,
        };
        let r = adapter(&request, &h, &FakeClock::default());
        assert!(r.failure.is_some());
        assert!(!r.may_have_changed);
        assert!(h.opened.lock().unwrap().is_empty());
        h.done();
    }
}
#[test]
fn adapter_epoch_change_during_revalidation_prevents_all_setting_packets() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    struct EpochSession {
        inner: Box<dyn HidSession>,
        epoch: Arc<AtomicU64>,
        reads: Arc<AtomicUsize>,
    }
    impl HidSession for EpochSession {
        fn write(&mut self, d: &[u8]) -> Result<(), ProviderError> {
            self.inner.write(d)
        }
        fn read(&mut self, n: usize, t: Duration) -> Result<Vec<u8>, ProviderError> {
            let result = self.inner.read(n, t);
            if self.reads.fetch_add(1, Ordering::Relaxed) == 7 {
                self.epoch.store(1, Ordering::Relaxed);
            }
            result
        }
        fn send_feature(&mut self, d: &[u8]) -> Result<(), ProviderError> {
            self.inner.send_feature(d)
        }
        fn feature(&mut self, id: u8, n: usize) -> Result<Vec<u8>, ProviderError> {
            self.inner.feature(id, n)
        }
    }
    struct EpochHid {
        inner: FakeHid,
        epoch: Arc<AtomicU64>,
        reads: Arc<AtomicUsize>,
    }
    impl HidTransport for EpochHid {
        fn enumerate(&self, v: u16) -> Result<Vec<HidInfo>, ProviderError> {
            self.inner.enumerate(v)
        }
        fn generation(&self) -> u64 {
            self.epoch.load(Ordering::Relaxed)
        }
        fn open(&self, i: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            Ok(Box::new(EpochSession {
                inner: self.inner.open(i)?,
                epoch: self.epoch.clone(),
                reads: self.reads.clone(),
            }))
        }
    }
    let d = device(0xc09b);
    let mut steps = normal(255, 3, Some(2));
    steps.extend(pair(255, 0, 1, &[0, 3], &[2]));
    let h = EpochHid {
        inner: FakeHid::new(vec![d], steps),
        epoch: Arc::new(AtomicU64::new(0)),
        reads: Arc::new(AtomicUsize::new(0)),
    };
    let r = adapter(
        &control_request(ControlAction::Apply(PollingRate::try_from(500).unwrap())),
        &h,
        &FakeClock::default(),
    );
    assert!(r.failure.unwrap().contains("connection changed"));
    assert!(!r.may_have_changed);
    h.inner.done();
}

#[test]
fn wired_software_value_above_captured_ceiling_is_not_restorable_or_changed() {
    let d = device(0xc09b);
    let cap = capability(&d, 255);
    let h = FakeHid::new(vec![d.clone()], normal(255, 6, Some(2)));
    let r = execute_fake(&h, &d, &cap, Some(500));
    assert_eq!(r.observed_hz, Some(8000));
    assert_eq!(r.previous_hz, None);
    assert_eq!(r.failure, Some(ProtocolFailure::Unsupported));
    assert!(!r.may_have_changed);
    h.done();
}

#[test]
fn mode_switch_after_set_does_not_claim_effective_rate_or_restore_value() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for (mode, error) in [
        (1, ProtocolFailure::OnboardMode),
        (0, ProtocolFailure::UnknownMode),
    ] {
        let mut steps = normal(1, 3, Some(2));
        steps.extend(pair(1, 11, 0x39, &[6], &[]));
        steps.extend(pair(1, 11, 0x2a, &[], &[6]));
        steps.extend(pair(1, 12, 0x2b, &[], &[mode]));
        let h = FakeHid::new(vec![d.clone()], steps);
        let r = execute_fake(&h, &d, &cap, Some(8000));
        assert_eq!(r.failure, Some(error));
        assert_eq!(r.observed_hz, Some(8000)); // Software getter, not the active profile.
        assert!(!r.software_mode);
        assert_eq!(r.previous_hz, None);
        assert!(r.may_have_changed);
        h.done();
    }
}
#[test]
fn mode_verification_failure_preserves_prior_partial_failure() {
    let d = device(0xc54d);
    let cap = capability(&d, 1);
    for failed_set in [false, true] {
        let mut steps = normal(1, 3, Some(2));
        if failed_set {
            steps.push(Step::WriteError(packet(1, 11, 0x39, &[6]), "set uncertain"));
        } else {
            steps.extend(pair(1, 11, 0x39, &[6], &[]));
        }
        steps.extend(pair(1, 11, 0x2a, &[], &[6]));
        steps.push(Step::WriteError(
            packet(1, 12, 0x2b, &[]),
            "mode unavailable",
        ));
        let h = FakeHid::new(vec![d.clone()], steps);
        let r = execute_fake(&h, &d, &cap, Some(8000));
        assert_eq!(
            r.failure,
            Some(ProtocolFailure::Transport(
                if failed_set {
                    "set uncertain"
                } else {
                    "mode unavailable"
                }
                .into()
            ))
        );
        assert!(!r.software_mode);
        assert_eq!(r.previous_hz, None);
        assert_eq!(r.observed_hz, Some(8000));
        assert!(r.may_have_changed);
        h.done();
    }
}

#[test]
fn adapter_post_set_mode_switch_suppresses_verified_rate_and_restore() {
    let d = device(0xc54d);
    let mut steps = route_scan(2, Some(2));
    steps.extend(normal(2, 3, Some(2)));
    steps.extend(pair(2, 11, 0x39, &[5], &[]));
    steps.extend(pair(2, 11, 0x2a, &[], &[5]));
    steps.extend(pair(2, 12, 0x2b, &[], &[1]));
    let h = FakeHid::new(vec![d], steps);
    let request = control_request(ControlAction::Apply(PollingRate::try_from(4000).unwrap()));
    let r = adapter(&request, &h, &FakeClock::default());
    assert!(
        r.failure
            .unwrap()
            .contains("software control mode in G HUB")
    );
    assert_eq!(r.previous, None);
    let o = r.observation.unwrap();
    assert_eq!(o.rate, None);
    assert!(o.evidence.contains("effective polling rate unavailable"));
    assert!(o.evidence.contains("4000 Hz"));
    assert!(r.may_have_changed);
    h.done();
}
