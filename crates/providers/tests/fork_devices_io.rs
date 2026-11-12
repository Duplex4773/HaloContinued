use hb_core::*;
use hb_providers::{HidProvider, protocols::padded};
use std::sync::atomic::AtomicBool;

mod common;
use common::*;

fn poll(provider: &mut HidProvider, hid: &FakeHid) -> Vec<Reading> {
    provider
        .poll(
            hid,
            &context(&FakeClock::default(), &AtomicBool::new(false)),
        )
        .unwrap()
}
fn reply(frame: &[u8]) -> Step {
    Step::Read(Ok(frame.into()))
}
fn split_frame(header: u8, command: u8, length: u8, level: u8, flag: u8) -> Vec<u8> {
    let mut frame = padded(&[header, command, 0, 0, 0, length, level, flag], 17);
    frame[16] = 0x55u8.wrapping_sub(frame[..16].iter().fold(0u8, |n, x| n.wrapping_add(*x)));
    frame
}

#[test]
fn split_receivers_require_their_own_reply_collection_and_check_link() {
    for (provider, vid, pid, header, length) in [
        ("irocks", 0x14a5, 0xc009, 8, 0),
        ("endgame_we", 0x3367, 0x1961, 9, 2),
    ] {
        let mut command = info(vid, pid, 0xff02);
        command.usage = 2;
        command.feature_length = Some(17);
        command.container = Some("receiver1".into());
        let mut input = info(vid, pid, 0xff01);
        input.usage = 0;
        input.path.push_str("-in");
        input.container = Some("receiver2".into());
        let mut provider = HidProvider::new(provider);
        let wrong = FakeHid::new(vec![command.clone(), input.clone()], vec![]);
        assert!(poll(&mut provider, &wrong).is_empty());
        assert!(wrong.opened.lock().unwrap().is_empty());

        input.container = command.container.clone();
        let link = [
            Step::Send(split_frame(8, 3, 0, 0, 0)),
            reply(&split_frame(header, 3, 1, 1, 0)),
        ];
        let mut bad = split_frame(header, 4, length, 100, 1);
        bad[16] ^= 1;
        let battery = [
            Step::Send(split_frame(8, 4, 0, 0, 0)),
            reply(&bad),
            reply(&split_frame(header, 4, length, 74, 0)),
        ];
        let steps = if pid == 0xc009 {
            [vec![reply(&[])], link.to_vec(), battery.to_vec()].concat()
        } else {
            [vec![reply(&[])], battery.to_vec(), link.to_vec()].concat()
        };
        let h = FakeHid::new(vec![command.clone(), input.clone()], steps);
        let rows = poll(&mut provider, &h);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].level, rows[0].charging), (Some(74), Some(false)));
        assert_eq!(rows[0].connection, Connection::Online);
        assert_eq!(rows[0].via, "wireless");
        h.done();
        if pid == 0xc009 {
            let h = FakeHid::new(
                vec![command, input],
                vec![
                    reply(&[]),
                    Step::Send(split_frame(8, 3, 0, 0, 0)),
                    reply(&split_frame(8, 3, 1, 0, 0)),
                ],
            );
            let rows = poll(&mut provider, &h);
            assert_eq!(rows[0].connection, Connection::Sleeping);
            assert_eq!(rows[0].level, Some(74));
            h.done();
        }
    }
}

#[test]
fn glorious_reads_level_and_never_queries_the_wired_id() {
    let mut receiver = info(0x258a, 0x2022, 0xffa0);
    receiver.feature_length = Some(65);
    let mut wired = info(0x258a, 0x2011, 0xffa0);
    wired.feature_length = Some(65);
    let mut provider = HidProvider::new("glorious");
    let h = FakeHid::new(
        vec![receiver.clone(), wired.clone()],
        vec![
            Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
            Step::Feature(0, 65, Ok(vec![0, 0xa1, 0, 0, 0, 0, 0x83, 0, 72])),
        ],
    );
    let rows = poll(&mut provider, &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(72), Some(false)));
    assert_eq!(
        h.opened.lock().unwrap().as_slice(),
        std::slice::from_ref(&receiver.path)
    );
    h.done();

    receiver.serial = "UNKNOWN".into();
    wired.serial = receiver.serial.clone();
    let asleep = [
        Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
        Step::Feature(0, 65, Ok(vec![0, 0xa4, 0, 0, 0, 0, 0x83, 1, 72])),
    ];
    let h = FakeHid::new(
        vec![receiver, wired],
        [asleep.to_vec(), asleep.to_vec()].concat(),
    );
    let rows = poll(&mut provider, &h);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert_eq!(rows[0].charging, Some(false));
    h.done();
}

#[test]
fn vaxee_matches_command_echo_and_reports_five_percent_steps() {
    let mut receiver = info(0x3057, 0x2001, 0xff05);
    receiver.feature_length = Some(65);
    let h = FakeHid::new(
        vec![receiver],
        vec![
            Step::Send(padded(&[14, 0xa5, 11, 1, 1], 65)),
            Step::Feature(14, 65, Ok(vec![14, 0xa5, 16, 1, 1, 1])),
            Step::Send(padded(&[14, 0xa5, 11, 1, 1], 65)),
            Step::Feature(14, 65, Ok(vec![14, 0xa5, 11, 1, 1, 17])),
            Step::Send(padded(&[14, 0xa5, 16, 1, 1], 65)),
            Step::Feature(14, 65, Ok(vec![14, 0xa5, 16, 1, 1, 1])),
        ],
    );
    let rows = poll(&mut HidProvider::new("vaxee"), &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(85), Some(true)));
    assert_eq!(rows[0].precision, Precision::Coarse);
    h.done();
}

#[test]
fn keyboard_queries_use_exact_vendor_collections_and_keep_unknown_charge() {
    let mut alienware = info(0x413c, 0x211d, 0xff00);
    alienware.interface = 1;
    alienware.feature_length = Some(65);
    let mut urx = info(0x36b0, 0x3002, 0xff60);
    urx.interface = 1;
    urx.usage = 0x61;
    urx.output_length = Some(33);
    for (provider, i, send, response, charging) in [
        (
            "alienware",
            alienware,
            Step::Send(padded(&[0, 0x0e, 0xa1], 65)),
            vec![0, 0x0e, 0xa1, 92, 4],
            Some(true),
        ),
        (
            "urx",
            urx,
            Step::Write(padded(&[0, 8, 0, 7], 32)),
            vec![8, 0, 7, 76],
            None,
        ),
    ] {
        let mut wrong = i.clone();
        wrong.usage_page = 1;
        wrong.path.push_str("-keyboard");
        let h = FakeHid::new(
            vec![wrong, i.clone()],
            vec![reply(&[]), send, reply(&response)],
        );
        let rows = poll(&mut HidProvider::new(provider), &h);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "keyboard");
        assert_eq!(rows[0].charging, charging);
        assert_eq!(h.opened.lock().unwrap().as_slice(), &[i.path]);
        h.done();
    }
}

#[test]
fn aula_shared_receiver_must_identify_as_f87_pro_before_battery_query() {
    let mut i = info(0x3554, 0xfa09, 0xff02);
    i.usage = 2;
    i.output_length = Some(20);
    let frame = |prefix: &[u8]| {
        let mut r = padded(prefix, 20);
        r[19] = r[..19].iter().fold(0u8, |n, x| n.wrapping_add(*x));
        r
    };
    for model in [[3, 0, 0, 0, 0, 0x8f], [3, 0, 0, 0, 1, 0x0b]] {
        let identity = frame(&[&[0x13, 5, 1, 0, 6], model.as_slice()].concat());
        let mut steps = vec![
            reply(&[]),
            Step::Write(frame(&[0x13, 5, 1])),
            reply(&identity),
        ];
        let supported = model[4] == 1;
        if supported {
            steps.extend([
                reply(&[]),
                Step::Write(frame(&[0x13, 0x4a, 1])),
                reply(&frame(&[0x13, 0x4a, 1, 0, 2, 95, 7])),
            ]);
        }
        let h = FakeHid::new(vec![i.clone()], steps);
        let rows = poll(&mut HidProvider::new("aula"), &h);
        assert_eq!(rows.len(), usize::from(supported));
        if supported {
            assert_eq!((rows[0].level, rows[0].charging), (Some(95), None));
        }
        h.done();
    }
}

#[test]
fn poly_bt600_attaches_and_checks_reply_address_and_operation() {
    let mut i = info(0x047f, 0x02f7, 0xffa2);
    i.usage = 3;
    i.interface = 3;
    i.output_length = Some(64);
    let packet =
        |n, address, op, group, item| vec![7, 1, 1, 0x10, n, address, 0, 0, op, group, item];
    let write =
        |n, address, op, group, item| Step::Write(padded(&packet(n, address, op, group, item), 64));
    let mut battery = packet(7, 2, 3, 10, 26);
    battery.extend([7, 11, 1]);
    let h = FakeHid::new(
        vec![i],
        vec![
            reply(&[]),
            Step::Write(vec![6, 1]),
            write(7, 0, 1, 1, 2),
            reply(&packet(7, 0, 8, 1, 2)),
            write(7, 0x20, 1, 1, 2),
            reply(&packet(7, 2, 8, 1, 2)),
            write(6, 0x20, 2, 10, 26),
            reply(&packet(7, 0, 3, 10, 26)),
            reply(&battery),
        ],
    );
    let rows = poll(&mut HidProvider::new("plantronics"), &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(70), Some(true)));
    h.done();
}

#[test]
fn barracuda_x_checks_sequence_and_restores_local_routing() {
    let mut i = info(0x1532, 0x0552, 0xff00);
    i.output_length = Some(64);
    let send = |group, seq, body: [u8; 3]| {
        Step::Write([&[1, 0x80, 7, 0x50, 0x41, group, seq], &body[..]].concat())
    };
    let receive = |group: u8, seq: u8, data: &[u8]| {
        [
            &[
                0x50,
                0x49,
                0,
                0,
                0,
                0,
                0,
                0,
                (2 + data.len()) as u8,
                0,
                group,
                seq | 0x80,
            ],
            data,
        ]
        .concat()
    };
    let h = FakeHid::new(
        vec![i],
        vec![
            reply(&[]),
            Step::Write(vec![1, 0x40]),
            reply(&[1, 0x40, 1, 1]),
            send(0x0e, 1, [2, 0xe1, 1]),
            reply(&receive(0x0e, 1, &[0])),
            send(6, 2, [1, 0, 0x31]),
            reply(&receive(6, 1, &[0, 0x36, 0x10])),
            reply(&receive(6, 2, &[0, 0xee, 0x0e])),
            send(0x0e, 3, [2, 0xe1, 0]),
            reply(&receive(0x0e, 3, &[0])),
        ],
    );
    let rows = poll(&mut HidProvider::new("razer"), &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(38), None));
    assert_eq!(rows[0].precision, Precision::Coarse);
    h.done();
}

#[test]
fn inzone_serial_accepts_a_battery_frame_split_across_reads() {
    let mut i = info(0x054c, 0x0e53, 0);
    i.usage = 0;
    i.interface = -1;
    let h = FakeHid::new(
        vec![i],
        vec![
            reply(&[]),
            Step::Write(vec![1, 0, 0xfc, 8, 0x96, 0xc3, 0x21, 1, 1, 1, 0, 0x7d]),
            Step::Write(vec![1, 0, 0xfc, 8, 0x96, 0xc3, 0x41, 2, 1, 1, 0, 0x9e]),
            Step::Write(vec![1, 0, 0xfc, 8, 0x96, 0xc3, 0x41, 4, 1, 1, 0, 0xa0]),
            reply(&[4, 0xff, 0x0b, 0, 0x96, 0xc3, 0x14, 4, 0x10, 1, 0, 0, 100, 0]),
            reply(&[0xff, 4, 0xff, 0x0b, 0, 0x96]),
            reply(&[0xc3, 0x14, 4, 0x10, 1, 0, 0, 100, 0xe6]),
        ],
    );
    let rows = poll(&mut HidProvider::new("sony_inzone"), &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(100), Some(false)));
    h.done();
}

#[test]
fn g535_uses_the_long_charge_register_on_the_answering_collection() {
    let mut vendor = info(0x046d, 0x0ac4, 0xff43);
    vendor.usage = 0x202;
    vendor.container = Some("headset-receiver".into());
    let mut consumer = info(0x046d, 0x0ac4, 0x0c);
    consumer.path.push_str("-consumer");
    consumer.container = vendor.container.clone();
    let h = FakeHid::new(
        vec![consumer, vendor.clone()],
        vec![
            reply(&[]),
            Step::Write(padded(&[0x11, 0xff, 5, 0x0d], 20)),
            reply(&[0x10, 0xff, 5, 0x0d, 0x0f, 0xe5, 1]),
        ],
    );
    let rows = poll(&mut HidProvider::new("logitech"), &h);
    assert_eq!((rows[0].level, rows[0].charging), (Some(85), Some(false)));
    assert_eq!(rows[0].kind, "headset");
    assert_eq!(h.opened.lock().unwrap().as_slice(), &[vendor.path]);
    h.done();
}
