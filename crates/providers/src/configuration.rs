//! Passive configuration inventory. Enumerates HID metadata without opening handles.
use crate::{
    provider::{is_bluetooth, receiver_key, trusted_identity},
    razer_controls,
};
use hb_core::*;
use std::collections::BTreeMap;

pub const CONFIGURATION_VENDORS: &[u16] = &[0x1532, 0x1b1c, 0x1038, 0x0b05];
pub const CORSAIR_UNAVAILABLE: &str = "Polling changes unavailable: this model requires a maintained software session, which is disabled by design.";
const LIMIT: usize = 512;
fn corsair_name(pid: u16) -> Option<&'static str> {
    match pid {
        0x1bb3 | 0x1bd4 => Some("Corsair K70 RGB Pro"),
        0x1b73 | 0x1bb9 => Some("Corsair K70 RGB TKL"),
        0x1b7c | 0x1b7d | 0x1bc5 => Some("Corsair K100"),
        0x1baf | 0x1bc3 | 0x1bcf => Some("Corsair K65 RGB Mini"),
        0x1bd7 => Some("Corsair K65 Pro Mini"),
        0x1bc0 => Some("Corsair K70 Max"),
        0x2b14 => Some("Corsair K70 Pro TKL"),
        _ => None,
    }
}

pub fn discover_keyboards(
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> Result<Vec<ConfigurationDevice>, ProviderError> {
    let mut groups: BTreeMap<String, Vec<HidInfo>> = BTreeMap::new();
    let mut count = 0usize;
    for vendor in [0x1532, 0x1b1c] {
        if !context.active() {
            return Err(ProviderError::new(
                "configuration discovery cancelled or deadline reached",
            ));
        }
        let devices = hid.enumerate(vendor)?;
        count = count.saturating_add(devices.len());
        if count > LIMIT {
            return Err(ProviderError::new(
                "configuration HID inventory exceeds 512 collections",
            ));
        }
        for info in devices {
            if info.vendor_id != vendor || is_bluetooth(&info.path) {
                continue;
            }
            let supported = match vendor {
                0x1532 => razer_controls::keyboard_name(info.product_id),
                0x1b1c => corsair_name(info.product_id),
                _ => None,
            };
            if supported.is_none() {
                continue;
            }
            let identity = trusted_identity(&info);
            if identity.trim().is_empty() {
                continue;
            }
            let source = if vendor == 0x1532 { "razer" } else { "corsair" };
            groups
                .entry(format!("{source}:{:04x}:{identity}", info.product_id))
                .or_default()
                .push(info);
        }
    }
    if !context.active() {
        return Err(ProviderError::new(
            "configuration discovery cancelled or deadline reached",
        ));
    }
    let mut result = Vec::new();
    for (key, infos) in groups {
        // Keep colliding serial identities visible as separate physical devices.
        // The suffix is needed only for collisions; ordinary device keys stay stable.
        let mut physical_groups: BTreeMap<String, Vec<&HidInfo>> = BTreeMap::new();
        for info in &infos {
            physical_groups
                .entry(receiver_key(info))
                .or_default()
                .push(info);
        }
        let ambiguous_identity = physical_groups.len() > 1;
        for (physical, infos) in physical_groups {
            let info = infos[0];
            let key = if ambiguous_identity {
                format!("{key}:physical:{physical}")
            } else {
                key.clone()
            };
            let razer = info.vendor_id == 0x1532;
            let controls: Vec<_> = infos
                .iter()
                .copied()
                .filter(|i| {
                    razer_controls::protocol(i) == Some(razer_controls::Protocol::KeyboardExtended)
                })
                .collect();
            let capability = if !razer {
                PollingCapability::Unavailable(CORSAIR_UNAVAILABLE.into())
            } else if ambiguous_identity || controls.len() > 1 {
                PollingCapability::Unavailable("Polling changes unavailable: multiple collections or physical devices share this identity.".into())
            } else if controls.is_empty() {
                PollingCapability::Unavailable("Polling changes unavailable: the required interface 3, 91-byte feature collection is missing.".into())
            } else {
                PollingCapability::ReadWrite
            };
            // Use the exact control metadata when available to retain serial/container guards.
            let info = controls.first().copied().unwrap_or(info);
            result.push(ConfigurationDevice {
                key,
                name: if razer {
                    razer_controls::keyboard_name(info.product_id)
                } else {
                    corsair_name(info.product_id)
                }
                .unwrap()
                .into(),
                kind: "keyboard".into(),
                source: if razer { "razer" } else { "corsair" }.into(),
                via: "usb".into(),
                connection: Connection::Online,
                serial: if info.serial.trim().is_empty() {
                    None
                } else {
                    Some(info.serial.clone())
                },
                container: info.container.clone(),
                capability,
            });
        }
    }
    Ok(result)
}
