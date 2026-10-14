//! Reviewed HaloBattery 1.15 captures on fake transports; no hardware claim.
mod common;
use common::*;
use hb_core::*;
use hb_providers::{HidProvider, protocols::padded, provider::*};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

fn poll(p: &mut HidProvider, h: &FakeHid, c: &FakeClock) -> Vec<Reading> {
    p.poll(h, &context(c, &AtomicBool::new(false))).unwrap()
}
fn rd(r: Vec<u8>) -> Step {
    Step::Read(Ok(r))
}
fn telemetry(level: u16, charge: bool) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[..3].copy_from_slice(&[0x4e, 2, 0x97]);
    r[17..19].copy_from_slice(&level.to_le_bytes());
    r[22] = u8::from(charge);
    r
}
fn nzxt_steps(level: u16, charge: bool) -> Vec<Step> {
    vec![
        rd(vec![]),
        Step::Write(padded(&[0x4e, 2, 0x81, 0, 0xb0], 64)),
        rd(vec![0x4e, 0xe5]),
        rd(telemetry(level, charge)),
    ]
}
fn blackshark(state: u8, level: u8) -> Vec<u8> {
    let mut r = padded(
        &[
            0xff, 0x0f, 5, 0xfe, 0x12, 4, 0x1f, 8, 5, 3, 5, state, 0x0e, 0xa0, level,
        ],
        64,
    );
    r[15] = 0;
    r
}
#[test]
fn blackshark_2020_only_vendor_collection_and_unknown_charging_are_preserved() {
    for (state, raw, level, charging) in [
        (1, 50, Some(50), Some(false)),
        (2, 30, Some(30), Some(false)),
        (3, 10, Some(10), Some(false)),
        (6, 100, Some(100), Some(false)),
        (9, 80, None, Some(true)),
        (0x77, 50, Some(50), None),
        (1, 255, None, Some(false)),
    ] {
        let i = info(0x1532, 0x0528, 0xff00);
        let mut bad = i.clone();
        bad.path = "keyboard-collection".into();
        bad.usage_page = 1;
        let h = FakeHid::new(
            vec![bad, i],
            vec![
                Step::Send(padded(&[0xff, 0x0a, 0, 0xfd, 4, 0x12, 0xf1, 2, 5], 64)),
                Step::Feature(0xff, 64, Ok(blackshark(state, raw))),
            ],
        );
        let rows = poll(&mut HidProvider::new("razer"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].level, rows[0].charging), (level, charging));
        assert_eq!(rows[0].kind, "headset");
        assert_eq!(rows[0].precision, Precision::Coarse);
        assert_eq!(h.opened.lock().unwrap().len(), 1);
        h.done();
    }
}
#[test]
fn blackshark_stale_header_request_echo_and_charging_cable_never_create_readings() {
    let mut stale = blackshark(9, 80);
    stale[..3].copy_from_slice(&[0xff, 1, 0]);
    let mut steps = vec![Step::Send(padded(
        &[0xff, 0x0a, 0, 0xfd, 4, 0x12, 0xf1, 2, 5],
        64,
    ))];
    steps.extend((0..5).map(|_| Step::Feature(0xff, 64, Ok(stale.clone()))));
    let h = FakeHid::new(
        vec![info(0x1532, 0x0528, 0xff00), info(0x1532, 0x052e, 0xff00)],
        steps,
    );
    assert!(poll(&mut HidProvider::new("razer"), &h, &FakeClock::default()).is_empty());
    h.done();
}
#[test]
fn nzxt_filters_collections_skips_ack_and_merges_proven_cable_identity() {
    let mut radio = info(0x1e71, 0x2101, 0xffca);
    radio.serial = "SYNTHETIC-UNIT".into();
    let mut cable = info(0x1e71, 0x2129, 0xffca);
    cable.serial = radio.serial.clone();
    let mut bad = radio.clone();
    bad.path = "unrelated-collection".into();
    bad.usage_page = 1;
    let mut steps = nzxt_steps(77, false);
    steps.extend(nzxt_steps(84, true));
    let h = FakeHid::new(vec![radio, cable, bad, info(0x1e71, 0x2131, 0xffca)], steps);
    let rows = poll(&mut HidProvider::new("nzxt"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].level, rows[0].charging), (Some(84), Some(true)));
    assert_eq!(rows[0].via, "usb");
    h.done();
}
#[test]
fn nzxt_identical_names_are_separate_and_enumeration_failures_remain_errors() {
    let mut a = info(0x1e71, 0x2101, 0xffca);
    a.serial = "UNIT-A".into();
    let mut b = a.clone();
    b.serial = "UNIT-B".into();
    b.path = "another-path".into();
    let mut steps = nzxt_steps(70, false);
    steps.extend(nzxt_steps(40, false));
    let h = FakeHid::new(vec![a, b], steps);
    let rows = poll(&mut HidProvider::new("nzxt"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].key, rows[1].key);
    h.done();
    let h = FakeHid::new(vec![info(0x1e71, 0x2101, 0xffca)], vec![])
        .fail_open("device-1e71-2101", "busy");
    assert!(
        HidProvider::new("nzxt")
            .poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)))
            .is_err()
    );
}
#[test]
fn mchose_a5_all_ids_only_ffff_and_exact_feature_command() {
    for pid in [0xf013, 0xf015, 0xf019] {
        let i = info(0x2023, pid, 0xffff);
        let mut wrong = i.clone();
        wrong.path = "sibling-ffa0".into();
        wrong.usage_page = 0xffa0;
        let packet = padded(&[0, 0, 0, 2, 2, 0, 0x83], 65);
        let h = FakeHid::new(
            vec![wrong, i, info(0x2023, 0xffff, 0xffff)],
            vec![
                Step::Send(packet),
                Step::Feature(0, 65, Ok(vec![0xa1, 0, 0, 0, 0, 0x83, 1, 73, 0])),
            ],
        );
        let rows = poll(&mut HidProvider::new("mchose"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].level, rows[0].charging), (Some(73), Some(true)));
        assert_eq!(h.opened.lock().unwrap().len(), 1);
        h.done();
    }
}
#[test]
fn mchose_a5_retries_once_and_rejects_unrelated_feature_reports() {
    let packet = padded(&[0, 0, 0, 2, 2, 0, 0x83], 65);
    let mut steps = vec![Step::Send(packet.clone())];
    for n in 0..8 {
        if n == 3 {
            steps.push(Step::Send(packet.clone()));
        }
        steps.push(Step::Feature(
            0,
            65,
            Ok(vec![0xa1, 0, 0, 0, 1, 0x83, 1, 73, 0]),
        ));
    }
    let h = FakeHid::new(vec![info(0x2023, 0xf013, 0xffff)], steps);
    assert!(poll(&mut HidProvider::new("mchose"), &h, &FakeClock::default()).is_empty());
    h.done();
}
fn bragi(value: u16, source: bool) -> Vec<u8> {
    let mut r = if source {
        vec![1, 1, 2, 0]
    } else {
        vec![1, 2, 0]
    };
    r.extend_from_slice(&value.to_le_bytes());
    r.resize(64, 0);
    r
}
fn bragi_steps(pid: u16, route: u8, first: u16, second: u16, charge: Option<u16>) -> Vec<Step> {
    let request = |property| {
        if pid == 0x1b7f {
            padded(&[0, route, 2, property], 65)
        } else {
            padded(&[2, route, 2, property], 64)
        }
    };
    let mut steps = vec![
        rd(vec![]),
        Step::Write(request(0x0f)),
        rd(bragi(first, pid != 0x1b7f)),
        Step::Write(request(0x0f)),
        rd(bragi(second, pid != 0x1b7f)),
    ];
    if let Some(charge) = charge {
        steps.extend([Step::Write(request(0x10)), rd(bragi(charge, true))]);
    }
    steps
}
#[test]
fn corsair_darkcore_exact_bragi_confirmation_filters_notice_collection() {
    for numbered in [false, true] {
        let i = info(0x1b1c, 0x1b7f, 0xff42);
        let mut notice = i.clone();
        notice.usage = 2;
        notice.path = "notice".into();
        let mut steps = bragi_steps(0x1b7f, 9, 550, 550, None);
        if numbered {
            for step in &mut steps {
                if let Step::Read(Ok(r)) = step
                    && !r.is_empty()
                {
                    r.insert(0, 0);
                }
            }
        }
        let h = FakeHid::new(vec![notice, i], steps);
        let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, Some(55));
        assert_eq!(rows[0].precision, Precision::Exact);
        assert_eq!(rows[0].kind, "mouse");
        assert_eq!(h.opened.lock().unwrap().len(), 1);
        h.done();
    }
}
#[test]
fn corsair_rejects_disagreeing_answers_instead_of_showing_foreign_level() {
    let h = FakeHid::new(
        vec![info(0x1b1c, 0x1b7f, 0xff42)],
        bragi_steps(0x1b7f, 9, 550, 600, None),
    );
    assert!(poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default()).is_empty());
    h.done();
    assert!(bragi_raw(&[8, 2, 0, 99, 0]).is_none());
    assert!(bragi_raw(&[1, 2, 1, 99, 0]).is_none());
}
#[test]
fn virtuoso_variants_use_bare_requests_and_wired_route_order() {
    for (pid, route) in [(0x0a3e, 9), (0x0a3d, 8), (0x0a64, 9), (0x0a62, 8)] {
        let h = FakeHid::new(
            vec![info(0x1b1c, pid, 0xff42)],
            bragi_steps(pid, route, 970, 970, Some(1)),
        );
        let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].level, rows[0].charging), (Some(97), Some(true)));
        assert_eq!(rows[0].kind, "headset");
        h.done();
    }
}
#[test]
fn virtuoso_same_proven_unit_uses_one_icon_and_charging_wins() {
    let mut receiver = info(0x1b1c, 0x0a3e, 0xff42);
    receiver.serial = "UNIT-TEST-SE".into();
    let mut cable = info(0x1b1c, 0x0a3d, 0xff42);
    cable.serial = receiver.serial.clone();
    // Sorted by physical identity/PID: cable first.
    let mut steps = bragi_steps(0x0a3d, 8, 980, 980, Some(1));
    steps.extend(bragi_steps(0x0a3e, 9, 970, 970, Some(2)));
    let h = FakeHid::new(vec![receiver, cable], steps);
    let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].level, rows[0].charging), (Some(98), Some(true)));
    h.done();
}
#[test]
fn pro3_bluetooth_reads_report_four_without_mode_switches() {
    let mut i = info(0x2dc8, 0x6009, 1);
    i.path = "hid#vid&2dc8#pro3".into();
    i.usage = 5;
    let mut r = vec![0; 16];
    r[0] = 4;
    r[14] = 20;
    let h = FakeHid::new(vec![i], vec![rd(r)]);
    let rows = poll(
        &mut HidProvider::new("eightbitdo"),
        &h,
        &FakeClock::default(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(20));
    assert_eq!(rows[0].via, "bluetooth");
    h.done();
}

#[test]
fn virtuoso_multi_receiver_uses_only_measured_bare_session_and_charge_commands() {
    let mut steps = vec![rd(vec![])];
    for (prop, route) in [(0x13, 8), (0x12, 8), (0x12, 9)] {
        steps.push(Step::Write(padded(&[2, route, 2, prop], 64)));
        steps.extend((0..96).map(|_| rd(vec![])));
    }
    let mut battery = bragi_steps(0x0a40, 9, 970, 970, Some(2));
    battery.remove(0);
    steps.extend(battery);
    let h = FakeHid::new(vec![info(0x1b1c, 0x0a40, 0xff42)], steps);
    let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].level, rows[0].charging), (Some(97), Some(false)));
    h.done();
}

#[test]
fn cloud_ii_core_both_connection_ids_share_one_proven_identity_and_charging_wins() {
    let mut cable = info(0x03f0, 0x0795, 0xff13);
    cable.serial = "UNIT-HYPERX".into();
    let mut receiver = info(0x03f0, 0x0995, 0xff13);
    receiver.serial = cable.serial.clone();
    let mut steps = Vec::new();
    for charging in [true, false] {
        for (cmd, r) in [
            (0x89, vec![0x66, 0x89, 0x0e, 0xd7, 48]),
            (0x8a, vec![0x66, 0x8a, u8::from(charging)]),
        ] {
            steps.extend([Step::Write(padded(&[0x66, cmd], 62)), rd(r)]);
        }
    }
    let h = FakeHid::new(vec![receiver, cable], steps);
    let rows = poll(
        &mut HidProvider::new("hyperx_cloud3"),
        &h,
        &FakeClock::default(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(48));
    assert_eq!(rows[0].charging, Some(true));
    h.done();
}

#[test]
fn pulsar_crazylight_both_transports_keep_last_level_indefinitely_while_present() {
    use hb_providers::protocols::pulsar_request;
    for pid in [0x5406, 0x3414] {
        let mut i = info(0x3710, pid, 0xff02);
        i.usage = 2;
        i.output_length = Some(17);
        let mut reply = pulsar_request();
        reply[6] = 75;
        reply[7] = 0;
        reply[16] = 0x55u8.wrapping_sub(reply[..16].iter().fold(0u8, |s, b| s.wrapping_add(*b)));
        let h = FakeHid::new(
            vec![i.clone()],
            vec![rd(vec![]), Step::Write(pulsar_request()), rd(reply)],
        );
        let clock = FakeClock::default();
        let mut p = HidProvider::new("pulsar");
        let first = poll(&mut p, &h, &clock).remove(0);
        h.done();
        clock.sleep(Duration::from_secs(86400));
        let silent = FakeHid::new(
            vec![i],
            vec![rd(vec![]), Step::Write(pulsar_request()), rd(vec![])],
        );
        let row = poll(&mut p, &silent, &clock).remove(0);
        assert_eq!(row.key, first.key);
        assert_eq!(row.level, Some(75));
        assert_eq!(row.timestamp, first.timestamp);
        assert_eq!(row.connection, Connection::Sleeping);
        silent.done();
        assert!(poll(&mut p, &FakeHid::new(vec![], vec![]), &clock).is_empty());
    }
}

#[test]
fn gamebuds_x_uses_existing_bud_status_fields() {
    let mut i = info(0x1038, 0x2317, 0xffc0);
    i.usage = 1;
    let h = FakeHid::new(
        vec![i],
        vec![
            Step::Write(vec![0, 0xb0]),
            rd(vec![0xb0, 0, 0, 3, 3, 70, 80]),
        ],
    );
    let rows = poll(
        &mut HidProvider::new("steelseries"),
        &h,
        &FakeClock::default(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(70));
    h.done();
}

#[test]
fn silent_wlmouse_walks_every_fifth_poll_and_inventory_changes_wake_it() {
    let d = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "wlmouse")
        .unwrap();
    let mut i = info(d.vid, d.pid, 0xff00);
    i.path = "silent-wl".into();
    let clock = FakeClock::default();
    let mut p = HidProvider::new("wlmouse");
    for _ in 0..3 {
        let h = FakeHid::new(vec![i.clone()], vec![rd(vec![])]);
        assert!(poll(&mut p, &h, &clock).is_empty());
        h.done();
    }
    for _ in 0..4 {
        let h = FakeHid::new(vec![i.clone()], vec![]);
        assert!(poll(&mut p, &h, &clock).is_empty());
        assert!(h.opened.lock().unwrap().is_empty());
    }
    let h = FakeHid::new(vec![i.clone()], vec![rd(vec![])]);
    assert!(poll(&mut p, &h, &clock).is_empty());
    assert_eq!(h.opened.lock().unwrap().len(), 1);
    h.done();
    i.path = "new-collection".into();
    let h = FakeHid::new(vec![i], vec![rd(vec![3, 0, 75, 0])]);
    assert_eq!(poll(&mut p, &h, &clock)[0].level, Some(75));
    h.done();
}

#[test]
fn access_errors_are_not_silent_receiver_backoff_and_cancelled_new_protocols_do_no_io() {
    let d = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "wlmouse")
        .unwrap();
    let i = info(d.vid, d.pid, 0xff00);
    let path = i.path.clone();
    let mut p = HidProvider::new("wlmouse");
    let clock = FakeClock::default();
    for _ in 0..8 {
        let h = FakeHid::new(vec![i.clone()], vec![]).fail_open(&path, "exclusive owner");
        assert!(
            p.poll(&h, &context(&clock, &AtomicBool::new(false)))
                .is_err()
        );
        assert_eq!(h.opened.lock().unwrap().len(), 1);
    }
    for (id, vid, pid, page) in [
        ("nzxt", 0x1e71, 0x2101, 0xffca),
        ("steam", 0x28de, 0x1304, 0xff00),
        ("mchose", 0x2023, 0xf013, 0xffff),
        ("razer", 0x1532, 0x0528, 0xff00),
        ("corsair", 0x1b1c, 0x0a40, 0xff42),
    ] {
        let h = FakeHid::new(vec![info(vid, pid, page)], vec![]);
        let cancel = AtomicBool::new(true);
        assert!(
            HidProvider::new(id)
                .poll(&h, &context(&clock, &cancel))
                .unwrap()
                .is_empty()
        );
        assert!(h.opened.lock().unwrap().is_empty());
    }
}

// Passive streams advance the fake clock by their read wait; this verifies late
// reports and bounded silence without sleeping or transmitting anything.
struct Stream {
    frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
    clock: Arc<FakeClock>,
}
impl HidSession for Stream {
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("Steam must be listen-only")
    }
    fn send_feature(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("Steam must not change controller mode")
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        panic!("Steam must not send feature queries")
    }
    fn read(&mut self, _: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        self.clock.sleep(timeout);
        Ok(self.frames.lock().unwrap().pop_front().unwrap_or_default())
    }
}
struct StreamHid {
    infos: Vec<HidInfo>,
    frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
    clock: Arc<FakeClock>,
}
impl HidTransport for StreamHid {
    fn enumerate(&self, vid: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .infos
            .iter()
            .filter(|i| i.vendor_id == vid)
            .cloned()
            .collect())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        Ok(Box::new(Stream {
            frames: self.frames.clone(),
            clock: self.clock.clone(),
        }))
    }
}
#[test]
fn steam_late_battery_sleep_retention_and_disconnect_never_send_packets() {
    let clock = Arc::new(FakeClock::default());
    let cancel = AtomicBool::new(false);
    let mut i = info(0x28de, 0x1304, 0xff00);
    i.interface = 2;
    let mut frames = VecDeque::from([vec![0x79, 2]]);
    frames.extend((0..100).map(|_| vec![]));
    frames.push_back(padded(&[0x43, 1, 95], 15));
    let mut h = StreamHid {
        infos: vec![i],
        frames: Arc::new(Mutex::new(frames)),
        clock: clock.clone(),
    };
    let mut p = HidProvider::new("steam");
    let rows = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(95));
    assert!(rows[0].online());
    clock.sleep(Duration::from_secs(86400));
    let rows = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert_eq!(rows[0].level, Some(95));
    h.frames.lock().unwrap().push_back(vec![0x46, 1]);
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    h.infos.clear();
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
}
#[test]
fn steam_unknown_puck_and_generic_keyboard_collections_are_not_opened() {
    let mut unknown = info(0x28de, 0x1142, 0xff00);
    unknown.interface = 2;
    let mut keyboard = info(0x28de, 0x1304, 1);
    keyboard.interface = 2;
    keyboard.usage = 6;
    let h = FakeHid::new(vec![unknown, keyboard], vec![]);
    assert!(poll(&mut HidProvider::new("steam"), &h, &FakeClock::default()).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
}
#[test]
fn parsers_reject_truncation_and_foreign_commands_and_keep_coarse_registers() {
    for length in 0..27 {
        assert!(nzxt_telemetry(&telemetry(50, false)[..length]).is_none());
    }
    assert!(nzxt_telemetry(&telemetry(101, false)).is_none());
    for length in 0..15 {
        assert!(steam_battery(&padded(&[0x43, 1, 50], 15)[..length]).is_none());
    }
    for state in [0, 3, 9, 255] {
        assert!(steam_battery(&padded(&[0x43, state, 50], 15)).is_none());
    }
    assert!(mchose_a5(&[0xa1, 0, 0, 0, 0, 0x82, 0, 50, 0]).is_none());
    assert_eq!(
        mchose_a5(&[0, 0xa1, 0, 0, 0, 0, 0x83, 0, 255])
            .unwrap()
            .level,
        100
    );
    assert_eq!(
        logitech_register_battery(0x0d, &[50, 0, 0x50])
            .unwrap()
            .charging,
        Some(true)
    );
    assert!(logitech_register_battery(0x0d, &[0, 0, 0x30]).is_none());
    for (code, level) in [(7, 90), (5, 50), (3, 20), (1, 5)] {
        let b = logitech_register_battery(7, &[code, 0x21]).unwrap();
        assert_eq!(b.level, level);
        assert!(b.coarse);
        assert_eq!(b.charging, Some(true));
    }
}
