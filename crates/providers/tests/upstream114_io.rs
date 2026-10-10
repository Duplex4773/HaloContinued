mod common;
use common::*;
use hb_core::*;
use hb_providers::{HidProvider, catalog::DEVICES, protocols as p};
use std::{sync::atomic::AtomicBool, time::Duration};
fn poll(provider: &mut HidProvider, h: &FakeHid, clock: &FakeClock) -> Vec<Reading> {
    provider
        .poll(h, &context(clock, &AtomicBool::new(false)))
        .unwrap()
}
fn read(r: &[u8]) -> Step {
    Step::Read(Ok(r.to_vec()))
}
fn device(vid: u16, pid: u16, page: u16, path: &str) -> HidInfo {
    let mut d = info(vid, pid, page);
    d.path = path.into();
    d.interface = 3;
    d.container = Some("synthetic-station".into());
    d
}
#[test]
fn all_gwolves_own_receiver_and_cable_ids_select_the_documented_exchange() {
    for d in DEVICES
        .iter()
        .filter(|d| d.provider == "gwolves" && !d.variant.is_empty())
    {
        let old = d.variant.starts_with("old:");
        let wired = d.variant.contains(":wired:");
        let mut i = device(d.vid, d.pid, 0xff00, "mouse");
        i.feature_length = Some(65);
        let (request, reply) = if old {
            (
                p::padded(&[0, 0, 2, 0x8f, u8::from(!wired)], 65),
                vec![0, 0xa1, 2, 0x8f, 0, 1, 73],
            )
        } else {
            (
                p::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65),
                vec![0, 0xa1, 0, 0, 2, 0, 0x83, 1, 73],
            )
        };
        let h = FakeHid::new(
            vec![i],
            vec![Step::Send(request), Step::Feature(0, 65, Ok(reply))],
        );
        let rows = poll(&mut HidProvider::new("gwolves"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1, "{:04x}", d.pid);
        assert_eq!(rows[0].level, Some(73));
        assert_eq!(rows[0].name, d.name);
        assert_eq!(
            rows[0].key,
            format!(
                "gwolves:{}:SYNTHETIC-STATION",
                d.variant.rsplit(':').next().unwrap()
            )
        );
        h.done();
    }
}
#[test]
fn gwolves_ace_cable_wins_and_distinct_models_stay_separate() {
    let mut cable = device(0x33e4, 0x5804, 0xff00, "cable");
    cable.feature_length = Some(65);
    let mut radio = cable.clone();
    radio.product_id = 0x5803;
    radio.path = "radio".into();
    let mut other = cable.clone();
    other.product_id = 0x5403;
    other.path = "other".into();
    let h = FakeHid::new(
        vec![radio, cable, other],
        vec![
            Step::Send(p::padded(&[0, 0, 2, 0x8f, 0], 65)),
            Step::Feature(0, 65, Ok(vec![0xa1, 2, 0x8f, 0, 0, 44])),
            Step::Send(p::padded(&[0, 0, 2, 0x8f, 1], 65)),
            Step::Feature(0, 65, Ok(vec![0xa1, 2, 0x8f, 0, 0, 55])),
        ],
    );
    let rows = poll(&mut HidProvider::new("gwolves"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|r| r.name == "G-Wolves HSK Pro ACE")
            .unwrap()
            .level,
        Some(44)
    );
    assert_eq!(*h.opened.lock().unwrap(), ["cable", "other"]);
    h.done();
}
fn razer_response(cmd: u8, value: u8) -> Vec<u8> {
    let mut r = vec![0; 91];
    r[1] = 2;
    r[7] = 7;
    r[8] = cmd;
    r[10] = value;
    r
}
#[test]
fn every_new_razer_keyboard_uses_known_tid_and_preferred_interface() {
    for (pid, tid, interface) in [
        (0x290, 0x9f, 2),
        (0x292, 0x1f, 3),
        (0x296, 0x9f, 2),
        (0x298, 0x1f, 3),
        (0x271, 0x9f, 3),
        (0x258, 0x1f, 3),
        (0x2ba, 0x9f, 3),
        (0x2b9, 0x1f, 3),
        (0x2d5, 0x9f, 2),
        (0x2d7, 0x1f, 3),
    ] {
        let mut wanted = device(0x1532, pid, 1, "preferred");
        wanted.interface = interface;
        let mut other = wanted.clone();
        other.interface = 0;
        other.path = "other".into();
        let h = FakeHid::new(
            vec![other, wanted],
            vec![
                Step::Send(p::razer_request(tid, 0x80)),
                Step::Feature(0, 91, Ok(razer_response(0x80, 181))),
                Step::Send(p::razer_request(tid, 0x84)),
                Step::Feature(0, 91, Ok(razer_response(0x84, 1))),
            ],
        );
        let rows = poll(&mut HidProvider::new("razer"), &h, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "keyboard");
        assert_eq!(rows[0].level, Some(71));
        assert_eq!(*h.opened.lock().unwrap(), ["preferred"]);
        h.done();
    }
}
#[test]
fn cloud3s_output_fallback_and_cross_collection_reply_then_cached_writer() {
    let infos = vec![
        device(0x03f0, 0x02cc, 0xff00, "configuration"),
        device(0x03f0, 0x02cc, 0xff01, "input"),
    ];
    let battery = p::padded(&[0xc, 2, 3, 1, 0, 6], 64);
    let charging = p::padded(&[0xc, 2, 3, 1, 0, 0x48], 64);
    let h = FakeHid::new(
        infos.clone(),
        vec![
            Step::WriteError(battery.clone(), "report ID absent"),
            Step::Write(battery.clone()),
            read(&[]),
            read(&[0xc, 0, 0, 0, 0, 6, 62]),
            Step::Write(charging.clone()),
            read(&[]),
            read(&[0xd, 0, 0, 0, 10, 2, 0]),
        ],
    );
    let mut provider = HidProvider::new("hyperx_cloud3s");
    let rows = poll(&mut provider, &h, &FakeClock::default());
    assert_eq!((rows[0].level, rows[0].charging), (Some(62), Some(true)));
    h.done();
    let h = FakeHid::new(
        infos,
        vec![
            Step::Write(battery),
            read(&[0xd, 0, 0, 0, 1, 55, 0]),
            Step::Write(charging),
            read(&[0xc, 0, 0, 0, 0, 0x48, 255]),
        ],
    );
    let rows = poll(&mut provider, &h, &FakeClock::default());
    assert_eq!((rows[0].level, rows[0].charging), (Some(55), None));
    h.done();
}
#[test]
fn cloud3s_invalid_battery_times_out_without_charging_command_or_icon() {
    let mut steps = vec![
        Step::Write(p::padded(&[0xc, 2, 3, 1, 0, 6], 64)),
        read(&[0xc, 0, 0, 0, 0, 6, 255]),
    ];
    steps.extend((0..99).map(|_| read(&[])));
    let h = FakeHid::new(vec![device(0x03f0, 0x06be, 0xff00, "station")], steps);
    let clock = FakeClock::default();
    assert!(poll(&mut HidProvider::new("hyperx_cloud3s"), &h, &clock).is_empty());
    assert_eq!(clock.monotonic(), Duration::from_secs(1));
    h.done();
}
fn direct_elite(level: u8, power: u8, charge: u8) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[..2].copy_from_slice(&[1, 0xb0]);
    r[6] = level;
    r[14] = power;
    r[15] = charge;
    r
}
#[test]
fn elite_direct_status_matches_issue138_and_offline_hides_icon() {
    for (level, power, charge, want) in [
        (31, 0, 2, Some((31, true))),
        (0, 0, 8, Some((0, false))),
        (76, 1, 2, None),
        (255, 1, 2, None),
    ] {
        let h = FakeHid::new(
            vec![device(0x1038, 0x2244, 0xffc0, "station")],
            vec![
                Step::Write(p::padded(&[1, 0xb0], 64)),
                Step::Read(Ok(direct_elite(level, power, charge))),
            ],
        );
        let rows = poll(
            &mut HidProvider::new("steelseries_elite"),
            &h,
            &FakeClock::default(),
        );
        assert_eq!(
            rows.first()
                .map(|r| (r.level.unwrap(), r.charging.unwrap())),
            want
        );
        h.done();
    }
}
#[test]
fn elite_split_notifications_report_cable_charging() {
    let h = FakeHid::new(
        vec![
            device(0x1038, 0x2244, 0xffc0, "configuration"),
            device(0x1038, 0x2244, 0xffc1, "input"),
        ],
        vec![
            Step::Write(p::padded(&[1, 0xb0], 64)),
            read(&[7, 0xb7, 49, 100, 8]),
            read(&[7, 0xb5, 0, 0, 2]),
        ],
    );
    let rows = poll(
        &mut HidProvider::new("steelseries_elite"),
        &h,
        &FakeClock::default(),
    );
    assert_eq!((rows[0].level, rows[0].charging), (Some(49), Some(true)));
    h.done();
}
fn direct(index: u8, function: u8, params: &[u8], reply: &[u8]) -> Vec<Step> {
    let mut request = vec![index, function | 1];
    request.extend(params);
    let mut response = vec![index, function | 1];
    response.extend(reply);
    vec![
        Step::Write(p::centurion_frame(&request)),
        Step::Read(Ok(p::centurion_frame(&response))),
    ]
}
fn bridge(index: u8, function: u8, params: &[u8], reply: &[u8]) -> Vec<Step> {
    let mut sub = vec![0, index, function | 1];
    sub.extend(params);
    let mut request = vec![3, 0x11, 0, sub.len() as u8];
    request.extend(sub);
    let mut response = vec![3, 0x10, 0, (reply.len() + 3) as u8, 0, index, function | 1];
    response.extend(reply);
    vec![
        Step::Write(p::centurion_frame(&request)),
        Step::Read(Ok(p::centurion_frame(&[3, 0x11]))),
        Step::Read(Ok(p::centurion_frame(&response))),
    ]
}
fn discover(battery: bool) -> Vec<Step> {
    let mut s = direct(0, 0, &[0, 1], &[1]);
    s.extend(direct(1, 0, &[], &[4]));
    for n in 0..4 {
        s.extend(direct(1, 0x10, &[n], &[0, 0, if n == 3 { 3 } else { n }]));
    }
    s.extend(bridge(0, 0, &[0, 1], &[1]));
    s.extend(bridge(1, 0, &[], &[5]));
    for n in 0..5 {
        s.extend(bridge(
            1,
            0x10,
            &[n],
            &[0, if n == 4 && battery { 1 } else { 0 }, n],
        ));
    }
    s
}
#[test]
fn centurion_discovers_feature_index_caches_and_accepts_charge_complete() {
    let mut s = discover(true);
    s.extend(bridge(4, 0, &[], &[83, 0, 3]));
    let infos = vec![device(0x046d, 0x0af7, 0xffa0, "centurion")];
    let h = FakeHid::new(infos.clone(), s);
    let mut provider = HidProvider::new("logitech_centurion");
    let rows = poll(&mut provider, &h, &FakeClock::default());
    assert_eq!((rows[0].level, rows[0].charging), (Some(83), Some(true)));
    h.done();
    let h = FakeHid::new(infos, bridge(4, 0, &[], &[255, 0, 0]));
    assert!(poll(&mut provider, &h, &FakeClock::default()).is_empty());
    h.done();
}
#[test]
fn centurion_legacy_fallback_requires_battery_signature_and_hides_power_off() {
    for off in [false, true] {
        let mut s = discover(false);
        s.push(Step::Write(p::padded(
            &[0x51, 8, 0, 3, 0x1a, 0, 3, 0, 4, 0x0a],
            64,
        )));
        if off {
            s.push(read(&[0x51, 5, 0, 0, 0, 0, 0]));
        } else {
            let mut r = vec![0; 13];
            r[0] = 0x51;
            r[1] = 0xb;
            r[8] = 4;
            r[10] = 57;
            r[12] = 2;
            s.push(Step::Read(Ok(r)));
        }
        let h = FakeHid::new(vec![device(0x046d, 0x0af7, 0xffa0, "centurion")], s);
        let rows = poll(
            &mut HidProvider::new("logitech_centurion"),
            &h,
            &FakeClock::default(),
        );
        assert_eq!(
            rows.first().and_then(|r| r.level),
            if off { None } else { Some(57) }
        );
        h.done();
    }
}
#[test]
fn unknown_ids_wrong_elite_interface_and_centurion_collection_never_open() {
    for (family, infos) in [
        (
            "steelseries_elite",
            vec![device(0x1038, 0x2246, 0xffc0, "unknown"), {
                let mut d = device(0x1038, 0x2244, 0xffc0, "wrong-interface");
                d.interface = 4;
                d
            }],
        ),
        (
            "logitech_centurion",
            vec![
                device(0x046d, 0x0af7, 0xff43, "hidpp"),
                device(0x046d, 0x9999, 0xffa0, "unknown"),
            ],
        ),
        (
            "hyperx_cloud3s",
            vec![device(0x03f0, 0xffff, 0xff00, "unknown")],
        ),
    ] {
        let h = FakeHid::new(infos, vec![]);
        assert!(poll(&mut HidProvider::new(family), &h, &FakeClock::default()).is_empty());
        assert!(h.opened.lock().unwrap().is_empty());
        h.done();
    }
}
#[test]
fn new_catalog_devices_do_not_expand_polling_write_allowlists() {
    use hb_providers::{logitech_controls, razer_controls};
    for d in DEVICES.iter().filter(|d| {
        d.provider == "razer"
            && [
                0x290, 0x292, 0x296, 0x298, 0x271, 0x258, 0x2ba, 0x2b9, 0x2d5, 0x2d7,
            ]
            .contains(&d.pid)
    }) {
        let mut i = device(d.vid, d.pid, 0xff00, "keyboard");
        i.feature_length = Some(91);
        assert!(razer_controls::protocol(&i).is_none());
    }
    let mut i = device(0x046d, 0x0af7, 0xff00, "headset");
    i.usage = 2;
    assert!(!logitech_controls::candidate(&i));
}
#[test]
fn new_headsets_report_transport_failures_and_preserve_family_identity_metadata() {
    for (family, vid, pid, page) in [
        ("hyperx_cloud3s", 0x03f0, 0x02cc, 0xff00),
        ("steelseries_elite", 0x1038, 0x2244, 0xffc0),
        ("logitech_centurion", 0x046d, 0x0af7, 0xffa0),
    ] {
        let h = FakeHid::new(vec![device(vid, pid, page, "blocked")], vec![])
            .fail_open("blocked", "access denied");
        let clock = FakeClock::default();
        assert!(
            HidProvider::new(family)
                .poll(&h, &context(&clock, &AtomicBool::new(false)))
                .is_err()
        );
        h.done();
    }
    let mut i = device(0x1038, 0x2244, 0xffc0, "station");
    i.serial = "synthetic-serial".into();
    let h = FakeHid::new(
        vec![i],
        vec![
            Step::Write(p::padded(&[1, 0xb0], 64)),
            Step::Read(Ok(direct_elite(31, 8, 8))),
        ],
    );
    let rows = poll(
        &mut HidProvider::new("steelseries_elite"),
        &h,
        &FakeClock::default(),
    );
    assert_eq!(rows[0].source, "steelseries_elite");
    assert_eq!(rows[0].serial.as_deref(), Some("synthetic-serial"));
    assert_eq!(rows[0].container.as_deref(), Some("synthetic-station"));
    h.done();
}
#[test]
fn cancelled_and_expired_new_headset_polls_send_no_commands() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(true);
    let h = FakeHid::new(vec![device(0x03f0, 0x02cc, 0xff00, "station")], vec![]);
    assert!(
        HidProvider::new("hyperx_cloud3s")
            .poll(&h, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    let active = AtomicBool::new(false);
    let ctx = PollContext {
        deadline: Duration::ZERO,
        ..context(&clock, &active)
    };
    assert!(
        HidProvider::new("hyperx_cloud3s")
            .poll(&h, &ctx)
            .unwrap()
            .is_empty()
    );
    assert!(h.opened.lock().unwrap().is_empty());
    h.done();
}
#[test]
fn split_headset_all_read_errors_return_failure_and_flood_window_is_bounded() {
    let request = p::padded(&[0xc, 2, 3, 1, 0, 6], 64);
    let mut steps = vec![Step::Write(request.clone())];
    steps.extend((0..100).map(|_| Step::Read(Err("disconnected"))));
    let h = FakeHid::new(vec![device(0x03f0, 0x02cc, 0xff00, "station")], steps);
    let clock = FakeClock::default();
    assert!(
        HidProvider::new("hyperx_cloud3s")
            .poll(&h, &context(&clock, &AtomicBool::new(false)))
            .is_err()
    );
    assert_eq!(clock.monotonic(), Duration::from_secs(1));
    h.done();
    let mut steps = vec![Step::Write(request)];
    steps.extend((0..100).map(|_| read(&[0xc, 0, 0, 0, 0, 0x77, 73])));
    let h = FakeHid::new(vec![device(0x03f0, 0x02cc, 0xff00, "station")], steps);
    let clock = FakeClock::default();
    assert!(poll(&mut HidProvider::new("hyperx_cloud3s"), &h, &clock).is_empty());
    assert_eq!(clock.monotonic(), Duration::from_secs(1));
    h.done();
}
#[test]
fn centurion_ack_without_headset_answer_is_offline_and_keeps_discovery_cache() {
    let infos = vec![device(0x046d, 0x0af7, 0xffa0, "centurion")];
    let mut provider = HidProvider::new("logitech_centurion");
    let mut steps = discover(true);
    steps.extend(bridge(4, 0, &[], &[57, 0, 0]));
    let h = FakeHid::new(infos.clone(), steps);
    assert_eq!(
        poll(&mut provider, &h, &FakeClock::default())[0].level,
        Some(57)
    );
    h.done();
    let mut steps = vec![
        Step::Write(p::centurion_frame(&[3, 0x11, 0, 3, 0, 4, 1])),
        Step::Read(Ok(p::centurion_frame(&[3, 0x11]))),
    ];
    steps.extend((0..149).map(|_| read(&[])));
    let h = FakeHid::new(infos.clone(), steps);
    assert!(poll(&mut provider, &h, &FakeClock::default()).is_empty());
    h.done();
    let h = FakeHid::new(infos, bridge(4, 0, &[], &[58, 0, 0]));
    assert_eq!(
        poll(&mut provider, &h, &FakeClock::default())[0].level,
        Some(58)
    );
    h.done();
}
