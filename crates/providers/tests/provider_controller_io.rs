//! Strict controller transactions, with no physical hardware verification claim.
mod common;
use common::*;
use hb_core::*;
use hb_providers::{HidProvider, catalog::DEVICES, protocols::padded, provider::is_bluetooth};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
const BT: &str = "hid#vid&0002054c_pid&0ce6#a-controller&0&0000#service";
const USB: &str = "hid#vid_054c&pid_0ce6#b-controller&0&0000#service";
fn d(f: &str, pid: u16, path: &str, serial: &str) -> HidInfo {
    let x = DEVICES
        .iter()
        .find(|x| x.provider == f && x.pid == pid)
        .unwrap();
    let mut i = info(x.vid, pid, 1);
    i.usage = 5;
    i.interface = -1;
    i.path = path.into();
    i.serial = serial.into();
    i
}
fn rd(b: &[u8]) -> Step {
    Step::Read(Ok(b.to_vec()))
}
fn empty(n: usize) -> Vec<Step> {
    (0..n).map(|_| rd(&[])).collect()
}
fn ps(sense: bool, bt: bool, status: u8) -> Vec<u8> {
    let mut r = vec![0; 78];
    r[0] = if bt {
        if sense { 0x31 } else { 0x11 }
    } else {
        1
    };
    r[if sense {
        if bt { 54 } else { 53 }
    } else if bt {
        32
    } else {
        30
    }] = status;
    r
}
fn basic() -> Vec<u8> {
    vec![1, 0x80, 0x80, 0x80, 0x80, 8, 0, 0, 0, 0]
}
fn poll(f: &'static str, h: &FakeHid) -> Vec<Reading> {
    HidProvider::new(f)
        .poll(h, &context(&FakeClock::default(), &AtomicBool::new(false)))
        .unwrap()
}
fn feature(sense: bool) -> Step {
    Step::Feature(if sense { 5 } else { 2 }, 64, Ok(vec![0; 64]))
}
#[test]
fn playstation_basic_bluetooth_is_passive_immediate_and_explained_for_both_models() {
    for (pid, sense) in [(0xce6, true), (0x9cc, false)] {
        let h = FakeHid::new(vec![d("playstation", pid, BT, "pad")], vec![rd(&basic())]);
        let clock = FakeClock::default();
        let mut p = HidProvider::new("playstation");
        let r = p
            .poll(&h, &context(&clock, &AtomicBool::new(false)))
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].level, None);
        assert_eq!(r[0].kind, "gamepad");
        assert!(r[0].approx.as_deref().unwrap_or("").contains("Steam"));
        assert!(clock.monotonic() < Duration::from_millis(100));
        assert_eq!(p.next_poll_delay(), None);
        assert!(p.diagnostics().join("\n").contains("basic Bluetooth mode"));
        assert_eq!(sense, pid == 0xce6);
        h.done();
    }
}
#[test]
fn playstation_already_full_bluetooth_never_gets_feature_switch() {
    for (pid, sense) in [(0xce6, true), (0x9cc, false)] {
        let h = FakeHid::new(
            vec![d("playstation", pid, BT, "pad")],
            vec![rd(&[0x2f, 1]), rd(&ps(sense, true, 7))],
        );
        let r = poll("playstation", &h);
        assert_eq!((r[0].level, r[0].charging), (Some(70), Some(false)));
        assert_eq!(r[0].precision, Precision::Coarse);
        assert!(r[0].approx.as_deref().unwrap_or("").starts_with("about "));
        h.done();
    }
}
#[test]
fn playstation_full_mode_feature_is_opt_in_and_usb_uses_harmless_feature() {
    for (pid, sense) in [(0xce6, true), (0x9cc, false)] {
        for bt in [false, true] {
            let h = FakeHid::new(
                vec![d("playstation", pid, if bt { BT } else { USB }, "pad")],
                vec![feature(sense), rd(&ps(sense, bt, 0x15))],
            );
            let clock = FakeClock::default();
            let cancel = AtomicBool::new(false);
            let mut c = context(&clock, &cancel);
            c.playstation_full_mode = bt;
            let r = HidProvider::new("playstation").poll(&h, &c).unwrap();
            assert_eq!((r[0].level, r[0].charging), (Some(50), Some(true)));
            h.done();
        }
    }
}
#[test]
fn playstation_two_usb_keys_are_distinct_and_stable_when_enumeration_reverses() {
    let a = d("playstation", 0xce6, USB, "");
    let b = d(
        "playstation",
        0xce6,
        "hid#vid_054c&pid_0ce6#c-controller&0&0000#service",
        "",
    );
    let mut s = vec![];
    for _ in 0..2 {
        s.extend([
            feature(true),
            rd(&ps(true, false, 8)),
            feature(true),
            rd(&ps(true, false, 0x13)),
        ]);
    }
    let mut h = FakeHid::new(vec![a, b], s);
    let clock = FakeClock::default();
    let c = AtomicBool::new(false);
    let mut p = HidProvider::new("playstation");
    let first = p.poll(&h, &context(&clock, &c)).unwrap();
    h.infos.reverse();
    let second = p.poll(&h, &context(&clock, &c)).unwrap();
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].key, first[1].key);
    assert_eq!(
        first.iter().map(|r| (&r.key, r.level)).collect::<Vec<_>>(),
        second.iter().map(|r| (&r.key, r.level)).collect::<Vec<_>>()
    );
    assert!(first.iter().all(|r| !r.key.ends_with(':')));
    h.done();
}
#[test]
fn playstation_collections_share_trusted_identity_but_not_other_usb_pad() {
    let mut a = d("playstation", 0xce6, USB, "");
    a.container = Some("one-pad".into());
    let mut a2 = a.clone();
    a2.path = "hid#vid_054c&pid_0ce6&col02#b-controller&0&0001#service".into();
    let b = d(
        "playstation",
        0xce6,
        "hid#vid_054c&pid_0ce6#c-controller&0&0000#service",
        "",
    );
    let h = FakeHid::new(
        vec![a, a2, b],
        vec![
            feature(true),
            rd(&ps(true, false, 8)),
            feature(true),
            rd(&ps(true, false, 8)),
            feature(true),
            rd(&ps(true, false, 4)),
        ],
    );
    let r = poll("playstation", &h);
    assert_eq!(r.len(), 2);
    assert_ne!(r[0].key, r[1].key);
    h.done();
}
#[test]
fn playstation_trusted_usb_source_wins_basic_bluetooth_without_merging_other_pad() {
    let a = d("playstation", 0xce6, BT, "shared");
    let b = d("playstation", 0xce6, USB, "shared");
    let other = d(
        "playstation",
        0xce6,
        "hid#vid_054c&pid_0ce6#c-other&0&0000#service",
        "other",
    );
    let h = FakeHid::new(
        vec![a, b, other],
        vec![
            rd(&basic()),
            feature(true),
            rd(&ps(true, false, 0x16)),
            feature(true),
            rd(&ps(true, false, 4)),
        ],
    );
    let r = poll("playstation", &h);
    assert_eq!(r.len(), 2);
    assert_eq!(
        r.iter().find(|r| r.key.ends_with("SHARED")).unwrap().level,
        Some(60)
    );
    assert_eq!(
        r.iter().find(|r| r.key.ends_with("OTHER")).unwrap().level,
        Some(40)
    );
    h.done();
}
fn nreport(rid: u8, b: u8) -> Vec<u8> {
    let mut r = vec![0; 49];
    r[0] = rid;
    r[1] = 0x5a;
    r[2] = b;
    if rid == 0x21 {
        r[13] = 0x82;
        r[14] = 2;
    }
    r
}
fn nwrite(counter: u8) -> Step {
    Step::Write(padded(
        &[1, counter & 15, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 2],
        49,
    ))
}
fn nsimple(counter: u8, b: u8) -> Vec<Step> {
    let mut s = vec![rd(&[0x3f, 0, 8])];
    s.extend(empty(20));
    s.extend([nwrite(counter), rd(&nreport(0x21, b))]);
    s
}
fn nsilent(counter: u8) -> Vec<Step> {
    let mut s = empty(20);
    s.push(nwrite(counter));
    s.extend(empty(60));
    s.push(nwrite(counter.wrapping_add(1)));
    s.extend(empty(60));
    s
}
const NBT: &str = "hid#vid&0002057e_pid&2009#pad-one&0&0000#service";
#[test]
fn nintendo_simple_mode_exact_49byte_device_info_query_and_coarse_label() {
    let h = FakeHid::new(vec![d("nintendo", 0x2009, NBT, "pad")], nsimple(0, 0x60));
    let clock = FakeClock::default();
    let r = HidProvider::new("nintendo")
        .poll(&h, &context(&clock, &AtomicBool::new(false)))
        .unwrap();
    assert_eq!(
        (
            &*r[0].name,
            r[0].level,
            r[0].charging,
            &*r[0].kind,
            &*r[0].source
        ),
        (
            "Nintendo Switch Pro Controller",
            Some(75),
            Some(false),
            "gamepad",
            "nintendo"
        )
    );
    assert_eq!(r[0].approx.as_deref(), Some("about 75% (medium)"));
    assert_eq!(clock.monotonic(), Duration::from_millis(100));
    h.done();
}
#[test]
fn nintendo_already_full_is_read_without_writes_and_undefined_level_rejected() {
    for (byte, expected) in [(0x90, Some((100, true))), (0xf0, None)] {
        let h = FakeHid::new(
            vec![d("nintendo", 0x2009, NBT, "pad")],
            vec![rd(&nreport(0x30, byte))],
        );
        let r = poll("nintendo", &h);
        assert_eq!(
            r.first().map(|r| (r.level.unwrap(), r.charging.unwrap())),
            expected
        );
        h.done();
    }
}
#[test]
fn nintendo_retry_second_request_and_silence_have_bounded_timing() {
    for success in [false, true] {
        let mut s = empty(20);
        s.push(nwrite(0));
        s.extend(empty(60));
        s.push(nwrite(1));
        if success {
            s.push(rd(&nreport(0x21, 0x40)));
        } else {
            s.extend(empty(60));
        }
        let h = FakeHid::new(vec![d("nintendo", 0x2009, NBT, "pad")], s);
        let clock = FakeClock::default();
        let r = HidProvider::new("nintendo")
            .poll(&h, &context(&clock, &AtomicBool::new(false)))
            .unwrap();
        assert_eq!(
            r.first().and_then(|r| r.level),
            if success { Some(50) } else { None }
        );
        assert_eq!(
            clock.monotonic(),
            Duration::from_millis(if success { 400 } else { 700 })
        );
        h.done();
    }
}
#[test]
fn nintendo_sleeping_cache_drops_charging_marks_last_known_and_expires() {
    let clock = FakeClock::default();
    let c = AtomicBool::new(false);
    let mut p = HidProvider::new("nintendo");
    let mut s = nsimple(0, 0x70);
    s.extend(nsilent(1));
    s.extend(nsilent(3));
    let h = FakeHid::new(vec![d("nintendo", 0x2009, NBT, "pad")], s);
    let live = p.poll(&h, &context(&clock, &c)).unwrap();
    clock.0.store(1000, Ordering::Relaxed);
    let sleepy = p.poll(&h, &context(&clock, &c)).unwrap();
    assert_eq!(sleepy[0].key, live[0].key);
    assert_eq!(
        (sleepy[0].level, sleepy[0].charging, &sleepy[0].connection),
        (Some(75), Some(false), &Connection::Sleeping)
    );
    assert_eq!(
        sleepy[0].approx.as_deref(),
        Some("about 75% (medium) (last known value)")
    );
    clock.0.store(300000, Ordering::Relaxed);
    assert!(p.poll(&h, &context(&clock, &c)).unwrap().is_empty());
    h.done();
}
#[test]
fn nintendo_counter_wraps_after_16_requests_and_usb_unknown_devices_never_open() {
    let mut s = vec![];
    for n in 0..17 {
        s.extend(nsimple(n, 0x60));
    }
    let h = FakeHid::new(vec![d("nintendo", 0x2009, NBT, "pad")], s);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("nintendo");
    for _ in 0..17 {
        assert_eq!(
            p.poll(&h, &context(&clock, &AtomicBool::new(false)))
                .unwrap()[0]
                .level,
            Some(75)
        );
    }
    h.done();
    let mut unknown = d("nintendo", 0x2009, NBT, "pad");
    unknown.product_id = 0x2017;
    let h = FakeHid::new(
        vec![
            unknown,
            d("nintendo", 0x2009, "hid#vid_057e&pid_2009#usb", ""),
        ],
        vec![],
    );
    assert!(poll("nintendo", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
    assert!(is_bluetooth(NBT));
    assert!(!is_bluetooth("hid#vid_057e&pid_2009#usb"));
    h.done();
}
#[test]
fn nintendo_two_joycons_keep_separate_ids_names_and_levels() {
    let mut s = nsimple(0, 0x80);
    s.extend(nsimple(1, 0x20));
    let h = FakeHid::new(
        vec![
            d("nintendo", 0x2006, "hid#vid&0002057e_pid&2006#left", "left"),
            d(
                "nintendo",
                0x2007,
                "hid#vid&0002057e_pid&2007#right",
                "right",
            ),
        ],
        s,
    );
    let r = poll("nintendo", &h);
    assert_eq!(r.len(), 2);
    assert_eq!(
        (&*r[0].name, r[0].level),
        ("Nintendo Joy-Con (L)", Some(100))
    );
    assert_eq!(
        (&*r[1].name, r[1].level),
        ("Nintendo Joy-Con (R)", Some(25))
    );
    assert_ne!(r[0].key, r[1].key);
    h.done();
}
fn eight(rid: u8, b: u8) -> Vec<u8> {
    let mut r = vec![0; 34];
    r[0] = rid;
    r[1] = 8;
    r[14] = b;
    r
}
const EBT: &str = "hid#vid&00022dc8_pid&6006#pad";
#[test]
fn eightbitdo_passive_enhanced_bluetooth_and_usb_report_ids() {
    for (pid, path, steps, level, charge) in [
        (0x6006, EBT, vec![rd(&eight(1, 0x80 | 62))], 62, true),
        (
            0x6003,
            "hid#vid_2dc8&pid_6003#usb",
            vec![rd(&eight(1, 75)), rd(&eight(4, 0x80 | 90))],
            90,
            true,
        ),
    ] {
        let h = FakeHid::new(vec![d("eightbitdo", pid, path, "pad")], steps);
        let r = poll("eightbitdo", &h);
        assert_eq!(
            (&*r[0].name, r[0].level, r[0].charging, &*r[0].kind),
            ("8BitDo Pro 2", Some(level), Some(charge), "gamepad")
        );
        assert_eq!(r[0].approx, None);
        assert_eq!(r[0].source, "eightbitdo");
        assert_eq!(
            r[0].via,
            if is_bluetooth(path) {
                "bluetooth"
            } else {
                "usb"
            }
        );
        assert_eq!(r[0].connection, Connection::Online);
        h.done();
    }
}
#[test]
fn eightbitdo_idle_unknown_mode_is_bounded_passive_and_explained() {
    let h = FakeHid::new(vec![d("eightbitdo", 0x6006, EBT, "pad")], empty(80));
    let clock = FakeClock::default();
    let mut p = HidProvider::new("eightbitdo");
    let r = p
        .poll(&h, &context(&clock, &AtomicBool::new(false)))
        .unwrap();
    assert_eq!(r[0].level, None);
    assert!(r[0].approx.as_deref().unwrap_or("").contains("Steam"));
    assert!(p.diagnostics().join("\n").contains("ordinary mode"));
    assert_eq!(clock.monotonic(), Duration::from_millis(400));
    h.done();
}
#[test]
fn eightbitdo_zero_padding_and_wrong_report_id_cannot_trigger_low_alerts() {
    for (rid, b) in [(1, 0), (3, 75), (1, 0x80), (1, 127)] {
        let h = FakeHid::new(
            vec![d("eightbitdo", 0x6006, EBT, "pad")],
            (0..16).map(|_| rd(&eight(rid, b))).collect(),
        );
        let mut p = HidProvider::new("eightbitdo");
        let r = p
            .poll(&h, &context(&FakeClock::default(), &AtomicBool::new(false)))
            .unwrap();
        assert_eq!(r[0].level, None);
        if rid == 1 && b == 0 {
            let diag = p.diagnostics().join("\n");
            assert!(diag.contains("no level in byte 14"));
            assert!(diag.contains("01 08"));
        }
        h.done();
    }
}
#[test]
fn eightbitdo_unopenable_still_shows_unknown_and_unknown_pid_does_not_open() {
    let i = d("eightbitdo", 0x6006, EBT, "pad");
    let h = FakeHid::new(vec![i.clone()], vec![]).fail_open(EBT, "access denied");
    let r = poll("eightbitdo", &h);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].level, None);
    let mut unknown = i;
    unknown.product_id = 0x6012;
    let h = FakeHid::new(vec![unknown], vec![]);
    assert!(poll("eightbitdo", &h).is_empty());
    assert!(h.opened.lock().unwrap().is_empty());
    h.done();
}

#[test]
fn playstation_normalized_usb_collections_preserve_identity_without_blank_serial_key() {
    let a = d(
        "playstation",
        0xce6,
        "hid#vid_054c&pid_0ce6&col01#b-controller&0&0000#service",
        "",
    );
    let b = d(
        "playstation",
        0xce6,
        "hid#vid_054c&pid_0ce6&col02#b-controller&0&0001#service",
        "",
    );
    let h = FakeHid::new(
        vec![a, b],
        vec![
            feature(true),
            rd(&ps(true, false, 8)),
            feature(true),
            rd(&ps(true, false, 8)),
        ],
    );
    let r = poll("playstation", &h);
    assert_eq!(r.len(), 1);
    assert!(!r[0].key.ends_with(':'));
    h.done();
}
