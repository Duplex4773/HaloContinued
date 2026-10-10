//! Allowlisted, one-shot DPI changes. No input interception or automatic enforcement.
use crate::{
    controls::GuardedSession,
    provider::{is_bluetooth, receiver_key, trusted_identity},
};
use hb_core::*;
use std::time::Duration;

pub type BackupWriter<'a> = dyn FnMut(&DpiBackup) -> Result<(), ProviderError> + 'a;
pub(crate) fn error(text: &str) -> ProviderError {
    ProviderError::new(text)
}

/// Modern 16-bit XY protocol only. Legacy byte encodings are deliberately excluded.
pub fn razer_maximum(pid: u16) -> Option<u16> {
    match pid {
        0x0094 => Some(18000),
        0x00a5..=0x00a8 | 0x00aa | 0x00ab | 0x00af | 0x00b0 => Some(30000),
        0x00b6 | 0x00b7 | 0x00c0..=0x00c3 | 0x00cc | 0x00cd => Some(35000),
        0x00be | 0x00bf => Some(45000),
        _ => None,
    }
}
fn razer_pid(d: &ConfigurationDevice) -> Option<u16> {
    let rest = d.key.strip_prefix("razer:")?;
    let (pid, identity) = rest.split_once(':')?;
    if identity.is_empty() || identity.contains(":physical:") {
        return None;
    }
    let pid = u16::from_str_radix(pid, 16).ok()?;
    razer_maximum(pid).map(|_| pid)
}
pub fn candidate(d: &ConfigurationDevice) -> bool {
    d.kind == "mouse"
        && match d.source.as_str() {
            "razer" => d.via == "usb" && razer_pid(d).is_some(),
            "logitech" => {
                (d.via.is_empty() || d.via == "usb")
                    && d.serial.as_ref().is_some_and(|s| {
                        s.len() == 8
                            && s.bytes().all(|b| b.is_ascii_hexdigit())
                            && s.bytes().any(|b| b != b'0')
                            && d.key.eq_ignore_ascii_case(&format!("logitech:{s}"))
                    })
            }
            _ => false,
        }
}
pub fn model_limits(d: &ConfigurationDevice) -> Option<DpiLimits> {
    if d.source != "razer" || !candidate(d) {
        return None;
    }
    let maximum = razer_maximum(razer_pid(d)?)?;
    Some(DpiLimits {
        x: vec![(100, maximum, 1)],
        y: vec![(100, maximum, 1)],
        from_model: true,
    })
}
pub fn execute(
    request: &DpiRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
    backup: &mut BackupWriter<'_>,
) -> DpiOutcome {
    let mut outcome = DpiOutcome::failed(request, "DPI controls are unavailable for this mouse.");
    outcome.limits = model_limits(&request.target.device);
    let result = (|| {
        if !candidate(&request.target.device) || !request.target.device.online() {
            return Err(error("Select a supported, awake mouse."));
        }
        if !context.active() || request.target.generation != hid.generation() {
            return Err(error(
                "Connection changed. Refresh DPI before trying again.",
            ));
        }
        if request.value.is_some() && request.expected.is_none() {
            return Err(error("Read the current DPI before applying a change."));
        }
        if request.value.is_some_and(|v| v.x < 100 || v.y < 100) {
            return Err(error("DPI must be at least 100 on both axes."));
        }
        if request.target.device.source == "logitech" {
            crate::dpi_logitech::execute(request, hid, context, backup, &mut outcome)
        } else {
            razer(request, hid, context, backup, &mut outcome)
        }
    })();
    outcome.failure = result.err().map(|e| e.message);
    if outcome.failure.is_some() {
        outcome.current = None;
    }
    outcome
}
pub(crate) fn prepare(
    request: &DpiRequest,
    original: &DpiState,
    raw: Vec<u8>,
    format: Option<u8>,
    context: &PollContext<'_>,
    backup: &mut BackupWriter<'_>,
    outcome: &mut DpiOutcome,
) -> Result<Option<DpiValue>, ProviderError> {
    outcome.current = Some(original.clone());
    let Some(value) = request.value else {
        return Ok(None);
    };
    if request.expected.as_ref() != Some(original) {
        return Err(error(
            "DPI or active profile changed elsewhere. Read current DPI again.",
        ));
    }
    if value == original.value {
        return Ok(None);
    }
    backup(&DpiBackup {
        device: request.target.device.key.clone(),
        timestamp: context.clock.unix(),
        original: original.clone(),
        requested: value,
        profile_bytes: raw,
        profile_format: format,
    })?;
    if !context.active() {
        return Err(error("DPI change cancelled before writing."));
    }
    outcome.previous = Some(original.clone());
    Ok(Some(value))
}
/// HID report includes ID 0. Only XY and read-only stage commands are permitted.
pub fn razer_packet(command: u8, value: Option<DpiValue>) -> [u8; 91] {
    let mut p = [0; 91];
    p[2] = 0x1f;
    p[6] = if command == 0x86 { 38 } else { 7 };
    p[7] = 4;
    p[8] = command;
    p[9] = u8::from(command == 0x05 || command == 0x86);
    if let Some(v) = value {
        p[10..12].copy_from_slice(&v.x.to_be_bytes());
        p[12..14].copy_from_slice(&v.y.to_be_bytes());
    }
    p[89] = p[3..89].iter().fold(0, |a, b| a ^ b);
    p
}
fn exchange(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    pid: u16,
    command: u8,
    value: Option<DpiValue>,
) -> Result<Vec<u8>, ProviderError> {
    if !context.active() {
        return Err(error("DPI request cancelled."));
    }
    session.send_feature(&razer_packet(command, value))?;
    context.sleep(Duration::from_micros(if pid == 0x0094 {
        400000
    } else {
        31000
    }));
    if !context.active() {
        return Err(error("DPI request cancelled."));
    }
    let r = session.feature(0, 91)?;
    if r.len() != 91
        || r[0] != 0
        || r[3..5] != [0, 0]
        || r[7] != 4
        || r[8] != command
        || r[6] > 80
        || r[6] < if command == 0x86 { 38 } else { 7 }
    {
        return Err(error(
            "The mouse returned an incomplete or unrelated DPI response.",
        ));
    }
    match r[1] {
        2 => Ok(r[9..9 + usize::from(r[6])].to_vec()),
        1 => Err(error("The mouse is busy. No automatic retry was made.")),
        5 => Err(error("This mouse does not accept the DPI command.")),
        _ => Err(error("The mouse rejected the DPI request.")),
    }
}
fn read_razer(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    pid: u16,
) -> Result<(DpiState, Vec<u8>), ProviderError> {
    let r = exchange(session, context, pid, 0x85, None)?;
    let value = DpiValue {
        x: u16::from_be_bytes([r[1], r[2]]),
        y: u16::from_be_bytes([r[3], r[4]]),
    };
    let maximum = razer_maximum(pid).unwrap();
    if !(100..=maximum).contains(&value.x) || !(100..=maximum).contains(&value.y) {
        return Err(error("The mouse reported an unknown DPI value."));
    }
    let stages = exchange(session, context, pid, 0x86, None)?;
    if !(1..=5).contains(&stages[2]) || stages[1] > stages[2] {
        return Err(error("The active DPI stage could not be verified."));
    }
    Ok((
        DpiState {
            value,
            profile: None,
            stage: Some(stages[1]),
        },
        stages,
    ))
}
fn razer(
    request: &DpiRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
    backup: &mut BackupWriter<'_>,
    out: &mut DpiOutcome,
) -> Result<(), ProviderError> {
    let d = &request.target.device;
    let pid = razer_pid(d).ok_or_else(|| error("Unsupported DPI model."))?;
    if request
        .value
        .is_some_and(|v| v.x > razer_maximum(pid).unwrap() || v.y > razer_maximum(pid).unwrap())
    {
        return Err(error("DPI exceeds this mouse's supported range."));
    }
    let infos = hid.enumerate(0x1532)?;
    if infos.len() > 512 {
        return Err(error("Too many mouse collections."));
    }
    let mut matching = infos.iter().filter(|i| {
        i.vendor_id == 0x1532
            && i.product_id == pid
            && i.interface == 0
            && i.feature_length == Some(91)
            && !is_bluetooth(&i.path)
            && d.key == format!("razer:{pid:04x}:{}", trusted_identity(i))
    });
    let i = matching
        .next()
        .ok_or_else(|| error("Wake or reconnect the mouse and refresh Devices."))?;
    if matching.next().is_some()
        || infos.iter().any(|o| {
            o.product_id == pid
                && trusted_identity(o) == trusted_identity(i)
                && receiver_key(o) != receiver_key(i)
        })
    {
        return Err(error("Mouse connection is ambiguous; no DPI command sent."));
    }
    if d.serial
        .as_ref()
        .is_some_and(|s| !s.trim().eq_ignore_ascii_case(i.serial.trim()))
        || d.container.as_ref().is_some_and(|c| {
            i.container
                .as_ref()
                .is_none_or(|other| !c.eq_ignore_ascii_case(other))
        })
    {
        return Err(error("Mouse identity changed. Refresh Devices."));
    }
    let mut session = GuardedSession::new(hid.open(i)?, hid, context, request.target.generation);
    let (before, raw) = read_razer(&mut session, context, pid)?;
    if let Some(value) = prepare(request, &before, raw.clone(), None, context, backup, out)? {
        let (fresh, stages) = read_razer(&mut session, context, pid)?;
        if fresh != before || stages != raw {
            return Err(error(
                "DPI changed while preparing the update. Read it again.",
            ));
        }
        out.may_have_changed = true;
        out.current = None;
        exchange(&mut session, context, pid, 0x05, Some(value))?;
        let (after, _) = read_razer(&mut session, context, pid)?;
        if after.value != value || after.stage != before.stage {
            return Err(error(
                "The mouse did not confirm the requested DPI. Read current DPI before retrying.",
            ));
        }
        out.current = Some(after);
    }
    Ok(())
}
