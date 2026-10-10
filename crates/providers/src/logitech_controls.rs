//! Explicit HID++ rate controls for evidence-backed models. No profile writes.
use hb_core::{HidInfo, HidSession, PollContext};
use std::time::Duration;
const RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFailure {
    CancelledOrDeadline,
    Transport(String),
    InvalidReply,
    Timeout,
    DeviceError(u8),
    Unsupported,
    IdentityMismatch,
    OnboardMode,
    UnknownMode,
    UnknownRate(u8),
    VerificationMismatch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub unit: String,
    pub model_wpid: u16,
    pub model_usb: u16,
    pub slot: u8,
    pub supported_hz: Vec<u32>,
    pub observed_hz: u32,
    pub onboard_mode: Option<u8>,
    rate_feature: u8,
    identity_feature: u8,
    mode_feature: Option<u8>,
    path: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollingResult {
    pub observed_hz: Option<u32>,
    pub previous_hz: Option<u32>,
    pub software_mode: bool,
    pub may_have_changed: bool,
    pub failure: Option<ProtocolFailure>,
}
pub fn candidate(info: &HidInfo) -> bool {
    info.vendor_id == 0x046d
        && info.usage_page == 0xff00
        && info.usage == 2
        && matches!(info.product_id, 0xc54d | 0xc53a | 0xc09b | 0xc0a0 | 0xc0a8)
}
/// A mouse capability mask does not establish the receiver's transport ceiling.
/// Only C54D has corroborated >1000 Hz routing for these exact model pairs.
pub fn connection_ceiling(info: &HidInfo) -> u32 {
    if info.product_id == 0xc54d {
        8000
    } else {
        1000
    }
}
struct Exchange<'a, 'b, 'c> {
    long: &'a mut dyn HidSession,
    short: &'a mut Option<&'b mut dyn HidSession>,
    context: &'a PollContext<'c>,
    counter: u8,
}
impl Exchange<'_, '_, '_> {
    fn request(
        &mut self,
        slot: u8,
        feature: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Vec<u8>, ProtocolFailure> {
        if !self.context.active() {
            return Err(ProtocolFailure::CancelledOrDeadline);
        }
        self.counter = self.counter % 15 + 1;
        let token = (function << 4) | self.counter;
        let mut packet = [0u8; 20];
        packet[..4].copy_from_slice(&[0x11, slot, feature, token]);
        packet[4..4 + params.len()].copy_from_slice(params);
        self.long
            .write(&packet)
            .map_err(|e| ProtocolFailure::Transport(e.message))?;
        let end = (self.context.clock.monotonic() + Duration::from_millis(600))
            .min(self.context.deadline);
        while self.context.active() && self.context.clock.monotonic() < end {
            for channel in 0..2 {
                let reply = if channel == 0 {
                    self.long.read(64, Duration::ZERO)
                } else if let Some(short) = self.short {
                    short.read(64, Duration::ZERO)
                } else {
                    continue;
                }
                .map_err(|e| ProtocolFailure::Transport(e.message))?;
                if reply.len() < 4 || reply[1] != slot {
                    continue;
                }
                if matches!(reply[2], 0x8f | 0xff)
                    && reply.len() >= 6
                    && reply[3] == feature
                    && reply[4] == token
                {
                    return Err(ProtocolFailure::DeviceError(reply[5]));
                }
                if reply[2] != feature || reply[3] != token {
                    continue;
                }
                if reply[0] != 0x11 || reply.len() != 20 {
                    return Err(ProtocolFailure::InvalidReply);
                }
                return Ok(reply[4..].to_vec());
            }
            self.context.sleep(Duration::from_millis(5));
        }
        if !self.context.active() {
            Err(ProtocolFailure::CancelledOrDeadline)
        } else {
            Err(ProtocolFailure::Timeout)
        }
    }
    fn feature(&mut self, slot: u8, id: u16) -> Result<Option<u8>, ProtocolFailure> {
        let r = self.request(slot, 0, 0, &id.to_be_bytes())?;
        Ok((r[0] != 0).then_some(r[0]))
    }
    fn identity(&mut self, slot: u8, index: u8) -> Result<(String, u16, u16), ProtocolFailure> {
        let r = self.request(slot, index, 0, &[])?;
        let unit = r[1..5]
            .iter()
            .map(|v| format!("{v:02X}"))
            .collect::<String>();
        if r[1..5].iter().all(|v| *v == 0) {
            return Err(ProtocolFailure::IdentityMismatch);
        }
        let mut offset = 7;
        let (mut wpid, mut usb) = (None, None);
        for bit in [1, 2, 4, 8] {
            if r[6] & bit != 0 {
                if offset + 2 > 13 {
                    return Err(ProtocolFailure::InvalidReply);
                }
                let id = u16::from_be_bytes([r[offset], r[offset + 1]]);
                if bit == 4 {
                    wpid = Some(id)
                }
                if bit == 8 {
                    usb = Some(id)
                }
                offset += 2;
            }
        }
        let ids = match (wpid, usb) {
            (Some(0x40a9), Some(0xc09b)) => (0x40a9, 0xc09b),
            (Some(0x40b8), Some(0xc0a0)) => (0x40b8, 0xc0a0),
            (Some(0x40bd), Some(0xc0a8)) => (0x40bd, 0xc0a8),
            _ => return Err(ProtocolFailure::Unsupported),
        };
        Ok((unit, ids.0, ids.1))
    }
    fn rate(&mut self, slot: u8, index: u8) -> Result<u32, ProtocolFailure> {
        let r = self.request(slot, index, 2, &[])?;
        RATES
            .get(r[0] as usize)
            .copied()
            .ok_or(ProtocolFailure::UnknownRate(r[0]))
    }
}
fn validate_target(info: &HidInfo, slot: u8, expected: &str) -> Result<String, ProtocolFailure> {
    if !candidate(info)
        || !(if matches!(info.product_id, 0xc54d | 0xc53a) {
            (1..=6).contains(&slot)
        } else {
            slot == 255
        })
    {
        return Err(ProtocolFailure::Unsupported);
    }
    let unit = expected.trim().to_ascii_uppercase();
    if unit.len() != 8
        || !unit.bytes().all(|b| b.is_ascii_hexdigit())
        || unit.bytes().all(|b| b == b'0')
    {
        return Err(ProtocolFailure::IdentityMismatch);
    }
    Ok(unit)
}
/// GET requests use HID writes but never SET a setting. Target unit must come from
/// the selected battery reading, not the receiver serial/name or arbitrary slot.
pub fn discover(
    long: &mut dyn HidSession,
    short: &mut Option<&mut dyn HidSession>,
    context: &PollContext<'_>,
    info: &HidInfo,
    slot: u8,
    expected_unit: &str,
) -> Result<Capabilities, ProtocolFailure> {
    let expected = validate_target(info, slot, expected_unit)?;
    let mut exchange = Exchange {
        long,
        short,
        context,
        counter: 0,
    };
    let identity_feature = exchange
        .feature(slot, 3)?
        .ok_or(ProtocolFailure::Unsupported)?;
    let (unit, model_wpid, model_usb) = exchange.identity(slot, identity_feature)?;
    if unit != expected || slot == 255 && model_usb != info.product_id {
        return Err(ProtocolFailure::IdentityMismatch);
    }
    if model_wpid == 0x40bd && info.product_id == 0xc53a {
        return Err(ProtocolFailure::Unsupported);
    }
    let rate_feature = exchange
        .feature(slot, 0x8061)?
        .ok_or(ProtocolFailure::Unsupported)?;
    let flags = exchange.request(slot, rate_feature, 1, &[])?;
    let mask = u16::from_be_bytes([flags[0], flags[1]]);
    let supported_hz = RATES
        .iter()
        .enumerate()
        .filter(|(i, h)| mask & (1 << i) != 0 && **h <= connection_ceiling(info))
        .map(|(_, h)| *h)
        .collect::<Vec<_>>();
    if supported_hz.is_empty() {
        return Err(ProtocolFailure::Unsupported);
    }
    let observed_hz = exchange.rate(slot, rate_feature)?;
    let mode_feature = exchange.feature(slot, 0x8100)?;
    let onboard_mode = if let Some(index) = mode_feature {
        Some(exchange.request(slot, index, 2, &[])?[0])
    } else {
        None
    };
    Ok(Capabilities {
        unit,
        model_wpid,
        model_usb,
        slot,
        supported_hz,
        observed_hz,
        onboard_mode,
        rate_feature,
        identity_feature,
        mode_feature,
        path: info.path.clone(),
    })
}
/// An explicit SET is attempted only after live revalidation on the same target.
/// Onboard profile mode is refused; this module never toggles mode or writes flash.
pub fn execute_rate(
    long: &mut dyn HidSession,
    short: &mut Option<&mut dyn HidSession>,
    context: &PollContext<'_>,
    info: &HidInfo,
    capability: &Capabilities,
    hz: Option<u32>,
) -> PollingResult {
    execute_rate_checked(long, short, context, info, capability, hz, None)
}
pub fn execute_rate_checked(
    long: &mut dyn HidSession,
    short: &mut Option<&mut dyn HidSession>,
    context: &PollContext<'_>,
    info: &HidInfo,
    capability: &Capabilities,
    hz: Option<u32>,
    expected: Option<u32>,
) -> PollingResult {
    let mut result = PollingResult {
        observed_hz: None,
        previous_hz: None,
        software_mode: false,
        may_have_changed: false,
        failure: None,
    };
    if capability.path != info.path
        || validate_target(info, capability.slot, &capability.unit).is_err()
    {
        result.failure = Some(ProtocolFailure::IdentityMismatch);
        return result;
    }
    if hz.is_some_and(|h| !capability.supported_hz.contains(&h)) {
        result.failure = Some(ProtocolFailure::Unsupported);
        return result;
    }
    let current = match discover(
        long,
        short,
        context,
        info,
        capability.slot,
        &capability.unit,
    ) {
        Ok(c) => c,
        Err(e) => {
            result.failure = Some(e);
            return result;
        }
    };
    result.observed_hz = Some(current.observed_hz);
    result.software_mode = current.onboard_mode == Some(2);
    result.previous_hz = (result.software_mode
        && current.supported_hz.contains(&current.observed_hz))
    .then_some(current.observed_hz);
    if expected.is_some_and(|rate| result.observed_hz != Some(rate)) {
        result.failure = Some(ProtocolFailure::VerificationMismatch);
        return result;
    }
    let Some(hz) = hz else { return result };
    if !current.supported_hz.contains(&current.observed_hz) {
        result.failure = Some(ProtocolFailure::Unsupported);
        return result;
    }
    if !current.supported_hz.contains(&hz) {
        result.failure = Some(ProtocolFailure::Unsupported);
        return result;
    }
    if current.observed_hz == hz {
        return result;
    }
    match current.onboard_mode {
        Some(2) => {}
        Some(1) => {
            result.failure = Some(ProtocolFailure::OnboardMode);
            return result;
        }
        _ => {
            result.failure = Some(ProtocolFailure::UnknownMode);
            return result;
        }
    }
    let mut exchange = Exchange {
        long,
        short,
        context,
        counter: 8,
    };
    if !context.active() {
        result.failure = Some(ProtocolFailure::CancelledOrDeadline);
        return result;
    }
    result.may_have_changed = true;
    let code = RATES.iter().position(|h| *h == hz).unwrap() as u8;
    if let Err(e) = exchange.request(current.slot, current.rate_feature, 3, &[code]) {
        result.failure = Some(e)
    }
    result.observed_hz = None;
    match exchange.rate(current.slot, current.rate_feature) {
        Ok(rate) => {
            result.observed_hz = Some(rate);
            if rate != hz && result.failure.is_none() {
                result.failure = Some(ProtocolFailure::VerificationMismatch)
            }
        }
        Err(e) => {
            if result.failure.is_none() {
                result.failure = Some(e)
            }
        }
    }
    // The configured rate is meaningful only while software control remains
    // active. Another application can switch profiles during the SET attempt.
    let verified_mode = match current.mode_feature {
        Some(index) => exchange.request(current.slot, index, 2, &[]).map(|r| r[0]),
        None => Err(ProtocolFailure::UnknownMode),
    };
    let mode_failure = match verified_mode {
        Ok(2) => None,
        Ok(1) => Some(ProtocolFailure::OnboardMode),
        Ok(_) => Some(ProtocolFailure::UnknownMode),
        Err(e) => Some(e),
    };
    if let Some(error) = mode_failure {
        result.software_mode = false;
        result.previous_hz = None;
        if result.failure.is_none() {
            result.failure = Some(error);
        }
    }
    result
}
