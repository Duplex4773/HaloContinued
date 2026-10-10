use hb_core::*;
use hb_providers::HidProvider;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
mod common;
use common::*;

#[test]
fn g7_uses_output_input_skips_noise_and_transient_error() {
    use hb_providers::protocols::padded;
    let hid = FakeHid::new(
        vec![info(0xa8a5, 0x2255, 0xff01)],
        vec![
            Step::Write(padded(&[0, 0x55, 0x30, 0xa5, 0x0b, 0x2e, 1, 1, 1], 65)),
            Step::Read(Ok(vec![8, 99])),
            Step::Read(Err("temporary disconnect")),
            Step::Read(Ok(vec![0xaa, 0x30, 0xa5, 0x0b, 10, 1, 1, 1, 46, 0])),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("mchose")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    assert_eq!(readings[0].key, "mchose:a8a5:device-a8a5-2255");
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_millis(60));
}

#[test]
fn g7_does_not_write_to_other_chip_products_or_collections() {
    let hid = FakeHid::new(
        vec![info(0xa8a5, 0x9999, 0xff01), info(0xa8a5, 0x2255, 0xff05)],
        vec![],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert!(
        HidProvider::new("mchose")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
}

fn mchose_steps(level: u8, charging: bool) -> Vec<Step> {
    let mut packet = vec![255; 21];
    packet[0] = 0x11;
    packet[1] = 0xf9;
    let mut response = vec![255; 21];
    response[0] = 0x11;
    response[1] = 0xf9;
    response[2] = 0xac;
    response[3] = 0xad;
    response[11] = level ^ 255;
    response[12] = u8::from(charging) ^ 255;
    vec![
        Step::Send(packet.clone()),
        Step::Feature(0x11, 21, Err("receiver pending")),
        Step::Send(packet),
        Step::Feature(0x11, 21, Ok(response)),
    ]
}
#[test]
fn mchose_vendor_family_includes_unknown_pids_and_prefers_charging_source() {
    let mut steps = mchose_steps(46, false);
    steps.extend(mchose_steps(47, true));
    let mut radio = info(0x5253, 0x1020, 0xff01);
    let mut cable = info(0x5253, 0x31, 0xff01);
    radio.serial = "same-physical-mouse".into();
    cable.serial = radio.serial.clone();
    let hid = FakeHid::new(vec![radio, cable], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("mchose")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings.len(), 1);
    assert_eq!(readings[0].level, Some(47));
    assert_eq!(readings[0].charging, Some(true));
    hid.done();
}

fn razer_reply(status: u8, cmd: u8, value: u8) -> Vec<u8> {
    let mut r = vec![0; 91];
    r[1] = status;
    r[7] = 7;
    r[8] = cmd;
    r[10] = value;
    r
}
#[test]
fn razer_retries_transaction_ids_and_caches_the_working_id() {
    use hb_providers::protocols::razer_request;
    let device = info(0x1532, 0x007c, 1);
    let preferred = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "razer" && d.pid == device.product_id)
        .unwrap()
        .parameter;
    let alternate = [0x1f, 0x3f, 0xff, 0x9f, 8]
        .into_iter()
        .find(|tid| *tid != preferred)
        .unwrap();
    let mut steps = vec![
        Step::Send(razer_request(preferred, 0x80)),
        Step::Feature(0, 91, Ok(razer_reply(5, 0x80, 0))),
    ];
    for _ in 0..2 {
        steps.extend([
            Step::Send(razer_request(alternate, 0x80)),
            Step::Feature(0, 91, Ok(razer_reply(2, 0x80, 128))),
            Step::Send(razer_request(alternate, 0x84)),
            Step::Feature(0, 91, Ok(razer_reply(2, 0x84, 1))),
        ]);
    }
    let hid = FakeHid::new(vec![device], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    for _ in 0..2 {
        let readings = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(readings[0].level, Some(50));
        assert_eq!(readings[0].charging, Some(true));
    }
    hid.done();
}

#[test]
fn cancellation_prevents_opening_devices() {
    let hid = FakeHid::new(vec![info(0xa8a5, 0x2255, 0xff01)], vec![]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(true);
    assert!(
        HidProvider::new("mchose")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
}

#[test]
fn sleeping_mchose_retains_reading_for_less_than_five_minutes() {
    let hid = FakeHid::new(vec![info(0x5253, 0x31, 0xff01)], mchose_steps(46, false));
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("mchose");
    let live = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    let observed = clock.monotonic().as_millis() as u64;
    hid.done();
    let absent = FakeHid::new(vec![], vec![]);
    clock.0.store(observed + 299000, Ordering::Relaxed);
    let sleeping = provider.poll(&absent, &context(&clock, &cancel)).unwrap();
    assert_eq!(sleeping[0].connection, Connection::Sleeping);
    assert_eq!(sleeping[0].timestamp, live[0].timestamp);
    clock.0.store(observed + 300000, Ordering::Relaxed);
    assert!(
        provider
            .poll(&absent, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn expired_deadline_prevents_device_io() {
    let hid = FakeHid::new(vec![info(0xa8a5, 0x2255, 0xff01)], vec![]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let context = PollContext {
        clock: &clock,
        cancelled: &cancel,
        deadline: Duration::ZERO,
        playstation_full_mode: false,
    };
    assert!(
        HidProvider::new("mchose")
            .poll(&hid, &context)
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
}

fn pa_reply(barracuda: bool, command: u8, value: u8) -> Vec<u8> {
    let mut r = vec![0; 64];
    r[0] = if barracuda { 1 } else { 2 };
    let c = if barracuda { 13 } else { 12 };
    r[c] = command;
    r[c + 1] = 1;
    r[c + 2] = 1;
    r[c + 3] = value;
    r
}

#[test]
fn blackshark_queries_wake_and_disable_remote_for_each_command() {
    use hb_providers::protocols::pa_request;
    let mut steps = vec![Step::Write(pa_request(false, 0xe1, Some(true)))];
    for (command, value) in [(0x21, 46), (0x2a, 1)] {
        steps.extend([
            Step::Read(Ok(vec![])),
            Step::Write(pa_request(false, 0xe1, Some(true))),
            Step::Write(pa_request(false, 0xe1, Some(true))),
            Step::Write(pa_request(false, command, None)),
            Step::Read(Ok(pa_reply(false, command, value))),
            Step::Write(pa_request(false, 0xe1, Some(false))),
        ]);
    }
    let hid = FakeHid::new(vec![info(0x1532, 0x0555, 0xff00)], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("razer")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    assert_eq!(readings[0].charging, Some(true));
    assert_eq!(clock.monotonic(), Duration::from_millis(245));
    hid.done();
}

#[test]
fn barracuda_drains_and_disables_remote_after_matching_both_commands() {
    use hb_providers::protocols::pa_request;
    let mut steps = vec![
        Step::Read(Ok(vec![])),
        Step::Write(pa_request(true, 0xe1, Some(true))),
    ];
    for (command, value) in [(0x21, 46), (0x2a, 0)] {
        steps.extend([
            Step::Read(Ok(vec![])),
            Step::Write(pa_request(true, command, None)),
            Step::Read(Ok(pa_reply(true, command, value))),
        ]);
    }
    steps.extend([
        Step::Read(Ok(vec![])),
        Step::Write(pa_request(true, 0xe1, Some(false))),
    ]);
    let hid = FakeHid::new(vec![info(0x1532, 0x053a, 0xff00)], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("barracuda")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    assert_eq!(readings[0].charging, Some(false));
    assert_eq!(clock.monotonic(), Duration::from_millis(50));
    hid.done();
}

fn hidpp_packet(feature: u8, token: u8, params: &[u8]) -> Vec<u8> {
    let mut r = hb_providers::protocols::padded(&[0x11, 255, feature, token], 20);
    r[4..4 + params.len()].copy_from_slice(params);
    r
}
fn hidpp_pair(feature: u8, token: u8, params: &[u8], reply: &[u8]) -> Vec<Step> {
    vec![
        Step::Write(hidpp_packet(feature, token, params)),
        Step::Read(Ok(hidpp_packet(feature, token, reply))),
    ]
}

#[test]
fn logitech_matches_tokens_reads_identity_and_reuses_complete_identity() {
    let mut steps = hidpp_pair(0, 0x1b, &[], &[2]);
    steps.extend(hidpp_pair(0, 0x0c, &[0, 5], &[2]));
    steps.extend(hidpp_pair(2, 0x0d, &[], &[5]));
    steps.extend(hidpp_pair(2, 0x1e, &[0], b"Mouse"));
    steps.extend(hidpp_pair(2, 0x2f, &[], &[3]));
    steps.extend(hidpp_pair(0, 0x0a, &[0, 3], &[3]));
    steps.extend(hidpp_pair(3, 0x0b, &[], &[0, 0xde, 0xad, 0xbe, 0xef]));
    steps.extend(hidpp_pair(0, 0x0c, &[0x10, 4], &[4]));
    steps.push(Step::Write(hidpp_packet(4, 0x1d, &[])));
    steps.push(Step::Read(Ok(hidpp_packet(4, 0x1c, &[99, 0, 0]))));
    steps.push(Step::Read(Ok(hidpp_packet(4, 0x1d, &[46, 0, 1]))));
    steps.extend(hidpp_pair(0, 0x1b, &[], &[2]));
    steps.extend(hidpp_pair(0, 0x0c, &[0x10, 4], &[4]));
    steps.extend(hidpp_pair(4, 0x1d, &[], &[47, 0, 0]));
    let mut device = info(0x046d, 0xc08b, 0xff00);
    device.usage = 2;
    let hid = FakeHid::new(vec![device], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("logitech");
    let first = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(first[0].key, "logitech:DEADBEEF");
    assert_eq!(first[0].name, "Mouse");
    assert_eq!(first[0].level, Some(46));
    let second = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(second[0].level, Some(47));
    hid.done();
}

#[test]
fn logitech_unknown_headset_collections_are_never_opened() {
    let hid = FakeHid::new(vec![info(0x046d, 0x9999, 0xff43)], vec![]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert!(
        HidProvider::new("logitech")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
}

#[test]
fn audeze_uses_control_input_reports_and_newest_frame() {
    use hb_providers::protocols::padded;
    let mut device = info(0x3329, 0x4b18, 0xff13);
    device.serial = "headset-1".into();
    let hid = FakeHid::new(
        vec![device],
        vec![
            Step::Write(padded(&[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c], 62)),
            Step::Input(7, 62, Ok(vec![0xd6, 0x0c, 0, 0, 86])),
            Step::Input(7, 62, Ok(vec![0xd6, 0x0c, 0, 0, 85])),
            Step::Input(7, 62, Ok(vec![0xd6, 0x0c, 0, 0, 84])),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("audeze")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(84));
    assert_eq!(readings[0].key, "audeze:HEADSET-1");
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_millis(180));
}

#[test]
fn asus_error_frame_stops_retries_on_optional_zero_prefix() {
    use hb_providers::protocols::padded;
    let device = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "asus")
        .unwrap();
    let hid = FakeHid::new(
        vec![info(device.vid, device.pid, 0xff00)],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(padded(&[0, 0x12, 7], 65)),
            Step::Read(Ok(vec![0, 0xff, 0xaa])),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert!(
        HidProvider::new("asus")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
}

#[test]
fn pulsar_drains_then_accepts_only_checksum_valid_power_reports() {
    use hb_providers::protocols::pulsar_request;
    let device = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "pulsar")
        .unwrap();
    let mut report = pulsar_request();
    report[6] = 46;
    report[7] = 1;
    report[16] = 0x55u8.wrapping_sub(report[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    let mut selected = info(device.vid, device.pid, 0xff02);
    selected.usage = 2;
    selected.output_length = Some(17);
    let hid = FakeHid::new(
        vec![selected],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(pulsar_request()),
            Step::Read(Ok(vec![8, 4])),
            Step::Read(Ok(report)),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("pulsar")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_millis(20));
}

#[test]
fn lofree_closes_both_transactions_and_requires_online_ack() {
    use hb_providers::protocols::padded;
    let mut steps = Vec::new();
    for (command, value) in [(0xaa, 1), (0x1a, 46)] {
        steps.extend([
            Step::Write(padded(&[4, 0, 0, 1], 32)),
            Step::Read(Ok(vec![4, 0, 0, 1])),
            Step::Write(padded(&[4, 0, 0, command, 0, 0, 0, 0], 32)),
            Step::Read(Ok(vec![4, 0, 0, command, 0, 0, 0, 0, value])),
            Step::Write(padded(&[4, 0, 0, 2], 32)),
            Step::Read(Ok(vec![4, 0, 0, 2])),
        ]);
    }
    let mut selected = info(0x388d, 0x25, 0xff1c);
    selected.usage = 0x92;
    let hid = FakeHid::new(vec![selected], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("lofree")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    hid.done();
}

#[test]
fn corsair_requires_heartbeat_and_drains_between_handshake_and_battery() {
    use hb_providers::protocols::padded;
    let device = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "corsair" && d.variant != "nxp")
        .unwrap();
    let mut selected = info(device.vid, device.pid, 0);
    selected.interface = 4;
    let hid = FakeHid::new(
        vec![selected],
        vec![
            Step::Write(padded(&[0, 2, 8, 2, 0x13], 65)),
            Step::Write(padded(&[0, 2, 8, 2, 0x12], 65)),
            Step::Read(Ok(vec![])),
            Step::Write(padded(&[0, 2, 9, 2, 0x12], 65)),
            Step::Read(Ok(vec![1])),
            Step::Read(Ok(vec![])),
            Step::Write(padded(&[0, 2, 9, 2, 0x0f], 65)),
            Step::Read(Ok(vec![0, 0, 0, 0, 0xcc, 1])),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("corsair")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    hid.done();
}

#[test]
fn nintendo_listens_then_sends_only_two_subcommands_with_wrapping_counter() {
    use hb_providers::protocols::padded;
    let mut device = info(0x057e, 0x2009, 1);
    device.path = "hid#vid&057e#bluetooth".into();
    let mut steps = vec![Step::Read(Ok(vec![])); 20];
    for counter in [0, 1] {
        steps.push(Step::Write(padded(
            &[1, counter, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 2],
            49,
        )));
        steps.extend(vec![Step::Read(Ok(vec![])); 60]);
    }
    let hid = FakeHid::new(vec![device], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert!(
        HidProvider::new("nintendo")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_millis(700));
}

#[test]
fn playstation_basic_bluetooth_returns_immediately_and_usb_feature_is_read() {
    let mut bluetooth = info(0x054c, 0x0ce6, 1);
    bluetooth.path = "hid#vid&054c#bluetooth".into();
    let basic = FakeHid::new(vec![bluetooth], vec![Step::Read(Ok(vec![1, 0, 0]))]);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("playstation")
        .poll(&basic, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, None);
    assert_eq!(clock.monotonic(), Duration::ZERO);
    basic.done();
    let mut report = vec![0; 64];
    report[0] = 1;
    report[53] = 0x17;
    let usb = FakeHid::new(
        vec![info(0x054c, 0x0ce6, 1)],
        vec![Step::Feature(5, 64, Ok(vec![])), Step::Read(Ok(report))],
    );
    let readings = HidProvider::new("playstation")
        .poll(&usb, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(70));
    assert_eq!(readings[0].charging, Some(true));
    usb.done();
}

#[test]
fn eightbitdo_is_passive_and_uses_distinct_usb_report_id() {
    let device = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "eightbitdo")
        .unwrap();
    let mut report = vec![0; 64];
    report[0] = 4;
    report[14] = 46 | 128;
    let hid = FakeHid::new(
        vec![info(device.vid, device.pid, 1)],
        vec![Step::Read(Ok(vec![1, 0, 0])), Step::Read(Ok(report))],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("eightbitdo")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings[0].level, Some(46));
    assert_eq!(readings[0].charging, Some(true));
    hid.done();
}

#[test]
fn razer_foreign_responses_cost_three_sends_and_cached_collection_is_reused() {
    use hb_providers::protocols::razer_request;
    let device = info(0x1532, 0x007c, 1);
    let tid = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "razer" && d.pid == device.product_id)
        .unwrap()
        .parameter;
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.push(Step::Send(razer_request(tid, 0x80)));
        for index in 1..=13 {
            let mut foreign = razer_reply(2, 3, 99);
            foreign[7] = 0x0f;
            steps.push(Step::Feature(0, 91, Ok(foreign)));
            if [4, 8].contains(&index) {
                steps.push(Step::Send(razer_request(tid, 0x80)));
            }
        }
    }
    let hid = FakeHid::new(vec![device], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    for _ in 0..2 {
        assert!(
            provider
                .poll(&hid, &context(&clock, &cancel))
                .unwrap()
                .is_empty()
        );
    }
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_millis(2040));
}

#[test]
fn remaining_hid_families_use_the_reference_channel_packet_and_positive_reply() {
    use hb_providers::protocols::padded;
    for family in [
        "wlmouse",
        "lamzu",
        "gwolves",
        "astro",
        "hyperx",
        "hyperx_cloud3",
        "hyperx_alpha2",
        "keychron",
        "jbl",
        "am_infinity",
    ] {
        let known = hb_providers::catalog::DEVICES
            .iter()
            .find(|d| d.provider == family && (family != "gwolves" || d.variant.is_empty()))
            .unwrap();
        let mut selected = info(known.vid, known.pid, 0);
        let steps = match family {
            "wlmouse" | "lamzu" | "gwolves" => {
                selected.usage_page = 0xffff;
                selected.interface = 2;
                selected.feature_length = Some(65);
                vec![
                    Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
                    Step::Feature(0, 65, Ok(vec![0xa1, 0, 0, 2, 0, 0x83, 0, 46])),
                ]
            }
            "astro" => {
                selected.usage_page = 0xff32;
                selected.usage = 0x74;
                vec![
                    Step::Write(padded(&[2, 12, 3, 0, 6, 12], 64)),
                    Step::Read(Ok(vec![2, 12, 0, 0, 6, 0, 46, 0, 0])),
                ]
            }
            "hyperx" => {
                selected.usage_page = 0xff90;
                selected.usage = 0x303;
                vec![
                    Step::Write(padded(&[6, 255, 187, 2, 0], 52)),
                    Step::Read(Ok(vec![6, 255, 187, 2, 0, 0, 0, 46])),
                    Step::Write(padded(&[6, 255, 187, 3, 0], 52)),
                    Step::Read(Ok(vec![6, 255, 187, 3, 0])),
                ]
            }
            "hyperx_cloud3" => {
                selected.usage_page = 0xff13;
                vec![
                    Step::Write(padded(&[0x66, 0x89], 62)),
                    Step::Read(Ok(vec![0x66, 0x0d, 1, 0, 46])),
                    Step::Write(padded(&[0x66, 0x8a], 62)),
                    Step::Read(Ok(vec![0x66, 0x0c, 0])),
                ]
            }
            "hyperx_alpha2" => {
                selected.usage_page = 0xff13;
                selected.usage = 0xff00;
                vec![
                    Step::Read(Ok(vec![])),
                    Step::Write(padded(&[0x50, 2], 64)),
                    Step::Read(Ok(vec![0x51, 2, 46, 0, 0, 0, 0])),
                ]
            }
            "keychron" => {
                selected.interface = 4;
                let mut reply = vec![0; 64];
                reply[0] = 0xb4;
                reply[1] = 6;
                reply[20] = 46;
                vec![Step::Send(padded(&[0xb3, 6], 64)), Step::Read(Ok(reply))]
            }
            "jbl" => {
                selected.usage_page = 0xff13;
                vec![
                    Step::ReadSized(64, Duration::ZERO, Ok(vec![0x2f, 99])),
                    Step::ReadSized(64, Duration::ZERO, Ok(vec![8, 46])),
                    Step::ReadSized(64, Duration::ZERO, Ok(vec![])),
                ]
            }
            "am_infinity" => {
                selected.usage_page = 0xffff;
                selected.usage = 2;
                vec![
                    Step::Send(padded(&[0, 0xf7], 65)),
                    Step::Feature(5, 65, Ok(vec![5, 0, 0, 0])),
                    Step::Send(padded(&[0, 0xf7], 67)),
                    Step::Feature(5, 65, Ok(vec![5, 0, 0, 46])),
                ]
            }
            _ => unreachable!(),
        };
        let hid = FakeHid::new(vec![selected], steps);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let readings = HidProvider::new(family)
            .poll(&hid, &context(&clock, &cancel))
            .unwrap();
        assert_eq!(readings.len(), 1, "{family}");
        assert_eq!(readings[0].level, Some(46), "{family}");
        assert_eq!(readings[0].charging, Some(false), "{family}");
        hid.done();
    }
}

#[test]
fn candidate_fallback_and_positive_capacity_selection_are_reference_policies() {
    use hb_providers::protocols::{padded, pulsar_request};
    let known = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "astro")
        .unwrap();
    let hid = FakeHid::new(
        vec![info(known.vid, known.pid, 1)],
        vec![
            Step::Write(padded(&[2, 12, 3, 0, 6, 12], 64)),
            Step::Read(Ok(vec![2, 12, 0, 0, 6, 0, 46, 0, 0])),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        HidProvider::new("astro")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()[0]
            .level,
        Some(46)
    );
    hid.done();
    let known = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "pulsar")
        .unwrap();
    let mut bad = info(known.vid, known.pid, 0xff02);
    bad.path = "wrong-size-control".into();
    bad.usage = 2;
    bad.output_length = Some(8);
    bad.container = Some("same-mouse".into());
    let mut good = info(known.vid, known.pid, 1);
    good.path = "fits".into();
    good.output_length = Some(17);
    good.container = bad.container.clone();
    let mut reply = pulsar_request();
    reply[6] = 46;
    reply[16] = 0x55u8.wrapping_sub(reply[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    let hid = FakeHid::new(
        vec![bad, good],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(pulsar_request()),
            Step::Read(Ok(reply)),
        ],
    );
    assert_eq!(
        HidProvider::new("pulsar")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()[0]
            .level,
        Some(46)
    );
    assert_eq!(*hid.opened.lock().unwrap(), vec!["fits".to_string()]);
    hid.done();
}

#[test]
fn unknown_product_ids_and_wrong_strict_collections_never_get_commands() {
    for &(family, _) in hb_providers::provider::FAMILIES {
        let known = hb_providers::catalog::DEVICES
            .iter()
            .find(|d| d.provider == family)
            .unwrap();
        let vid = if family == "mchose" {
            0xa8a5
        } else {
            known.vid
        };
        let mut unknown = info(vid, 0x9999, 0xff43);
        unknown.product = "wireless receiver".into();
        let hid = FakeHid::new(vec![unknown], vec![]);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        assert!(
            HidProvider::new(family)
                .poll(&hid, &context(&clock, &cancel))
                .unwrap()
                .is_empty(),
            "{family}"
        );
        assert!(hid.opened.lock().unwrap().is_empty(), "{family}");
    }
    for family in [
        "hyperx",
        "hyperx_cloud3",
        "hyperx_alpha2",
        "asus",
        "lamzu",
        "gwolves",
        "lofree",
        "am_infinity",
        "barracuda",
        "steelseries",
    ] {
        let known = hb_providers::catalog::DEVICES
            .iter()
            .find(|d| d.provider == family)
            .unwrap();
        let mut wrong = info(known.vid, known.pid, 1);
        wrong.interface = 9;
        let hid = FakeHid::new(vec![wrong], vec![]);
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        assert!(
            HidProvider::new(family)
                .poll(&hid, &context(&clock, &cancel))
                .unwrap()
                .is_empty(),
            "{family}"
        );
        assert!(hid.opened.lock().unwrap().is_empty(), "{family}");
    }
}

#[test]
fn lofree_start_reply_and_stop_share_a_single_three_second_deadline() {
    use hb_providers::protocols::padded;
    let mut steps = vec![Step::Write(padded(&[4, 0, 0, 1], 32))];
    steps.extend(vec![Step::Read(Ok(vec![])); 29]);
    steps.extend([
        Step::Read(Ok(vec![4, 0, 0, 1])),
        Step::Write(padded(&[4, 0, 0, 0xaa, 0, 0, 0, 0], 32)),
        Step::Read(Ok(vec![])),
        Step::Write(padded(&[4, 0, 0, 2], 32)),
    ]);
    let mut selected = info(0x388d, 0x25, 0xff1c);
    selected.usage = 0x92;
    let hid = FakeHid::new(vec![selected], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    assert!(
        HidProvider::new("lofree")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
    assert_eq!(clock.monotonic(), Duration::from_secs(3));
}
#[test]
fn unopenable_eightbitdo_still_reports_controller_without_battery() {
    let known = hb_providers::catalog::DEVICES
        .iter()
        .find(|d| d.provider == "eightbitdo")
        .unwrap();
    let selected = info(known.vid, known.pid, 1);
    let hid =
        FakeHid::new(vec![selected.clone()], vec![]).fail_open(&selected.path, "access denied");
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("eightbitdo")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings.len(), 1);
    assert_eq!(readings[0].level, None);
    assert_eq!(readings[0].kind, "gamepad");
    assert!(readings[0].online());
}

#[test]
fn ambiguous_mchose_receivers_remain_distinct_even_when_one_charges() {
    let mut a = info(0x5253, 0x31, 0xff01);
    let mut b = a.clone();
    a.path = "receiver-a".into();
    b.path = "receiver-b".into();
    let mut steps = mchose_steps(30, true);
    steps.extend(mchose_steps(70, false));
    let hid = FakeHid::new(vec![a, b], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let readings = HidProvider::new("mchose")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(readings.len(), 2);
    assert_ne!(readings[0].key, readings[1].key);
    assert_eq!(
        readings
            .iter()
            .map(|r| r.level.unwrap())
            .collect::<Vec<_>>(),
        vec![30, 70]
    );
    hid.done();
}

struct ShiftClock {
    monotonic: FakeClock,
    wall: std::sync::atomic::AtomicI64,
}
impl Clock for ShiftClock {
    fn unix(&self) -> i64 {
        self.wall.load(Ordering::Relaxed)
    }
    fn monotonic(&self) -> Duration {
        self.monotonic.monotonic()
    }
    fn sleep(&self, duration: Duration) {
        self.monotonic.sleep(duration);
    }
}
fn shifted_context<'a>(clock: &'a ShiftClock, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: clock.monotonic() + Duration::from_secs(10),
        playstation_full_mode: false,
    }
}
#[test]
fn sleeping_cache_expiry_uses_elapsed_time_despite_wall_clock_changes() {
    let clock = ShiftClock {
        monotonic: FakeClock::default(),
        wall: std::sync::atomic::AtomicI64::new(10000),
    };
    let cancel = AtomicBool::new(false);
    let hid = FakeHid::new(vec![info(0x5253, 0x31, 0xff01)], mchose_steps(46, false));
    let mut provider = HidProvider::new("mchose");
    let original = provider
        .poll(&hid, &shifted_context(&clock, &cancel))
        .unwrap();
    let observed = clock.monotonic();
    let absent = FakeHid::new(vec![], vec![]);
    clock.wall.store(-10000, Ordering::Relaxed);
    clock.monotonic.0.store(
        (observed + Duration::from_secs(299)).as_millis() as u64,
        Ordering::Relaxed,
    );
    let cached = provider
        .poll(&absent, &shifted_context(&clock, &cancel))
        .unwrap();
    assert_eq!(cached[0].timestamp, original[0].timestamp);
    clock.wall.store(100000, Ordering::Relaxed);
    assert_eq!(
        provider
            .poll(&absent, &shifted_context(&clock, &cancel))
            .unwrap()
            .len(),
        1
    );
    clock.monotonic.0.store(
        (observed + Duration::from_secs(300)).as_millis() as u64,
        Ordering::Relaxed,
    );
    clock.wall.store(-10000, Ordering::Relaxed);
    assert!(
        provider
            .poll(&absent, &shifted_context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn blackshark_no_wake_reopens_once_and_does_not_cache_rejected_path() {
    use hb_providers::protocols::pa_request;
    let i = info(0x1532, 0x555, 0xff00);
    let packet = pa_request(false, 0xe1, Some(true));
    let hid = FakeHid::new(
        vec![i.clone()],
        (0..8)
            .map(|_| Step::WriteError(packet.clone(), "suspended"))
            .collect(),
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert_eq!(clock.monotonic(), Duration::from_millis(3300));
    assert_eq!(hid.opened.lock().unwrap().len(), 2);
    hid.done();
    let again = FakeHid::new(
        vec![i],
        (0..8)
            .map(|_| Step::WriteError(packet.clone(), "suspended"))
            .collect(),
    );
    assert!(
        provider
            .poll(&again, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert_eq!(again.opened.lock().unwrap().len(), 2);
    again.done();
}

fn audeze_short_steps(level: u8) -> Vec<Step> {
    let mut steps = vec![Step::Write(hb_providers::protocols::padded(
        &[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c],
        62,
    ))];
    steps.extend((0..3).map(|_| Step::Input(7, 62, Ok(vec![0xd6, 0x0c, 0, 0, level]))));
    steps
}
#[test]
fn audeze_startup_zero_is_pending_then_real_zero_after_monotonic_grace() {
    let mut cable = info(0x3329, 0x4b1a, 0xff13);
    cable.serial = "headset".into();
    let clock = ShiftClock {
        monotonic: FakeClock::default(),
        wall: std::sync::atomic::AtomicI64::new(10000),
    };
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("audeze");
    let hid = FakeHid::new(vec![cable.clone()], audeze_short_steps(0));
    let first = provider
        .poll(&hid, &shifted_context(&clock, &cancel))
        .unwrap();
    assert_eq!(first[0].level, None);
    assert_eq!(first[0].charging, Some(false));
    assert_eq!(provider.next_poll_delay(), Some(Duration::from_secs(3)));
    hid.done();
    clock.wall.store(-10000, Ordering::Relaxed);
    clock.sleep(Duration::from_secs(90));
    let hid = FakeHid::new(vec![cable.clone()], audeze_short_steps(0));
    let later = provider
        .poll(&hid, &shifted_context(&clock, &cancel))
        .unwrap();
    assert_eq!(later[0].level, Some(0));
    assert_eq!(later[0].charging, Some(true));
    assert_eq!(provider.next_poll_delay(), None);
    hid.done();
    assert!(
        provider
            .poll(
                &FakeHid::new(vec![], vec![]),
                &shifted_context(&clock, &cancel)
            )
            .unwrap()
            .is_empty()
    );
    let hid = FakeHid::new(vec![cable], audeze_short_steps(0));
    assert_eq!(
        provider
            .poll(&hid, &shifted_context(&clock, &cancel))
            .unwrap()[0]
            .level,
        None
    );
    hid.done();
}
#[test]
fn audeze_missing_vendor_reports_unknown_and_group_off_string_prevents_all_writes() {
    let mut standard = info(0x3329, 0x4b18, 0x0c);
    standard.serial = "headset".into();
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("audeze");
    let hid = FakeHid::new(vec![standard.clone()], vec![]);
    let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].level, None);
    assert_eq!(r[0].key, "audeze:HEADSET");
    assert!(hid.opened.lock().unwrap().is_empty());
    standard.product = " Audeze Maxwell Dongle ".into();
    let mut vendor = standard.clone();
    vendor.product = "Audeze Maxwell HID".into();
    vendor.path = "vendor".into();
    vendor.usage_page = 0xff13;
    let hid = FakeHid::new(vec![vendor, standard], vec![]);
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
}
fn audeze_echo_steps(short: bool) -> Vec<Step> {
    let mut steps = vec![];
    if short {
        steps.push(Step::Write(hb_providers::protocols::padded(
            &[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c],
            62,
        )));
        steps.extend((0..3).map(|_| Step::Input(7, 62, Ok(vec![7, 1, 2, 0]))));
    }
    for packet in hb_providers::catalog::AUDEZE_REQUESTS {
        steps.push(Step::Write(hb_providers::protocols::padded(packet, 62)));
        steps.push(Step::Input(7, 62, Ok(vec![7, 1, 2, 0])));
    }
    steps.extend((0..2).map(|_| Step::Input(7, 62, Ok(vec![7, 1, 2, 0]))));
    steps
}
#[test]
fn audeze_repeated_echo_shows_stuck_and_recovery_clears_stuck_and_short_cache() {
    let mut device = info(0x3329, 0x4b18, 0xff13);
    device.serial = "headset".into();
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("audeze");
    let mut stuck_key = String::new();
    for (short, expected) in [(true, 0), (false, 1)] {
        let hid = FakeHid::new(vec![device.clone()], audeze_echo_steps(short));
        let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(r.len(), expected);
        if expected == 1 {
            stuck_key = r[0].key.clone();
            assert_eq!(r[0].connection, Connection::Online);
            assert_eq!(
                r[0].approx.as_deref(),
                Some("no answer from the headset - unplug the dongle and plug it back in")
            );
        }
        hid.done();
    }
    let mut recovery = vec![];
    for packet in hb_providers::catalog::AUDEZE_REQUESTS {
        recovery.push(Step::Write(hb_providers::protocols::padded(packet, 62)));
        recovery.push(Step::Input(7, 62, Ok(vec![0xd6, 0x0c, 0, 0, 75])));
    }
    recovery.extend((0..2).map(|_| Step::Input(7, 62, Ok(vec![]))));
    let hid = FakeHid::new(vec![device.clone()], recovery);
    let recovered = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(recovered[0].level, Some(75));
    assert_eq!(recovered[0].key, stuck_key);
    assert_eq!(recovered[0].approx, None);
    hid.done();
    let hid = FakeHid::new(vec![device.clone()], audeze_short_steps(76));
    assert_eq!(
        provider.poll(&hid, &context(&clock, &cancel)).unwrap()[0].level,
        Some(76)
    );
    hid.done();
    let hid = FakeHid::new(vec![device], audeze_echo_steps(true));
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
}

fn blackshark_offline_steps() -> Vec<Step> {
    use hb_providers::protocols::pa_request;
    let mut steps = vec![
        Step::Write(pa_request(false, 0xe1, Some(true))),
        Step::Read(Ok(vec![])),
        Step::Write(pa_request(false, 0xe1, Some(true))),
    ];
    for _ in 0..3 {
        steps.push(Step::Write(pa_request(false, 0xe1, Some(true))));
        steps.push(Step::Write(pa_request(false, 0x21, None)));
        steps.extend((0..3).map(|_| Step::Read(Ok(vec![]))));
    }
    steps.push(Step::Write(pa_request(false, 0xe1, Some(false))));
    steps
}
#[test]
fn blackshark_nowake_candidate_does_not_block_working_path_and_offline_path_is_cached() {
    use hb_providers::protocols::pa_request;
    let mut bad = info(0x1532, 0x555, 0xff00);
    bad.path = "a-rejected".into();
    bad.serial = "headset".into();
    let mut good = bad.clone();
    good.path = "b-accepted".into();
    let mut later = good.clone();
    later.path = "c-unneeded".into();
    let mut steps: Vec<_> = (0..8)
        .map(|_| Step::WriteError(pa_request(false, 0xe1, Some(true)), "suspended"))
        .collect();
    steps.extend(blackshark_offline_steps());
    let hid = FakeHid::new(vec![later.clone(), good.clone(), bad.clone()], steps);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
    assert_eq!(
        *hid.opened.lock().unwrap(),
        vec!["a-rejected", "a-rejected", "b-accepted"]
    );
    let hid = FakeHid::new(vec![bad, good, later], blackshark_offline_steps());
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
    assert_eq!(*hid.opened.lock().unwrap(), vec!["b-accepted"]);
}
#[test]
fn razer_rejected_path_cooldown_expires_when_wall_clock_goes_backwards() {
    let device = info(0x1532, 0x0094, 0xff00);
    let clock = ShiftClock {
        monotonic: FakeClock::default(),
        wall: std::sync::atomic::AtomicI64::new(10000),
    };
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    let failed =
        FakeHid::new(vec![device.clone()], vec![]).fail_open(device.path.clone(), "access");
    assert!(
        provider
            .poll(&failed, &shifted_context(&clock, &cancel))
            .is_err()
    );
    clock.wall.store(-10000, Ordering::Relaxed);
    clock.sleep(Duration::from_secs(299));
    let quiet = FakeHid::new(vec![device.clone()], vec![]);
    assert!(
        provider
            .poll(&quiet, &shifted_context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(quiet.opened.lock().unwrap().is_empty());
    clock.sleep(Duration::from_secs(1));
    let failed = FakeHid::new(vec![device.clone()], vec![]).fail_open(device.path, "access");
    assert!(
        provider
            .poll(&failed, &shifted_context(&clock, &cancel))
            .is_err()
    );
    assert_eq!(failed.opened.lock().unwrap().len(), 1);
}

#[test]
fn playstation_only_merges_trusted_identity_and_charging_wins_over_basic_bluetooth() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for shared in [false, true] {
        let mut bt = info(0x054c, 0x05c4, 1);
        bt.usage = 5;
        bt.path = "a-vid&bluetooth".into();
        bt.serial = "controller-radio".into();
        let mut usb = bt.clone();
        usb.path = "b-usb".into();
        if !shared {
            usb.serial = "controller-cable".into();
        }
        let mut report = vec![0; 31];
        report[0] = 1;
        report[30] = 0x15;
        let hid = FakeHid::new(
            vec![usb, bt],
            vec![
                Step::Read(Ok(vec![1; 10])),
                Step::Feature(2, 64, Ok(vec![])),
                Step::Read(Ok(report)),
            ],
        );
        let mut provider = HidProvider::new("playstation");
        let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(r.len(), if shared { 1 } else { 2 });
        assert!(
            r.iter()
                .any(|r| r.level == Some(50) && r.charging == Some(true))
        );
        assert_eq!(provider.next_poll_delay(), None);
        hid.done();
    }
}
#[test]
fn playstation_unopenable_pending_is_bounded_and_disconnect_resets_it() {
    let device = info(0x054c, 0x05c4, 1);
    let clock = ShiftClock {
        monotonic: FakeClock::default(),
        wall: std::sync::atomic::AtomicI64::new(10000),
    };
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("playstation");
    let hid =
        FakeHid::new(vec![device.clone()], vec![]).fail_open(device.path.clone(), "held by game");
    assert_eq!(
        provider
            .poll(&hid, &shifted_context(&clock, &cancel))
            .unwrap()[0]
            .level,
        None
    );
    assert_eq!(provider.next_poll_delay(), Some(Duration::from_secs(3)));
    clock.sleep(Duration::from_secs(120));
    clock.wall.store(-10000, Ordering::Relaxed);
    provider
        .poll(&hid, &shifted_context(&clock, &cancel))
        .unwrap();
    assert_eq!(provider.next_poll_delay(), None);
    provider
        .poll(
            &FakeHid::new(vec![], vec![]),
            &shifted_context(&clock, &cancel),
        )
        .unwrap();
    provider
        .poll(&hid, &shifted_context(&clock, &cancel))
        .unwrap();
    assert_eq!(provider.next_poll_delay(), Some(Duration::from_secs(3)));
}

fn lofree_steps(online: u8, level: u8, noise: bool) -> Vec<Step> {
    use hb_providers::protocols::padded;
    let mut steps = vec![];
    for (cmd, value) in if online == 0 {
        vec![(0xaa, 0)]
    } else {
        vec![(0xaa, online), (0x1a, level)]
    } {
        steps.push(Step::Write(padded(&[4, 0, 0, 1], 32)));
        if noise {
            steps.push(Step::Read(Ok(vec![1, 0, 0, 1])));
        }
        steps.push(Step::Read(Ok(vec![4, 0, 0, 1])));
        steps.push(Step::Write(padded(&[4, 0, 0, cmd], 32)));
        if noise {
            steps.push(Step::Read(Ok(vec![4, 0, 0, 0x55, 0, 3, 0, 0, 99])));
        }
        steps.push(Step::Read(Ok(vec![4, 0, 0, cmd, 0, 0, 0, 0, value])));
        steps.push(Step::Write(padded(&[4, 0, 0, 2], 32)));
        steps.push(Step::Read(Ok(vec![4, 0, 0, 2])));
    }
    steps
}
#[test]
fn lofree_offline_noise_range_and_only_trusted_cable_suppression() {
    let mut radio = info(0x388d, 0x25, 0xff1c);
    radio.usage = 0x92;
    radio.serial = "keyboard-radio".into();
    radio.product = "HYZEN67@Lofree".into();
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for (online, level, noise) in [(0, 66, false), (1, 40, true), (1, 200, false)] {
        let hid = FakeHid::new(vec![radio.clone()], lofree_steps(online, level, noise));
        let r = HidProvider::new("lofree")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap();
        if online == 0 || level > 100 {
            assert!(r.is_empty());
        } else {
            assert_eq!(r[0].name, "Lofree HYZEN67");
            assert_eq!(r[0].level, Some(40));
            assert_eq!(r[0].kind, "keyboard");
        }
        hid.done();
    }
    let mut cable = radio.clone();
    cable.product_id = 0x24;
    cable.path = "cable".into();
    let hid = FakeHid::new(vec![radio.clone(), cable.clone()], vec![]);
    assert!(
        HidProvider::new("lofree")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
    cable.serial = "other-keyboard".into();
    let hid = FakeHid::new(vec![radio, cable], lofree_steps(1, 60, false));
    assert_eq!(
        HidProvider::new("lofree")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()[0]
            .level,
        Some(60)
    );
    hid.done();
}
#[test]
fn barracuda_offline_costs_one_query_and_cleanup_even_after_failed_query_write() {
    use hb_providers::protocols::pa_request;
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for failed_query in [false, true] {
        let mut steps = vec![
            Step::Read(Ok(vec![])),
            Step::Write(pa_request(true, 0xe1, Some(true))),
        ];
        for _ in 0..if failed_query { 1 } else { 4 } {
            steps.push(Step::Read(Ok(vec![])));
            if failed_query {
                steps.push(Step::WriteError(
                    pa_request(true, 0x21, None),
                    "query rejected",
                ));
            } else {
                steps.push(Step::Write(pa_request(true, 0x21, None)));
                steps.extend((0..6).map(|_| Step::Read(Ok(vec![]))));
            }
        }
        steps.extend([
            Step::Read(Ok(vec![])),
            Step::Write(pa_request(true, 0xe1, Some(false))),
        ]);
        let hid = FakeHid::new(vec![info(0x1532, 0x53a, 0xff00)], steps);
        let mut provider = HidProvider::new("barracuda");
        let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].level, None);
        assert_eq!(r[0].charging, Some(false));
        assert_eq!(r[0].connection, Connection::Sleeping);
        assert_eq!(r[0].name, "Razer Barracuda Pro (2.4 GHz)");
        assert!(
            provider
                .diagnostics()
                .iter()
                .any(|s| s.contains("no battery reply"))
        );
        hid.done();
    }
}

#[test]
fn identical_pid_hardware_paths_remain_separate_and_classic_cache_is_per_device() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut a = info(0x1038, 0x12b3, 0xff43);
    a.usage = 0x202;
    a.interface = 3;
    a.path = "receiver-a".into();
    let mut b = a.clone();
    b.path = "receiver-b".into();
    let steps = vec![
        Step::Write(vec![6, 0x12]),
        Step::Read(Ok(vec![6, 0x12, 0, 40])),
        Step::Write(vec![6, 0x12]),
        Step::Read(Ok(vec![6, 0x12, 0, 70])),
    ];
    let hid = FakeHid::new(vec![a.clone(), b.clone()], steps.clone());
    let mut p = HidProvider::new("steelseries");
    let first = p.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].key, first[1].key);
    hid.done();
    let hid = FakeHid::new(vec![b, a], steps);
    let second = p.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(
        second.iter().map(|r| &r.key).collect::<Vec<_>>(),
        first.iter().map(|r| &r.key).collect::<Vec<_>>()
    );
    hid.done();
}

#[test]
fn placeholder_serials_and_containers_do_not_merge_identical_headsets() {
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for placeholder in [
        "",
        "Unknown",
        "NONE",
        "N/A",
        "NULL",
        "00000000",
        "{00000000-0000-0000-0000-000000000000}",
    ] {
        let mut a = info(0x1038, 0x12b3, 0xff43);
        a.usage = 0x202;
        a.interface = 3;
        a.path = "headset-a".into();
        a.serial = placeholder.into();
        a.container = Some(placeholder.into());
        let mut b = a.clone();
        b.path = "headset-b".into();
        let hid = FakeHid::new(
            vec![a, b],
            vec![
                Step::Write(vec![6, 0x12]),
                Step::Read(Ok(vec![6, 0x12, 0, 40])),
                Step::Write(vec![6, 0x12]),
                Step::Read(Ok(vec![6, 0x12, 0, 70])),
            ],
        );
        let r = HidProvider::new("steelseries")
            .poll(&hid, &context(&clock, &cancel))
            .unwrap();
        assert_eq!(r.len(), 2, "{placeholder}");
        assert_ne!(r[0].key, r[1].key);
        assert!(r[0].key.ends_with("headset-a"));
        assert!(r[1].key.ends_with("headset-b"));
        hid.done();
    }
}

#[test]
fn razer_charging_transport_error_preserves_valid_level_and_working_path() {
    use hb_providers::protocols::razer_request;
    let hid = FakeHid::new(
        vec![info(0x1532, 0x0094, 0xff00)],
        vec![
            Step::Send(razer_request(0x1f, 0x80)),
            Step::Feature(0, 91, Ok(razer_reply(2, 0x80, 128))),
            Step::SendError(razer_request(0x1f, 0x84), "charging busy"),
        ],
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("razer");
    let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(r[0].level, Some(50));
    assert_eq!(r[0].charging, Some(false));
    assert!(
        provider
            .diagnostics()
            .iter()
            .any(|s| s.contains("charging query"))
    );
    hid.done();
}

#[test]
fn container_aliases_are_canonical_and_opaque_ampersand_paths_remain_distinct() {
    let mut a = info(0x1038, 0x12b3, 0xff43);
    a.usage = 0x202;
    a.interface = 3;
    a.path = "receiver-a&1".into();
    a.serial = "0000".into();
    a.container = Some(" {AbCd-1234} ".into());
    let mut b = a.clone();
    b.path = "receiver-a&2".into();
    b.container = Some("{ABCD-1234}".into());
    assert_eq!(
        hb_providers::provider::receiver_key(&a),
        hb_providers::provider::receiver_key(&b)
    );
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let hid = FakeHid::new(
        vec![a.clone(), b.clone()],
        vec![
            Step::Write(vec![6, 0x12]),
            Step::Read(Ok(vec![6, 0x12, 0, 40])),
        ],
    );
    let r = HidProvider::new("steelseries")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].key.ends_with("{ABCD-1234}"));
    hid.done();
    a.container = Some("0000".into());
    b.container = None;
    assert_eq!(hb_providers::provider::receiver_key(&a), "receiver-a&1");
    assert_eq!(hb_providers::provider::receiver_key(&b), "receiver-a&2");
    let hid = FakeHid::new(
        vec![a, b],
        vec![
            Step::Write(vec![6, 0x12]),
            Step::Read(Ok(vec![6, 0x12, 0, 40])),
            Step::Write(vec![6, 0x12]),
            Step::Read(Ok(vec![6, 0x12, 0, 70])),
        ],
    );
    let r = HidProvider::new("steelseries")
        .poll(&hid, &context(&clock, &cancel))
        .unwrap();
    assert_eq!(r.len(), 2);
    assert_ne!(r[0].key, r[1].key);
    hid.done();
}

#[test]
fn audeze_actual_unplug_replug_keeps_identity_and_resets_stuck_hint() {
    let mut device = info(0x3329, 0x4b18, 0xff13);
    device.serial = "headset".into();
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut provider = HidProvider::new("audeze");
    let mut key = String::new();
    for short in [true, false] {
        let hid = FakeHid::new(vec![device.clone()], audeze_echo_steps(short));
        let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        if !short {
            key = r[0].key.clone();
            assert!(r[0].approx.as_deref().unwrap().contains("unplug"));
        } else {
            assert!(r.is_empty());
        }
        hid.done();
    }
    assert!(
        provider
            .poll(&FakeHid::new(vec![], vec![]), &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    let hid = FakeHid::new(vec![device.clone()], audeze_short_steps(75));
    let r = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
    assert_eq!(r[0].key, key);
    assert_eq!(r[0].level, Some(75));
    assert_eq!(r[0].approx, None);
    hid.done();
    let hid = FakeHid::new(vec![device], audeze_echo_steps(true));
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
}
