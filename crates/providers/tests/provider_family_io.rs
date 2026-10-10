//! Strict fake-HID transaction regressions; these do not claim physical hardware verification.
mod common;
use common::*;
use hb_core::*;
use hb_providers::{HidProvider, catalog::DEVICES, protocols::padded};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
fn device(family: &str, pid: u16, page: u16, usage: u16, iface: i32, path: &str) -> HidInfo {
    let d = DEVICES
        .iter()
        .find(|d| d.provider == family && d.pid == pid)
        .unwrap();
    let mut i = info(d.vid, pid, page);
    i.usage = usage;
    i.interface = iface;
    i.path = path.into();
    i.container = Some(format!("fixture-{family}-{pid:04x}"));
    i
}
fn poll(family: &'static str, hid: &FakeHid) -> Vec<Reading> {
    let clock = FakeClock::default();
    HidProvider::new(family)
        .poll(hid, &context(&clock, &AtomicBool::new(false)))
        .unwrap()
}
fn rd(bytes: &[u8]) -> Step {
    Step::Read(Ok(bytes.to_vec()))
}
fn ft(bytes: &[u8]) -> Step {
    Step::Feature(0, 65, Ok(bytes.to_vec()))
}
fn classic(pid: u16) -> HidInfo {
    let (page, usage, iface) = match pid {
        0x12b3 => (0xff43, 0x202, 3),
        0x12ad | 0x1260 => (0xff00, 1, 5),
        0x12e0 | 0x12e5 => (0xff00, 1, 4),
        _ => (0xff00, 1, 0),
    };
    device("steelseries", pid, page, usage, iface, "classic")
}
#[test]
fn steel_classic_arctis1_and_arctis9_exact_transactions_names_and_gate() {
    for (pid, request, reply, level, charging, name) in [
        (
            0x12b3,
            vec![6, 0x12],
            vec![6, 0x12, 0, 85],
            85,
            Some(false),
            "Arctis 1 Wireless",
        ),
        (
            0x12c2,
            vec![0, 0x20],
            vec![0xaa, 1, 0, 0x7f, 1],
            50,
            Some(true),
            "Arctis 9",
        ),
    ] {
        let h = FakeHid::new(vec![classic(pid)], vec![Step::Write(request), rd(&reply)]);
        let r = poll("steelseries", &h);
        assert_eq!(r.len(), 1);
        assert_eq!(
            (&*r[0].name, r[0].level, r[0].charging, &*r[0].kind),
            (name, Some(level), charging, "headset")
        );
        h.done();
    }
    for (pid, request, reply) in [
        (0x12b3, vec![6, 0x12], vec![6, 0x12, 1, 85]),
        (0x12c2, vec![0, 0x20], vec![0x55, 0, 0, 0, 0]),
    ] {
        let h = FakeHid::new(vec![classic(pid)], vec![Step::Write(request), rd(&reply)]);
        assert!(poll("steelseries", &h).is_empty());
        h.done();
    }
}
#[test]
fn steel_arctis7_connection_precedes_level_and_off_never_requests_level() {
    let h = FakeHid::new(
        vec![classic(0x12ad)],
        vec![
            Step::Write(vec![6, 0x14]),
            rd(&[6, 0x14, 3, 0]),
            Step::Write(vec![6, 0x18]),
            rd(&[6, 0x18, 104, 0]),
        ],
    );
    assert_eq!(poll("steelseries", &h)[0].level, Some(100));
    h.done();
    let h = FakeHid::new(
        vec![classic(0x12ad)],
        vec![Step::Write(vec![6, 0x14]), rd(&[6, 0x14, 1, 0])],
    );
    assert!(poll("steelseries", &h).is_empty());
    h.done();
    let h = FakeHid::new(
        vec![classic(0x1260)],
        vec![Step::Write(vec![6, 0x18]), rd(&[6, 0x18, 0, 0])],
    );
    assert!(poll("steelseries", &h).is_empty());
    h.done();
}
#[test]
fn steel_pro_wireless_31byte_state_and_level_requests() {
    for (state, level) in [(4, Some(75)), (2, None)] {
        let mut steps = vec![Step::Write(padded(&[0x41, 0xaa], 31)), rd(&[state, 0])];
        if level.is_some() {
            steps.extend([Step::Write(padded(&[0x40, 0xaa], 31)), rd(&[3])]);
        }
        let h = FakeHid::new(vec![classic(0x1290)], steps);
        let r = poll("steelseries", &h);
        assert_eq!(r.first().and_then(|r| r.level), level);
        if level.is_some() {
            assert_eq!(r[0].precision, Precision::Coarse);
        }
        h.done();
    }
}
#[test]
fn steel_classic_response_matching_and_cached_answering_collection() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("steelseries");
    let wrong = device("steelseries", 0x12ad, 0xff00, 1, 5, "a-wrong");
    let right = device("steelseries", 0x12ad, 0xff01, 1, 5, "b-right");
    let mut steps = vec![Step::Write(vec![6, 0x14])];
    steps.extend((0..10).map(|_| rd(&[6, 0x13, 3, 0])));
    for _ in 0..2 {
        steps.extend([
            Step::Write(vec![6, 0x14]),
            rd(&[6, 0x14, 3, 0]),
            Step::Write(vec![6, 0x18]),
            rd(&[6, 0x18, 70, 0]),
        ]);
    }
    let h = FakeHid::new(vec![right, wrong], steps);
    for _ in 0..2 {
        assert_eq!(
            p.poll(&h, &context(&clock, &cancel)).unwrap()[0].level,
            Some(70)
        );
    }
    assert_eq!(
        *h.opened.lock().unwrap(),
        vec!["a-wrong", "b-right", "b-right"]
    );
    h.done();
}
fn nova_pro(code: u8, state: u8) -> Vec<u8> {
    let mut r = vec![0; 16];
    r[0] = 0xb0;
    r[6] = code;
    r[15] = state;
    r
}
#[test]
fn steel_nova_pro_every_coarse_step_cable_state_and_both_stations() {
    for code in 0..=8 {
        for (pid, iface, state) in [(0x12e0, 4, 8), (0x12e0, 3, 2), (0x12e5, 4, 8)] {
            let h = FakeHid::new(
                vec![device("steelseries", pid, 0xff00, 1, iface, "pro")],
                vec![Step::Write(vec![6, 0xb0]), rd(&nova_pro(code, state))],
            );
            let r = poll("steelseries", &h);
            assert_eq!(r[0].level, Some((code as u16 * 100 / 8) as u8));
            assert_eq!(r[0].charging, Some(state == 2));
            assert_eq!(r[0].precision, Precision::Coarse);
            assert_eq!(
                r[0].approx,
                Some(format!("about {}%", code as u16 * 100 / 8))
            );
            if pid == 0x12e5 {
                assert_eq!(r[0].name, "Arctis Nova Pro Wireless X");
            }
            h.done();
        }
    }
}
#[test]
fn steel_nova_pro_rejects_short_invalid_gate_and_level_codes() {
    for reply in [
        nova_pro(5, 1),
        nova_pro(9, 8),
        nova_pro(5, 0),
        nova_pro(5, 3),
        nova_pro(5, 4),
        nova_pro(5, 128),
        nova_pro(5, 255),
        nova_pro(5, 8)[..15].to_vec(),
    ] {
        let repeats = if reply.len() == 16 && [1, 2, 8].contains(&reply[15]) {
            1
        } else {
            10
        };
        let mut steps = vec![Step::Write(vec![6, 0xb0])];
        steps.extend((0..repeats).map(|_| rd(&reply)));
        let h = FakeHid::new(vec![classic(0x12e0)], steps);
        assert!(poll("steelseries", &h).is_empty());
        h.done();
    }
}
#[test]
fn steel_nova_pro_ignores_foreign_reports_and_coexists_with_nova7() {
    let h = FakeHid::new(
        vec![
            classic(0x12e0),
            device("steelseries", 0x22a1, 0xffc0, 1, 3, "nova7"),
        ],
        vec![
            Step::Write(vec![0, 0xb0]),
            rd(&[0xb0, 3, 73, 3]),
            Step::Write(vec![6, 0xb0]),
            rd(&{
                let mut foreign = nova_pro(5, 8);
                foreign[0] = 1;
                foreign
            }),
            rd(&nova_pro(5, 8)),
        ],
    );
    let r = poll("steelseries", &h);
    assert_eq!(r.len(), 2);
    assert_eq!(
        r.iter().find(|r| r.name == "Arctis Nova 7").unwrap().level,
        Some(73)
    );
    assert_eq!(
        r.iter()
            .find(|r| r.name == "Arctis Nova Pro Wireless")
            .unwrap()
            .level,
        Some(62)
    );
    h.done();
}
#[test]
fn steel_standard_wrong_vendor_and_unknown_product_collections_receive_no_io() {
    let h = FakeHid::new(
        vec![
            device("steelseries", 0x12c2, 0xc, 1, 0, "consumer"),
            device("steelseries", 0x1260, 1, 6, 5, "keyboard"),
            device("steelseries", 0x12b3, 0xff00, 0x202, 3, "wrong-arctis1"),
            device("steelseries", 0x22a1, 0xff00, 1, 3, "wrong-nova"),
            device("steelseries", 0x12e0, 1, 1, 4, "audio"),
            info(0x1038, 0x1280, 0xffc0),
            info(0x1038, 0x1234, 0xffc0),
        ],
        vec![],
    );
    assert!(poll("steelseries", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
    assert!(
        !DEVICES
            .iter()
            .any(|d| d.provider == "steelseries" && d.pid == 0x1280)
    );
    h.done();
}
#[test]
fn steel_b0_models_use_reference_echo_and_link_charging_fields() {
    for (pid, reply, expected) in [
        (0x22a1, vec![0xb0, 3, 73, 3], Some((73, false))),
        (0x22a1, vec![0xb0, 3, 69, 1], Some((69, true))),
        (0x22a1, vec![0xb0, 2, 73, 3], None),
        (0x220a, vec![0xb0, 3, 3, 3], Some((75, false))),
        (0x2232, vec![0xb0, 1, 0, 60, 1], Some((60, true))),
        (0x2232, vec![0xb0, 2, 0, 60, 0], None),
        (0x220e, vec![0xb0, 3, 2, 1], Some((50, true))),
        (0x220e, vec![0xb0, 1, 2, 0], None),
        (0x220e, vec![0xb0, 3, 9, 0], Some((100, false))),
        (0x230a, vec![0xb0, 0, 0, 3, 3, 80, 60], Some((60, false))),
        (0x230a, vec![0xb0, 0, 0, 3, 2, 80, 0], Some((80, false))),
        (0x230a, vec![0xb0, 0, 0, 2, 2, 0, 0], None),
    ] {
        let h = FakeHid::new(
            vec![device("steelseries", pid, 0xffc0, 1, 3, "b0")],
            vec![Step::Write(vec![0, 0xb0]), rd(&reply)],
        );
        let r = poll("steelseries", &h);
        assert_eq!(
            r.first().map(|r| (r.level.unwrap(), r.charging.unwrap())),
            expected,
            "pid={pid:04x}"
        );
        h.done();
    }
}
#[test]
fn steel_aerox_all_wireless_editions_exact_d2_packet_and_all_steps() {
    for pid in [0x1838, 0x1852, 0x1858, 0x185c, 0x1860, 0x1874] {
        for step in 1..=21 {
            for charge in [0, 0x80] {
                let selected = device("steelseries", pid, 0xffc0, 1, 3, "control");
                let mut wrong = selected.clone();
                wrong.path = "keyboard".into();
                wrong.interface = 1;
                wrong.usage_page = 1;
                wrong.usage = 6;
                let h = FakeHid::new(
                    vec![wrong, selected],
                    vec![
                        Step::Write(padded(&[0, 0xd2], 64)),
                        rd(&[0xd2, step | charge, 0, 0x2e, 0x34, 0, 0, 0]),
                    ],
                );
                let r = poll("steelseries", &h);
                assert_eq!(
                    (r[0].level, r[0].charging, &*r[0].kind),
                    (Some((step - 1) * 5), Some(charge != 0), "mouse")
                );
                assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
                assert_eq!(
                    r[0].name,
                    DEVICES
                        .iter()
                        .find(|d| d.provider == "steelseries" && d.pid == pid)
                        .unwrap()
                        .name
                );
                h.done();
            }
        }
    }
    let h = FakeHid::new(
        vec![device("steelseries", 0x1858, 0xffc0, 1, 3, "aerox")],
        vec![Step::Write(padded(&[0, 0xd2], 64)), rd(&[0xd2, 0])],
    );
    assert!(poll("steelseries", &h).is_empty());
    h.done();
}
fn nxp(index: u8, numbered: bool) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[0] = 0x0e;
    r[1] = 0x50;
    r[4] = index;
    r[5] = 2;
    if numbered {
        r.insert(0, 0)
    }
    r
}
fn nxp_dev(path: &str, page: u16, usage: u16, iface: i32) -> HidInfo {
    device("corsair", 0x1b7f, page, usage, iface, path)
}
#[test]
fn corsair_nxp_all_steps_report_id_compatibility_and_coarse_gauge() {
    for (numbered, index, level) in [false, true].into_iter().flat_map(|n| {
        [0, 15, 30, 50, 100]
            .into_iter()
            .enumerate()
            .map(move |(i, l)| (n, i as u8, l))
    }) {
        let h = FakeHid::new(
            vec![nxp_dev("nxp", 0xff42, 1, 1)],
            vec![
                Step::Write(padded(&[0, 0x0e, 0x50], 65)),
                rd(&nxp(index, numbered)),
            ],
        );
        let r = poll("corsair", &h);
        assert_eq!(
            (&*r[0].key, r[0].level, r[0].charging, &*r[0].kind),
            (
                "corsair:1b7f:FIXTURE-CORSAIR-1B7F",
                Some(level),
                Some(false),
                "mouse"
            )
        );
        assert_eq!(r[0].precision, Precision::Coarse);
        assert_eq!(r[0].approx, Some(format!("about {level}%")));
        h.done();
    }
}
#[test]
fn corsair_nxp_priority_retry_and_dedup_are_independent_of_enumeration_order() {
    for inputs in [
        vec![
            nxp_dev("usage2", 0xff42, 2, 1),
            nxp_dev("usage1", 0xff42, 1, 1),
        ],
        vec![
            nxp_dev("iface2", 0xff42, 2, 2),
            nxp_dev("usage1", 0xff42, 1, 1),
        ],
    ] {
        let h = FakeHid::new(
            inputs,
            vec![Step::Write(padded(&[0, 0x0e, 0x50], 65)), rd(&nxp(4, true))],
        );
        assert_eq!(poll("corsair", &h)[0].level, Some(100));
        assert_eq!(*h.opened.lock().unwrap(), vec!["usage1"]);
        h.done();
    }
    let h = FakeHid::new(
        vec![
            nxp_dev("first", 0xff42, 1, 1),
            nxp_dev("second", 0xff42, 2, 2),
        ],
        vec![
            Step::Write(padded(&[0, 0x0e, 0x50], 65)),
            rd(&[]),
            Step::Write(padded(&[0, 0x0e, 0x50], 65)),
            rd(&nxp(4, true)),
        ],
    );
    assert_eq!(poll("corsair", &h)[0].level, Some(100));
    assert_eq!(*h.opened.lock().unwrap(), vec!["first", "second"]);
    h.done();
}
#[test]
fn corsair_nxp_no_keyboard_fallback_when_vendor_exists_but_missing_vendor_can_fallback() {
    let h = FakeHid::new(
        vec![
            nxp_dev("keyboard", 1, 2, 0),
            nxp_dev("vendor", 0xff42, 1, 1),
        ],
        vec![Step::Write(padded(&[0, 0x0e, 0x50], 65)), rd(&[])],
    );
    assert!(poll("corsair", &h).is_empty());
    assert_eq!(*h.opened.lock().unwrap(), vec!["vendor"]);
    h.done();
    let h = FakeHid::new(
        vec![nxp_dev("fallback", 1, 2, 0)],
        vec![Step::Write(padded(&[0, 0x0e, 0x50], 65)), rd(&nxp(1, true))],
    );
    assert_eq!(poll("corsair", &h)[0].level, Some(15));
    h.done();
}
#[test]
fn corsair_nxp_rejects_out_of_table_short_and_empty_replies() {
    for reply in [
        nxp(5, true),
        nxp(255, false),
        vec![0, 0x0e, 0x50, 0, 3],
        vec![0],
        vec![],
    ] {
        let h = FakeHid::new(
            vec![nxp_dev("nxp", 0xff42, 1, 1)],
            vec![Step::Write(padded(&[0, 0x0e, 0x50], 65)), rd(&reply)],
        );
        assert!(poll("corsair", &h).is_empty());
        h.done();
    }
}
fn feature_reply(charging: u8, level: u8) -> Vec<u8> {
    vec![0, 0xa1, 0, 2, 2, 0, 0x83, charging, level]
}
fn feature_steps(charging: u8, level: u8) -> Vec<Step> {
    vec![
        Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
        ft(&feature_reply(charging, level)),
    ]
}
#[test]
fn lamzu_only_interface2_vendor_collection_receives_feature_packet() {
    let shape = [
        (0, 1, 2),
        (1, 0xc, 1),
        (1, 1, 6),
        (1, 1, 0x80),
        (1, 0xffa0, 1),
        (1, 0xffff, 1),
        (2, 0xffff, 0),
    ];
    let infos: Vec<_> = shape
        .iter()
        .enumerate()
        .map(|(n, (i, p, u))| device("lamzu", 0x1e, *p, *u, *i, &format!("collection-{n}")))
        .collect();
    let h = FakeHid::new(infos.clone(), feature_steps(0, 81));
    let r = poll("lamzu", &h);
    assert_eq!(
        (
            &*r[0].key,
            &*r[0].name,
            r[0].level,
            r[0].charging,
            &*r[0].kind
        ),
        (
            "lamzu:FIXTURE-LAMZU-001E",
            "LAMZU Maya X",
            Some(81),
            Some(false),
            "mouse"
        )
    );
    assert_eq!(*h.opened.lock().unwrap(), vec!["collection-6"]);
    h.done();
    let h = FakeHid::new(infos[..6].to_vec(), vec![]);
    assert!(poll("lamzu", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
}
#[test]
fn lamzu_cable_charging_source_wins_without_poking_dongle() {
    let h = FakeHid::new(
        vec![
            {
                let mut i = device("lamzu", 0x1e, 0xffff, 0, 2, "dongle");
                i.serial = "same-maya".into();
                i
            },
            {
                let mut i = device("lamzu", 0x1c, 0xffff, 0, 2, "cable");
                i.serial = "same-maya".into();
                i
            },
        ],
        feature_steps(1, 71),
    );
    let r = poll("lamzu", &h);
    assert_eq!((r[0].level, r[0].charging), (Some(71), Some(true)));
    assert_eq!(*h.opened.lock().unwrap(), vec!["cable"]);
    h.done();
}
#[test]
fn lamzu_and_gwolves_retry_feature_reads_and_keep_sleeping_cache_under_five_minutes() {
    for (family, pid, attempts) in [("lamzu", 0x1e, 10), ("gwolves", 0x3854, 15)] {
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let mut p = HidProvider::new(family);
        let mut i = device(family, pid, 0xffff, 0, 2, "control");
        i.feature_length = Some(65);
        let mut steps = vec![
            Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
            Step::Feature(0, 65, Err("transient reconnect")),
            ft(&feature_reply(0, 77)),
        ];
        for _ in 0..2 {
            steps.push(Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)));
            steps.extend((0..attempts).map(|_| ft(&[0; 65])));
        }
        let h = FakeHid::new(vec![i], steps);
        let live = p.poll(&h, &context(&clock, &cancel)).unwrap();
        assert_eq!(live[0].level, Some(77));
        assert_eq!(clock.monotonic(), Duration::from_millis(100));
        clock.0.store(299000, Ordering::Relaxed);
        let sleepy = p.poll(&h, &context(&clock, &cancel)).unwrap();
        assert_eq!(sleepy[0].connection, Connection::Sleeping);
        assert_eq!(sleepy[0].timestamp, live[0].timestamp);
        assert_eq!(sleepy[0].key, live[0].key);
        clock.0.store(300000, Ordering::Relaxed);
        assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
        h.done();
    }
}
#[test]
fn gwolves_selects_actual_feature_length_at_any_collection_not_usage_guesses() {
    for target in [0, 5, 7] {
        let shape = [
            (0, 1, 2),
            (1, 0xff05, 0),
            (1, 0xff03, 0),
            (1, 0xc, 1),
            (1, 1, 0x80),
            (1, 0xff02, 2),
            (1, 0xff04, 2),
            (1, 0xff06, 2),
            (1, 1, 2),
            (2, 1, 6),
        ];
        let infos: Vec<_> = shape
            .iter()
            .enumerate()
            .map(|(n, (iface, page, usage))| {
                let mut i = device(
                    "gwolves",
                    0x3854,
                    *page,
                    *usage,
                    *iface,
                    &format!("collection-{n}"),
                );
                i.feature_length = Some(if n == target {
                    65
                } else if n % 2 == 0 {
                    9
                } else {
                    0
                });
                i
            })
            .collect();
        let h = FakeHid::new(infos, feature_steps(1, 64));
        let r = poll("gwolves", &h);
        assert_eq!(
            (&*r[0].name, r[0].level, r[0].charging),
            ("G-Wolves mouse", Some(64), Some(true))
        );
        assert_eq!(
            *h.opened.lock().unwrap(),
            vec![format!("collection-{target}")]
        );
        h.done();
    }
    let mut i = device("gwolves", 0x3854, 0xff02, 2, 1, "wrong-size");
    i.feature_length = Some(9);
    let h = FakeHid::new(vec![i], vec![]);
    assert!(poll("gwolves", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
}
fn infinity() -> HidInfo {
    device("am_infinity", 0x5007, 0xffff, 2, 2, "infinity")
}
fn infinity_steps(first: Vec<u8>, second: Option<Vec<u8>>) -> Vec<Step> {
    let mut s = vec![
        Step::Send(padded(&[0, 0xf7], 65)),
        Step::Feature(5, 65, Ok(first)),
    ];
    if let Some(r) = second {
        s.extend([
            Step::Send(padded(&[0, 0xf7], 67)),
            Step::Feature(5, 65, Ok(r)),
        ]);
    }
    s
}
#[test]
fn infinity_report_id_tolerance_clamp_and_exact_zero_payload_request() {
    for reply in [
        vec![5, 0, 0, 100, 1, 1, 1, 2],
        vec![0, 0, 100, 1, 1, 1, 2],
        vec![5, 0, 0, 250, 1, 1, 1, 2],
    ] {
        let h = FakeHid::new(
            vec![
                infinity(),
                device("am_infinity", 0x5007, 0xffff, 1, 1, "wrong-usage"),
                device("am_infinity", 0x5007, 1, 6, 1, "keyboard"),
            ],
            infinity_steps(reply, None),
        );
        let r = poll("am_infinity", &h);
        assert_eq!(
            (
                &*r[0].key,
                &*r[0].name,
                r[0].level,
                r[0].charging,
                &*r[0].kind
            ),
            (
                "am_infinity:FIXTURE-AM_INFINITY-5007",
                "AM Infinity 8K Mouse",
                Some(100),
                Some(false),
                "mouse"
            )
        );
        assert_eq!(*h.opened.lock().unwrap(), vec!["infinity"]);
        h.done();
    }
}
#[test]
fn infinity_junk_zero_and_alternative_length_retry() {
    for first in [vec![0; 65], vec![5, 0xad, 4, 99, 1, 1, 1, 2]] {
        let h = FakeHid::new(
            vec![infinity()],
            infinity_steps(first, Some(vec![5, 0, 0, 100, 1, 1, 1, 2])),
        );
        assert_eq!(poll("am_infinity", &h)[0].level, Some(100));
        h.done();
    }
    let h = FakeHid::new(
        vec![infinity()],
        infinity_steps(vec![0; 65], Some(vec![0; 65])),
    );
    assert!(poll("am_infinity", &h).is_empty());
    h.done();
    let h = FakeHid::new(
        vec![infinity()],
        vec![
            Step::SendError(padded(&[0, 0xf7], 65), "wrong buffer size"),
            Step::Send(padded(&[0, 0xf7], 67)),
            Step::Feature(5, 65, Ok(vec![5, 0, 0, 64, 1, 1, 1, 2])),
        ],
    );
    assert_eq!(poll("am_infinity", &h)[0].level, Some(64));
    h.done();
}
#[test]
fn infinity_sleeping_cache_expires_and_missing_collection_or_unknown_pid_has_no_io() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("am_infinity");
    let mut steps = infinity_steps(vec![5, 0, 0, 100, 1, 1, 1, 2], None);
    for _ in 0..2 {
        steps.extend(infinity_steps(vec![0; 65], Some(vec![0; 65])));
    }
    let h = FakeHid::new(vec![infinity()], steps);
    let live = p.poll(&h, &context(&clock, &cancel)).unwrap();
    clock.0.store(299000, Ordering::Relaxed);
    let r = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(
        (r[0].level, &r[0].connection, r[0].timestamp),
        (Some(100), &Connection::Sleeping, live[0].timestamp)
    );
    clock.0.store(300000, Ordering::Relaxed);
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    h.done();
    let mut unknown = infinity();
    unknown.product_id = 0x5008;
    let h = FakeHid::new(
        vec![unknown, device("am_infinity", 0x5007, 1, 2, 0, "mouse")],
        vec![],
    );
    assert!(poll("am_infinity", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
}
#[test]
fn hyperx2_strict_usage_level_echo_charging_request_and_100ms_spacing() {
    for pid in [0x0696, 0x018b] {
        let valid = device("hyperx", pid, 0xff90, 0x303, 3, "control");
        let mut wrong = valid.clone();
        wrong.path = "wrong".into();
        wrong.usage_page = 0xff00;
        let h = FakeHid::new(
            vec![wrong, valid],
            vec![
                Step::Write(padded(&[6, 255, 187, 2, 0], 52)),
                rd(&[6, 255, 187, 2, 0, 0, 0, 85]),
                Step::Write(padded(&[6, 255, 187, 3, 0], 52)),
                rd(&[6, 255, 187, 3, 1]),
            ],
        );
        let clock = FakeClock::default();
        let r = HidProvider::new("hyperx")
            .poll(&h, &context(&clock, &AtomicBool::new(false)))
            .unwrap();
        assert_eq!(
            (&*r[0].name, r[0].level, r[0].charging),
            ("HyperX Cloud II Wireless", Some(85), Some(true))
        );
        assert_eq!(clock.monotonic(), Duration::from_millis(200));
        assert_eq!(*h.opened.lock().unwrap(), vec!["control"]);
        h.done();
    }
    for reply in [
        vec![6, 255, 187, 3, 0, 0, 0, 85],
        vec![6, 255, 187, 2, 0, 0, 0, 255],
        vec![6, 255, 187, 2],
    ] {
        let h = FakeHid::new(
            vec![device("hyperx", 0x0696, 0xff90, 0x303, 0, "control")],
            vec![Step::Write(padded(&[6, 255, 187, 2, 0], 52)), rd(&reply)],
        );
        assert!(poll("hyperx", &h).is_empty());
        h.done();
    }
}
#[test]
fn hyperx3_incorrect_function_fallback_is_same_feature_packet_for_both_commands() {
    for error in [
        "WriteFile: (0x00000001) Incorrect function.",
        "incorrect function",
    ] {
        let mut steps = vec![];
        for (cmd, reply) in [
            (0x89, vec![0x66, 0x89, 1, 0, 80]),
            (0x8a, vec![0x66, 0x8a, 1]),
        ] {
            let packet = padded(&[0x66, cmd], 62);
            steps.extend([
                Step::WriteError(packet.clone(), error),
                Step::Send(packet),
                rd(&reply),
            ]);
        }
        let h = FakeHid::new(
            vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
            steps,
        );
        let clock = FakeClock::default();
        let r = HidProvider::new("hyperx_cloud3")
            .poll(&h, &context(&clock, &AtomicBool::new(false)))
            .unwrap();
        assert_eq!((r[0].level, r[0].charging), (Some(80), Some(true)));
        assert_eq!(clock.monotonic(), Duration::from_millis(200));
        h.done();
    }
}
#[test]
fn hyperx3_unrelated_output_errors_do_not_fallback_and_feature_failure_is_reported() {
    for error in [
        "device disconnected",
        "WriteFile: (0x0000001F) A device attached to the system is not functioning.",
    ] {
        let h = FakeHid::new(
            vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
            vec![Step::WriteError(padded(&[0x66, 0x89], 62), error)],
        );
        let result = HidProvider::new("hyperx_cloud3")
            .poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)));
        assert!(result.is_err());
        h.done();
    }
    let packet = padded(&[0x66, 0x89], 62);
    let h = FakeHid::new(
        vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
        vec![
            Step::WriteError(packet.clone(), "incorrect function"),
            Step::SendError(packet, "feature report refused"),
        ],
    );
    let mut p = HidProvider::new("hyperx_cloud3");
    assert!(
        p.poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)))
            .is_err()
    );
    assert!(
        p.diagnostics()
            .iter()
            .any(|s| s.contains("feature report refused"))
    );
    h.done();
}
#[test]
fn hyperx3_reads_noise_then_accepted_battery_and_charging_echo() {
    let h = FakeHid::new(
        vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
        vec![
            Step::Write(padded(&[0x66, 0x89], 62)),
            rd(&[1, 99]),
            rd(&[0x66, 0x0d, 1, 0, 80]),
            Step::Write(padded(&[0x66, 0x8a], 62)),
            rd(&[0x66, 0x0c, 2]),
        ],
    );
    let r = poll("hyperx_cloud3", &h);
    assert_eq!((r[0].level, r[0].charging), (Some(80), Some(true)));
    h.done();
}
fn alpha() -> HidInfo {
    device("hyperx_alpha2", 0x08be, 0xff13, 0xff00, 2, "controller")
}
fn alpha_reply(level: u8, charging: bool) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[..7].copy_from_slice(&[0x51, 2, level, 0, 0x1a, 0, if charging { 0x86 } else { 0 }]);
    r
}
#[test]
fn hyperx_alpha2_controller_usage_drain_cap_and_flood_noise_matching() {
    for (initial_noise, post_noise, level, charging) in [(0, 20, 51, false), (64, 36, 67, true)] {
        let mut steps: Vec<_> = (0..initial_noise).map(|_| rd(&[0xff, 1])).collect();
        if initial_noise < 64 {
            steps.push(rd(&[]));
        }
        steps.push(Step::Write(padded(&[0x50, 2], 64)));
        steps.extend((0..post_noise).map(|_| rd(&[0xff, 1])));
        steps.extend([
            rd(&[0x61, 2, 90, 0, 1, 0x14]),
            rd(&alpha_reply(level, charging)),
        ]);
        let h = FakeHid::new(
            vec![
                device("hyperx_alpha2", 0x08be, 0xff13, 1, 0, "audio"),
                alpha(),
                device("hyperx_alpha2", 0x08be, 1, 6, 3, "keyboard"),
            ],
            steps,
        );
        let r = poll("hyperx_alpha2", &h);
        assert_eq!(
            (&*r[0].key, &*r[0].name, r[0].level, r[0].charging),
            (
                "hyperx:08be:FIXTURE-HYPERX_ALPHA2-08BE",
                "HyperX Cloud Alpha 2",
                Some(level),
                Some(charging)
            )
        );
        assert_eq!(*h.opened.lock().unwrap(), vec!["controller"]);
        h.done();
    }
}
#[test]
fn hyperx_alpha2_missing_controller_and_chat_half_never_receive_requests() {
    let mut chat = alpha();
    chat.product_id = 0x0abe;
    let h = FakeHid::new(
        vec![chat, device("hyperx_alpha2", 0x08be, 0xff13, 1, 0, "audio")],
        vec![],
    );
    assert!(poll("hyperx_alpha2", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
    h.done();
}

struct TimedHid<'a> {
    inner: &'a FakeHid,
    clock: std::sync::Arc<FakeClock>,
}
struct TimedSession {
    inner: Box<dyn HidSession>,
    clock: std::sync::Arc<FakeClock>,
}
impl HidTransport for TimedHid<'_> {
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        self.inner.enumerate(vendor)
    }
    fn open(&self, i: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        Ok(Box::new(TimedSession {
            inner: self.inner.open(i)?,
            clock: self.clock.clone(),
        }))
    }
}
impl HidSession for TimedSession {
    fn write(&mut self, b: &[u8]) -> Result<(), ProviderError> {
        self.inner.write(b)
    }
    fn send_feature(&mut self, b: &[u8]) -> Result<(), ProviderError> {
        self.inner.send_feature(b)
    }
    fn feature(&mut self, id: u8, len: usize) -> Result<Vec<u8>, ProviderError> {
        self.inner.feature(id, len)
    }
    fn read(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        let r = self.inner.read(len, timeout);
        self.clock.sleep(Duration::from_millis(10));
        r
    }
}
#[test]
fn hyperx_alpha2_only_keepalive_queue_obeys_poll_deadline() {
    let clock = std::sync::Arc::new(FakeClock::default());
    let cancelled = AtomicBool::new(false);
    let mut steps = vec![rd(&[]), Step::Write(padded(&[0x50, 2], 64))];
    steps.extend((0..14).map(|_| rd(&[0x61, 2, 90, 0, 1, 0x14])));
    let h = FakeHid::new(vec![alpha()], steps);
    let timed = TimedHid {
        inner: &h,
        clock: clock.clone(),
    };
    let c = PollContext {
        clock: &*clock,
        cancelled: &cancelled,
        deadline: Duration::from_millis(150),
        playstation_full_mode: false,
    };
    assert!(
        HidProvider::new("hyperx_alpha2")
            .poll(&timed, &c)
            .unwrap()
            .is_empty()
    );
    assert_eq!(clock.monotonic(), Duration::from_millis(150));
    h.done();
}
#[test]
fn gwolves_trusted_cable_identity_names_model_and_wins_before_receiver_query() {
    let mut cable = device("gwolves", 0x4219, 1, 2, 2, "z-cable");
    let mut receiver = device("gwolves", 0x3854, 0xff02, 2, 0, "a-receiver");
    for i in [&mut cable, &mut receiver] {
        i.feature_length = Some(65);
        i.serial = "shared-mouse-id".into();
    }
    let h = FakeHid::new(vec![receiver, cable], feature_steps(1, 71));
    let r = poll("gwolves", &h);
    assert_eq!(
        (&*r[0].name, r[0].level, r[0].charging),
        ("G-Wolves WARG", Some(71), Some(true))
    );
    assert_eq!(*h.opened.lock().unwrap(), vec!["z-cable"]);
    h.done();
}
#[test]
fn infinity_two_junk_frames_cannot_be_a_battery_and_unknown_pids_never_open() {
    let h = FakeHid::new(
        vec![infinity()],
        infinity_steps(
            vec![5, 0xad, 4, 99, 1, 1, 1, 2],
            Some(vec![5, 0xad, 4, 99, 1, 1, 1, 2]),
        ),
    );
    assert!(poll("am_infinity", &h).is_empty());
    h.done();
    for family in ["lamzu", "gwolves"] {
        let known = if family == "lamzu" { 0x1e } else { 0x3854 };
        let mut i = device(family, known, 0xffff, 0, 2, "unknown");
        i.product_id = 0xffff; // 0x3808 is a supported HTM Plus cable in 1.14.0.
        i.feature_length = Some(65);
        let h = FakeHid::new(vec![i], vec![]);
        assert!(poll(family, &h).is_empty());
        assert!(h.opened.lock().unwrap().is_empty());
        h.done();
    }
}

#[test]
fn steel_nova7_captured_eight_byte_replies_and_foreign_noise_are_matched() {
    for (reply, expected) in [
        (vec![0xb0, 3, 0x49, 3, 0x1e, 0x64, 0, 0], Some((73, false))),
        (vec![0xb0, 3, 0x45, 1, 0x22, 0x64, 0, 0], Some((69, true))),
        (vec![0xb0, 2, 0x49, 0, 0x64, 0x64, 0, 0], None),
        (vec![0xb0, 2, 0x49, 3, 0x1e, 0x64, 0, 0], None),
        (vec![0xb0, 3, 100, 2], Some((100, true))),
    ] {
        let h = FakeHid::new(
            vec![device("steelseries", 0x22a1, 0xffc0, 1, 3, "nova7")],
            vec![Step::Write(vec![0, 0xb0]), rd(&[1, 0, 99, 2]), rd(&reply)],
        );
        let r = poll("steelseries", &h);
        assert_eq!(
            r.first().map(|r| (r.level.unwrap(), r.charging.unwrap())),
            expected
        );
        h.done();
    }
}
#[test]
fn gwolves_receiver_issue82_exact_name_level_and_packet() {
    let mut i = device("gwolves", 0x3854, 0xff02, 2, 1, "receiver");
    i.feature_length = Some(65);
    let h = FakeHid::new(vec![i], feature_steps(0, 77));
    let r = poll("gwolves", &h);
    assert_eq!(
        (
            &*r[0].name,
            r[0].level,
            r[0].charging,
            &*r[0].kind,
            &r[0].connection
        ),
        (
            "G-Wolves mouse",
            Some(77),
            Some(false),
            "mouse",
            &Connection::Online
        )
    );
    h.done();
}

#[test]
fn family_failed_and_accepted_feature_reports_have_actionable_diagnostics() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let packet = padded(&[0x66, 0x89], 62);
    let h = FakeHid::new(
        vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
        vec![
            Step::WriteError(packet.clone(), "incorrect function"),
            Step::SendError(packet, "feature report refused"),
        ],
    );
    let mut p = HidProvider::new("hyperx_cloud3");
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    let diag = p.diagnostics().join("\n");
    assert!(diag.contains("retrying as a feature report"));
    assert!(diag.contains("feature report ->"));
    assert!(diag.contains("feature report refused"));
    assert!(!diag.contains("feature report accepted"));
    h.done();
    let mut steps = vec![];
    for (cmd, reply) in [
        (0x89, vec![0x66, 0x89, 1, 0, 80]),
        (0x8a, vec![0x66, 0x8a, 1]),
    ] {
        let packet = padded(&[0x66, cmd], 62);
        steps.extend([
            Step::WriteError(packet.clone(), "incorrect function"),
            Step::Send(packet),
            rd(&reply),
        ]);
    }
    let h = FakeHid::new(
        vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
        steps,
    );
    let r = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(r[0].level, Some(80));
    let diag = p.diagnostics().join("\n");
    assert_eq!(diag.matches("retrying as a feature report").count(), 2);
    assert_eq!(diag.matches("feature report accepted").count(), 2);
    h.done();
    let h = FakeHid::new(
        vec![device("hyperx_cloud3", 0x05b7, 0xff13, 1, 3, "cloud3")],
        vec![Step::WriteError(
            padded(&[0x66, 0x89], 62),
            "WriteFile: device not functioning",
        )],
    );
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    let diag = p.diagnostics().join("\n");
    assert!(diag.contains("not functioning"));
    assert!(!diag.contains("retrying as a feature report"));
    h.done();
    let h = FakeHid::new(
        vec![infinity()],
        infinity_steps(vec![0; 65], Some(vec![0; 65])),
    );
    let mut p = HidProvider::new("am_infinity");
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    let diag = p.diagnostics().join("\n");
    assert!(diag.contains("no charge in the status report"));
    assert!(diag.contains("mouse off or asleep"));
    h.done();
}
#[test]
fn independent_family_identities_do_not_share_charging_or_sleeping_readings() {
    for (family, pid) in [("lamzu", 0x1e), ("gwolves", 0x3854)] {
        let mut a = device(family, pid, 0xffff, 0, 2, "a-control");
        let mut b = a.clone();
        b.path = "b-control".into();
        a.feature_length = Some(65);
        b.feature_length = Some(65);
        a.serial = "mouse-A".into();
        b.serial = "mouse-B".into();
        let mut s = feature_steps(1, 81);
        s.extend(feature_steps(0, 32));
        let h = FakeHid::new(vec![b, a], s);
        let r = poll(family, &h);
        assert_eq!(r.len(), 2);
        assert_ne!(r[0].key, r[1].key);
        assert_eq!(
            r.iter().find(|r| r.key.ends_with("MOUSE-A")).unwrap().level,
            Some(81)
        );
        assert_eq!(
            r.iter().find(|r| r.key.ends_with("MOUSE-B")).unwrap().level,
            Some(32)
        );
        h.done();
    }
}

#[test]
fn corsair_headset_catalog_remains_separate_from_nxp_mouse() {
    let got: std::collections::BTreeMap<_, _> = DEVICES
        .iter()
        .filter(|d| d.provider == "corsair" && d.variant != "nxp")
        .map(|d| (d.pid, d.name))
        .collect();
    assert_eq!(
        got,
        std::collections::BTreeMap::from([
            (0x2a08, "Corsair Void v2 Wireless"),
            (0x2a02, "Corsair Virtuoso Max Wireless"),
            (0x0a97, "Corsair HS80 Max Wireless")
        ])
    );
    assert!(!got.contains_key(&0x1b7f));
}
#[test]
fn gwolves_feature_without_a1_or_a2_marker_never_becomes_a_level() {
    let mut i = device("gwolves", 0x3854, 0xff02, 2, 1, "control");
    i.feature_length = Some(65);
    let mut s = vec![Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65))];
    s.extend((0..15).map(|_| ft(&[0, 0, 0, 2, 2, 0, 0x83, 0, 50])));
    let h = FakeHid::new(vec![i], s);
    assert!(poll("gwolves", &h).is_empty());
    h.done();
}
