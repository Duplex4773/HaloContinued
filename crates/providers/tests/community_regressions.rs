use hb_core::*;
use hb_providers::{
    HidProvider,
    protocols::{padded, pulsar_request},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
mod common;
use common::*;

fn poll(p: &mut HidProvider, h: &FakeHid, c: &FakeClock) -> Vec<Reading> {
    p.poll(h, &context(c, &AtomicBool::new(false))).unwrap()
}
fn endpoint(vid: u16, pid: u16, page: u16, usage: u16, path: &str) -> HidInfo {
    let mut i = info(vid, pid, page);
    i.usage = usage;
    i.path = path.into();
    i.serial = "SYNTHETIC-SAME-PHYSICAL-DEVICE".into();
    i
}
fn nearlink(report: u8, seq: u8, level: u8, charging: bool) -> Vec<Step> {
    let mut r = padded(&[report, seq, 0, 0x1a], 64);
    r[8] = level;
    r[9] = u8::from(charging);
    vec![
        Step::Read(Ok(vec![])),
        Step::Write(padded(&[report, seq, 0, 0x1a], 64)),
        Step::Read(Ok(r)),
    ]
}
fn silent_nearlink(report: u8, seq: u8) -> Vec<Step> {
    let mut steps = vec![
        Step::Read(Ok(vec![])),
        Step::Write(padded(&[report, seq, 0, 0x1a], 64)),
    ];
    steps.extend(vec![Step::Read(Ok(vec![])); 4]);
    steps
}
fn nearlink_endpoints() -> (HidInfo, HidInfo) {
    (
        endpoint(0x373b, 0x108b, 0xffb2, 1, "cable"),
        endpoint(0x373b, 0x108c, 0xffb3, 1, "receiver"),
    )
}
#[test]
fn fresh_alias_replaces_sleeping_cached_alias_in_the_same_poll() {
    let (cable, radio) = nearlink_endpoints();
    let mut steps = nearlink(4, 1, 97, true);
    steps.extend(nearlink(5, 2, 91, false));
    let first = FakeHid::new(vec![cable.clone(), radio.clone()], steps);
    let mut p = HidProvider::new("atk_nearlink");
    let clock = FakeClock::default();
    assert_eq!(poll(&mut p, &first, &clock)[0].level, Some(97));
    first.done();
    let next = FakeHid::new(vec![cable, radio], nearlink(5, 3, 90, false))
        .fail_open("cable", "temporarily unavailable");
    let rows = poll(&mut p, &next, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].connection, Connection::Online);
    assert_eq!(rows[0].level, Some(90));
    assert_eq!(rows[0].charging, Some(false));
    next.done();
}
#[test]
fn sleep_cache_keeps_the_last_published_reading_instead_of_an_unselected_alias() {
    let (cable, radio) = nearlink_endpoints();
    let mut steps = nearlink(4, 1, 97, true);
    steps.extend(nearlink(5, 2, 91, false));
    let first = FakeHid::new(vec![cable, radio.clone()], steps);
    let mut p = HidProvider::new("atk_nearlink");
    let clock = FakeClock::default();
    let shown = poll(&mut p, &first, &clock).remove(0);
    first.done();
    let next = FakeHid::new(vec![radio], silent_nearlink(5, 3));
    let cached = poll(&mut p, &next, &clock).remove(0);
    assert_eq!(cached.level, shown.level);
    assert_eq!(cached.timestamp, shown.timestamp);
    assert_eq!(cached.connection, Connection::Sleeping);
    next.done();
}
fn turbo(level: u8) -> Vec<Step> {
    vec![
        Step::Write(padded(&[0x55, 0xaa, 1, 0, 1, 0, 1, 0x0b, 0], 64)),
        Step::Read(Ok(vec![0x55, 0xaa, 1, 0, 3, 0, 1, 0x0b, 1, level, 0])),
    ]
}
#[test]
fn proven_control_collection_is_remembered_separately_for_each_transport() {
    let mut infos = vec![];
    for (pid, prefix) in [(0x6008, "receiver"), (0x600a, "cable")] {
        infos.push(endpoint(
            0x3837,
            pid,
            0xff01,
            1,
            &format!("{prefix}-0-unavailable"),
        ));
        infos.push(endpoint(
            0x3837,
            pid,
            0xff02,
            1,
            &format!("{prefix}-1-status"),
        ));
    }
    let mut steps = turbo(60);
    steps.extend(turbo(60));
    let first = FakeHid::new(infos.clone(), steps.clone())
        .fail_open("receiver-0-unavailable", "denied")
        .fail_open("cable-0-unavailable", "denied");
    let mut p = HidProvider::new("mchose");
    let clock = FakeClock::default();
    assert_eq!(poll(&mut p, &first, &clock).len(), 1);
    first.done();
    let next = FakeHid::new(infos, steps)
        .fail_open("receiver-0-unavailable", "denied")
        .fail_open("cable-0-unavailable", "denied");
    assert_eq!(poll(&mut p, &next, &clock).len(), 1);
    assert_eq!(
        *next.opened.lock().unwrap(),
        ["receiver-1-status", "cable-1-status"]
    );
    next.done();
}
#[test]
fn present_receiver_retains_its_icon_when_only_standard_collections_are_available() {
    let control = endpoint(0x3554, 0xf5d5, 0xff02, 2, "control");
    let frame = vec![8, 4, 0, 0, 0, 2, 90, 0, 0x10, 0xad, 0, 0, 0, 0, 0, 0, 0x30];
    let first = FakeHid::new(
        vec![control],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(pulsar_request()),
            Step::Read(Ok(frame)),
        ],
    );
    let mut p = HidProvider::new("pulsar");
    let clock = FakeClock::default();
    let shown = poll(&mut p, &first, &clock).remove(0);
    first.done();
    let next = FakeHid::new(
        vec![endpoint(0x3554, 0xf5d5, 1, 2, "standard-mouse")],
        vec![],
    );
    let rows = poll(&mut p, &next, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, shown.key);
    assert_eq!(rows[0].level, shown.level);
    assert_eq!(rows[0].timestamp, shown.timestamp);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert!(next.opened.lock().unwrap().is_empty());
}
#[test]
fn gwolves_unknown_caps_do_not_drop_a_previously_proven_compx_device() {
    let mut control = endpoint(0x33e4, 0x3854, 0xff02, 2, "control");
    control.output_length = Some(17);
    let frame = vec![8, 4, 0, 0, 0, 2, 80, 0, 0x0f, 0xad, 0, 0, 0, 0, 0, 0, 0x3b];
    let first = FakeHid::new(
        vec![control.clone()],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(pulsar_request()),
            Step::Read(Ok(frame)),
        ],
    );
    let mut p = HidProvider::new("gwolves");
    let clock = FakeClock::default();
    let shown = poll(&mut p, &first, &clock).remove(0);
    first.done();
    control.output_length = None;
    let next = FakeHid::new(vec![control], vec![]);
    let rows = poll(&mut p, &next, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, shown.level);
    assert_eq!(rows[0].connection, Connection::Sleeping);
    assert!(next.opened.lock().unwrap().is_empty());
}

#[test]
fn metadata_recovery_never_transfers_cached_battery_to_a_replacement_device() {
    let (_, radio) = nearlink_endpoints();
    let first = FakeHid::new(vec![radio.clone()], nearlink(5, 1, 90, false));
    let mut p = HidProvider::new("atk_nearlink");
    let clock = FakeClock::default();
    assert_eq!(poll(&mut p, &first, &clock).len(), 1);
    first.done();
    let mut replacement = radio;
    replacement.serial = "SYNTHETIC-DIFFERENT-PHYSICAL-DEVICE".into();
    replacement.usage_page = 1;
    replacement.usage = 2;
    let next = FakeHid::new(vec![replacement], vec![]);
    assert!(poll(&mut p, &next, &clock).is_empty());
    assert!(next.opened.lock().unwrap().is_empty());
    let disconnected = FakeHid::new(vec![], vec![]);
    assert!(poll(&mut p, &disconnected, &clock).is_empty());
}

#[test]
fn metadata_only_recovery_does_not_extend_the_existing_sleep_cache_lifetime() {
    let control = endpoint(0x3837, 0x6008, 0xff01, 1, "receiver");
    let first = FakeHid::new(vec![control.clone()], turbo(60));
    let mut p = HidProvider::new("mchose");
    let clock = FakeClock::default();
    let shown = poll(&mut p, &first, &clock).remove(0);
    first.done();
    let mut standard = control;
    standard.usage_page = 1;
    standard.usage = 2;
    let next = FakeHid::new(vec![standard], vec![]);
    clock.sleep(std::time::Duration::from_secs(299));
    let cached = poll(&mut p, &next, &clock).remove(0);
    assert_eq!(cached.timestamp, shown.timestamp);
    assert_eq!(cached.connection, Connection::Sleeping);
    clock.sleep(std::time::Duration::from_secs(1));
    assert!(poll(&mut p, &next, &clock).is_empty());
    assert!(next.opened.lock().unwrap().is_empty());
}

#[test]
fn returning_legacy_gwolves_control_collection_takes_over_without_a_duplicate() {
    let mut control = endpoint(0x33e4, 0x3854, 0xff02, 2, "compx");
    control.output_length = Some(17);
    let first = FakeHid::new(
        vec![control.clone()],
        vec![
            Step::Read(Ok(vec![])),
            Step::Write(pulsar_request()),
            Step::Read(Ok(vec![
                8, 4, 0, 0, 0, 2, 80, 0, 0x0f, 0xad, 0, 0, 0, 0, 0, 0, 0x3b,
            ])),
        ],
    );
    let mut p = HidProvider::new("gwolves");
    let clock = FakeClock::default();
    assert_eq!(poll(&mut p, &first, &clock).len(), 1);
    first.done();
    let mut legacy = control.clone();
    legacy.path = "legacy".into();
    legacy.feature_length = Some(65);
    let next = FakeHid::new(
        vec![control, legacy],
        vec![
            Step::Send(padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
            Step::Feature(0, 65, Ok(vec![0, 0xa1, 0, 2, 2, 0, 0x83, 0, 79])),
        ],
    );
    let rows = poll(&mut p, &next, &clock);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, Some(79));
    assert_eq!(*next.opened.lock().unwrap(), ["legacy"]);
    next.done();
}

#[derive(Clone, Copy, PartialEq)]
enum CancelAt {
    Open,
    Drain,
    Input,
}
struct CancelHid {
    base: FakeHid,
    cancel: Arc<AtomicBool>,
    at: CancelAt,
    after: usize,
}
struct CancelSession {
    inner: Box<dyn HidSession>,
    cancel: Arc<AtomicBool>,
    at: CancelAt,
    after: usize,
    seen: usize,
}
impl CancelSession {
    fn trigger(&mut self, at: CancelAt) {
        if self.at == at {
            self.seen += 1;
            if self.seen == self.after {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
}
impl HidSession for CancelSession {
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.inner.write(data)
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.inner.send_feature(data)
    }
    fn feature(&mut self, id: u8, len: usize) -> Result<Vec<u8>, ProviderError> {
        self.inner.feature(id, len)
    }
    fn read(&mut self, len: usize, timeout: std::time::Duration) -> Result<Vec<u8>, ProviderError> {
        let r = self.inner.read(len, timeout);
        self.trigger(CancelAt::Drain);
        r
    }
    fn input_report(&mut self, id: u8, len: usize) -> Result<Vec<u8>, ProviderError> {
        let r = self.inner.input_report(id, len);
        self.trigger(CancelAt::Input);
        r
    }
}
impl HidTransport for CancelHid {
    fn enumerate(&self, vid: u16) -> Result<Vec<HidInfo>, ProviderError> {
        self.base.enumerate(vid)
    }
    fn open(&self, i: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        let inner = self.base.open(i)?;
        if self.at == CancelAt::Open {
            self.cancel.store(true, Ordering::Relaxed);
        }
        Ok(Box::new(CancelSession {
            inner,
            cancel: self.cancel.clone(),
            at: self.at,
            after: self.after,
            seen: 0,
        }))
    }
}
#[test]
fn cancellation_during_open_does_not_send_a_battery_request() {
    for (source, vid, pid, page, usage) in [
        ("pulsar", 0x3554, 0xf5d5, 0xff02, 2),
        ("atk_nearlink", 0x373b, 0x108c, 0xffb3, 1),
        ("aula", 0x0c45, 0xfefc, 0xff80, 1),
        ("skullcandy", 0x34f0, 0x3210, 0xff13, 1),
    ] {
        let cancel = Arc::new(AtomicBool::new(false));
        let h = CancelHid {
            base: FakeHid::new(vec![endpoint(vid, pid, page, usage, "control")], vec![]),
            cancel: cancel.clone(),
            at: CancelAt::Open,
            after: 1,
        };
        assert!(
            HidProvider::new(source)
                .poll(&h, &context(&FakeClock::default(), &cancel))
                .unwrap()
                .is_empty()
        );
        h.base.done();
    }
}
#[test]
fn cancellation_during_drain_or_prepare_stops_before_the_next_request() {
    for (source, vid, pid, page, usage, at, after, steps) in [
        (
            "pulsar",
            0x3554,
            0xf5d5,
            0xff02,
            2,
            CancelAt::Drain,
            1,
            vec![Step::Read(Ok(vec![]))],
        ),
        (
            "atk_nearlink",
            0x373b,
            0x108c,
            0xffb3,
            1,
            CancelAt::Drain,
            1,
            vec![Step::Read(Ok(vec![]))],
        ),
        (
            "hyperx",
            0x0951,
            0x1718,
            0xff13,
            1,
            CancelAt::Input,
            1,
            vec![Step::Input(6, 62, Ok(vec![]))],
        ),
        (
            "skullcandy",
            0x34f0,
            0x3210,
            0xff13,
            1,
            CancelAt::Input,
            1,
            vec![Step::Input(7, 62, Ok(vec![]))],
        ),
        (
            "turtlebeach",
            0x10f5,
            0x2251,
            0xff13,
            1,
            CancelAt::Input,
            5,
            vec![Step::Input(7, 62, Ok(vec![])); 5],
        ),
    ] {
        let cancel = Arc::new(AtomicBool::new(false));
        let h = CancelHid {
            base: FakeHid::new(vec![endpoint(vid, pid, page, usage, "control")], steps),
            cancel: cancel.clone(),
            at,
            after,
        };
        assert!(
            HidProvider::new(source)
                .poll(&h, &context(&FakeClock::default(), &cancel))
                .unwrap()
                .is_empty()
        );
        h.base.done();
    }
}
#[test]
fn shield_usb_reading_wins_even_when_bluetooth_arrives_last_with_charging_set() {
    let mut usb = endpoint(0x0955, 0x7214, 1, 5, "usb");
    usb.serial = "SYNTHETIC-A-USB".into();
    let mut bt = endpoint(0x0955, 0x7214, 1, 5, "hid#vid&0955#bluetooth");
    bt.serial = "SYNTHETIC-Z-BLUETOOTH".into();
    let mut steps = vec![];
    for (level, state) in [(81, 1), (80, 2)] {
        for cmd in [7, 0x3a, 0x10] {
            let mut r = padded(&[3, cmd, 0], 33);
            match cmd {
                7 => r[14] = level,
                0x3a => r[3..6].copy_from_slice(&[1, 1, state]),
                _ => r[5] = 1,
            };
            steps.extend([Step::Write(padded(&[4, cmd, 0], 33)), Step::Read(Ok(r))]);
        }
    }
    let h = FakeHid::new(vec![usb, bt], steps);
    let rows = poll(&mut HidProvider::new("shield"), &h, &FakeClock::default());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].via, "usb");
    assert_eq!(rows[0].level, Some(81));
    assert_eq!(rows[0].charging, Some(false));
    h.done();
}
