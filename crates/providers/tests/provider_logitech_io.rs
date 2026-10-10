mod common;
use common::*;
use hb_core::*;
use hb_providers::HidProvider;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[derive(Default)]
struct Exchange {
    steps: Vec<Step>,
    counter: u8,
    short: bool,
    noise: bool,
}
impl Exchange {
    fn packet(slot: u8, feature: u8, token: u8, params: &[u8]) -> Vec<u8> {
        let mut p = vec![0; 20];
        p[..4].copy_from_slice(&[0x11, slot, feature, token]);
        p[4..4 + params.len()].copy_from_slice(params);
        p
    }
    fn start(&mut self, slot: u8, feature: u8, function: u8, params: &[u8]) -> u8 {
        self.counter = self.counter.wrapping_add(1);
        let token = (function << 4) | (10 + self.counter % 6);
        self.steps
            .push(Step::Write(Self::packet(slot, feature, token, params)));
        token
    }
    fn read(&mut self, data: Vec<u8>) {
        self.steps
            .push(Step::ReadSized(64, Duration::ZERO, Ok(data)));
    }
    fn answer(&mut self, slot: u8, feature: u8, function: u8, params: &[u8], answer: &[u8]) {
        let token = self.start(slot, feature, function, params);
        if self.noise {
            self.read(Self::packet(slot, feature, token.wrapping_sub(1), &[99]));
            if self.short {
                self.read(vec![0x10, slot, 0x8f, feature, 3, 9, 0]);
            }
        }
        self.read(Self::packet(slot, feature, token, answer));
    }
    fn error(&mut self, slot: u8, feature: u8, function: u8, params: &[u8], code: u8) {
        let token = self.start(slot, feature, function, params);
        if self.short {
            self.read(vec![]);
        }
        self.read(vec![0x10, slot, 0x8f, feature, token, code, 0]);
    }
    fn silent_ping(&mut self, slot: u8, millis: usize) {
        self.start(slot, 0, 1, &[]);
        for _ in 0..millis / 5 {
            self.read(vec![]);
            if self.short {
                self.read(vec![]);
            }
        }
    }
    fn identity(&mut self, slot: u8, name: &str, unit: [u8; 4]) {
        self.answer(slot, 0, 0, &[0, 5], &[2]);
        self.answer(slot, 2, 0, &[], &[name.len() as u8]);
        for (idx, chunk) in name.as_bytes().chunks(16).enumerate() {
            self.answer(slot, 2, 1, &[(idx * 16) as u8], chunk);
        }
        self.answer(slot, 2, 2, &[], &[3]);
        self.answer(slot, 0, 0, &[0, 3], &[3]);
        self.answer(slot, 3, 0, &[], &[0, unit[0], unit[1], unit[2], unit[3]]);
    }
    fn no_identity(&mut self, slot: u8) {
        self.answer(slot, 0, 0, &[0, 5], &[0]);
        self.answer(slot, 0, 0, &[0, 3], &[0]);
    }
    fn unified(&mut self, slot: u8, params: &[u8]) {
        self.answer(slot, 0, 0, &[0x10, 4], &[4]);
        self.answer(slot, 4, 1, &[], params);
    }
    fn adc(&mut self, slot: u8, params: &[u8], error: bool) {
        for f in [0x1004u16, 0x1000, 0x1001] {
            self.answer(slot, 0, 0, &f.to_be_bytes(), &[0]);
        }
        self.answer(slot, 0, 0, &[0x1f, 0x20], &[6]);
        if error {
            self.error(slot, 6, 0, &[], 9)
        } else {
            self.answer(slot, 6, 0, &[], params)
        }
    }
    fn empty_slots(&mut self) {
        for slot in 2..=6 {
            self.error(slot, 0, 1, &[], 8);
        }
    }
    fn mouse(&mut self, name: &str, unit: [u8; 4], level: u8) {
        self.answer(1, 0, 1, &[], &[2]);
        self.identity(1, name, unit);
        self.unified(1, &[level, 0, 0]);
        self.empty_slots();
    }
    fn reset(&mut self) {
        self.counter = 0;
    }
}
fn collection(pid: u16, path: &str, usage: u16) -> HidInfo {
    let mut d = info(0x046d, pid, 0xff00);
    d.path = path.into();
    d.usage = usage;
    d
}
fn receiver(pid: u16, instance: &str, short: bool) -> Vec<HidInfo> {
    let base = format!(r"\\?\HID#VID_046D&PID_{pid:04X}&MI_02&Col");
    let long = collection(pid, &format!("{base}02#{instance}&0&0001#guid"), 2);
    if short {
        vec![
            collection(pid, &format!("{base}01#{instance}&0&0000#guid"), 1),
            long,
        ]
    } else {
        vec![long]
    }
}
fn poll(p: &mut HidProvider, h: &FakeHid, c: &FakeClock) -> Vec<Reading> {
    p.poll(h, &context(c, &AtomicBool::new(false))).unwrap()
}
#[test]
fn receiver_pid_without_receiver_name_and_short_long_grouping() {
    let mut ex = Exchange {
        short: true,
        ..Default::default()
    };
    ex.mouse("G502 LIGHTSPEED", [0xc1, 0x5e, 9, 0xcd], 76);
    let hid = FakeHid::new(receiver(0xc547, "7&aaaa", true), ex.steps);
    let clock = FakeClock::default();
    let rows = poll(&mut HidProvider::new("logitech"), &hid, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            &*rows[0].name,
            rows[0].level,
            rows[0].charging,
            &*rows[0].kind,
            &*rows[0].key
        ),
        (
            "G502 LIGHTSPEED",
            Some(76),
            Some(false),
            "mouse",
            "logitech:C15E09CD"
        )
    );
    assert_eq!(hid.opened.lock().unwrap().len(), 2);
    hid.done();
}
#[test]
fn foreign_errors_and_late_swid_replies_never_replace_matching_battery() {
    let mut ex = Exchange {
        short: true,
        noise: true,
        ..Default::default()
    };
    ex.mouse("G502", [1, 2, 3, 4], 76);
    let hid = FakeHid::new(receiver(0xc539, "7&aaaa", true), ex.steps);
    let rows = poll(
        &mut HidProvider::new("logitech"),
        &hid,
        &FakeClock::default(),
    );
    assert_eq!(rows[0].level, Some(76));
    assert_eq!(rows[0].name, "G502");
    hid.done();
}
#[test]
fn identity_failure_retries_and_replaces_fallback_key_on_next_poll() {
    let mut ex = Exchange::default();
    ex.answer(1, 0, 1, &[], &[2]);
    ex.no_identity(1);
    ex.unified(1, &[76, 0, 0]);
    ex.empty_slots();
    ex.reset();
    ex.mouse("New Mouse", [1, 2, 3, 4], 77);
    let hid = FakeHid::new(receiver(0xc539, "7&aaaa", false), ex.steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("logitech");
    let first = poll(&mut p, &hid, &clock);
    assert_eq!(first[0].name, "Logitech device");
    assert_eq!(first[0].key, "logitech:7&aaaa&0:c539:1");
    let next = poll(&mut p, &hid, &clock);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].name, "New Mouse");
    assert_eq!(next[0].key, "logitech:01020304");
    hid.done();
}
#[test]
fn empty_slot_invalidates_old_identity_before_replacement() {
    let mut ex = Exchange::default();
    ex.mouse("Old", [1, 2, 3, 4], 76);
    ex.reset();
    ex.error(1, 0, 1, &[], 8);
    ex.empty_slots();
    ex.reset();
    ex.mouse("Replacement", [5, 6, 7, 8], 43);
    let hid = FakeHid::new(receiver(0xc539, "7&aaaa", false), ex.steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("logitech");
    poll(&mut p, &hid, &clock);
    poll(&mut p, &hid, &clock);
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Replacement");
    assert_eq!(rows[0].key, "logitech:05060708");
    assert_eq!(rows[0].level, Some(43));
    hid.done();
}
#[test]
fn sleeping_ping_uses_shorter_retry_and_expires_after_five_minutes() {
    let mut ex = Exchange::default();
    ex.mouse("Mouse", [1, 2, 3, 4], 76);
    ex.reset();
    ex.silent_ping(1, 2000);
    ex.empty_slots();
    ex.reset();
    ex.silent_ping(1, 600);
    ex.empty_slots();
    ex.reset();
    ex.error(1, 0, 1, &[], 9);
    ex.empty_slots();
    let hid = FakeHid::new(receiver(0xc539, "7&aaaa", false), ex.steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("logitech");
    poll(&mut p, &hid, &clock);
    let sleeping = poll(&mut p, &hid, &clock);
    assert_eq!(sleeping[0].connection, Connection::Sleeping);
    assert_eq!(sleeping[0].level, Some(76));
    assert_eq!(clock.monotonic(), Duration::from_millis(2000));
    poll(&mut p, &hid, &clock);
    assert_eq!(clock.monotonic(), Duration::from_millis(2600));
    clock.0.store(300_000, Ordering::Relaxed);
    assert!(poll(&mut p, &hid, &clock).is_empty());
    hid.done();
}
#[test]
fn unified_coarse_good_is_explicit_and_not_exact_percentage() {
    let mut ex = Exchange::default();
    ex.answer(1, 0, 1, &[], &[2]);
    ex.no_identity(1);
    ex.unified(1, &[0, 4, 0, 0]);
    ex.empty_slots();
    let hid = FakeHid::new(receiver(0xc539, "7&aaaa", false), ex.steps);
    let rows = poll(
        &mut HidProvider::new("logitech"),
        &hid,
        &FakeClock::default(),
    );
    assert_eq!(rows[0].level, Some(50));
    assert_eq!(rows[0].precision, Precision::Coarse);
    assert_eq!(rows[0].approx.as_deref(), Some("about 50% (good)"));
    hid.done();
}
#[test]
fn identical_receivers_keep_separate_units_and_collection_failures_are_local() {
    let mut infos = receiver(0xc52b, "7&aaaa", true);
    infos.extend(receiver(0xc52b, "7&bbbb", true));
    let mut ex = Exchange {
        short: true,
        ..Default::default()
    };
    ex.mouse("Mouse", [1, 2, 3, 4], 76);
    ex.reset();
    ex.mouse("Mouse", [5, 6, 7, 8], 77);
    let hid = FakeHid::new(infos.clone(), ex.steps);
    let rows = poll(
        &mut HidProvider::new("logitech"),
        &hid,
        &FakeClock::default(),
    );
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].key, rows[1].key);
    assert_eq!(
        rows.iter().map(|r| r.level.unwrap()).collect::<Vec<_>>(),
        [76, 77]
    );
    hid.done();
    let failed_path = infos[1].path.clone();
    let mut ex = Exchange::default();
    ex.mouse("Survivor", [5, 6, 7, 8], 42);
    let hid = FakeHid::new(infos, ex.steps)
        .fail_open(failed_path, "access denied")
        .fail_open(
            receiver(0xc52b, "7&bbbb", true)[0].path.clone(),
            "short collection denied",
        );
    let mut p = HidProvider::new("logitech");
    let rows = poll(&mut p, &hid, &FakeClock::default());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Survivor");
    assert!(p.diagnostics().iter().any(|s| s.contains("access denied")));
    hid.done();
}
#[test]
fn headset_adc_charging_off_consumer_and_inactive_error() {
    for (pid, page, usage, params, level, charging, error) in [
        (
            0x0a87,
            0xff43,
            0x202,
            [0x0f, 0x7b, 3],
            Some(76),
            Some(true),
            false,
        ),
        (0x0aba, 0xff43, 0x202, [0x0f, 0x7b, 0], None, None, false),
        (
            0x0ac4,
            0x0c,
            1,
            [0x0e, 0xa0, 1],
            Some(28),
            Some(false),
            false,
        ),
        (0x0aba, 0xff43, 0x202, [0, 0, 0], None, None, true),
    ] {
        let mut d = info(0x046d, pid, page);
        d.usage = usage;
        let mut ex = Exchange::default();
        ex.answer(255, 0, 1, &[], &[2]);
        ex.no_identity(255);
        ex.adc(255, &params, error);
        let hid = FakeHid::new(vec![d], ex.steps);
        let mut p = HidProvider::new("logitech");
        let rows = poll(&mut p, &hid, &FakeClock::default());
        if let Some(level) = level {
            assert_eq!(rows[0].level, Some(level));
            assert_eq!(rows[0].charging, charging);
            assert_eq!(rows[0].kind, "headset");
            if pid == 0x0a87 {
                assert_eq!(rows[0].name, "Logitech G935");
            }
        } else {
            assert!(rows.is_empty());
            if error {
                assert!(p.diagnostics().iter().any(|s| s.contains("inactive")));
            }
        }
        hid.done();
    }
}
#[test]
fn hidpp10_and_switched_off_diagnostics_and_sleeping_cache() {
    let mut ex = Exchange::default();
    ex.mouse("Mouse", [1, 2, 3, 4], 76);
    ex.reset();
    ex.error(1, 0, 1, &[], 9);
    ex.empty_slots();
    ex.reset();
    ex.error(1, 0, 1, &[], 1);
    ex.empty_slots();
    let hid = FakeHid::new(receiver(0xc52b, "7&aaaa", false), ex.steps);
    let mut p = HidProvider::new("logitech");
    let clock = FakeClock::default();
    poll(&mut p, &hid, &clock);
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert!(p.diagnostics().iter().any(|s| s.contains("switched off")));
    poll(&mut p, &hid, &clock);
    assert!(p.diagnostics().iter().any(|s| s.contains("HID++ 1.0")));
    hid.done();
}
#[test]
fn vendor_collection_wins_over_headset_and_foreign_consumers_in_any_order() {
    for reverse in [false, true] {
        let mut infos = receiver(0xc08d, "7&aaaa", true);
        let mut foreign = info(0x046d, 0xc08d, 0xff43);
        foreign.usage = 0x202;
        foreign.path = "foreign-headset".into();
        infos.push(foreign);
        let mut consumer = info(0x046d, 0xc539, 0xc);
        consumer.path = "foreign-consumer".into();
        infos.push(consumer);
        if reverse {
            infos.reverse();
        }
        let mut ex = Exchange {
            short: true,
            ..Default::default()
        };
        ex.answer(255, 0, 1, &[], &[2]);
        ex.no_identity(255);
        ex.unified(255, &[76, 0, 0]);
        let hid = FakeHid::new(infos, ex.steps);
        let rows = poll(
            &mut HidProvider::new("logitech"),
            &hid,
            &FakeClock::default(),
        );
        assert_eq!(rows[0].level, Some(76));
        assert!(
            hid.opened
                .lock()
                .unwrap()
                .iter()
                .all(|s| !s.contains("foreign"))
        );
        hid.done();
    }
}
fn razer_response(status: u8, cmd: u8, value: u8) -> Vec<u8> {
    let mut r = vec![0; 91];
    r[1] = status;
    r[7] = 7;
    r[8] = cmd;
    r[10] = value;
    r
}
fn razer_success(tid: u8, raw: u8, charging: u8) -> Vec<Step> {
    use hb_providers::protocols::razer_request;
    vec![
        Step::Send(razer_request(tid, 0x80)),
        Step::Feature(0, 91, Ok(razer_response(2, 0x80, raw))),
        Step::Send(razer_request(tid, 0x84)),
        Step::Feature(0, 91, Ok(razer_response(2, 0x84, charging))),
    ]
}
fn razer_busy(tid: u8) -> Vec<Step> {
    use hb_providers::protocols::razer_request;
    let mut steps = vec![Step::Send(razer_request(tid, 0x80))];
    for i in 1..=13 {
        let mut reply = razer_response(2, 3, 99);
        reply[7] = 0xf;
        steps.push(Step::Feature(0, 91, Ok(reply)));
        if [4, 8].contains(&i) {
            steps.push(Step::Send(razer_request(tid, 0x80)));
        }
    }
    steps
}
#[test]
fn razer_normal_request_repeated_reply_and_busy_cache_expiry() {
    use hb_providers::protocols::razer_request;
    let mut steps = razer_success(0x1f, 181, 0);
    steps.extend(razer_busy(0x1f));
    steps.extend(razer_busy(0x1f));
    let mut d = info(0x1532, 0x00b9, 1);
    d.serial = "mouse-1".into();
    let hid = FakeHid::new(vec![d.clone()], steps);
    let clock = FakeClock::default();
    let mut p = HidProvider::new("razer");
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(
        (rows[0].level, rows[0].charging, rows[0].online()),
        (Some(71), Some(false), true)
    );
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!((rows[0].level, rows[0].online()), (Some(71), false));
    clock.0.store(300_000, Ordering::Relaxed);
    let expired = poll(&mut p, &hid, &clock);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].level, None);
    assert_eq!(expired[0].connection, Connection::Sleeping);
    hid.done();
    let mut steps = vec![Step::Send(razer_request(0x1f, 0x80))];
    for _ in 0..4 {
        let mut r = razer_response(2, 3, 99);
        r[7] = 0xf;
        steps.push(Step::Feature(0, 91, Ok(r)));
    }
    steps.extend(razer_success(0x1f, 128, 1));
    let hid = FakeHid::new(vec![d], steps);
    let rows = poll(&mut HidProvider::new("razer"), &hid, &FakeClock::default());
    assert_eq!(
        (rows[0].level, rows[0].charging, rows[0].online()),
        (Some(50), Some(true), true)
    );
    hid.done();
}
#[test]
fn razer_model_preferred_ids_and_keyboard_nonvendor_collection() {
    for (pid, tid, name, page, interface, raw, charging) in [
        (0x0083, 0xff, "Razer Basilisk X HyperSpeed", 1, 0, 255, 0),
        (0x00a8, 0x1f, "Razer Naga V2 Pro", 1, 0, 181, 0),
        (
            0x00c8,
            0x1f,
            "Razer Pro Click V2 Vertical Edition",
            1,
            0,
            181,
            0,
        ),
        (0x025c, 0x9f, "Razer BlackWidow V3 Pro", 0x59, 3, 181, 1),
    ] {
        let mut d = info(0x1532, pid, page);
        d.interface = interface;
        d.product = name.into();
        let hid = FakeHid::new(vec![d], razer_success(tid, raw, charging));
        let rows = poll(&mut HidProvider::new("razer"), &hid, &FakeClock::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, name);
        assert_eq!(rows[0].level, Some(if raw == 255 { 100 } else { 71 }));
        assert_eq!(rows[0].charging, Some(charging != 0));
        if pid == 0x25c {
            assert_eq!(rows[0].kind, "keyboard");
        }
        hid.done();
    }
    let hid = FakeHid::new(vec![info(0x1532, 0x0078, 1)], vec![]);
    assert!(poll(&mut HidProvider::new("razer"), &hid, &FakeClock::default()).is_empty());
    assert!(hid.opened.lock().unwrap().is_empty());
    hid.done();
}
#[test]
fn razer_every_catalog_preferred_id_is_reached_after_wrong_first_transaction() {
    use hb_providers::protocols::razer_request;
    let tids: std::collections::BTreeSet<_> = hb_providers::catalog::DEVICES
        .iter()
        .filter(|d| d.provider == "razer" && ![0x555, 0x556].contains(&d.pid))
        .map(|d| d.parameter)
        .collect();
    assert_eq!(
        tids,
        std::collections::BTreeSet::from([0x1f, 0x3f, 0x9f, 0xff])
    );
    for tid in tids {
        let preferred = if tid == 0x1f { 0x3f } else { 0x1f };
        let device = hb_providers::catalog::DEVICES
            .iter()
            .find(|d| {
                d.provider == "razer"
                    && d.parameter == preferred
                    && ![0x555, 0x556].contains(&d.pid)
            })
            .unwrap();
        let d = info(0x1532, device.pid, 1);
        let mut attempts = vec![preferred];
        for t in [0x1f, 0x3f, 0xff, 0x9f, 8] {
            if !attempts.contains(&t) {
                attempts.push(t);
            }
        }
        let mut steps = vec![];
        for attempt in attempts {
            if attempt == tid {
                steps.extend(razer_success(attempt, 181, 0));
                break;
            }
            steps.extend([
                Step::Send(razer_request(attempt, 0x80)),
                Step::Feature(0, 91, Ok(razer_response(5, 0x80, 0))),
            ]);
        }
        let hid = FakeHid::new(vec![d], steps);
        let rows = poll(&mut HidProvider::new("razer"), &hid, &FakeClock::default());
        assert_eq!(rows.len(), 1, "fallback {tid:02x}");
        assert_eq!(rows[0].level, Some(71));
        hid.done();
    }
}
#[test]
fn logitech_unrelated_consumer_and_unknown_headset_never_opened() {
    let mut unknown = info(0x046d, 0xb99, 0xff43);
    unknown.usage = 0x202;
    let mut consumer = info(0x046d, 0xc539, 0xc);
    consumer.usage = 1;
    let hid = FakeHid::new(vec![unknown, consumer], vec![]);
    assert!(
        poll(
            &mut HidProvider::new("logitech"),
            &hid,
            &FakeClock::default()
        )
        .is_empty()
    );
    assert!(hid.opened.lock().unwrap().is_empty());
    hid.done();
}
#[test]
fn logitech_receiver_and_direct_device_unit_deduplicate_preferring_charging() {
    let mut ex = Exchange::default();
    ex.answer(255, 0, 1, &[], &[2]);
    ex.identity(255, "Mouse", [1, 2, 3, 4]);
    ex.unified(255, &[50, 0, 1]);
    ex.reset();
    ex.mouse("Mouse", [1, 2, 3, 4], 49);
    let mut infos = vec![collection(0xc08b, "direct", 2)];
    infos.extend(receiver(0xc539, "7&aaaa", false));
    let hid = FakeHid::new(infos, ex.steps);
    let rows = poll(
        &mut HidProvider::new("logitech"),
        &hid,
        &FakeClock::default(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "logitech:01020304");
    assert_eq!(rows[0].level, Some(50));
    assert_eq!(rows[0].charging, Some(true));
    hid.done();
}

#[test]
fn logitech_keys_are_stable_when_unrelated_receivers_arrive_and_leave() {
    for unit in [[1, 2, 3, 4], [0, 0, 0, 0]] {
        let clock = FakeClock::default();
        let mut p = HidProvider::new("logitech");
        let mut ex = Exchange::default();
        ex.mouse("First", unit, 76);
        let first = FakeHid::new(receiver(0xc52b, "7&aaaa", false), ex.steps);
        let rows = poll(&mut p, &first, &clock);
        let stable = rows[0].key.clone();
        first.done();
        let mut infos = receiver(0xc52b, "7&aaaa", false);
        infos.extend(receiver(0xc52b, "7&bbbb", false));
        let mut ex = Exchange::default();
        ex.answer(1, 0, 1, &[], &[2]);
        ex.unified(1, &[77, 0, 0]);
        ex.empty_slots();
        ex.reset();
        ex.mouse("Second", [5, 6, 7, 8], 32);
        let both = FakeHid::new(infos, ex.steps);
        let rows = poll(&mut p, &both, &clock);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().find(|r| r.name == "First").unwrap().key, stable);
        both.done();
        let mut ex = Exchange::default();
        ex.answer(1, 0, 1, &[], &[2]);
        ex.unified(1, &[78, 0, 0]);
        ex.empty_slots();
        let last = FakeHid::new(receiver(0xc52b, "7&aaaa", false), ex.steps);
        let rows = poll(&mut p, &last, &clock);
        assert_eq!(rows.iter().find(|r| r.name == "First").unwrap().key, stable);
        last.done();
    }
}
fn razer_off(tid: u8) -> Vec<Step> {
    use hb_providers::protocols::razer_request;
    vec![
        Step::Send(razer_request(tid, 0x80)),
        Step::Feature(0, 91, Ok(razer_response(4, 0x80, 0))),
    ]
}
fn razer_device(pid: u16, serial: &str, path: &str) -> HidInfo {
    let mut d = info(0x1532, pid, 1);
    d.serial = serial.into();
    d.path = path.into();
    d
}
#[test]
fn razer_cable_suppresses_only_trusted_same_hardware_receiver() {
    for (receiver_serial, cable_serial, remaining) in [
        ("mouse-1", "mouse-1", 1),
        ("mouse-1", "mouse-2", 2),
        ("000000000000", "000000000000", 2),
    ] {
        let rx = razer_device(0x007b, receiver_serial, "rx");
        let usb = razer_device(0x007a, cable_serial, "usb");
        let clock = FakeClock::default();
        let mut p = HidProvider::new("razer");
        let hid = FakeHid::new(vec![rx.clone()], razer_success(0xff, 128, 0));
        assert_eq!(poll(&mut p, &hid, &clock).len(), 1);
        hid.done();
        let mut steps = razer_off(0xff);
        steps.extend(razer_success(0xff, 181, 1));
        let hid = FakeHid::new(vec![rx, usb], steps);
        let rows = poll(&mut p, &hid, &clock);
        assert_eq!(rows.len(), remaining);
        assert_eq!(rows.iter().filter(|r| r.online()).count(), 1);
        assert_eq!(
            rows.iter().find(|r| r.online()).unwrap().charging,
            Some(true)
        );
        hid.done();
    }
}
#[test]
fn razer_receiver_alone_two_live_receivers_and_two_sleeping_devices_keep_icons() {
    let clock = FakeClock::default();
    let rx = razer_device(0x007b, "mouse-1", "rx");
    let usb = razer_device(0x007a, "mouse-2", "usb");
    let mut p = HidProvider::new("razer");
    let hid = FakeHid::new(vec![rx.clone()], razer_success(0xff, 128, 0));
    poll(&mut p, &hid, &clock);
    hid.done();
    let hid = FakeHid::new(vec![rx.clone()], razer_off(0xff));
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(50));
    assert!(!rows[0].online());
    hid.done();
    let mut steps = razer_success(0xff, 128, 0);
    steps.extend(razer_success(0xff, 181, 1));
    let hid = FakeHid::new(vec![rx.clone(), usb.clone()], steps);
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(Reading::online));
    hid.done();
    let mut steps = razer_off(0xff);
    steps.extend(razer_off(0xff));
    let hid = FakeHid::new(vec![rx, usb], steps);
    let rows = poll(&mut p, &hid, &clock);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| !r.online()));
    hid.done();
}
#[test]
fn razer_second_receiver_and_unrelated_model_never_hidden_by_live_cable() {
    for pid in [0x007b, 0x007d] {
        let a = razer_device(if pid == 0x007d { 0x007a } else { 0x007b }, "first", "rx");
        let b = razer_device(pid, "second", "zz-other");
        let tid = if pid == 0x007d { 0x3f } else { 0xff };
        let clock = FakeClock::default();
        let mut p = HidProvider::new("razer");
        let mut steps = razer_success(0xff, 128, 0);
        steps.extend(razer_success(tid, 181, 0));
        let hid = FakeHid::new(vec![a.clone(), b.clone()], steps);
        assert_eq!(poll(&mut p, &hid, &clock).len(), 2);
        hid.done();
        let mut steps = razer_success(0xff, 128, 0);
        steps.extend(razer_off(tid));
        let hid = FakeHid::new(vec![a, b], steps);
        let rows = poll(&mut p, &hid, &clock);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().filter(|r| r.online()).count(), 1);
        hid.done();
    }
}

#[test]
fn razer_long_sleep_retains_identity_without_expired_level_and_unplug_removes_it() {
    let mut d = info(0x1532, 0x00b9, 1);
    d.serial = "mixed-Mouse-Serial".into();
    let mut steps = razer_success(0x1f, 181, 0);
    steps.extend(razer_busy(0x1f));
    steps.extend(razer_busy(0x1f));
    steps.extend(razer_success(0x1f, 128, 0));
    let mut hid = FakeHid::new(vec![d], steps);
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("razer");
    let awake = poll(&mut provider, &hid, &clock).remove(0);
    assert_eq!(awake.level, Some(71));
    clock.0.store(299000, Ordering::Relaxed);
    let sleeping = poll(&mut provider, &hid, &clock).remove(0);
    assert_eq!(sleeping.level, Some(71));
    assert_eq!(sleeping.connection, Connection::Sleeping);
    clock.0.store(301000, Ordering::Relaxed);
    let expired = poll(&mut provider, &hid, &clock).remove(0);
    assert_eq!((expired.level, expired.charging), (None, None));
    assert_eq!(expired.timestamp, awake.timestamp);
    assert_eq!(expired.key, awake.key);
    assert_eq!(expired.serial, awake.serial);
    assert_eq!(expired.connection, Connection::Sleeping);
    clock.0.store(302000, Ordering::Relaxed);
    let recovered = poll(&mut provider, &hid, &clock).remove(0);
    assert_eq!(recovered.level, Some(50));
    assert_eq!(recovered.key, awake.key);
    assert_eq!(recovered.serial, awake.serial);
    assert!(recovered.online());
    hid.infos.clear();
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    engine.apply("razer", Ok(vec![recovered]), 0.0, false);
    for n in 1..=3 {
        let absent = poll(&mut provider, &hid, &clock);
        assert!(absent.is_empty());
        engine.apply("razer", Ok(absent), n as f64, false);
    }
    assert!(engine.readings().is_empty());
    hid.done();
}

#[test]
fn razer_exclusive_open_error_remains_explicit_then_backoff_preserves_known_identity() {
    let mut d = info(0x1532, 0x00b9, 1);
    d.serial = "known-mouse".into();
    let path = d.path.clone();
    let mut hid = FakeHid::new(vec![d], razer_success(0x1f, 181, 0));
    let clock = FakeClock::default();
    let mut provider = HidProvider::new("razer");
    let awake = poll(&mut provider, &hid, &clock).remove(0);
    let key = awake.key.clone();
    let mut engine = Engine::new(Settings::default(), Estimator::default());
    engine.apply("razer", Ok(vec![awake]), 0.0, false);
    hid.open_errors.insert(path, "exclusive access conflict");
    let error = provider
        .poll(&hid, &context(&clock, &AtomicBool::new(false)))
        .unwrap_err();
    assert!(error.message.contains("exclusive access conflict"));
    engine.apply("razer", Err(error), 1.0, false);
    let stale = engine.readings();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].key, key);
    assert_eq!(stale[0].connection, Connection::Stale);
    let opened = hid.opened.lock().unwrap().len();
    clock.0.store(100000, Ordering::Relaxed);
    let backoff = poll(&mut provider, &hid, &clock);
    assert_eq!(backoff.len(), 1);
    assert_eq!(backoff[0].key, key);
    assert_eq!(backoff[0].connection, Connection::Sleeping);
    assert_eq!(hid.opened.lock().unwrap().len(), opened);
    hid.infos.clear();
    assert!(poll(&mut provider, &hid, &clock).is_empty());
    hid.done();
    let unknown = FakeHid::new(vec![info(0x1532, 0x00b9, 1)], vec![]);
    let mut unknown = unknown;
    unknown
        .open_errors
        .insert(unknown.infos[0].path.clone(), "exclusive access conflict");
    let mut fresh = HidProvider::new("razer");
    assert!(
        fresh
            .poll(&unknown, &context(&clock, &AtomicBool::new(false)))
            .is_err()
    );
    assert!(poll(&mut fresh, &unknown, &clock).is_empty());
    unknown.done();
}

#[test]
fn definitive_empty_receiver_slot_forgets_unit_and_repair_reads_new_identity() {
    let mut first = Exchange::default();
    first.mouse("Invented old mouse", [1, 2, 3, 4], 76);
    let mut provider = HidProvider::new("logitech");
    let clock = FakeClock::default();
    let first_hid = FakeHid::new(receiver(0xc547, "invented-receiver", false), first.steps);
    let old = poll(&mut provider, &first_hid, &clock);
    assert_eq!(old.len(), 1);
    first_hid.done();
    let mut unpaired = Exchange::default();
    for slot in 1..=6 {
        unpaired.error(slot, 0, 1, &[], 8);
    }
    let empty_hid = FakeHid::new(receiver(0xc547, "invented-receiver", false), unpaired.steps);
    assert!(poll(&mut provider, &empty_hid, &clock).is_empty());
    empty_hid.done();
    let mut second = Exchange::default();
    second.mouse("Invented replacement", [5, 6, 7, 8], 61);
    let second_hid = FakeHid::new(receiver(0xc547, "invented-receiver", false), second.steps);
    let new = poll(&mut provider, &second_hid, &clock);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].name, "Invented replacement");
    assert_eq!(new[0].level, Some(61));
    assert_ne!(new[0].key, old[0].key);
    second_hid.done();
}
