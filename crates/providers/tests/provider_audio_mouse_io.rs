mod common;
use common::*;
use hb_core::*;
use hb_providers::{
    HidProvider,
    protocols::{pa_request, padded},
};
use std::{sync::atomic::AtomicBool, time::Duration};
fn poll(p: &mut HidProvider, h: &FakeHid, c: &FakeClock) -> PollResult {
    p.poll(h, &context(c, &AtomicBool::new(false)))
}
fn lofree_entries(pid: u16) -> Vec<HidInfo> {
    [
        (0, 1, 6),
        (1, 1, 6),
        (1, 0xc, 1),
        (1, 0xff1c, 0x92),
        (1, 1, 2),
        (1, 1, 0x80),
        (2, 0xc, 1),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (interface, page, usage))| {
        let mut d = info(0x388d, pid, page);
        d.interface = interface;
        d.usage = usage;
        d.path = format!("{pid:04x}-{interface}-{page:04x}-{index}");
        d.product = "HYZEN67@Lofree".into();
        d
    })
    .collect()
}
fn read(data: Vec<u8>) -> Step {
    Step::ReadSized(64, Duration::from_millis(100), Ok(data))
}
fn lofree_transaction(command: u8, value: u8) -> Vec<Step> {
    vec![
        Step::Write(padded(&[4, 0, 0, 1], 32)),
        read(vec![4, 0, 0, 1]),
        Step::Write(padded(&[4, 0, 0, command], 32)),
        read(vec![4, 0, 0, command, 0, 0, 0, 0, value]),
        Step::Write(padded(&[4, 0, 0, 2], 32)),
        read(vec![4, 0, 0, 2]),
    ]
}
#[test]
fn lofree_issue82_seven_collections_select_only_vendor_and_cable_never_written() {
    let mut steps = lofree_transaction(0xaa, 1);
    steps.extend(lofree_transaction(0x1a, 66));
    let infos = lofree_entries(0x25);
    let selected = infos[3].path.clone();
    let h = FakeHid::new(infos, steps);
    let rows = poll(&mut HidProvider::new("lofree"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            &*rows[0].name,
            rows[0].level,
            &*rows[0].kind,
            &*rows[0].source
        ),
        ("Lofree HYZEN67", Some(66), "keyboard", "lofree")
    );
    assert_eq!(*h.opened.lock().unwrap(), [selected]);
    h.done();
    let h = FakeHid::new(lofree_entries(0x24), vec![]);
    assert!(
        poll(&mut HidProvider::new("lofree"), &h, &FakeClock::default())
            .unwrap()
            .is_empty()
    );
    assert!(h.opened.lock().unwrap().is_empty());
    h.done();
}
#[test]
fn lofree_start_silence_never_sends_online_or_battery_command() {
    let mut steps = vec![Step::Write(padded(&[4, 0, 0, 1], 32))];
    steps.extend((0..30).map(|_| read(vec![])));
    let h = FakeHid::new(lofree_entries(0x25), steps);
    let clock = FakeClock::default();
    assert!(
        poll(&mut HidProvider::new("lofree"), &h, &clock)
            .unwrap()
            .is_empty()
    );
    assert_eq!(clock.monotonic(), Duration::from_secs(3));
    h.done();
}
#[test]
fn lofree_wrong_battery_offset_is_refused_and_transaction_closes() {
    let mut steps = lofree_transaction(0xaa, 1);
    steps.extend([
        Step::Write(padded(&[4, 0, 0, 1], 32)),
        read(vec![4, 0, 0, 1]),
        Step::Write(padded(&[4, 0, 0, 0x1a], 32)),
        read(vec![4, 0, 0, 0x1a, 0, 3, 0, 0, 99]),
    ]);
    steps.extend((0..30).map(|_| read(vec![])));
    steps.push(Step::Write(padded(&[4, 0, 0, 2], 32)));
    let h = FakeHid::new(lofree_entries(0x25), steps);
    let clock = FakeClock::default();
    assert!(
        poll(&mut HidProvider::new("lofree"), &h, &clock)
            .unwrap()
            .is_empty()
    );
    assert_eq!(clock.monotonic(), Duration::from_secs(3));
    h.done();
}
fn drain() -> Step {
    Step::ReadSized(64, Duration::from_millis(20), Ok(vec![]))
}
fn pa_reply(command: u8, value: u8) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[0] = 1;
    r[13..17].copy_from_slice(&[command, 1, 1, value]);
    r
}
#[test]
fn barracuda_failed_wake_retries_after_150ms_then_queries_and_cleans_up() {
    let mut steps = vec![
        drain(),
        Step::WriteError(pa_request(true, 0xe1, Some(true)), "receiver waking"),
        drain(),
        Step::Write(pa_request(true, 0xe1, Some(true))),
    ];
    for (command, value) in [(0x21, 50), (0x2a, 0)] {
        steps.extend([
            drain(),
            Step::Write(pa_request(true, command, None)),
            Step::ReadSized(64, Duration::from_millis(150), Ok(pa_reply(command, value))),
        ]);
    }
    steps.extend([drain(), Step::Write(pa_request(true, 0xe1, Some(false)))]);
    let h = FakeHid::new(vec![info(0x1532, 0x53a, 0xff00)], steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("barracuda");
    let rows = poll(&mut p, &h, &clock).unwrap();
    assert_eq!(rows[0].level, Some(50));
    assert_eq!(rows[0].charging, Some(false));
    assert_eq!(clock.monotonic(), Duration::from_millis(200));
    assert!(p.diagnostics().iter().any(|s| s.contains("write remote")));
    h.done();
}
#[test]
fn barracuda_rejected_remote_mode_sends_no_queries_or_disable_frame() {
    let mut steps = vec![];
    for _ in 0..3 {
        steps.extend([
            drain(),
            Step::WriteError(pa_request(true, 0xe1, Some(true)), "access denied"),
        ]);
    }
    let h = FakeHid::new(vec![info(0x1532, 0x53a, 0xff00)], steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("barracuda");
    let rows = poll(&mut p, &h, &clock).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, None);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert_eq!(clock.monotonic(), Duration::from_millis(900));
    assert!(
        p.diagnostics()
            .iter()
            .any(|s| s.contains("receiver does not accept commands"))
    );
    h.done();
}
#[test]
fn blackshark_unopenable_control_path_returns_failure_without_writes() {
    let mut d = info(0x1532, 0x555, 0xff00);
    d.path = "unopenable-control".into();
    let h = FakeHid::new(vec![d.clone()], vec![]).fail_open(&d.path, "access denied");
    let mut p = HidProvider::new("razer");
    let error = poll(&mut p, &h, &FakeClock::default()).unwrap_err();
    assert!(error.message.contains("access denied"));
    assert_eq!(*h.opened.lock().unwrap(), [d.path]);
    h.done();
}
#[test]
fn audeze_empty_echo_requires_five_frames_and_rejects_real_battery() {
    use hb_providers::protocols::audeze_echo_only;
    let mut echo = vec![0; 62];
    echo[..3].copy_from_slice(&[7, 0, 0x80]);
    let mut other = echo.clone();
    other[2] = 0;
    assert!(audeze_echo_only(&vec![echo.clone(); 5]));
    assert!(audeze_echo_only(&[
        other,
        echo.clone(),
        echo.clone(),
        echo.clone(),
        echo.clone()
    ]));
    assert!(!audeze_echo_only(&vec![echo.clone(); 4]));
    let mut battery = vec![0; 62];
    battery[..10].copy_from_slice(&[7, 5, 0x5d, 7, 0, 0xd6, 0xc, 0, 0, 80]);
    assert!(!audeze_echo_only(&[
        echo.clone(),
        echo.clone(),
        echo.clone(),
        echo,
        battery
    ]));
    assert!(!audeze_echo_only(&[]));
}
#[test]
fn barracuda_offline_snapshot_keeps_explicit_kind_source_and_stable_identity() {
    let mut steps = vec![drain(), Step::Write(pa_request(true, 0xe1, Some(true)))];
    for _ in 0..4 {
        steps.extend([drain(), Step::Write(pa_request(true, 0x21, None))]);
        steps.extend((0..6).map(|_| Step::ReadSized(64, Duration::from_millis(150), Ok(vec![]))));
    }
    steps.extend([drain(), Step::Write(pa_request(true, 0xe1, Some(false)))]);
    let mut d = info(0x1532, 0x53a, 0xff00);
    d.serial = "headset-receiver-1".into();
    let h = FakeHid::new(vec![d], steps);
    let rows = poll(
        &mut HidProvider::new("barracuda"),
        &h,
        &FakeClock::default(),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            &*rows[0].key,
            &*rows[0].name,
            rows[0].level,
            rows[0].charging,
            rows[0].online(),
            &*rows[0].source,
            &*rows[0].kind
        ),
        (
            "barracuda:053a:HEADSET-RECEIVER-1",
            "Razer Barracuda Pro (2.4 GHz)",
            None,
            Some(false),
            false,
            "barracuda",
            "headset"
        )
    );
    h.done();
}
