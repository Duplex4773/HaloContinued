//! One-shot hardware sleep timers. No host idle detection or automatic enforcement.
use crate::{
    controls::GuardedSession,
    provider::{is_bluetooth, receiver_key, trusted_identity},
};
use hb_core::*;
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, Debug)]
pub enum Wire {
    Razer { tid: u8, wait_us: u64 },
    SteelSeries { wireless: bool },
    Rival650,
    Asus,
}
#[derive(Clone, Copy, Debug)]
pub struct SleepModel {
    pub vendor: u16,
    pub pid: u16,
    pub source: &'static str,
    pub name: &'static str,
    pub interface: i32,
    pub minimum: u16,
    pub maximum: u16,
    pub wire: Wire,
}
pub use crate::sleep_catalog::MODELS;
fn prefix(m: &SleepModel) -> String {
    if m.source == "asus" {
        format!("asus:{}:", m.name.to_lowercase().replace(' ', "-"))
    } else {
        format!("{}:{:04x}:", m.source, m.pid)
    }
}
impl SleepModel {
    pub fn choices(&self) -> Vec<u16> {
        if matches!(self.wire, Wire::Asus) {
            vec![0, 60, 120, 180, 300, 600]
        } else {
            (self.minimum..=self.maximum).step_by(60).collect()
        }
    }
    pub fn readable(&self) -> bool {
        matches!(self.wire, Wire::Razer { .. } | Wire::Asus)
    }
}
pub fn model(device: &ConfigurationDevice) -> Option<&'static SleepModel> {
    if device.kind != "mouse" || device.via != "usb" {
        return None;
    }
    MODELS.iter().find(|m| {
        device.source == m.source
            && device
                .key
                .strip_prefix(&prefix(m))
                .is_some_and(|s| !s.is_empty() && !s.contains(":physical:"))
    })
}
fn collection(m: &SleepModel, i: &HidInfo) -> bool {
    i.vendor_id == m.vendor
        && i.product_id == m.pid
        && i.interface == m.interface
        && !is_bluetooth(&i.path)
        && match m.wire {
            Wire::Razer { .. } => i.feature_length == Some(91),
            Wire::Asus => i.usage_page >= 0xff00 && i.output_length == Some(65),
            _ => i.usage_page >= 0xff00 && i.output_length.is_some_and(|n| (9..=65).contains(&n)),
        }
}
pub fn unavailable(device: &ConfigurationDevice) -> &'static str {
    match device.source.as_str() {
        "logitech" => "Sleep-time changes are not available for this Logitech mouse yet.",
        "corsair" => "Sleep-time changes are not available for this Corsair mouse yet.",
        _ => "Sleep-time changes are not available for this mouse or connection yet.",
    }
}
pub fn discover(
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> Result<Vec<ConfigurationDevice>, ProviderError> {
    let mut result = Vec::new();
    for vendor in [0x1532, 0x1038, 0x0b05] {
        if !context.active() {
            return Err(ProviderError::new("Sleep discovery cancelled"));
        }
        let infos = hid.enumerate(vendor)?;
        if infos.len() > 512 {
            return Err(ProviderError::new("Too many HID collections"));
        }
        let mut groups: BTreeMap<String, Vec<(&HidInfo, &SleepModel)>> = BTreeMap::new();
        for i in &infos {
            if let Some(m) = MODELS.iter().find(|m| collection(m, i)) {
                groups
                    .entry(format!("{}{}", prefix(m), trusted_identity(i)))
                    .or_default()
                    .push((i, m));
            }
        }
        for (key, group) in groups {
            if group.len() != 1 {
                continue;
            }
            let (i, m) = group[0];
            if infos.iter().any(|o| {
                o.product_id == i.product_id
                    && trusted_identity(o) == trusted_identity(i)
                    && receiver_key(o) != receiver_key(i)
            }) {
                continue;
            }
            result.push(ConfigurationDevice {
                key,
                name: m.name.into(),
                kind: "mouse".into(),
                source: m.source.into(),
                via: "usb".into(),
                connection: Connection::Online,
                serial: (!i.serial.trim().is_empty()).then(|| i.serial.clone()),
                container: i.container.clone(),
                capability: if crate::razer_controls::protocol(i).is_some() {
                    PollingCapability::ReadWrite
                } else {
                    PollingCapability::Unavailable(
                        "Polling controls are not supported for this model.".into(),
                    )
                },
            });
        }
    }
    Ok(result)
}
pub fn execute(
    request: &SleepRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> SleepOutcome {
    let mut out = SleepOutcome::failed(request, "Sleep setting unavailable");
    let d = &request.target.device;
    let Some(m) = model(d) else {
        return out;
    };
    let generation = hid.generation();
    out.generation = generation;
    let result = (|| -> Result<(), ProviderError> {
        if !d.online() || !context.active() || request.target.generation != generation {
            return Err(ProviderError::new(
                "Device connection changed. Refresh before trying again.",
            ));
        }
        if request.seconds.is_some_and(|s| !m.choices().contains(&s)) {
            return Err(ProviderError::new("Unsupported sleep duration"));
        }
        let infos = hid.enumerate(m.vendor)?;
        let mut matches = infos.iter().filter_map(|i| {
            MODELS
                .iter()
                .find(|candidate| {
                    collection(candidate, i)
                        && candidate.source == d.source
                        && d.key == format!("{}{}", prefix(candidate), trusted_identity(i))
                })
                .map(|candidate| (i, candidate))
        });
        let (i, m) = matches.next().ok_or_else(|| {
            ProviderError::new("Wake or reconnect the mouse and refresh Devices.")
        })?;
        if matches.next().is_some()
            || infos.iter().any(|o| {
                o.product_id == i.product_id
                    && trusted_identity(o) == trusted_identity(i)
                    && receiver_key(o) != receiver_key(i)
            })
        {
            return Err(ProviderError::new(
                "Ambiguous mouse connection; no command sent",
            ));
        }
        if d.serial
            .as_ref()
            .is_some_and(|s| !s.trim().eq_ignore_ascii_case(i.serial.trim()))
            || d.container.as_ref().is_some_and(|c| {
                i.container
                    .as_ref()
                    .is_none_or(|v| !v.eq_ignore_ascii_case(c))
            })
        {
            return Err(ProviderError::new(
                "Mouse identity changed; refresh Devices",
            ));
        }
        if !context.active() || generation != hid.generation() {
            return Err(ProviderError::new(
                "Connection changed or request cancelled",
            ));
        }
        let stamp = crate::controls::connection_stamp(&request.target, hid, i)?;
        let mut session = GuardedSession::new(
            crate::controls::open_connected(hid, i, stamp)?,
            hid,
            context,
            generation,
        );
        out.connection_stamp = stamp;
        match m.wire {
            Wire::Asus => asus_sleep(
                &mut session,
                context,
                request.seconds,
                request.expected,
                &mut out,
            )?,
            Wire::Razer { tid, wait_us } => {
                let before = razer_exchange(&mut session, context, tid, wait_us, None)?;
                if !(m.minimum..=m.maximum).contains(&before) {
                    return Err(ProviderError::new(
                        "Mouse reported an unknown sleep duration",
                    ));
                }
                if request.expected.is_some_and(|expected| expected != before) {
                    return Err(ProviderError::new(
                        "Sleep time changed elsewhere; automatic change cancelled",
                    ));
                }
                out.seconds = Some(before);
                if let Some(seconds) = request.seconds.filter(|s| *s != before) {
                    out.may_have_changed = true;
                    out.seconds = None;
                    razer_exchange(&mut session, context, tid, wait_us, Some(seconds))?;
                    let after = razer_exchange(&mut session, context, tid, wait_us, None)?;
                    if after != seconds {
                        return Err(ProviderError::new(
                            "Mouse did not confirm the requested duration. Refresh before retrying.",
                        ));
                    }
                    out.seconds = Some(after);
                }
            }
            wire => {
                let Some(seconds) = request.seconds else {
                    return Err(ProviderError::new(
                        "This model cannot report its current sleep timer.",
                    ));
                };
                let packet = steel_packet(wire, seconds, i.output_length.unwrap());
                out.may_have_changed = true;
                session.write(&packet)?;
                context.sleep(Duration::from_millis(50));
                if matches!(wire, Wire::SteelSeries { wireless: true }) {
                    let response = session.read(64, Duration::from_millis(250))?;
                    if response.is_empty() {
                        return Err(ProviderError::new(
                            "No reply from the mouse. The timer may have changed; do not retry automatically.",
                        ));
                    }
                }
                // Delivery is not readback verification. Do not claim the value is confirmed,
                // and do not issue a broad 'save all settings' flash command.
            }
        }
        if !context.active() || generation != hid.generation() {
            out.seconds = None;
            return Err(ProviderError::new(
                "Connection changed or request cancelled; setting is unknown",
            ));
        }
        Ok(())
    })();
    out.failure = result.err().map(|e| e.message);
    out
}
pub fn razer_packet(tid: u8, seconds: Option<u16>) -> [u8; 91] {
    let mut r = [0; 91];
    r[2] = tid;
    r[6] = 2;
    r[7] = 7;
    r[8] = if seconds.is_some() { 3 } else { 0x83 };
    if let Some(s) = seconds {
        r[9..11].copy_from_slice(&s.to_be_bytes());
    }
    r[89] = r[3..89].iter().fold(0, |a, b| a ^ b);
    r
}
fn razer_exchange(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    tid: u8,
    wait_us: u64,
    seconds: Option<u16>,
) -> Result<u16, ProviderError> {
    if !context.active() {
        return Err(ProviderError::new("Sleep request cancelled"));
    }
    let request = razer_packet(tid, seconds);
    session.send_feature(&request)?;
    context.sleep(Duration::from_micros(wait_us));
    if !context.active() {
        return Err(ProviderError::new("Sleep request cancelled"));
    }
    let r = session.feature(0, 91)?;
    if r.len() != 91
        || r[0] != 0
        || r[3..5] != [0, 0]
        || r[7] != 7
        || r[8] != request[8]
        || r[6] != 2
    {
        return Err(ProviderError::new(
            "Unexpected sleep response; refresh before retrying",
        ));
    }
    if r[1] != 2 {
        return Err(ProviderError::new(match r[1] {
            1 => "Mouse is busy; no automatic retry",
            5 => "Mouse does not support this sleep command",
            _ => "Mouse rejected the sleep command",
        }));
    }
    Ok(u16::from_be_bytes([r[9], r[10]]))
}
pub fn steel_packet(wire: Wire, seconds: u16, length: usize) -> Vec<u8> {
    let mut r = vec![0; length];
    match wire {
        Wire::Rival650 => {
            r[1..7].copy_from_slice(&[0x2b, 1, 1, 0, 0, 0]);
            r[7..9].copy_from_slice(&seconds.to_le_bytes());
        }
        Wire::SteelSeries { wireless } => {
            r[1] = if wireless { 0x69 } else { 0x29 };
            r[2..5].copy_from_slice(&(u32::from(seconds) * 1000).to_le_bytes()[..3]);
        }
        _ => {}
    }
    r
}

fn asus_exchange(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    bytes: &[u8],
) -> Result<Vec<u8>, ProviderError> {
    if !context.active() {
        return Err(ProviderError::new("Sleep request cancelled"));
    }
    // Discard stale input before sending a new command, with a strict bound.
    for n in 0..8 {
        if session.read(65, Duration::ZERO)?.is_empty() {
            break;
        }
        if n == 7 {
            return Err(ProviderError::new(
                "Mouse has pending responses; try again shortly",
            ));
        }
    }
    let mut packet = [0; 65];
    packet[..bytes.len()].copy_from_slice(bytes);
    session.write(&packet)?;
    for _ in 0..8 {
        if !context.active() {
            break;
        }
        let r = session.read(65, Duration::from_millis(100))?;
        if r.len() < 11 {
            return Err(ProviderError::new(
                "Mouse did not return a complete sleep response",
            ));
        }
        if r[1..3] == [0xff, 0xaa] {
            return Err(ProviderError::new("Mouse rejected the sleep command"));
        }
        if r[..3] == packet[..3] {
            return Ok(r);
        }
    }
    Err(ProviderError::new("No matching response from the mouse"))
}
fn asus_decode(code: u8) -> Result<u16, ProviderError> {
    match code {
        0 => Ok(60),
        1 => Ok(120),
        2 => Ok(180),
        3 => Ok(300),
        4 => Ok(600),
        255 => Ok(0),
        _ => Err(ProviderError::new(
            "Mouse reported an unknown sleep setting",
        )),
    }
}
fn asus_sleep(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    seconds: Option<u16>,
    expected: Option<u16>,
    out: &mut SleepOutcome,
) -> Result<(), ProviderError> {
    let before = asus_exchange(session, context, &[0, 0x12, 7])?;
    if before[5] == 0 || before[5] > 100 || before[7] > 50 {
        return Err(ProviderError::new(
            "Wake the mouse before changing sleep settings",
        ));
    }
    let current = asus_decode(before[6])?;
    if expected.is_some_and(|expected| expected != current) {
        return Err(ProviderError::new(
            "Sleep time changed elsewhere; automatic change cancelled",
        ));
    }
    out.seconds = Some(current);
    if let Some(seconds) = seconds.filter(|s| *s != current) {
        let code = match seconds {
            60 => 0,
            120 => 1,
            180 => 2,
            300 => 3,
            600 => 4,
            0 => 255,
            _ => return Err(ProviderError::new("Unsupported sleep duration")),
        };
        // The command includes the hardware low-battery threshold: preserve its read value.
        out.seconds = None;
        out.may_have_changed = true;
        asus_exchange(session, context, &[0, 0x51, 0x37, 0, 0, code, 0, before[7]])?;
        asus_exchange(session, context, &[0, 0x50, 3])?;
        let after = asus_exchange(session, context, &[0, 0x12, 7])?;
        if asus_decode(after[6])? != seconds || after[7] != before[7] {
            return Err(ProviderError::new(
                "Mouse did not confirm the sleep setting; refresh before retrying",
            ));
        }
        out.seconds = Some(seconds);
    }
    Ok(())
}
