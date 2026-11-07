//! Readable metadata only. This module never authorizes hardware commands.
use hb_core::ConfigurationDevice;

pub fn connection_label(device: &ConfigurationDevice) -> &'static str {
    connection_label_parts(&device.key, &device.kind, &device.source, &device.via)
}
/// Display-only variant for borrowed dashboard metadata.
pub fn connection_label_parts(key: &str, kind: &str, source: &str, via: &str) -> &'static str {
    if via.eq_ignore_ascii_case("bluetooth") || via.eq_ignore_ascii_case("ble") {
        return "Bluetooth";
    }
    let pid = key
        .strip_prefix("razer:")
        .and_then(|s| s.split(':').next())
        .and_then(|p| u16::from_str_radix(p, 16).ok());
    let model = pid.filter(|_| source == "razer").and_then(|pid| {
        crate::sleep_controls::MODELS
            .iter()
            .find(|m| m.source == "razer" && m.pid == pid)
    });
    if hb_core::controls::wired_mouse(key, kind, source, via)
        || model.is_some_and(|m| m.name.contains("(Wired)"))
    {
        "USB cable"
    } else if via == "wireless"
        || model.is_some_and(|m| m.name.contains("(Wireless)") || m.name.contains("(Receiver)"))
        || source == "razer"
            && key.split(':').nth(1).is_some_and(|p| {
                matches!(
                    p,
                    "009f"
                        | "00a6"
                        | "00a8"
                        | "00ab"
                        | "00b0"
                        | "00b7"
                        | "00bf"
                        | "00c1"
                        | "00c3"
                        | "00cd"
                )
            })
    {
        "Wireless · USB receiver"
    } else if via == "usb" {
        "USB connection"
    } else {
        "Device connection"
    }
}
pub fn parent_battery_verified(device: &ConfigurationDevice) -> bool {
    if device.source == "eightbitdo" && device.key.starts_with("eightbitdo:6009:") {
        return device.via == "bluetooth";
    }
    PARENT_PREFIXES.iter().any(|p| {
        device.key.starts_with(p) && device.key.split(':').next() == Some(device.source.as_str())
    }) || device.source == "asus" && device.key.starts_with("asus:omni:")
        || device.source == "shield" && device.key.starts_with("shield:board:")
        || device.source == "xinput" && device.name == "FlyDigi Vader 5 Pro"
}
// Exact connection prefixes from the documented parent hardware confirmations.
const PARENT_PREFIXES: &[&str] = &[
    "ajazz:5c2f:",
    "asus:18d6:",
    "asus:1ace:",
    "asus:1c5a:",
    "atk_keyboard:5088:",
    "atk_nearlink:108c:",
    "aula:22a0:",
    "aula:fefc:",
    "corsair:0a40:",
    "corsair:0a6b:",
    "corsair:1b7f:",
    "corsair:2b07:",
    "dareu:1074:",
    "eweadn:062f:",
    "finalmouse:0100:",
    "hyperx:1718:",
    "hyperx_cloud3:0795:",
    "hyperx_cloud3:0995:",
    "inphic:fa60:",
    "inphic:fa65:",
    "jabra:24c7:",
    "lamzu:f50f:",
    "lamzu:f510:",
    "lofree:024f:",
    "mchose:2001:",
    "mchose:385d:",
    "mchose:6008:",
    "mchose:f013:",
    "monsgeek:5026:",
    "nzxt:2101:",
    "nzxt:2129:",
    "pulsar:0300:",
    "pulsar:10c9:",
    "pulsar:11d9:",
    "pulsar:1216:",
    "pulsar:3414:",
    "pulsar:5403:",
    "pulsar:5406:",
    "pulsar:f505:",
    "pulsar:f55e:",
    "pulsar:f5d5:",
    "pulsar:f5dc:",
    "pulsar:f5de:",
    "pulsar:f5df:",
    "pulsar:f5f6:",
    "pulsar:f5f7:",
    "rapoo:1413:",
    "rapoo:1924:",
    "razer:00e5:",
    "razer:00e6:",
    "razer:027b:",
    "razer:0528:",
    "razer:057a:",
    "razer:0583:",
    "skullcandy:3210:",
    "skullcandy:5310:",
    "steam:1304:",
    "steam:1305:",
    "steelseries:2317:",
    "turtlebeach:2251:",
    "turtlebeach:229b:",
];
