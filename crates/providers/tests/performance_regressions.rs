#[path = "../../core/tests/support/allocation_counter.rs"]
mod allocation_counter;
use allocation_counter::{TrackingAllocator, measure};
use hb_core::*;
use hb_providers::sleep_controls::{MODELS, model};

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

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
