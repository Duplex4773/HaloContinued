#[path = "../../core/tests/support/allocation_counter.rs"]
mod allocation_counter;
use allocation_counter::{TrackingAllocator, measure};
use hb_core::*;
use hb_providers::sleep_controls::{MODELS, model};

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

#[path = "../src/hex.rs"]
mod hex;

#[test]
fn catalog_lookup_preserves_the_exact_allowlist_without_an_allocated_index() {
    use hb_providers::catalog::{DEVICES, find_device};
    let key = |d: &hb_providers::catalog::Device| (d.provider, d.vid, d.pid);
    assert!(
        DEVICES.windows(2).all(|w| key(&w[0]) < key(&w[1])),
        "Catalog keys must remain unique and sorted by provider, vendor and product"
    );
    let (_, allocations) = measure(|| {
        for d in DEVICES {
            for provider in [d.provider, "", "unsupported-provider"] {
                for vid in [d.vid, d.vid.wrapping_sub(1), d.vid.wrapping_add(1)] {
                    for pid in [d.pid, d.pid.wrapping_sub(1), d.pid.wrapping_add(1)] {
                        let expected = DEVICES
                            .iter()
                            .find(|entry| key(entry) == (provider, vid, pid));
                        let actual = find_device(provider, vid, pid);
                        let fields = |d: &hb_providers::catalog::Device| {
                            (d.provider, d.vid, d.pid, d.name, d.variant, d.parameter)
                        };
                        assert_eq!(actual.map(fields), expected.map(fields));
                    }
                }
            }
        }
    });
    assert_eq!(allocations.calls, 0);
    assert_eq!(allocations.bytes, 0);
}

#[test]
fn hexadecimal_identity_and_diagnostics_preserve_every_byte_with_one_allocation() {
    let bytes: Vec<u8> = (0..=255).collect();
    for length in [0, 1, 4, 14, 32, 256] {
        for uppercase in [false, true] {
            for spaced in [false, true] {
                let source = &bytes[..length];
                let (expected, before) = measure(|| {
                    source
                        .iter()
                        .map(|b| {
                            if uppercase {
                                format!("{b:02X}")
                            } else {
                                format!("{b:02x}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(if spaced { " " } else { "" })
                });
                let (actual, after) = measure(|| hex::encode(source, uppercase, spaced));
                assert_eq!(actual, expected);
                assert_eq!(after.calls, usize::from(length != 0));
                assert_eq!(after.bytes, actual.len());
                if length == 32 && !uppercase && spaced {
                    println!("32-byte diagnostic: before {before:?}; after {after:?}");
                }
            }
        }
    }
}

fn device(m: &hb_providers::sleep_controls::SleepModel) -> ConfigurationDevice {
    ConfigurationDevice {
        key: if m.source == "asus" {
            format!("asus:{}:SYNTHETIC", m.name.to_lowercase().replace(' ', "-"))
        } else {
            format!("{}:{:04x}:SYNTHETIC", m.source, m.pid)
        },
        name: m.name.into(),
        kind: "mouse".into(),
        source: m.source.into(),
        via: "usb".into(),
        connection: Connection::Online,
        serial: None,
        container: None,
        capability: PollingCapability::ReadWrite,
    }
}

#[test]
fn sleep_model_lookup_does_not_allocate_formatted_prefixes() {
    let devices: Vec<_> = MODELS.iter().map(device).collect();
    let (_, allocations) = measure(|| {
        for device in &devices {
            assert!(model(device).is_some_and(|m| m.name == device.name));
        }
    });
    assert_eq!(allocations.calls, 0, "Sleep model lookup: {allocations:?}");
    assert_eq!(allocations.bytes, 0);
}

#[test]
fn sleep_validation_matches_offered_choices_without_allocations() {
    for model in MODELS {
        let choices = model.choices();
        let (_, allocations) = measure(|| {
            for seconds in 0..=model.maximum.saturating_add(60) {
                assert_eq!(model.supports(seconds), choices.contains(&seconds));
            }
        });
        assert_eq!(allocations.calls, 0);
        assert_eq!(allocations.bytes, 0);
    }
}
