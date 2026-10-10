//! Optional, independently implemented polling controls. Wire evidence and limits:
//! docs/polling-razer-evidence.md. No device writes occur during discovery.
use hb_core::{HidInfo, HidSession, PollContext};
use std::time::Duration;

pub const EXTENDED_RATES: &[u32] = &[125, 500, 1000, 2000, 4000, 8000];
pub const KEYBOARD_RATES: &[u32] = &[125, 250, 500, 1000, 2000, 4000, 8000];
pub const LEGACY_RATES: &[u32] = &[125, 500, 1000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Extended,
    KeyboardExtended,
    /// Dedicated Mini SE / Viper V3 Pro receiver: same commands, longer settle.
    ExtendedWireless,
    Legacy,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFailure {
    CancelledOrDeadline,
    Transport(String),
    InvalidReply,
    Busy,
    Unsupported,
    DeviceStatus(u8),
    UnknownRate(u8),
    VerificationMismatch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollingResult {
    /// Confirmed get-before value, retained independently of verification.
    pub previous_hz: Option<u32>,
    pub observed_hz: Option<u32>,
    /// A SET may have reached the mouse, including a failed send/acknowledgement.
    pub may_have_changed: bool,
    pub failure: Option<ProtocolFailure>,
}
pub fn protocol(info: &HidInfo) -> Option<Protocol> {
    if info.vendor_id != 0x1532 || info.feature_length != Some(91) {
        return None;
    }
    if info.interface == 3 && keyboard_name(info.product_id).is_some() {
        return Some(Protocol::KeyboardExtended);
    }
    if info.interface != 0 {
        return None;
    }
    match info.product_id {
        0x00be | 0x00bf => Some(Protocol::Extended),
        0x009f | 0x00c1 => Some(Protocol::ExtendedWireless),
        0x00b6 | 0x00b7 => Some(Protocol::Legacy),
        _ => None,
    }
}
impl Protocol {
    pub fn rates(self) -> &'static [u32] {
        match self {
            Self::Extended | Self::ExtendedWireless => EXTENDED_RATES,
            Self::Legacy => LEGACY_RATES,
            Self::KeyboardExtended => KEYBOARD_RATES,
        }
    }
    fn extended(self) -> bool {
        self != Self::Legacy
    }
    fn settle(self) -> Duration {
        Duration::from_millis(if self == Self::KeyboardExtended {
            1
        } else if self == Self::ExtendedWireless {
            60
        } else {
            31
        })
    }
    fn get_id(self) -> u8 {
        if self.extended() { 0xc0 } else { 0x85 }
    }
    fn set_id(self) -> u8 {
        if self.extended() { 0x40 } else { 0x05 }
    }
    fn code(self, hz: u32) -> Option<u8> {
        if !self.rates().contains(&hz) {
            return None;
        }
        Some(((if self.extended() { 8000 } else { 1000 }) / hz) as u8)
    }
}
pub fn request(protocol: Protocol, set: Option<(u32, u8)>) -> Option<[u8; 91]> {
    let mut report = [0; 91];
    report[2] = 0x1f;
    report[6] = if set.is_some() && protocol.extended() {
        2
    } else {
        1
    };
    report[8] = if set.is_some() {
        protocol.set_id()
    } else {
        protocol.get_id()
    };
    if let Some((hz, step)) = set {
        let code = protocol.code(hz)?;
        if protocol.extended() {
            if step > 1 || (protocol == Protocol::KeyboardExtended && step != 0) {
                return None;
            }
            report[9] = step;
            report[10] = code;
        } else {
            report[9] = code;
        }
    }
    report[89] = report[3..89].iter().fold(0, |a, b| a ^ b);
    Some(report)
}
fn exchange(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    protocol: Protocol,
    report: &[u8; 91],
) -> Result<[u8; 90], ProtocolFailure> {
    if !context.active() {
        return Err(ProtocolFailure::CancelledOrDeadline);
    }
    session
        .send_feature(report)
        .map_err(|e| ProtocolFailure::Transport(e.message))?;
    context.sleep(protocol.settle());
    if !context.active() {
        return Err(ProtocolFailure::CancelledOrDeadline);
    }
    let raw = session
        .feature(0, 91)
        .map_err(|e| ProtocolFailure::Transport(e.message))?;
    if !context.active() {
        return Err(ProtocolFailure::CancelledOrDeadline);
    }
    // Windows includes the report ID. Never guess offsets for a malformed frame.
    if raw.len() != 91 || raw[0] != 0 {
        return Err(ProtocolFailure::InvalidReply);
    }
    let r: [u8; 90] = raw[1..].try_into().unwrap();
    // The Windows reference matches command and remaining packets, not TID/CRC.
    // Do not invent stronger untested firmware assumptions here.
    if r[2..4] != [0, 0] || r[6] != 0 || r[7] != report[8] || r[5] > 80 {
        return Err(ProtocolFailure::InvalidReply);
    }
    match r[0] {
        2 => Ok(r),
        1 => Err(ProtocolFailure::Busy),
        5 => Err(ProtocolFailure::Unsupported),
        n => Err(ProtocolFailure::DeviceStatus(n)),
    }
}
pub fn read_rate(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    protocol: Protocol,
) -> Result<u32, ProtocolFailure> {
    let r = exchange(
        session,
        context,
        protocol,
        &request(protocol, None).unwrap(),
    )?;
    // Extended getter's data_size is historically 1 despite arg1 containing rate.
    if r[5] < 1 {
        return Err(ProtocolFailure::InvalidReply);
    }
    let code = r[if protocol.extended() { 9 } else { 8 }];
    protocol
        .rates()
        .iter()
        .copied()
        .find(|hz| protocol.code(*hz) == Some(code))
        .ok_or(ProtocolFailure::UnknownRate(code))
}
pub fn execute_rate(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    protocol: Protocol,
    hz: Option<u32>,
) -> PollingResult {
    execute_rate_checked(session, context, protocol, hz, None)
}
pub fn execute_rate_checked(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    protocol: Protocol,
    hz: Option<u32>,
    expected: Option<u32>,
) -> PollingResult {
    let mut result = PollingResult {
        previous_hz: None,
        observed_hz: None,
        may_have_changed: false,
        failure: None,
    };
    if hz.is_some_and(|h| protocol.code(h).is_none()) {
        result.failure = Some(ProtocolFailure::Unsupported);
        return result;
    }
    match read_rate(session, context, protocol) {
        Ok(rate) => {
            result.previous_hz = Some(rate);
            result.observed_hz = Some(rate);
        }
        Err(e) => {
            result.failure = Some(e);
            return result;
        }
    }
    if expected.is_some_and(|rate| result.observed_hz != Some(rate)) {
        result.failure = Some(ProtocolFailure::VerificationMismatch);
        return result;
    }
    let Some(hz) = hz else {
        return result;
    };
    if result.observed_hz == Some(hz) {
        return result;
    }
    for step in 0..if protocol.extended() && protocol != Protocol::KeyboardExtended {
        2
    } else {
        1
    } {
        if !context.active() {
            result.failure = Some(ProtocolFailure::CancelledOrDeadline);
            break;
        }
        result.may_have_changed = true;
        if let Err(e) = exchange(
            session,
            context,
            protocol,
            &request(protocol, Some((hz, step))).unwrap(),
        ) {
            result.failure = Some(e);
            break;
        }
    }
    // Keep a failure even if readback matches after a partial write/BUSY response.
    // A healthy readback can still explain what the device retained.
    if result.may_have_changed {
        result.observed_hz = None;
        match read_rate(session, context, protocol) {
            Ok(rate) => {
                result.observed_hz = Some(rate);
                if rate != hz && result.failure.is_none() {
                    result.failure = Some(ProtocolFailure::VerificationMismatch);
                }
            }
            Err(e) => {
                if result.failure.is_none() {
                    result.failure = Some(e);
                }
            }
        }
    }
    result
}

/// Exact wired keyboard models verified against the pinned Windows reference.
pub fn keyboard_name(pid: u16) -> Option<&'static str> {
    match pid {
        0x026b => Some("Razer Huntsman V2 Tenkeyless"),
        0x026c => Some("Razer Huntsman V2"),
        0x0287 => Some("Razer BlackWidow V4"),
        0x028d => Some("Razer BlackWidow V4 Pro"),
        0x02a5 => Some("Razer BlackWidow V4 75%"),
        _ => None,
    }
}
