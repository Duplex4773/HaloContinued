use hb_core::*;
use hb_providers::{configuration::*, controls::HidDeviceController};
use std::{sync::atomic::AtomicBool, time::Duration};
struct Clock;
impl hb_core::Clock for Clock {
    fn unix(&self) -> i64 {
        0
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) {}
}
struct Transport(Vec<HidInfo>);
impl HidTransport for Transport {
    fn enumerate(&self, v: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .0
            .iter()
            .filter(|i| i.vendor_id == v)
            .cloned()
            .collect())
    }
    fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        panic!("passive discovery/unavailable controls must never open")
    }
}
fn info(vendor: u16, pid: u16) -> HidInfo {
    HidInfo {
        vendor_id: vendor,
        product_id: pid,
        path: "synthetic-usb-path".into(),
        serial: "SYNTHETIC-KEYBOARD".into(),
        container: Some("SYNTHETIC-CONTAINER".into()),
        interface: 3,
        feature_length: Some(91),
        ..Default::default()
    }
}
fn context<'a>(clock: &'a Clock, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: Duration::from_secs(1),
        playstation_full_mode: false,
    }
}
#[test]
fn passive_razer_exact_allowlist_and_stable_identity_without_battery_readings() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    for (pid, name) in [
        (0x026b, "Razer Huntsman V2 Tenkeyless"),
        (0x026c, "Razer Huntsman V2"),
        (0x0287, "Razer BlackWidow V4"),
        (0x028d, "Razer BlackWidow V4 Pro"),
        (0x02a5, "Razer BlackWidow V4 75%"),
    ] {
        let d = discover_keyboards(&Transport(vec![info(0x1532, pid)]), &ctx).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, name);
        assert_eq!(d[0].key, format!("razer:{pid:04x}:SYNTHETIC-KEYBOARD"));
        assert_eq!(d[0].kind, "keyboard");
        assert_eq!(d[0].via, "usb");
        assert!(d[0].online());
        assert_eq!(d[0].capability, PollingCapability::ReadWrite);
    }
    let d = discover_keyboards(
        &Transport(vec![
            info(0x1532, 0x00be),
            info(0x1532, 0x02a6),
            info(0x1b1c, 0x1bc4),
            info(0x1b1c, 0x1bc6),
        ]),
        &ctx,
    )
    .unwrap();
    assert!(d.is_empty());
}
#[test]
fn corsair_every_approved_pid_is_visible_but_never_sends_configuration() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    for pid in [
        0x1bb3, 0x1bd4, 0x1b73, 0x1bb9, 0x1b7c, 0x1b7d, 0x1bc5, 0x1baf, 0x1bc3, 0x1bcf, 0x1bd7,
        0x1bc0, 0x2b14,
    ] {
        let hid = Transport(vec![info(0x1b1c, pid)]);
        let d = discover_keyboards(&hid, &ctx).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].capability,
            PollingCapability::Unavailable(CORSAIR_UNAVAILABLE.into())
        );
        struct NoIo;
        impl HidTransport for NoIo {
            fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
                panic!("unavailable must not enumerate")
            }
            fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
                panic!("unavailable must not open")
            }
        }
        for action in [
            ControlAction::Read,
            ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
        ] {
            let request = ControlRequest {
                request: 1,
                target: ControlTarget {
                    device: d[0].clone(),
                    generation: 0,
                },
                action,
            };
            let result = HidDeviceController.execute(&request, &NoIo, &ctx);
            assert_eq!(result.failure.as_deref(), Some(CORSAIR_UNAVAILABLE));
            assert!(!result.may_have_changed);
        }
    }
}
#[test]
fn missing_negative_duplicate_and_conflicting_identity_collections_are_unavailable() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    for (interface, feature_length) in [(0, Some(91)), (3, None), (3, Some(90)), (4, Some(91))] {
        let mut i = info(0x1532, 0x026b);
        i.interface = interface;
        i.feature_length = feature_length;
        let d = discover_keyboards(&Transport(vec![i]), &ctx).unwrap();
        assert_eq!(d.len(), 1);
        assert!(matches!(d[0].capability, PollingCapability::Unavailable(_)));
    }
    let original = info(0x1532, 0x026b);
    let duplicate = HidInfo {
        path: "second-synthetic-path".into(),
        ..original.clone()
    };
    let d = discover_keyboards(&Transport(vec![original.clone(), duplicate]), &ctx).unwrap();
    assert_eq!(d.len(), 1);
    assert!(matches!(d[0].capability, PollingCapability::Unavailable(_)));
    let other = HidInfo {
        container: Some("DIFFERENT-CONTAINER".into()),
        path: "second-synthetic-path".into(),
        ..original.clone()
    };
    let d = discover_keyboards(&Transport(vec![original.clone(), other.clone()]), &ctx).unwrap();
    let reversed = discover_keyboards(&Transport(vec![other, original]), &ctx).unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d, reversed);
    assert_ne!(d[0].key, d[1].key);
    assert!(d.iter().all(|d| d.key.contains(":physical:")
        && matches!(d.capability, PollingCapability::Unavailable(_))));
    struct NoIo;
    impl HidTransport for NoIo {
        fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
            panic!("unavailable collision must never enumerate")
        }
        fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            panic!("unavailable collision must never open")
        }
    }
    for device in d {
        for action in [
            ControlAction::Read,
            ControlAction::Apply(PollingRate::try_from(8000).unwrap()),
        ] {
            let request = ControlRequest {
                request: 1,
                target: ControlTarget {
                    device: device.clone(),
                    generation: 0,
                },
                action,
            };
            let result = HidDeviceController.execute(&request, &NoIo, &ctx);
            assert!(result.failure.is_some());
            assert!(!result.may_have_changed);
        }
    }
    // Normal non-control collections for the same physical device do not cause ambiguity.
    let other = HidInfo {
        interface: 0,
        feature_length: Some(0),
        ..info(0x1532, 0x026b)
    };
    let d = discover_keyboards(&Transport(vec![other, info(0x1532, 0x026b)]), &ctx).unwrap();
    assert_eq!(d[0].capability, PollingCapability::ReadWrite);
}
#[test]
fn placeholders_fall_back_to_container_and_paths_do_not_merge_unrelated_devices() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    let mut a = info(0x1532, 0x026b);
    a.serial = "0000".into();
    let mut b = a.clone();
    b.container = Some("SECOND-CONTAINER".into());
    let d = discover_keyboards(&Transport(vec![a, b]), &ctx).unwrap();
    assert_eq!(d.len(), 2);
    assert_ne!(d[0].key, d[1].key);
    let mut a = info(0x1532, 0x026b);
    a.serial = "UNKNOWN".into();
    a.container = None;
    let mut b = a.clone();
    b.path = "another-physical-path".into();
    let d = discover_keyboards(&Transport(vec![a, b]), &ctx).unwrap();
    assert_eq!(d.len(), 2);
    assert_ne!(d[0].key, d[1].key);
}
#[test]
fn discovery_has_bounded_inventory_and_obeys_cancellation() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    assert!(discover_keyboards(&Transport(vec![info(0x1532, 0x026b); 513]), &ctx).is_err());
    let cancel = AtomicBool::new(true);
    assert!(discover_keyboards(&Transport(vec![]), &context(&clock, &cancel)).is_err());
    let d = HidInfo {
        path: "bluetooth-vid&1234".into(),
        ..info(0x1532, 0x026b)
    };
    assert!(
        discover_keyboards(&Transport(vec![d]), &ctx)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn colliding_valid_serial_without_container_keeps_separate_stable_path_keys() {
    let clock = Clock;
    let cancel = AtomicBool::new(false);
    let ctx = context(&clock, &cancel);
    let mut a = info(0x1532, 0x026b);
    a.container = None;
    a.path = "first-physical-path".into();
    let mut b = a.clone();
    b.path = "second-physical-path".into();
    let d = discover_keyboards(&Transport(vec![a.clone(), b.clone()]), &ctx).unwrap();
    let reversed = discover_keyboards(&Transport(vec![b, a]), &ctx).unwrap();
    assert_eq!(d.len(), 2);
    assert_eq!(d, reversed);
    assert_ne!(d[0].key, d[1].key);
    assert!(
        d.iter()
            .all(|d| matches!(d.capability, PollingCapability::Unavailable(_)))
    );
}
