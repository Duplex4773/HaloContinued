//! Hardware-free strict transactions for remaining provider families.
mod common;
use common::*;
use hb_core::*;
use hb_providers::{
    HidProvider,
    catalog::DEVICES,
    protocols::{padded, pulsar_request},
};
use std::{sync::atomic::AtomicBool, time::Duration};
fn d(family: &str, pid: u16, page: u16, usage: u16, iface: i32, path: &str) -> HidInfo {
    let x = DEVICES
        .iter()
        .find(|d| d.provider == family && d.pid == pid)
        .unwrap();
    let mut i = info(x.vid, pid, page);
    i.usage = usage;
    i.interface = iface;
    i.path = path.into();
    i.container = Some(format!("fixture-{family}-{pid:04x}"));
    i
}
fn rd(b: &[u8]) -> Step {
    Step::Read(Ok(b.to_vec()))
}
fn sized(n: usize, t: u64, b: &[u8]) -> Step {
    Step::ReadSized(n, Duration::from_millis(t), Ok(b.to_vec()))
}
fn poll(f: &'static str, h: &FakeHid) -> Vec<Reading> {
    HidProvider::new(f)
        .poll(h, &context(&FakeClock::default(), &AtomicBool::new(false)))
        .unwrap()
}
fn asus(level: u8, charging: u8, numbered: bool) -> Vec<u8> {
    let mut r = vec![0; 10];
    r[0] = 0x12;
    r[1] = 7;
    r[4] = level;
    r[9] = charging;
    if numbered {
        r.insert(0, 0)
    }
    r
}
fn asus_steps(r: &[u8]) -> Vec<Step> {
    vec![
        sized(65, 0, &[]),
        Step::Write(padded(&[0, 0x12, 7], 65)),
        sized(65, 300, r),
    ]
}
#[test]
fn asus_exact_request_reference_reading_and_strict_vendor_interface() {
    let h = FakeHid::new(
        vec![
            d("asus", 0x1a72, 1, 2, 0, "mouse"),
            d("asus", 0x1a72, 0xffc1, 1, 2, "wrong-iface"),
            d("asus", 0x1a72, 0xff01, 1, 0, "control"),
        ],
        asus_steps(&asus(87, 0, false)),
    );
    let r = poll("asus", &h);
    assert_eq!(
        (
            &*r[0].name,
            r[0].level,
            r[0].charging,
            &*r[0].kind,
            &*r[0].source
        ),
        (
            "ROG Gladius III Aimpoint",
            Some(87),
            Some(false),
            "mouse",
            "asus"
        )
    );
    assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
    h.done();
}
#[test]
fn asus_all_catalog_models_percent_steps_report_id_and_ranges() {
    for x in DEVICES.iter().filter(|d| d.provider == "asus") {
        for numbered in [false, true] {
            let raw = if x.parameter == 25 { 2 } else { 87 };
            let h = FakeHid::new(
                vec![d("asus", x.pid, 0xff01, 1, 0, "control")],
                asus_steps(&asus(raw, 1, numbered)),
            );
            let r = poll("asus", &h);
            assert_eq!(
                (&*r[0].name, r[0].level, r[0].charging),
                (
                    x.name,
                    Some(if x.parameter == 25 { 50 } else { 87 }),
                    Some(true)
                )
            );
            if x.parameter == 25 {
                assert_eq!(r[0].approx, Some("about 50%, charging".into()));
            }
            h.done();
        }
    }
    for (pid, raw, charge) in [(0x1a72, 0, 0), (0x1a72, 101, 0), (0x1960, 5, 0)] {
        let h = FakeHid::new(
            vec![d("asus", pid, 0xff01, 1, 0, "control")],
            asus_steps(&asus(raw, charge, false)),
        );
        assert!(poll("asus", &h).is_empty());
        h.done();
    }
    let h = FakeHid::new(
        vec![d("asus", 0x1a72, 0xff01, 1, 0, "control")],
        asus_steps(&asus(0, 1, false)),
    );
    assert_eq!(poll("asus", &h)[0].level, Some(0));
    h.done();
}
#[test]
fn asus_drain_noise_retry_and_terminal_errors() {
    let mut s = (0..16).map(|_| sized(65, 0, &[1, 3])).collect::<Vec<_>>();
    s.extend([
        Step::Write(padded(&[0, 0x12, 7], 65)),
        sized(65, 300, &[]),
        Step::Write(padded(&[0, 0x12, 7], 65)),
        rd(&[1, 2]),
        rd(&asus(50, 0, false)),
    ]);
    let h = FakeHid::new(vec![d("asus", 0x1a72, 0xff01, 1, 0, "control")], s);
    assert_eq!(poll("asus", &h)[0].level, Some(50));
    h.done();
    for terminal in [vec![0xff, 0xaa], vec![0, 0xff, 0xaa], vec![0; 65]] {
        let h = FakeHid::new(
            vec![d("asus", 0x1a72, 0xff01, 1, 0, "control")],
            asus_steps(&terminal),
        );
        assert!(poll("asus", &h).is_empty());
        h.done();
    }
    let mut s = vec![sized(65, 0, &[])];
    for _ in 0..3 {
        s.extend([Step::Write(padded(&[0, 0x12, 7], 65)), rd(&[])]);
    }
    let h = FakeHid::new(vec![d("asus", 0x1a72, 0xff01, 1, 0, "control")], s);
    assert!(poll("asus", &h).is_empty());
    h.done();
}
#[test]
fn asus_shared_serial_cable_charging_wins_both_enumeration_orders() {
    for reverse in [false, true] {
        let mut a = d("asus", 0x1a72, 0xff01, 1, 0, "a-receiver");
        let mut b = d("asus", 0x1a70, 0xff01, 1, 0, "b-cable");
        a.serial = "same-mouse".into();
        b.serial = a.serial.clone();
        let mut i = vec![a, b];
        if reverse {
            i.reverse();
        }
        let mut s = asus_steps(&asus(80, 0, false));
        s.extend(asus_steps(&asus(81, 1, false)));
        let h = FakeHid::new(i, s);
        let r = poll("asus", &h);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].level, r[0].charging), (Some(81), Some(true)));
        h.done();
    }
}
fn pulsar(level: u8, charge: u8, cmd: u8) -> Vec<u8> {
    let mut r = vec![0; 17];
    r[0] = 8;
    r[1] = cmd;
    r[6] = level;
    r[7] = charge;
    r[8] = 0x0f;
    r[9] = 0x3c;
    r[16] = 0x55u8.wrapping_sub(r[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    r
}
fn pulsar_steps(reply: &[u8]) -> Vec<Step> {
    vec![
        sized(17, 30, &[]),
        Step::Write(pulsar_request()),
        sized(17, 250, reply),
    ]
}
#[test]
fn pulsar_all_claimed_ids_exact_frame_names_and_charging() {
    for x in DEVICES.iter().filter(|d| d.provider == "pulsar") {
        let h = FakeHid::new(
            vec![
                d("pulsar", x.pid, 0xff02, 2, 1, "control"),
                d("pulsar", x.pid, 1, 6, 2, "keyboard"),
            ],
            pulsar_steps(&pulsar(33, 1, 4)),
        );
        let r = poll("pulsar", &h);
        assert_eq!(
            (&*r[0].name, r[0].level, r[0].charging, &*r[0].kind),
            (x.name, Some(33), Some(true), "mouse")
        );
        assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
        h.done();
    }
    assert_eq!(pulsar_request().len(), 17);
    assert_eq!(
        pulsar_request().iter().fold(0u8, |a, b| a.wrapping_add(*b)),
        0x55
    );
}
#[test]
fn pulsar_output_capability_selects_fit_or_unknown_and_skips_incompatible() {
    for next in [Some(17), None] {
        let mut wrong = d("pulsar", 0xf58a, 0xff02, 2, 1, "a-control");
        wrong.output_length = Some(64);
        let mut yes = d("pulsar", 0xf58a, 0xff04, 2, 1, "b-fit");
        yes.output_length = next;
        let h = FakeHid::new(vec![wrong, yes], pulsar_steps(&pulsar(72, 0, 4)));
        assert_eq!(poll("pulsar", &h)[0].level, Some(72));
        assert_eq!(*h.opened.lock().unwrap(), vec!["b-fit"]);
        h.done();
    }
    let mut wrong = d("pulsar", 0xf58a, 0xff05, 0, 1, "a-short");
    wrong.output_length = Some(8);
    let mut yes = d("pulsar", 0xf58a, 0xff03, 0, 1, "b-fit");
    yes.output_length = Some(17);
    let h = FakeHid::new(vec![wrong, yes], pulsar_steps(&pulsar(48, 0, 4)));
    assert_eq!(poll("pulsar", &h)[0].level, Some(48));
    assert_eq!(*h.opened.lock().unwrap(), vec!["b-fit"]);
    h.done();
}
#[test]
fn pulsar_noise_checksum_range_short_and_sleep_are_rejected() {
    let mut s = vec![
        rd(&[]),
        Step::Write(pulsar_request()),
        rd(&pulsar(78, 0, 10)),
        rd(&pulsar(41, 0, 4)),
    ];
    let h = FakeHid::new(
        vec![d("pulsar", 0xf58a, 0xff02, 2, 1, "control")],
        s.clone(),
    );
    assert_eq!(poll("pulsar", &h)[0].level, Some(41));
    h.done();
    let mut bad = pulsar(78, 0, 4);
    bad[16] ^= 1;
    for reply in [bad, pulsar(200, 0, 4), vec![8, 4, 0], vec![]] {
        s = vec![rd(&[]), Step::Write(pulsar_request()), rd(&reply)];
        if !reply.is_empty() {
            s.push(rd(&[]));
        }
        let h = FakeHid::new(
            vec![d("pulsar", 0xf58a, 0xff02, 2, 1, "control")],
            s.clone(),
        );
        assert!(poll("pulsar", &h).is_empty());
        h.done();
    }
}
#[test]
fn pulsar_literal_hitscan_captures_are_read_as_raw_levels() {
    for r in [
        vec![8, 4, 0, 0, 0, 2, 100, 0, 0x11, 0x29, 0, 0, 0, 0, 0, 0, 0xa9],
        vec![8, 4, 0, 0, 0, 2, 75, 0, 0x10, 0x73, 0, 0, 0, 0, 0, 0, 0x79],
        vec![8, 4, 0, 0, 0, 2, 100, 0, 0x11, 0x2d, 0, 0, 0, 0, 0, 0, 0xa5],
    ] {
        let h = FakeHid::new(
            vec![d("pulsar", 0x200, 0xff02, 2, 1, "control")],
            pulsar_steps(&r),
        );
        assert_eq!(poll("pulsar", &h)[0].level, Some(r[6]));
        h.done();
    }
}
#[test]
fn astro_exact_request_strict_collection_command_noise_dock_and_invalid_levels() {
    for (level, dock) in [(0, 0), (75, 1), (100, 0), (101, 1)] {
        let h = FakeHid::new(
            vec![
                d("astro", 0xb1c, 1, 1, 8, "wrong"),
                d("astro", 0xb1c, 0xff32, 0x74, 3, "control"),
            ],
            vec![
                Step::Write(padded(&[2, 12, 3, 0, 6, 12], 64)),
                rd(&[2, 12, 6, 0, 5, 12, 99, 0, 0]),
                rd(&[2, 12, 6, 0, 6, 12, level, 0, dock]),
            ],
        );
        let r = poll("astro", &h);
        assert_eq!(
            r.first().map(|r| (r.level.unwrap(), r.charging.unwrap())),
            if level <= 100 {
                Some((level, dock != 0))
            } else {
                None
            }
        );
        assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
        h.done();
    }
}
#[test]
fn keychron_feature_retry_delay_exact_interrupt_reads_and_strict_interface() {
    let clock = FakeClock::default();
    let h = FakeHid::new(
        vec![
            d("keychron", 0xd028, 1, 2, 0, "wrong"),
            d("keychron", 0xd028, 0xff00, 1, 4, "control"),
        ],
        vec![
            Step::SendError(padded(&[0xb3, 6], 64), "transient"),
            Step::Send(padded(&[0xb3, 6], 64)),
            sized(64, 500, &[0xb4, 5]),
            Step::Send(padded(&[0xb3, 6], 64)),
            sized(64, 500, &{
                let mut r = vec![0; 64];
                r[0] = 0xb4;
                r[1] = 6;
                r[20] = 82;
                r
            }),
        ],
    );
    let r = HidProvider::new("keychron")
        .poll(&h, &context(&clock, &AtomicBool::new(false)))
        .unwrap();
    assert_eq!(r[0].level, Some(82));
    assert_eq!(clock.monotonic(), Duration::from_millis(200));
    assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
    h.done();
}
#[test]
fn jbl_read_only_reports_ignore_mute_power_and_keep_last_level_indefinitely() {
    let mut p = HidProvider::new("jbl");
    let clock = FakeClock::default();
    let c = AtomicBool::new(false);
    let s = vec![
        sized(64, 0, &[0x2f, 1]),
        sized(64, 0, &[9, 1]),
        sized(64, 0, &[8, 95]),
        sized(64, 0, &[]),
        sized(64, 0, &[]),
    ];
    let h = FakeHid::new(
        vec![
            d("jbl", 0x2088, 0xff13, 1, 5, "control"),
            d("jbl", 0x2088, 1, 2, 1, "wrong"),
        ],
        s,
    );
    let live = p.poll(&h, &context(&clock, &c)).unwrap();
    assert_eq!(live[0].level, Some(95));
    clock.0.store(900000, std::sync::atomic::Ordering::Relaxed);
    let sleepy = p.poll(&h, &context(&clock, &c)).unwrap();
    assert_eq!(sleepy[0].key, live[0].key);
    assert_eq!(sleepy[0].connection, Connection::Sleeping);
    assert_eq!(sleepy[0].level, Some(95));
    assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
    h.done();
}
#[test]
fn wlmouse_feature_lengths_retry_and_passive_heartbeat_fallback() {
    let mut s = vec![
        Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
        Step::Feature(0, 65, Err("wrong length")),
        Step::Feature(0, 64, Ok(vec![0xa2, 0, 2, 2, 0, 0x83, 1, 81])),
    ];
    let h = FakeHid::new(
        vec![d("wlmouse", 0xa880, 0xffff, 1, 2, "control")],
        s.clone(),
    );
    assert_eq!(poll("wlmouse", &h)[0].level, Some(81));
    h.done();
    s = vec![Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65))];
    for _ in 0..15 {
        s.extend([
            Step::Feature(0, 65, Ok(vec![])),
            Step::Feature(0, 64, Ok(vec![])),
        ]);
    }
    s.extend([rd(&[0x2f, 1]), rd(&[3, 0, 64, 1])]);
    let h = FakeHid::new(vec![d("wlmouse", 0xa880, 0xffff, 1, 2, "control")], s);
    let r = poll("wlmouse", &h);
    assert_eq!((r[0].level, r[0].charging), (Some(64), Some(true)));
    h.done();
}

#[test]
fn unknown_misc_devices_and_strict_collections_never_get_requests() {
    for f in ["asus", "pulsar", "astro", "keychron", "jbl", "wlmouse"] {
        let known = DEVICES.iter().find(|d| d.provider == f).unwrap();
        let mut i = info(known.vid, 0x1234, 0xffff);
        i.feature_length = Some(65);
        let h = FakeHid::new(vec![i], vec![]);
        assert!(poll(f, &h).is_empty());
        assert!(h.opened.lock().unwrap().is_empty());
        h.done();
    }
}

#[test]
fn asus_original_parser_edges_are_verified_through_transactions() {
    for (pid, raw, charge, expected, label) in [
        (0x1a72, 40, 1, 40, None),
        (0x1960, 3, 0, 75, Some("about 75%")),
        (0x1960, 4, 1, 100, Some("about 100%, charging")),
        (0x1960, 2, 0, 50, Some("about 50%")),
    ] {
        let h = FakeHid::new(
            vec![d("asus", pid, 0xff01, 1, 0, "control")],
            asus_steps(&asus(raw, charge, false)),
        );
        let r = poll("asus", &h);
        assert_eq!(r[0].level, Some(expected));
        assert_eq!(r[0].approx.as_deref(), label);
        h.done();
    }
    let h = FakeHid::new(
        vec![d("asus", 0x1a72, 0xff01, 1, 0, "control")],
        asus_steps(&[0x12, 7, 0, 0]),
    );
    assert!(poll("asus", &h).is_empty());
    h.done();
    assert!(
        DEVICES
            .iter()
            .filter(|d| d.provider == "asus")
            .all(|d| [1, 25].contains(&d.parameter))
    );
}
#[test]
fn pulsar_capability_rejections_and_voltage_have_actionable_diagnostics() {
    let mut wrong = d("pulsar", 0xf58a, 0xff02, 2, 1, "a-control");
    wrong.output_length = Some(64);
    let mut yes = d("pulsar", 0xf58a, 0xff04, 2, 1, "b-fit");
    yes.output_length = Some(17);
    let h = FakeHid::new(vec![wrong, yes], pulsar_steps(&pulsar(78, 0, 4)));
    let mut p = HidProvider::new("pulsar");
    let r = p
        .poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)))
        .unwrap();
    assert_eq!(r[0].key, "pulsar:3554f58a:FIXTURE-PULSAR-F58A");
    assert_eq!(r[0].level, Some(78));
    let diag = p.diagnostics().join("\n");
    assert!(diag.contains("ff02:0002"));
    assert!(diag.contains("output=64"));
    assert!(diag.contains("17-byte frame"));
    assert!(diag.contains("skipped"));
    assert!(diag.contains("output=17"));
    assert!(diag.contains("3900 mV"));
    h.done();
    let mut bad = pulsar(78, 0, 4);
    bad[16] ^= 1;
    let mut s = pulsar_steps(&bad);
    s.push(rd(&[]));
    let h = FakeHid::new(vec![d("pulsar", 0xf58a, 0xff02, 2, 1, "control")], s);
    assert!(
        p.poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)))
            .unwrap()
            .is_empty()
    );
    assert!(p.diagnostics().iter().any(|s| s.contains("no power reply")));
    h.done();
}

#[test]
fn pulsar_fitting_control_and_interface_one_fallback_are_selected() {
    for (page, usage, length, path) in [
        (0xff02, 2, Some(17), "control"),
        (0xff05, 0, None, "fallback"),
    ] {
        let mut yes = d("pulsar", 0xf58a, page, usage, 1, path);
        yes.output_length = length;
        let h = FakeHid::new(
            vec![d("pulsar", 0xf58a, 1, 0x80, 1, "consumer"), yes],
            pulsar_steps(&pulsar(51, 0, 4)),
        );
        assert_eq!(poll("pulsar", &h)[0].level, Some(51));
        assert_eq!(*h.opened.lock().unwrap(), vec![path]);
        h.done();
    }
}
#[test]
fn asus_distinct_same_model_receivers_keep_separate_readings() {
    let mut a = d("asus", 0x1a72, 0xff01, 1, 0, "a");
    let mut b = d("asus", 0x1a72, 0xff01, 1, 0, "b");
    a.serial = "mouse-A".into();
    b.serial = "mouse-B".into();
    let mut s = asus_steps(&asus(80, 0, false));
    s.extend(asus_steps(&asus(31, 1, false)));
    let h = FakeHid::new(vec![a, b], s);
    let r = poll("asus", &h);
    assert_eq!(r.len(), 2);
    assert_ne!(r[0].key, r[1].key);
    assert_eq!(
        r.iter().find(|r| r.key.ends_with("MOUSE-A")).unwrap().level,
        Some(80)
    );
    assert_eq!(
        r.iter().find(|r| r.key.ends_with("MOUSE-B")).unwrap().level,
        Some(31)
    );
    h.done();
}

#[test]
fn wlmouse_feature_send_failure_can_still_receive_status() {
    let h = FakeHid::new(
        vec![d("wlmouse", 0xa880, 0xffff, 1, 2, "control")],
        vec![
            Step::SendError(
                padded(&[0, 0, 0, 2, 2, 0, 0x83], 65),
                "output endpoint unavailable",
            ),
            Step::Feature(0, 65, Ok(vec![0, 0xa1, 0, 2, 2, 0, 0x83, 0, 75])),
        ],
    );
    let r = poll("wlmouse", &h);
    assert_eq!(r[0].level, Some(75));
    h.done();
}
#[test]
fn wlmouse_passive_vendor_report_and_five_minute_identity_cache() {
    let clock = FakeClock::default();
    let cancelled = AtomicBool::new(false);
    let mut p = HidProvider::new("wlmouse");
    let h = FakeHid::new(
        vec![d("wlmouse", 0xa868, 0xff01, 0, 1, "passive")],
        vec![
            sized(64, 40, &[0x2f, 1]),
            sized(64, 40, &[0, 3, 0, 62, 1]),
            sized(64, 40, &[]),
            sized(64, 40, &[]),
        ],
    );
    let live = p.poll(&h, &context(&clock, &cancelled)).unwrap();
    assert_eq!((live[0].level, live[0].charging), (Some(62), Some(true)));
    clock.0.store(299000, std::sync::atomic::Ordering::Relaxed);
    let sleepy = p.poll(&h, &context(&clock, &cancelled)).unwrap();
    assert_eq!(sleepy[0].connection, Connection::Sleeping);
    assert_eq!(sleepy[0].key, live[0].key);
    clock.0.store(300000, std::sync::atomic::Ordering::Relaxed);
    assert!(p.poll(&h, &context(&clock, &cancelled)).unwrap().is_empty());
    h.done();
}
