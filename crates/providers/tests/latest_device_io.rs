use hb_core::*;
use hb_providers::{HidProvider, community::vendor_controller_battery, protocols::padded};
use std::sync::atomic::{AtomicBool, Ordering};
mod common;
use common::*;

fn poll(provider: &mut HidProvider, hid: &FakeHid, clock: &FakeClock) -> Vec<Reading> {
    provider
        .poll(hid, &context(clock, &AtomicBool::new(false)))
        .unwrap()
}

fn rd(bytes: &[u8]) -> Step {
    Step::Read(Ok(bytes.into()))
}

#[test]
fn aula_f108_requires_exact_collection_and_valid_checksum() {
    let mut device = info(0x05ac, 0x024f, 0xff60);
    device.usage = 0x61;
    device.interface = 3;
    device.output_length = Some(33);
    let mut provider = HidProvider::new("aula");
    let clock = FakeClock::default();
    let other = FakeHid::new(vec![device.clone()], vec![]);
    assert!(poll(&mut provider, &other, &clock).is_empty());
    assert!(other.opened.lock().unwrap().is_empty());
    device.product = "F108Pro Dongle".into();
    let mut request = padded(&[0, 0x20, 1], 33);
    request[32] = 0x21;
    let mut reply = request.clone();
    reply[4] = 33;
    reply[32] = 0x42;
    let mut corrupt = reply.clone();
    corrupt[4] = 80;
    let hid = FakeHid::new(
        vec![device],
        vec![Step::Write(request), rd(&corrupt), rd(&reply[1..])],
    );
    let readings = poll(&mut provider, &hid, &clock);
    assert_eq!(readings[0].level, Some(33));
    assert_eq!(readings[0].charging, None);
    assert_eq!(readings[0].kind, "keyboard");
    hid.done();
}

#[test]
fn mchose_k7_v2_validates_device_info_echo_and_checksum() {
    let device = info(0x3837, 0x1018, 0xff01);
    let reply = [
        0x4d, 1, 1, 0x10, 0, 9, 0, 0, 0x37, 0x38, 0x26, 0x40, 4, 0, 0, 0, 0, 0x10, 2, 0, 0x55, 0,
        8, 0xe4, 0xde,
    ];
    let mut corrupt = reply;
    corrupt[20] = 100;
    let hid = FakeHid::new(
        vec![device],
        vec![
            Step::Write(padded(&[0x4d, 1, 1, 0, 0, 9, 0, 0, 8], 64)),
            rd(&corrupt),
            rd(&reply),
        ],
    );
    let readings = poll(&mut HidProvider::new("mchose"), &hid, &FakeClock::default());
    assert_eq!(readings[0].level, Some(85));
    assert_eq!(readings[0].charging, Some(false));
    hid.done();
}

#[test]
fn eweadn_opens_only_the_two_collections_of_the_same_receiver() {
    let mut input = info(0x04f3, 0x026f, 0x000a);
    input.path = "input".into();
    input.container = Some("X23-ONE".into());
    input.interface = 1;
    input.usage = 0;
    input.input_length = Some(32);
    input.feature_length = Some(0);
    let mut query = input.clone();
    query.path = "query".into();
    query.input_length = Some(0);
    query.feature_length = Some(32);
    let mut unrelated = query.clone();
    unrelated.path = "other-receiver".into();
    unrelated.container = Some("X23-TWO".into());
    let mut provider = HidProvider::new("eweadn");
    let clock = FakeClock::default();
    let ambiguous = FakeHid::new(vec![input.clone(), unrelated.clone()], vec![]);
    assert!(poll(&mut provider, &ambiguous, &clock).is_empty());
    assert!(ambiguous.opened.lock().unwrap().is_empty());
    let hid = FakeHid::new(
        vec![input, unrelated, query],
        vec![
            rd(&[]),
            Step::Feature(5, 32, Ok(vec![])),
            rd(&padded(&[4, 1, 0, 1, 70, 0, 0, 0, 1, 0x11], 32)),
        ],
    );
    let readings = poll(&mut provider, &hid, &clock);
    assert_eq!(readings[0].level, Some(70));
    assert_eq!(&*hid.opened.lock().unwrap(), &["input", "query"]);
    hid.done();
}

#[test]
fn atk_prefers_proven_cable_and_caches_receiver_without_merging_vgn() {
    let mut receiver = info(0x320f, 0x5088, 0xff1c);
    receiver.usage = 0x92;
    receiver.serial = "ATK-SYNTHETIC".into();
    receiver.product = "ATK V75X 2.4G Dongle".into();
    let mut cable = receiver.clone();
    cable.product_id = 0x5055;
    cable.path = "usb-cable".into();
    cable.product = "ATK V75X".into();
    let mut vgn = receiver.clone();
    vgn.path = "vgn".into();
    vgn.product = "VGN N75 PRO 2.4G Dongle".into();
    let answer = |sequence, level, charging| {
        [
            Step::Write(padded(&[4, sequence, 0, 0x1a], 32)),
            rd(&[4, sequence, 0, 0x1a, 0, 0, 0, 0, level, charging]),
        ]
    };
    let mut provider = HidProvider::new("atk_keyboard");
    let clock = FakeClock::default();
    let hid = FakeHid::new(
        vec![receiver.clone(), cable, vgn.clone()],
        [answer(1, 91, 1), answer(2, 59, 0)].concat(),
    );
    let readings = poll(&mut provider, &hid, &clock);
    assert_eq!(readings.len(), 2);
    assert_eq!(
        (
            readings[0].via.as_str(),
            readings[0].level,
            readings[0].charging
        ),
        ("usb", Some(91), Some(true))
    );
    assert_eq!(
        (readings[1].name.as_str(), readings[1].charging),
        ("VGN N75 PRO", None)
    );
    assert_eq!(&*hid.opened.lock().unwrap(), &["usb-cable", "vgn"]);
    hid.done();
    let receiver_only = FakeHid::new(vec![receiver.clone()], answer(3, 92, 0).into());
    let fresh = poll(&mut provider, &receiver_only, &clock).remove(0);
    assert_eq!(fresh.key, readings[0].key);
    assert_eq!(fresh.charging, Some(false));
    clock.0.store(60_000, Ordering::Relaxed);
    let cached = FakeHid::new(vec![receiver], vec![]);
    let reading = poll(&mut provider, &cached, &clock).remove(0);
    assert_eq!(
        (reading.level, reading.timestamp),
        (fresh.level, fresh.timestamp)
    );
    assert!(cached.opened.lock().unwrap().is_empty());
}

#[test]
fn captured_harpe_void_delux_and_ajazz_frames_report_the_correct_battery() {
    let mut harpe = info(0x0b05, 0x1ad0, 0xff01);
    harpe.interface = 2;
    harpe.path = "hid#vid_0b05&pid_1ad0&mi_02&col03#synthetic".into();
    let mut void = info(0x1b1c, 0x0a55, 0xffc5);
    void.interface = 3;
    let mut delux = info(0x1d57, 0xfa65, 0x000a);
    delux.usage = 0;
    let mut ajazz = info(0x0c45, 0xfefc, 0xff70);
    ajazz.usage = 0x71;
    ajazz.interface = 3;
    ajazz.output_length = Some(65);
    ajazz.product = "AJAZZ AK820MAX".into();
    let mut reply = padded(&[0x55, 0x10], 64);
    reply[25..27].copy_from_slice(&[56, 1]);
    for (family, device, steps, level, charging) in [
        (
            "asus",
            harpe,
            vec![
                rd(&[]),
                Step::Write(padded(&[3, 0x12, 7], 64)),
                rd(&[3, 0x12, 7, 0, 0, 53, 2, 0x14, 0xfd, 0x0e, 0, 0]),
            ],
            53,
            false,
        ),
        (
            "corsair",
            void,
            vec![Step::Write(vec![0xc9, 0x64]), rd(&[0x64, 0, 0xc4, 0xb1, 4])],
            68,
            true,
        ),
        (
            "inphic",
            delux,
            [vec![rd(&[]); 7], vec![rd(&[3, 0x3f, 0x40, 1, 74])]].concat(),
            74,
            false,
        ),
        (
            "ajazz",
            ajazz,
            vec![
                rd(&[]),
                Step::Write(padded(&[0, 0xaa, 0x10], 65)),
                rd(&reply),
            ],
            56,
            true,
        ),
    ] {
        let hid = FakeHid::new(vec![device], steps);
        let readings = poll(&mut HidProvider::new(family), &hid, &FakeClock::default());
        assert_eq!(
            (readings[0].level, readings[0].charging),
            (Some(level), Some(charging))
        );
        if family == "inphic" {
            assert_eq!(readings[0].name, "Delux M900 Pro");
        }
        hid.done();
    }
}

#[test]
fn controller_status_requires_a_unique_matching_hid_collection() {
    let mut beitong = info(0x20bc, 0x515c, 0x00ff);
    beitong.interface = 1;
    beitong.usage = 3;
    let mut gamesir = info(0x3537, 0x10c6, 0xfff0);
    gamesir.usage = 0x40;
    let mut state = padded(&[0x12], 64);
    state[35..37].copy_from_slice(&[1, 95]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for (device, request, reply, level) in [
        (
            beitong,
            padded(&[2, 0x15], 64),
            padded(&[2, 0x15, 90, 0, 0x51], 64),
            90,
        ),
        (gamesir, vec![0x0f, 0x0b], state, 95),
    ] {
        let (vid, pid) = (device.vendor_id, device.product_id);
        let mut another = device.clone();
        another.path.push_str("-duplicate");
        let duplicate = FakeHid::new(vec![device.clone(), another], vec![]);
        let ctx = context(&clock, &cancel);
        assert!(
            vendor_controller_battery(vid, pid, &duplicate, &ctx)
                .unwrap()
                .is_none()
        );
        assert!(duplicate.opened.lock().unwrap().is_empty());
        let hid = FakeHid::new(
            vec![device],
            vec![rd(&[]), Step::Write(request), rd(&[0x13]), rd(&reply)],
        );
        let battery = vendor_controller_battery(vid, pid, &hid, &ctx)
            .unwrap()
            .unwrap();
        assert_eq!((battery.level, battery.charging), (level, Some(true)));
        hid.done();
    }
}
