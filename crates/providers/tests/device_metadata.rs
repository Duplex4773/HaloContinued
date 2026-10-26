use hb_core::*;
use hb_providers::device_metadata::*;
fn device(key: &str, source: &str) -> ConfigurationDevice {
    let mut r = Reading::new(key, "Mouse", source, 100);
    r.kind = "mouse".into();
    r.via = "usb".into();
    ConfigurationDevice::from_reading(&r)
}
#[test]
fn connection_labels_use_reviewed_connection_metadata_without_assuming_wireless() {
    for (key, label) in [
        ("razer:0024:UNIT", "USB cable"),
        ("razer:003e:UNIT", "USB cable"),
        ("razer:00bf:UNIT", "Wireless · USB receiver"),
        ("razer:ffff:UNIT", "USB connection"),
    ] {
        assert_eq!(connection_label(&device(key, "razer")), label);
    }
    let mut d = device("bluetooth:UNIT", "bluetooth");
    d.via = "bluetooth".into();
    assert_eq!(connection_label(&d), "Bluetooth");
    d.via.clear();
    assert_eq!(connection_label(&d), "Device connection");
}
#[test]
fn documented_parent_verification_is_separate_from_settings_verification() {
    for (source, key) in [
        ("steam", "steam:1304:UNIT:0"),
        ("nzxt", "nzxt:2101:UNIT"),
        ("pulsar", "pulsar:5406:UNIT"),
        ("corsair", "corsair:0a40:UNIT"),
        ("mchose", "mchose:f013:UNIT"),
        ("pulsar", "pulsar:5403:UNIT"),
    ] {
        assert!(parent_battery_verified(&device(key, source)), "{key}");
    }
    for key in [
        "razer:ffff:UNIT",
        "razer:00bf:UNIT",
        "corsair:0a3e:UNIT",
        "mchose:f015:UNIT",
    ] {
        assert!(
            !parent_battery_verified(&device(key, key.split(':').next().unwrap())),
            "{key}"
        );
    }
    assert!(!parent_battery_verified(&device(
        "nzxt:2101:UNIT",
        "simulation"
    )));
    let mut d = device("eightbitdo:6009:UNIT", "eightbitdo");
    assert!(!parent_battery_verified(&d));
    d.via = "bluetooth".into();
    assert!(parent_battery_verified(&d));
}
