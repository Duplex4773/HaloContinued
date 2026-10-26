use hb_core::*;
use hb_providers::{
    HidProvider,
    catalog::DEVICES,
    protocols::{padded, pulsar_request, razer_request},
};
use std::sync::atomic::{AtomicBool, Ordering};
mod common;
use common::*;

fn poll(provider: &mut HidProvider, hid: &FakeHid, clock: &FakeClock) -> PollResult {
    provider.poll(hid, &context(clock, &AtomicBool::new(false)))
}
fn device(vid: u16, pid: u16, page: u16, usage: u16) -> HidInfo {
    let mut i = info(vid, pid, page);
    i.usage = usage;
    i
}
fn rd(r: &[u8]) -> Step {
    Step::Read(Ok(r.into()))
}
fn packet(hex: &str) -> Vec<u8> {
    hex.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

#[test]
fn pinned_reviewed_catalog_has_unique_ids_and_excludes_unconfirmed_transports() {
    let rows: serde_json::Value =
        serde_json::from_str(include_str!("../reviewed-devices.json")).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for entry in rows.as_array().unwrap() {
        let row = &entry["device"];
        let (provider, vid, pid) = (
            row[0].as_str().unwrap(),
            row[1].as_u64().unwrap() as u16,
            row[2].as_u64().unwrap() as u16,
        );
        assert!(seen.insert((provider, vid, pid)));
        let d = DEVICES
            .iter()
            .find(|d| d.provider == provider && d.vid == vid && d.pid == pid)
            .unwrap();
        assert_eq!(d.name, row[3].as_str().unwrap());
        assert_eq!(d.variant, row[4].as_str().unwrap());
        assert_eq!(entry["revision"].as_str().unwrap().len(), 40);
        assert!(entry["pull_request"].as_u64().unwrap() > 0);
    }
    assert_eq!(seen.len(), 42);
    for (vid, pid) in [
        (0x1532, 0x00df),
        (0x1532, 0x00de),
        (0x03f0, 0x098d),
        (0x3710, 0x3402),
        (0x373b, 0x11b6),
        (0x0b05, 0x18d7),
        (0x10f5, 0x2250),
        (0x260d, 0x1084),
    ] {
        assert!(
            !DEVICES.iter().any(|d| d.vid == vid && d.pid == pid),
            "{vid:04x}:{pid:04x}"
        );
    }
}

#[test]
fn new_compx_id_only_additions_use_existing_exchange_and_literal_capture() {
    for (vid, pid) in [
        (0x3554, 0xf5d5),
        (0x3554, 0xf55e),
        (0x3554, 0xf5f7),
        (0x3554, 0xf5f6),
        (0x3770, 0x0300),
        (0x373b, 0x11d9),
    ] {
        let h = FakeHid::new(
            vec![device(vid, pid, 0xff02, 2)],
            vec![
                rd(&[]),
                Step::Write(pulsar_request()),
                Step::Read(Ok(packet(
                    "08 04 00 00 00 02 5a 00 10 ad 00 00 00 00 00 00 30",
                ))),
            ],
        );
        let rows = poll(&mut HidProvider::new("pulsar"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, Some(90));
        assert_eq!(rows[0].kind, "mouse");
        h.done();
    }
}

#[test]
fn gwolves_receiver_without_feature_collection_uses_confirmed_compx_alternative() {
    let mut i = device(0x33e4, 0x3854, 0xff02, 2);
    i.output_length = Some(17);
    let h = FakeHid::new(
        vec![i.clone()],
        vec![
            rd(&[]),
            Step::Write(pulsar_request()),
            Step::Read(Ok(packet(
                "08 04 00 00 00 02 50 00 0f ad 00 00 00 00 00 00 3b",
            ))),
        ],
    );
    let clock = FakeClock::default();
    let mut p = HidProvider::new("gwolves");
    let first = poll(&mut p, &h, &clock).unwrap().remove(0);
    assert_eq!(first.level, Some(80));
    h.done();
    clock.0.store(900_000, Ordering::Relaxed);
    let silent = FakeHid::new(
        vec![i],
        vec![
            rd(&[]),
            Step::Write(pulsar_request()),
            rd(&[]),
            rd(&[]),
            rd(&[]),
            rd(&[]),
        ],
    );
    let cached = poll(&mut p, &silent, &clock).unwrap().remove(0);
    assert_eq!(cached.level, Some(80));
    assert_eq!(cached.timestamp, first.timestamp);
    assert_eq!(cached.connection, Connection::Sleeping);
    silent.done();
    assert!(
        poll(&mut p, &FakeHid::new(vec![], vec![]), &clock)
            .unwrap()
            .is_empty()
    );
}

fn flow_frame(cmd: u8, level: u8, charging: u8) -> Vec<u8> {
    let mut r = padded(&[8, cmd, 0, 0, 0, 0x80, level, charging], 17);
    r[16] = 0x55u8.wrapping_sub(r[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    r
}
#[test]
fn flow_lite_requires_vendor_collection_and_online_check_with_keyboard_flag() {
    let apple = FakeHid::new(vec![device(0x05ac, 0x024f, 1, 6)], vec![]);
    assert!(
        poll(
            &mut HidProvider::new("lofree"),
            &apple,
            &FakeClock::default()
        )
        .unwrap()
        .is_empty()
    );
    assert!(apple.opened.lock().unwrap().is_empty());
    for online in [0, 1] {
        let mut steps = vec![
            Step::Write(packet("08 03 00 00 00 80 00 00 00 00 00 00 00 00 00 00 ca")),
            Step::Read(Ok(flow_frame(3, online, 0))),
        ];
        if online == 1 {
            steps.extend([
                Step::Write(packet("08 04 00 00 00 80 00 00 00 00 00 00 00 00 00 00 c9")),
                Step::Read(Ok(flow_frame(4, 62, 1))),
            ]);
        }
        let h = FakeHid::new(vec![device(0x05ac, 0x024f, 0xff02, 2)], steps);
        let rows = poll(&mut HidProvider::new("lofree"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows.len(), usize::from(online == 1));
        if online == 1 {
            assert_eq!(rows[0].kind, "keyboard");
            assert_eq!(rows[0].level, Some(62));
            assert_eq!(rows[0].charging, Some(true));
        }
        h.done();
    }
}

#[test]
fn sonix_literal_capture_uses_feature_interface_three_not_unverified_cable() {
    let mut i = device(0x3710, 0x5403, 0xff00, 1);
    i.interface = 3;
    i.feature_length = Some(65);
    let mut q = padded(&[0, 0, 8, 0x81, 1], 65);
    q[63] = 0x8a;
    let h = FakeHid::new(
        vec![i.clone()],
        vec![
            Step::Send(q),
            Step::Feature(0, 65, Ok(packet("00 01 08 81 01 00 00 18"))),
        ],
    );
    let rows = poll(&mut HidProvider::new("pulsar"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(24));
    assert_eq!(rows[0].charging, None);
    h.done();
    for wrong in [0, 1] {
        let mut i = i.clone();
        if wrong == 0 {
            i.usage_page = 1;
        } else {
            i.interface = 2;
        }
        let h = FakeHid::new(vec![i], vec![]);
        assert!(
            poll(&mut HidProvider::new("pulsar"), &h, &FakeClock::default())
                .unwrap()
                .is_empty()
        );
        assert!(h.opened.lock().unwrap().is_empty());
    }
}

#[test]
fn kingston_cloud_ii_uses_62byte_exchange_with_prepare_reads() {
    let mut steps = vec![];
    for (cmd, reply) in [
        (2, packet("0b 00 bb 02 00 00 00 4a")),
        (3, packet("0b 00 bb 03 02 00 00 00")),
    ] {
        steps.extend([
            Step::Input(6, 62, Ok(vec![])),
            Step::Write(padded(
                &[
                    6, 0, 2, 0, 0x9a, 0, 0, 0x68, 0x4a, 0x8e, 0x0a, 0, 0, 0, 0xbb, cmd, 0,
                ],
                62,
            )),
            Step::Read(Ok(reply)),
        ]);
    }
    let h = FakeHid::new(
        vec![
            device(0x0951, 0x1718, 0xff13, 1),
            device(0x0951, 0x1718, 0x0c, 1),
        ],
        steps,
    );
    let rows = poll(&mut HidProvider::new("hyperx"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(74));
    assert_eq!(rows[0].charging, Some(true));
    assert_eq!(h.opened.lock().unwrap().len(), 1);
    h.done();
}

#[test]
fn corsair_k65_uses_endpoint9_twice_without_querying_nonexistent_charging() {
    let h = FakeHid::new(
        vec![device(0x1b1c, 0x2b07, 0xff42, 1)],
        vec![
            rd(&[]),
            Step::Write(padded(&[0, 9, 2, 0x0f], 65)),
            Step::Read(Ok(packet("01 02 00 e8 03"))),
            Step::Write(padded(&[0, 9, 2, 0x0f], 65)),
            Step::Read(Ok(packet("01 02 00 e8 03"))),
        ],
    );
    let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(100));
    assert_eq!(rows[0].charging, None);
    assert_eq!(rows[0].kind, "keyboard");
    h.done();
}

#[test]
fn strix_go_reads_voltage_bands_not_constant64_and_compensates_charging() {
    for (mv, state, want) in [
        (3743u16, 3, 50),
        (3897, 0x0a, 50),
        (3665, 3, 25),
        (3828, 0x0a, 25),
        (4200, 0x0a, 100),
    ] {
        let mut i = device(0x0b05, 0x18d6, 0xff00, 1);
        i.feature_length = Some(64);
        let mut reply = padded(&[0xff, 0x1b], 64);
        reply[9] = state;
        reply[11..13].copy_from_slice(&mv.to_le_bytes());
        reply[13] = 64;
        let h = FakeHid::new(
            vec![i],
            vec![
                Step::Send(padded(&[0xff, 8, 0, 0xfd, 4, 0x12, 0xf1, 3, 0x52, 1], 64)),
                Step::Feature(0xff, 64, Ok(reply)),
            ],
        );
        let rows = poll(&mut HidProvider::new("asus"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows[0].level, Some(want));
        assert_eq!(rows[0].precision, Precision::Coarse);
        assert_eq!(rows[0].kind, "headset");
        h.done();
    }
}

#[test]
fn atk_nearlink_both_verified_transports_match_sequence_and_skip_late_report() {
    for (pid, report, page) in [(0x108c, 5, 0xffb3), (0x108b, 4, 0xffb2)] {
        let mut reply = padded(&[report, 1, 0, 0x1a], 64);
        reply[8] = if pid == 0x108c { 91 } else { 97 };
        reply[9] = u8::from(pid == 0x108b);
        let mut old = reply.clone();
        old[1] = 255;
        old[8] = 5;
        let h = FakeHid::new(
            vec![device(0x373b, pid, page, 1)],
            vec![
                rd(&[]),
                Step::Write(padded(&[report, 1, 0, 0x1a], 64)),
                Step::Read(Ok(old)),
                Step::Read(Ok(reply)),
            ],
        );
        let rows = poll(
            &mut HidProvider::new("atk_nearlink"),
            &h,
            &FakeClock::default(),
        )
        .unwrap();
        assert_eq!(rows[0].level, Some(if pid == 0x108c { 91 } else { 97 }));
        assert_eq!(rows[0].charging, Some(pid == 0x108b));
        h.done();
    }
}

#[test]
fn atk_v75x_checks_product_string_because_vid_pid_is_shared() {
    let mut i = device(0x320f, 0x5088, 0xff1c, 0x92);
    i.product = "Other keyboard".into();
    let h = FakeHid::new(vec![i.clone()], vec![]);
    assert!(
        poll(
            &mut HidProvider::new("atk_keyboard"),
            &h,
            &FakeClock::default()
        )
        .unwrap()
        .is_empty()
    );
    assert!(h.opened.lock().unwrap().is_empty());
    i.product = "ATK V75X 2.4G Dongle".into();
    i.output_length = Some(64); // Windows pads the query to the longest report.
    let h = FakeHid::new(
        vec![i],
        vec![
            Step::Write(padded(&[4, 1, 0, 0x1a], 32)),
            Step::Read(Ok(packet("04 01 00 1a 00 00 00 00 3c 01"))),
        ],
    );
    let rows = poll(
        &mut HidProvider::new("atk_keyboard"),
        &h,
        &FakeClock::default(),
    )
    .unwrap();
    assert_eq!(rows[0].level, Some(60));
    assert_eq!(rows[0].charging, Some(true));
    assert_eq!(rows[0].kind, "keyboard");
    h.done();
}

#[test]
fn aula_keyboard_accepts_both_captured_update_types_and_drops_offline_rows() {
    for (lead, frame_type) in [(0, 0), (0, 0x18), (1, 0), (1, 0x18)] {
        let mut reply = vec![0; 32 + lead];
        reply[lead..lead + 3].copy_from_slice(&[0x55, 0x10, frame_type]);
        reply[25 + lead] = 80;
        let i = device(0x0c45, 0xfefc, 0xff80, 1);
        let h = FakeHid::new(
            vec![i.clone()],
            vec![
                Step::Write(padded(&[0, 0xaa, 0x10], 33)),
                Step::Read(Ok(reply)),
            ],
        );
        let mut p = HidProvider::new("aula");
        let clock = FakeClock::default();
        let rows = poll(&mut p, &h, &clock).unwrap();
        assert_eq!(rows[0].level, Some(80));
        assert_eq!(rows[0].charging, None);
        h.done();
        let mut steps = vec![Step::Write(padded(&[0, 0xaa, 0x10], 33))];
        steps.extend(vec![rd(&[]); 15]);
        let h = FakeHid::new(vec![i], steps);
        assert!(poll(&mut p, &h, &clock).unwrap().is_empty());
        h.done();
    }
}

#[test]
fn aula_mouse_closes_transaction_and_rejects_cached_reply_after_link_down() {
    for down in [false, true] {
        let mut answer = packet("04 00 00 1a 00 00 00 00 59 00");
        answer.resize(32, 0);
        let mut steps = vec![
            Step::Write(padded(&[4, 0, 0, 1], 32)),
            rd(&[4, 0, 0, 1]),
            Step::Write(padded(&[4, 0, 0, 0x1a], 32)),
        ];
        if down {
            steps.push(rd(&[4, 0xaa, 0, 0xaa, 0, 0, 0, 0, 0]));
        }
        steps.extend([
            Step::Read(Ok(answer)),
            Step::Write(padded(&[4, 0, 0, 2], 32)),
            rd(&[4, 0, 0, 2]),
        ]);
        let h = FakeHid::new(vec![device(0x320f, 0x22a0, 0xff1c, 0x92)], steps);
        let rows = poll(&mut HidProvider::new("aula"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows.len(), usize::from(!down));
        if !down {
            assert_eq!(rows[0].level, Some(89));
        }
        h.done();
    }
}

#[test]
fn dareu_opens_input_before_feature_query_on_same_receiver_and_rejects_wrong_header() {
    let mut command = device(0x260d, 0x1074, 0xff02, 2);
    command.serial = "SYNTHETIC-DAREU".into();
    let mut input = device(0x260d, 0x1074, 0xff01, 0);
    input.path.push_str("-input");
    input.serial = command.serial.clone();
    let capture = packet("09 04 00 00 00 02 5a 00 00 00 00 00 00 00 00 00 ec");
    let h = FakeHid::new(
        vec![command, input],
        vec![
            rd(&[]),
            Step::Send(pulsar_request()),
            rd(&packet(
                "08 04 00 00 00 02 5a 00 00 00 00 00 00 00 00 00 ed",
            )),
            Step::Read(Ok(capture)),
        ],
    );
    let rows = poll(&mut HidProvider::new("dareu"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(90));
    assert_eq!(rows[0].charging, None);
    h.done();
}

#[test]
fn passive_inphic_and_attack_shark_receive_zero_writes_and_keep_models_separate() {
    for (pid, frame, want, charging, precision) in [
        (0xfa65, "03 95 40 01 4b", 75, Some(false), Precision::Exact),
        (0xfa60, "03 55 40 01 1f", 31, Some(false), Precision::Exact),
        (0xfa60, "03 10 40 01 09", 90, None, Precision::Coarse),
        (0xfa60, "03 4e 40 01 1e", 30, Some(false), Precision::Exact),
    ] {
        let i = device(0x1d57, pid, 0x000a, 0);
        let h = FakeHid::new(vec![i], vec![Step::Read(Ok(packet(frame)))]);
        let rows = poll(&mut HidProvider::new("inphic"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows[0].level, Some(want));
        assert_eq!(rows[0].charging, charging);
        assert_eq!(rows[0].precision, precision);
        h.done();
    }
    let h = FakeHid::new(
        vec![device(0x1d57, 0xfa60, 0x000a, 0)],
        vec![rd(&[3, 0x55, 0x40, 3, 30]), rd(&[3, 0x55, 0x40, 1, 30])],
    );
    let rows = poll(&mut HidProvider::new("inphic"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].charging, Some(true));
    assert_eq!(rows[0].level, Some(30));
    h.done();
}

#[test]
fn passive_rapoo_both_shapes_on_all_verified_connections_use_no_writes() {
    for pid in [0x1413, 0x4613, 0x1924, 0x4824] {
        for capture in [
            packet("07 00 00 00 00 00 00 02 54"),
            packet("bd 01 00 07 00 46 00 01 54"),
        ] {
            let h = FakeHid::new(
                vec![device(0x24ae, pid, 0xff00, 2)],
                vec![Step::Read(Ok(capture.clone()))],
            );
            let rows = poll(&mut HidProvider::new("rapoo"), &h, &FakeClock::default()).unwrap();
            assert_eq!(rows[0].level, Some(84));
            assert_eq!(rows[0].charging, Some(capture[0] == 7));
            h.done();
        }
    }
}

#[test]
fn mchose_v9_variants_have_distinct_keys_and_hardware_specific_charging() {
    for (vid, pid, state, charging) in [
        (0x291d, 0x385d, 0, Some(true)),
        (0x291d, 0x385d, 2, Some(false)),
        (0x291d, 0x385d, 9, None),
        (0x3837, 0x6008, 2, None),
        (0x3837, 0x600a, 3, None),
    ] {
        let turbo = vid == 0x3837;
        let mut steps = vec![];
        if turbo {
            steps.push(Step::Write(padded(
                &[0x55, 0xaa, 1, 0, 1, 0, 1, 0x0b, 0],
                64,
            )));
            steps.extend(vec![rd(&[]); 3]);
        }
        steps.extend([
            Step::Write(padded(&[0x55, 0x65, 1], 64)),
            rd(&[0x55, 0x65, 60, state]),
        ]);
        let h = FakeHid::new(vec![device(vid, pid, 0xff01, 1)], steps);
        let rows = poll(&mut HidProvider::new("mchose"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows[0].level, Some(60));
        assert_eq!(rows[0].kind, "headset");
        assert_eq!(rows[0].charging, charging);
        assert!(rows[0].key.contains(if turbo { "6008" } else { "385d" }));
        h.done();
    }
}

#[test]
fn turbo_explicit_sleep_reply_stops_before_fallback_and_keeps_cached_reading_grey() {
    let i = device(0x3837, 0x6008, 0xff01, 1);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("mchose");
    let query = padded(&[0x55, 0xaa, 1, 0, 1, 0, 1, 0x0b, 0], 64);
    let live = FakeHid::new(
        vec![i.clone()],
        vec![
            Step::Write(query.clone()),
            rd(&[0x55, 0xaa, 1, 0, 3, 0, 1, 0x0b, 1, 60, 0]),
        ],
    );
    let row = poll(&mut p, &live, &clock).unwrap().remove(0);
    assert_eq!(row.level, Some(60));
    live.done();
    let asleep = FakeHid::new(
        vec![i],
        vec![
            Step::Write(query),
            rd(&[0x55, 0xaa, 1, 0, 3, 0, 1, 0x0b, 0, 60, 0]),
        ],
    );
    let r = poll(&mut p, &asleep, &clock).unwrap().remove(0);
    assert_eq!(r.connection, Connection::Sleeping);
    assert_eq!(r.timestamp, row.timestamp);
    asleep.done();
}

#[test]
fn finalmouse_real_capture_voltage_fallback_charging_and_sleep_stop() {
    for charging in [false, true] {
        let mut steps = vec![
            Step::Write(padded(&[4, 2, 0xa4, 0], 64)),
            rd(&[5, 3, 0x24, 1, 1]),
            Step::Write(padded(&[4, 2, 0xa6, 0], 64)),
        ];
        steps.extend(vec![rd(&[]); 10]);
        steps.extend([
            Step::Write(padded(&[4, 2, 0x85, 0], 64)),
            if charging {
                rd(&[5, 4, 5, 2, 0xb4, 0x10])
            } else {
                rd(&[5, 4, 5, 2, 0x64, 0x10])
            },
            Step::Write(padded(&[4, 2, 0xa5, 0], 64)),
            rd(&[5, 3, 0x25, 1, u8::from(charging)]),
        ]);
        let i = device(0x361d, 0x0100, 0xff00, 1);
        let h = FakeHid::new(vec![i.clone()], steps);
        let clock = FakeClock::default();
        let mut p = HidProvider::new("finalmouse");
        let rows = poll(&mut p, &h, &clock).unwrap();
        assert_eq!(rows[0].level, Some(if charging { 88 } else { 78 }));
        assert_eq!(rows[0].charging, Some(charging));
        assert_eq!(rows[0].precision, Precision::Coarse);
        h.done();
        let off = FakeHid::new(
            vec![i],
            vec![
                Step::Write(padded(&[4, 2, 0xa4, 0], 64)),
                rd(&[5, 3, 0x24, 1, 0]),
            ],
        );
        assert_eq!(
            poll(&mut p, &off, &clock).unwrap()[0].connection,
            Connection::Sleeping
        );
        off.done();
    }
}

fn skull_reply(level: u8) -> Vec<u8> {
    vec![
        7, 15, 0, 5, 0x5b, 2, 0, 0xd6, 0x0c, 5, 0x5d, 5, 0, 0xd6, 0x0c, 0, 0, level,
    ]
}
#[test]
fn skullcandy_skips_old_snapshot_and_parses_indication_behind_ack() {
    for pid in [0x3210, 0x5310] {
        let h = FakeHid::new(
            vec![device(0x34f0, pid, 0xff13, 1)],
            vec![
                Step::Input(7, 62, Ok(skull_reply(80))),
                Step::Write(padded(&[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c, 0], 62)),
                Step::Input(7, 62, Ok(skull_reply(80))),
                Step::Input(7, 62, Ok(skull_reply(79))),
            ],
        );
        let rows = poll(
            &mut HidProvider::new("skullcandy"),
            &h,
            &FakeClock::default(),
        )
        .unwrap();
        assert_eq!(rows[0].level, Some(79));
        assert_eq!(rows[0].charging, None);
        h.done();
    }
}

fn gsi_inputs(doc: &str) -> Vec<Step> {
    doc.as_bytes()
        .chunks(59)
        .map(|data| {
            let mut r = vec![7, data.len() as u8, 0];
            r.extend_from_slice(data);
            Step::Input(7, 62, Ok(r))
        })
        .collect()
}
#[test]
fn turtle_pro_uses_captured_opener_and_reassembles_json_in_bounded_frames() {
    for pid in [0x229b, 0x229e] {
        let q = packet(
            "06 18 00 05 5a 14 00 02 99 48 01 c0 4a 01 00 00 00 00 00 00 00 61 00 53 49 6e 66",
        );
        let mut steps = vec![Step::Write(padded(&q, 62))];
        steps.extend(gsi_inputs("{\"OR\":\"GSI\",\"KVP\":{\"220\":\"Personal name is ignored\",\"240\":\"77\",\"250\":\"1\"}}"));
        let h = FakeHid::new(vec![device(0x10f5, pid, 0xff13, 1)], steps);
        let rows = poll(
            &mut HidProvider::new("turtlebeach"),
            &h,
            &FakeClock::default(),
        )
        .unwrap();
        assert_eq!(rows[0].level, Some(77));
        assert_eq!(rows[0].charging, Some(true));
        assert_eq!(rows[0].name, "Turtle Beach Stealth Pro II");
        h.done();
    }
}
#[test]
fn turtle_700_uses_confirmed_coap_tunnel_without_inventing_charging() {
    for pid in [0x2251, 0x2253] {
        let mut steps = vec![Step::Input(7, 62, Ok(vec![])); 5];
        steps.push(Step::Write(padded(
            &[6, 6, 0, 5, 0x5a, 2, 0, 0x42, 0x99],
            62,
        )));
        steps.extend(vec![Step::Input(7, 62, Ok(vec![])); 5]);
        steps.push(Step::Write(padded(
            &[
                6, 14, 0, 5, 0x5a, 10, 0, 2, 0x99, 0x40, 1, 0, 1, 0xb3, b'G', b'S', b'I',
            ],
            62,
        )));
        steps.extend(gsi_inputs(
            "{\"OR\":\"GSI\",\"KVP\":{\"240\":71,\"250\":1}}",
        ));
        let h = FakeHid::new(vec![device(0x10f5, pid, 0xff13, 1)], steps);
        let rows = poll(
            &mut HidProvider::new("turtlebeach"),
            &h,
            &FakeClock::default(),
        )
        .unwrap();
        assert_eq!(rows[0].level, Some(71));
        assert_eq!(rows[0].charging, None);
        h.done();
    }
}

#[test]
fn ajazz_receiver_never_animates_cached_charging_and_prefers_info_to_announcement() {
    for (vid, pid, wired) in [(0x249a, 0x5c2f, false), (0x248a, 0x5d2e, true)] {
        let mut r = vec![0; 32];
        r[..4].copy_from_slice(&[0x10, 0, 1, 0x0b]);
        r[13] = 56;
        r[14] = 0;
        r[31] = 56;
        let mut event = vec![0; 32];
        event[..3].copy_from_slice(&[0xc0, 1, 55]);
        let mut i = device(vid, pid, 0, 0);
        i.interface = 2;
        i.output_length = Some(33);
        let h = FakeHid::new(
            vec![i],
            vec![
                Step::Write(padded(&[0, 0x10], 33)),
                Step::Read(Ok(event)),
                Step::Read(Ok(r)),
            ],
        );
        let rows = poll(&mut HidProvider::new("ajazz"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows[0].level, Some(56));
        assert_eq!(rows[0].charging, Some(wired));
        h.done();
    }
}

fn shield_steps(level: u8, charging: u8, serial: u8) -> Vec<Step> {
    let mut steps = vec![];
    for cmd in [7, 0x3a, 0x10] {
        let mut r = padded(&[3, cmd, 0], 33);
        if cmd == 7 {
            r[14] = level;
        }
        if cmd == 0x3a {
            r[3..6].copy_from_slice(&[1, 1, charging]);
        }
        if cmd == 0x10 {
            r[5] = serial;
        }
        steps.extend([Step::Write(padded(&[4, cmd, 0], 33)), Step::Read(Ok(r))]);
    }
    steps
}
#[test]
fn shield_board_identity_unifies_usb_and_bluetooth_and_prefers_usb() {
    let mut bt = device(0x0955, 0x7214, 1, 5);
    bt.path = "hid#vid&0955#bluetooth".into();
    let usb = device(0x0955, 0x7214, 1, 5);
    let mut steps = shield_steps(80, 0, 1);
    steps.extend(shield_steps(81, 2, 1));
    let h = FakeHid::new(vec![bt, usb], steps);
    let rows = poll(&mut HidProvider::new("shield"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].via, "usb");
    assert_eq!(rows[0].level, Some(81));
    assert_eq!(rows[0].charging, Some(true));
    assert_eq!(rows[0].kind, "gamepad");
    h.done();
}

#[test]
fn duplicate_keyboards_and_controllers_are_never_merged_by_name() {
    for (family, vid, pid, page, usage) in [
        ("aula", 0x0c45, 0xfefc, 0xff80, 1),
        ("shield", 0x0955, 0x7214, 1, 5),
    ] {
        let mut infos = vec![];
        let mut steps = vec![];
        for serial in 1..=2 {
            let mut i = device(vid, pid, page, usage);
            i.serial = format!("SYNTHETIC-{serial}");
            i.path.push(char::from(b'0' + serial));
            infos.push(i);
            if family == "shield" {
                steps.extend(shield_steps(80, 0, serial));
            } else {
                let mut r = padded(&[0, 0x55, 0x10, 0x18], 33);
                r[26] = 80;
                steps.extend([Step::Write(padded(&[0, 0xaa, 0x10], 33)), Step::Read(Ok(r))]);
            }
        }
        let h = FakeHid::new(infos, steps);
        let rows = poll(&mut HidProvider::new(family), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].key, rows[1].key);
        h.done();
    }
}

#[test]
fn new_provider_errors_are_not_confused_with_empty_discovery_and_cancellation_stops_io() {
    for (family, vid, pid, page, usage) in [
        ("aula", 0x0c45, 0xfefc, 0xff80, 1),
        ("shield", 0x0955, 0x7214, 1, 5),
        ("turtlebeach", 0x10f5, 0x229b, 0xff13, 1),
        ("rapoo", 0x24ae, 0x1413, 0xff00, 2),
    ] {
        let i = device(vid, pid, page, usage);
        let mut p = HidProvider::new(family);
        assert!(
            poll(&mut p, &FakeHid::new(vec![], vec![]), &FakeClock::default())
                .unwrap()
                .is_empty()
        );
        let h = FakeHid::new(vec![i.clone()], vec![]).fail_open(i.path.clone(), "Access denied");
        assert!(poll(&mut p, &h, &FakeClock::default()).is_err());
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(true);
        let h = FakeHid::new(vec![i], vec![]);
        assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
        assert!(h.opened.lock().unwrap().is_empty());
    }
}

#[test]
fn unknown_products_wrong_collections_and_ambiguous_flydigi_receive_no_io() {
    for (family, vid, pid) in [
        ("aula", 0x0c45, 0xfefc),
        ("atk_nearlink", 0x373b, 0x108c),
        ("finalmouse", 0x361d, 0x100),
        ("hyperx", 0x0951, 0x1718),
        ("corsair", 0x1b1c, 0x2b07),
        ("inphic", 0x1d57, 0xfa60),
    ] {
        let h = FakeHid::new(
            vec![device(vid, pid, 1, 6), device(vid, 0xffff, 0xff00, 1)],
            vec![],
        );
        assert!(
            poll(&mut HidProvider::new(family), &h, &FakeClock::default())
                .unwrap()
                .is_empty()
        );
        assert!(h.opened.lock().unwrap().is_empty(), "{family}");
    }
    let i = device(0x37d7, 0x2401, 0xffa0, 1);
    let h = FakeHid::new(vec![i.clone(), i], vec![]);
    assert!(
        hb_providers::community::flydigi_battery(
            &h,
            &context(&FakeClock::default(), &AtomicBool::new(false))
        )
        .unwrap()
        .is_none()
    );
    assert!(h.opened.lock().unwrap().is_empty());
}

#[test]
fn flydigi_queries_only_its_exact_vendor_endpoint_and_preserves_coarse_steps() {
    for (state, level, charging) in [(0x05, 100, false), (0x15, 100, true), (0x21, 20, true)] {
        let mut r = padded(&[0x5a, 0xa5, 1], 32);
        r[11] = state;
        let h = FakeHid::new(
            vec![device(0x37d7, 0x2401, 0xffa0, 1)],
            vec![
                Step::Write(vec![0, 0x5a, 0xa5, 7, 0, 0]),
                Step::Write(vec![0, 0x5a, 0xa5, 1, 2, 0]),
                rd(&[0x5a, 0xa5, 7]),
                Step::Read(Ok(r)),
            ],
        );
        let b = hb_providers::community::flydigi_battery(
            &h,
            &context(&FakeClock::default(), &AtomicBool::new(false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(b.level, level);
        assert_eq!(b.charging, Some(charging));
        assert!(b.coarse);
        h.done();
    }
}

#[test]
fn razer_pro_type_prefers_9f_and_unproven_timeout_does_not_pin_wrong_collection() {
    let mut i = device(0x1532, 0x027b, 1, 6);
    i.interface = 3;
    let reply = |status, cmd, value| {
        let mut r = vec![0; 91];
        r[1] = status;
        r[7] = 7;
        r[8] = cmd;
        r[10] = value;
        r
    };
    let h = FakeHid::new(
        vec![i],
        vec![
            Step::Send(razer_request(0x9f, 0x80)),
            Step::Feature(0, 91, Ok(reply(4, 0x80, 0))),
            Step::Send(razer_request(0x1f, 0x80)),
            Step::Feature(0, 91, Ok(reply(2, 0x80, 128))),
            Step::Send(razer_request(0x1f, 0x84)),
            Step::Feature(0, 91, Ok(reply(2, 0x84, 0))),
        ],
    );
    let rows = poll(&mut HidProvider::new("razer"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(50));
    assert_eq!(rows[0].kind, "keyboard");
    h.done();
}

#[test]
fn omni_queries_only_confirmed_paired_models_and_keeps_child_identities_distinct() {
    for known in [false, true] {
        let mut infos = vec![];
        for page in [0xff02, 0xff01, 0xff00] {
            let mut i = device(0x0b05, 0x1ace, page, 1);
            i.interface = 2;
            i.container = Some("SYNTHETIC-OMNI-RECEIVER".into());
            i.path = format!("omni-{page:04x}");
            infos.push(i);
        }
        let mut pairs = padded(&[1, 0xa0, 0, 0, 0], 64);
        pairs[5..7].copy_from_slice(if known { &[0x65, 0x1b] } else { &[0x85, 0x1a] });
        pairs[9..11].copy_from_slice(if known { &[6, 0x1b] } else { &[0x72, 0x1a] });
        let mut steps = vec![
            rd(&[]),
            Step::Write(padded(&[1, 0xa0, 0, 0], 64)),
            Step::Read(Ok(pairs)),
        ];
        if known {
            let mut mouse = padded(&[3, 0x12, 7], 64);
            mouse[5] = 55;
            let mut keyboard = padded(&[2, 0x12, 1], 64);
            keyboard[11] = 66;
            keyboard[9] = 1;
            steps.extend([
                rd(&[]),
                Step::Write(padded(&[3, 0x12, 7], 64)),
                Step::Read(Ok(mouse)),
                rd(&[]),
                Step::Write(padded(&[2, 0x12, 1], 64)),
                Step::Read(Ok(keyboard)),
            ]);
        }
        let h = FakeHid::new(infos, steps);
        let rows = poll(&mut HidProvider::new("asus"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows.len(), if known { 2 } else { 0 });
        if known {
            assert_ne!(rows[0].key, rows[1].key);
            assert!(rows.iter().all(|r| r.container.is_none()));
            assert_eq!(rows[0].level, Some(55));
            assert_eq!(rows[1].level, Some(66));
            assert_eq!(rows[1].kind, "keyboard");
        }
        h.done();
    }
}

#[test]
fn passive_startup_sweep_is_throttled_and_refresh_preserves_sleeping_cache() {
    let clock = FakeClock::default();
    let i = device(0x1d57, 0xfa65, 0x000a, 0);
    let mut p = HidProvider::new("inphic");
    let silent = FakeHid::new(vec![i.clone()], vec![rd(&[]); 5]);
    assert!(poll(&mut p, &silent, &clock).unwrap().is_empty());
    silent.done();
    let throttled = FakeHid::new(vec![i.clone()], vec![]);
    assert!(poll(&mut p, &throttled, &clock).unwrap().is_empty());
    assert!(throttled.opened.lock().unwrap().is_empty());
    clock.0.store(60_000, Ordering::Relaxed);
    let live = FakeHid::new(vec![i.clone()], vec![rd(&[3, 0x95, 0x40, 1, 75])]);
    let first = poll(&mut p, &live, &clock).unwrap().remove(0);
    live.done();
    p.invalidate();
    let off = FakeHid::new(vec![i.clone()], vec![rd(&[]); 5]);
    let cached = poll(&mut p, &off, &clock).unwrap().remove(0);
    assert_eq!(cached.timestamp, first.timestamp);
    assert_eq!(cached.level, Some(75));
    off.done();
    let cancelled = AtomicBool::new(true);
    assert!(
        p.poll(&FakeHid::new(vec![], vec![]), &context(&clock, &cancelled))
            .unwrap()
            .is_empty()
    );
    let off = FakeHid::new(vec![i], vec![]);
    assert_eq!(poll(&mut p, &off, &clock).unwrap()[0].level, Some(75));
    assert!(off.opened.lock().unwrap().is_empty());
    off.done();
    assert!(
        poll(&mut p, &FakeHid::new(vec![], vec![]), &clock)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn corsair_void_accepts_64byte_shape_and_reuses_65byte_fallback() {
    for fallback in [false, true] {
        let mut steps = vec![];
        if fallback {
            steps.push(Step::WriteError(
                padded(&[2, 8, 2, 0x13], 64),
                "Invalid parameter",
            ));
        }
        let q = |head: &[u8]| {
            if fallback {
                padded(head, 65)
            } else {
                padded(&head[1..], 64)
            }
        };
        steps.extend([
            Step::Write(q(&[0, 2, 8, 2, 0x13])),
            Step::Write(q(&[0, 2, 8, 2, 0x12])),
            rd(&[]),
            Step::Write(q(&[0, 2, 9, 2, 0x12])),
            rd(&[1]),
            rd(&[]),
            Step::Write(q(&[0, 2, 9, 2, 0x0f])),
            rd(&[0, 0, 0, 0, 0x76, 2]),
        ]);
        let mut i = device(0x1b1c, 0x2a08, 0xff42, 1);
        i.interface = 4;
        i.output_length = if fallback { None } else { Some(64) };
        let h = FakeHid::new(vec![i], steps);
        let rows = poll(&mut HidProvider::new("corsair"), &h, &FakeClock::default()).unwrap();
        assert_eq!(rows[0].level, Some(63));
        h.done();
    }
}

#[test]
fn audeze_xbox_off_dongle_receives_no_battery_commands() {
    let mut i = device(0x3329, 0x4b18, 0xff13, 1);
    i.product = " Audeze Maxwell XBOX Dongle ".into();
    let h = FakeHid::new(vec![i], vec![]);
    assert!(
        poll(&mut HidProvider::new("audeze"), &h, &FakeClock::default())
            .unwrap()
            .is_empty()
    );
    assert!(h.opened.lock().unwrap().is_empty());
}

#[test]
fn barracuda_new_pa_receiver_uses_same_confirmed_protocol_without_touching_older_id() {
    use hb_providers::protocols::pa_request;
    let mut steps = vec![Step::Write(pa_request(false, 0xe1, Some(true)))];
    for (cmd, value) in [(0x21, 94), (0x2a, 0)] {
        let mut r = vec![0; 64];
        r[0] = 2;
        r[12..16].copy_from_slice(&[cmd, 1, 1, value]);
        steps.extend([
            rd(&[]),
            Step::Write(pa_request(false, 0xe1, Some(true))),
            Step::Write(pa_request(false, 0xe1, Some(true))),
            Step::Write(pa_request(false, cmd, None)),
            Step::Read(Ok(r)),
            Step::Write(pa_request(false, 0xe1, Some(false))),
        ]);
    }
    let h = FakeHid::new(vec![device(0x1532, 0x0583, 0xff00, 1)], steps);
    let rows = poll(&mut HidProvider::new("razer"), &h, &FakeClock::default()).unwrap();
    assert_eq!(rows[0].level, Some(94));
    assert_eq!(rows[0].kind, "headset");
    h.done();
    let h = FakeHid::new(vec![device(0x1532, 0x0583, 0xff00, 1)], vec![]);
    assert!(
        poll(
            &mut HidProvider::new("barracuda"),
            &h,
            &FakeClock::default()
        )
        .unwrap()
        .is_empty()
    );
    assert!(h.opened.lock().unwrap().is_empty());
}
