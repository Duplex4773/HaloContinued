use hb_core::*;
use hb_providers::{HidProvider, protocols};
use std::sync::atomic::AtomicBool;
mod common;
use common::*;

fn read(provider: &mut HidProvider, hid: &FakeHid) -> PollResult {
    provider.poll(
        hid,
        &context(&FakeClock::default(), &AtomicBool::new(false)),
    )
}
fn headset(unit: &str) -> HidInfo {
    let mut info = info(0x046d, 0x0ac4, 0xff43);
    info.usage = 0x202;
    info.path.push_str(unit);
    info.container = Some(unit.into());
    info
}
fn battery_steps() -> Vec<Step> {
    vec![
        Step::Read(Ok(vec![])),
        Step::Write(protocols::padded(&[0x11, 0xff, 5, 0x0d], 20)),
        Step::Read(Ok(vec![0x10, 0xff, 5, 0x0d, 0x0f, 0xe5, 1])),
    ]
}
#[test]
fn removed_receiver_does_not_borrow_another_receivers_presence() {
    let mut provider = HidProvider::new("logitech");
    let first = read(
        &mut provider,
        &FakeHid::new(vec![headset("A")], battery_steps()),
    )
    .unwrap();
    let second = read(
        &mut provider,
        &FakeHid::new(vec![headset("B")], battery_steps()),
    )
    .unwrap();
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].key, second[0].key);
}
#[test]
fn access_failure_is_not_successful_empty_discovery() {
    let device = headset("A");
    let hid = FakeHid::new(vec![device.clone()], vec![]).fail_open(device.path, "access denied");
    assert!(read(&mut HidProvider::new("logitech"), &hid).is_err());
}
#[test]
fn access_failure_does_not_hide_behind_a_cached_sleeping_reading() {
    let mut provider = HidProvider::new("logitech");
    let device = headset("A");
    read(
        &mut provider,
        &FakeHid::new(vec![device.clone()], battery_steps()),
    )
    .unwrap();
    let hid = FakeHid::new(vec![device.clone()], vec![]).fail_open(device.path, "access denied");
    assert!(read(&mut provider, &hid).is_err());
}

#[test]
fn serial_discovery_failure_does_not_poison_sony_hid_inventory() {
    struct SeparateDiscovery;
    impl HidTransport for SeparateDiscovery {
        fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
            Ok(vec![])
        }
        fn enumerate_serial(&self, _: u16) -> Result<HidSnapshot, ProviderError> {
            Err(ProviderError::new("COM discovery unavailable"))
        }
        fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            panic!("empty inventory must not open a device")
        }
    }
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let context = context(&clock, &cancel);
    assert!(
        HidProvider::new("sony_inzone")
            .poll(&SeparateDiscovery, &context)
            .is_err()
    );
    assert!(
        HidProvider::new("playstation")
            .poll(&SeparateDiscovery, &context)
            .unwrap()
            .is_empty()
    );
}
#[test]
fn unknown_charge_is_preserved_and_voltage_levels_are_approximate() {
    let mut device = info(0x03f0, 0x05b7, 0xff13);
    device.interface = 3;
    let hid = FakeHid::new(
        vec![device],
        vec![
            Step::Write(protocols::padded(&[0x66, 0x89], 62)),
            Step::Read(Ok(vec![0x66, 0x0d, 1, 0, 19])),
            Step::Write(protocols::padded(&[0x66, 0x8a], 62)),
            Step::Read(Ok(vec![0x66, 0x0c, 99])),
        ],
    );
    let rows = read(&mut HidProvider::new("hyperx_cloud3"), &hid).unwrap();
    assert_eq!(rows[0].charging, None);
    hid.done();
    for feature in [0x1001, 0x1f20] {
        assert!(
            protocols::logitech(feature, &[0x0e, 0xdb, 1])
                .unwrap()
                .coarse
        );
    }
}
#[test]
fn matching_glorious_cable_overrides_receiver_charge_evidence() {
    let mut receiver = info(0x258a, 0x2022, 0xffa0);
    receiver.feature_length = Some(65);
    receiver.serial = "UNIT-001".into();
    let mut cable = receiver.clone();
    cable.product_id = 0x2011;
    cable.path.push_str("-cable");
    let hid = FakeHid::new(
        vec![receiver, cable],
        vec![
            Step::Send(protocols::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)),
            Step::Feature(0, 65, Ok(vec![0, 0xa1, 0, 0, 0, 0, 0x83, 0, 72])),
        ],
    );
    let rows = read(&mut HidProvider::new("glorious"), &hid).unwrap();
    assert_eq!(rows[0].charging, Some(true));
    assert!(rows[0].charging_inferred);
    assert_eq!(rows[0].via, "usb");
    hid.done();
}

fn glorious_devices() -> (HidInfo, HidInfo) {
    let mut receiver = info(0x258a, 0x2022, 0xffa0);
    receiver.feature_length = Some(65);
    receiver.serial = "TEST-UNIT-ONE".into();
    let mut cable = receiver.clone();
    cable.product_id = 0x2011;
    cable.path.push_str("-cable");
    (receiver, cable)
}
fn glorious_steps(reply: bool) -> Vec<Step> {
    let request = Step::Send(protocols::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65));
    if reply {
        vec![
            request,
            Step::Feature(0, 65, Ok(vec![0, 0xa1, 0, 0, 0, 0, 0x83, 0, 72])),
        ]
    } else {
        vec![
            request.clone(),
            Step::Feature(0, 65, Ok(vec![0; 65])),
            request,
            Step::Feature(0, 65, Ok(vec![0; 65])),
        ]
    }
}
#[test]
fn removing_glorious_cable_does_not_retain_inferred_charging() {
    use std::sync::atomic::Ordering;
    let (receiver, cable) = glorious_devices();
    let mut provider = HidProvider::new("glorious");
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    for (timestamp, wired) in [(100, true), (200, false)] {
        clock.0.store(timestamp * 1000, Ordering::Relaxed);
        let infos = if wired {
            vec![receiver.clone(), cable.clone()]
        } else {
            vec![receiver.clone()]
        };
        let hid = FakeHid::new(infos, glorious_steps(wired));
        let rows = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].charging, Some(wired));
        assert_eq!(rows[0].charging_inferred, wired);
        assert_eq!(rows[0].via, if wired { "usb" } else { "wireless" });
        assert_eq!(
            rows[0].connection,
            if wired {
                Connection::Online
            } else {
                Connection::Sleeping
            }
        );
        assert_eq!(rows[0].level, Some(72));
        assert_eq!(rows[0].timestamp, 100);
        hid.done();
    }
}
#[test]
fn glorious_cable_only_evidence_does_not_refresh_a_cached_battery_level() {
    use std::sync::atomic::Ordering;
    let (receiver, cable) = glorious_devices();
    let mut provider = HidProvider::new("glorious");
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    clock.0.store(100_000, Ordering::Relaxed);
    let hid = FakeHid::new(vec![receiver.clone()], glorious_steps(true));
    let initial = provider
        .poll(&hid, &context(&clock, &cancel))
        .unwrap()
        .remove(0);
    hid.done();
    for timestamp in [200, 300] {
        clock.0.store(timestamp * 1000, Ordering::Relaxed);
        let hid = FakeHid::new(vec![receiver.clone(), cable.clone()], glorious_steps(false));
        let rows = provider.poll(&hid, &context(&clock, &cancel)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, timestamp as i64);
        assert_eq!(rows[0].level, None);
        assert_eq!(rows[0].approx, None);
        assert_eq!(rows[0].key, initial.key);
        assert_eq!(rows[0].serial, initial.serial);
        assert_eq!(rows[0].kind, initial.kind);
        assert_eq!(rows[0].charging, Some(true));
        assert!(rows[0].charging_inferred);
        assert_eq!(rows[0].via, "usb");
        assert_eq!(*hid.opened.lock().unwrap(), vec![receiver.path.clone()]);
        hid.done();
    }
    // Cable evidence must not extend the battery sample's cache lifetime.
    clock.0.store(401_000, Ordering::Relaxed);
    let hid = FakeHid::new(vec![receiver], glorious_steps(false));
    assert!(
        provider
            .poll(&hid, &context(&clock, &cancel))
            .unwrap()
            .is_empty()
    );
    hid.done();
}
