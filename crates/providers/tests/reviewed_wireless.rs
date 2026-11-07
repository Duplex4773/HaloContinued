use hb_core::*;
use hb_providers::{HidProvider, protocols::padded};
use std::{sync::atomic::AtomicBool, time::Duration};

mod common;
use common::*;

fn poll(provider: &mut HidProvider, hid: &FakeHid, clock: &FakeClock) -> Vec<Reading> {
    provider
        .poll(hid, &context(clock, &AtomicBool::new(false)))
        .unwrap()
}

fn read(bytes: &[u8]) -> Step {
    Step::Read(Ok(bytes.to_vec()))
}

#[test]
fn reviewed_receivers_use_only_their_battery_collections() {
    let mut eweadn = info(0x089d, 0x062f, 0xff01);
    eweadn.usage = 0x10;
    eweadn.output_length = Some(33);
    let mut k87 = info(0x41e4, 0x2001, 0xff02);
    k87.output_length = Some(20);
    let mut mons = info(0x3151, 0x5026, 0xffff);
    mons.interface = 2;
    mons.usage = 2;
    mons.feature_length = Some(65);

    for (source, device, kind, level, steps) in [
        (
            "eweadn",
            eweadn,
            "mouse",
            70,
            vec![
                Step::Write(padded(&[0, 0x30], 33)),
                read(&[
                    0, 0x30, 0, 1, 11, b'M', b'1', b'5', b'6', 0, 0, 0, 0, 0, 70, 1,
                ]),
            ],
        ),
        (
            "mchose",
            k87,
            "keyboard",
            100,
            vec![
                Step::Write(padded(&[0x13, 0x4a, 1, 0, 0, 2], 19)),
                read(&padded(&[0x13, 0x4a, 1, 0, 2, 99, 1], 20)),
                read(&[
                    0x13, 0x4a, 1, 0, 2, 100, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xc5,
                ]),
            ],
        ),
        (
            "monsgeek",
            mons,
            "keyboard",
            54,
            vec![
                Step::Send(padded(&[0, 0xf7], 65)),
                Step::Feature(0, 65, Ok(padded(&[0, 0, 54, 100, 1, 0, 0, 1, 0], 65))),
            ],
        ),
    ] {
        let mut wrong = device.clone();
        wrong.usage_page = 1;
        wrong.path.push_str("-input");
        let hid = FakeHid::new(vec![wrong, device.clone()], steps);
        let rows = poll(&mut HidProvider::new(source), &hid, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, Some(level));
        assert_eq!(rows[0].kind, kind);
        if kind == "keyboard" {
            assert_eq!(rows[0].charging, None);
        }
        assert_eq!(*hid.opened.lock().unwrap(), [device.path]);
        hid.done();
    }
}

#[test]
fn jabra_filters_out_unrelated_sequence_and_reads_headset_name() {
    let mut dongle = info(0x0b0e, 0x24c7, 0xff00);
    dongle.interface = 3;
    let hid = FakeHid::new(
        vec![dongle],
        vec![
            Step::Write(padded(&[5, 4, 0, 1, 0x46, 0x12, 2], 64)),
            read(&[5, 0, 4, 0, 0xc8, 0x12, 2, 0, 99]),
            read(&[5, 0, 4, 1, 0xc8, 0x12, 2, 0, 21]),
            Step::Write(padded(&[5, 4, 0, 2, 0x46, 2, 0], 64)),
            read(&[5, 0, 4, 2, 0xca, 2, 0, 3, b'6', b'5', b'X']),
        ],
    );
    let rows = poll(&mut HidProvider::new("jabra"), &hid, &FakeClock::default());
    assert_eq!(rows[0].level, Some(21));
    assert_eq!(rows[0].name, "65X");
    hid.done();
}

#[test]
fn hs80_prefers_proven_cable_identity_and_uses_power_only_fallback() {
    let mut usb = info(0x1b1c, 0x0a69, 0xff42);
    usb.interface = 3;
    usb.serial = "HS80-TEST".into();
    let mut rf = usb.clone();
    rf.product_id = 0x0a6b;
    rf.path.push_str("-rf");
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("corsair");
    let hid = FakeHid::new(
        vec![rf.clone(), usb.clone()],
        vec![
            read(&[]),
            Step::Write(padded(&[2, 8, 2, 0x0f], 64)),
            read(&[1, 0, 2, 0, 0xf4, 1]),
        ],
    );
    let rows = poll(&mut provider, &hid, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(50));
    assert!(rows[0].charging_inferred);
    hid.done();

    usb.product_id = 0x0a6a;
    usb.usage_page = 1;
    let hid = FakeHid::new(
        vec![rf, usb],
        vec![
            read(&[]),
            Step::Write(padded(&[2, 9, 2, 0x0f], 64)),
            read(&[1, 1, 2, 0, 0xbc, 2]),
        ],
    );
    let next = poll(&mut provider, &hid, &clock);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].key, rows[0].key);
    assert_eq!(next[0].level, Some(70));
    assert_eq!(next[0].charging, Some(true));
    hid.done();
}

#[test]
fn blackshark_keeps_the_receiver_open_and_refresh_does_not_bypass_quiet_period() {
    let device = info(0x1532, 0x057a, 0xff14);
    let packet = |class: u8, direction: u8, kind: u8, value: u8| {
        let mut r = padded(
            &[
                2,
                0,
                0x60,
                0,
                0,
                0,
                4 + u8::from(kind != 0),
                0,
                0,
                direction,
                class,
                kind,
                u8::from(kind != 0),
                value,
            ],
            64,
        );
        r[62] = r[..62].iter().fold(0, |sum, x| sum ^ x);
        r
    };
    let mut steps = vec![read(&[])];
    for (class, dir) in [(2, 0), (0x2a, 0), (0x2a, 0x80)] {
        steps.push(Step::Write(packet(class, dir, 0, 0)));
        steps.extend((0..50).map(|_| read(&[])));
    }
    steps.extend([
        Step::Write(packet(0x21, 0x80, 0, 0)),
        read(&packet(0x21, 0x80, 1, 97)),
        read(&packet(0x2a, 0x80, 2, 1)),
        read(&[]),
        read(&[]),
        Step::Write(packet(0x2a, 0x80, 0, 0)),
    ]);
    // The headset answers the next level query but its charge reply is lost.
    steps.extend((0..50).map(|_| read(&[])));
    steps.extend([
        Step::Write(packet(0x21, 0x80, 0, 0)),
        read(&packet(0x21, 0x80, 1, 96)),
        read(&[]),
    ]);
    let hid = FakeHid::new(vec![device], steps);
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("razer");
    assert_eq!(poll(&mut provider, &hid, &clock)[0].level, Some(97));
    clock.sleep(Duration::from_secs(60));
    assert_eq!(poll(&mut provider, &hid, &clock)[0].charging, Some(true));
    assert_eq!(hid.opened.lock().unwrap().len(), 1);
    clock.sleep(Duration::from_secs(240));
    let refreshed = poll(&mut provider, &hid, &clock);
    assert_eq!(refreshed[0].level, Some(96));
    assert_eq!(
        refreshed[0].charging, None,
        "A missed charge reply must not reuse old evidence"
    );
    provider.invalidate();
    let stale = poll(&mut provider, &hid, &clock);
    assert_eq!(stale[0].level, Some(96));
    assert_eq!(stale[0].connection, Connection::Sleeping);
    assert_eq!(stale[0].charging, None);
    hid.done();
}

#[test]
fn jabra_does_not_repeat_an_unavailable_name_query_on_each_battery_poll() {
    let mut dongle = info(0x0b0e, 0x24c7, 0xff00);
    dongle.interface = 3;
    let hid = FakeHid::new(
        vec![dongle],
        vec![
            Step::Write(padded(&[5, 4, 0, 1, 0x46, 0x12, 2], 64)),
            read(&[5, 0, 4, 1, 0xc8, 0x12, 2, 0, 21]),
            Step::Write(padded(&[5, 4, 0, 2, 0x46, 2, 0], 64)),
            read(&[5, 0, 4, 2, 0xc6, 0xfe]),
            Step::Write(padded(&[5, 4, 0, 3, 0x46, 0x12, 2], 64)),
            read(&[5, 0, 4, 3, 0xc8, 0x12, 2, 0, 20]),
        ],
    );
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("jabra");
    assert_eq!(poll(&mut provider, &hid, &clock)[0].level, Some(21));
    clock.sleep(Duration::from_secs(60));
    assert_eq!(poll(&mut provider, &hid, &clock)[0].level, Some(20));
    hid.done();
}

#[test]
fn receiver_recovers_when_its_cached_collection_becomes_inaccessible() {
    let mut primary = info(0x1b1c, 0x0a6b, 0xff42);
    primary.interface = 3;
    primary.serial = "SYNTHETIC-HS80".into();
    let mut alternate = primary.clone();
    alternate.path.push_str("-alternate");
    let infos = vec![primary.clone(), alternate.clone()];
    let steps = || {
        vec![
            read(&[]),
            Step::Write(padded(&[2, 9, 2, 0x0f], 64)),
            read(&[1, 1, 2, 0, 0xf4, 1]),
        ]
    };
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("corsair");
    let hid = FakeHid::new(infos.clone(), steps());
    assert_eq!(poll(&mut provider, &hid, &clock)[0].level, Some(50));
    hid.done();
    let inaccessible =
        FakeHid::new(infos.clone(), vec![]).fail_open(&primary.path, "collection busy");
    assert!(
        provider
            .poll(&inaccessible, &context(&clock, &AtomicBool::new(false)))
            .is_err()
    );
    let recovered = FakeHid::new(infos, steps()).fail_open(&primary.path, "collection busy");
    assert_eq!(poll(&mut provider, &recovered, &clock)[0].level, Some(50));
    assert_eq!(
        *recovered.opened.lock().unwrap(),
        [primary.path, alternate.path]
    );
    recovered.done();
}
