//! Explicit, evidence-scoped A7 V2 Ultra+ polling read/modify/readback.
//! The caller owns receiver serialization and transport epoch guards.
//! See docs/polling-mchose-evidence.md; no discovery calls this module.
use hb_core::{HidInfo, HidSession, PollContext};
use std::time::Duration;

pub const RATES: &[u32] = &[125, 500, 1000, 2000, 4000, 8000];
const SHORT: u8 = 0x11;
const LONG: u8 = 0x12;
const LENGTH: usize = 65;
const CONFIG_LENGTH: usize = 63;
const FIRMWARE: [u8; 4] = [5, 46, 2, 4];
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFailure {
    Unsupported,
    CancelledOrDeadline,
    Transport(String),
    InvalidReply,
    UnstableReply,
    UnsupportedIdentity,
    ConfigurationChanged,
    ProfileChanged,
    UnrelatedConfigurationChanged,
    VerificationMismatch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollingResult {
    pub previous_hz: Option<u32>,
    pub observed_hz: Option<u32>,
    pub supported_hz: Vec<u32>,
    pub may_have_changed: bool,
    pub failure: Option<ProtocolFailure>,
}
pub fn candidate(info: &HidInfo) -> bool {
    info.vendor_id == 0x3837
        && info.product_id == 0x100b
        && (info.usage_page, info.usage) == (0xff01, 1)
        && info.feature_length == Some(LENGTH)
}
fn active(context: &PollContext<'_>) -> Result<(), ProtocolFailure> {
    if context.active() {
        Ok(())
    } else {
        Err(ProtocolFailure::CancelledOrDeadline)
    }
}
fn command(id: u8, cmd: u8, payload: &[u8]) -> [u8; LENGTH] {
    let mut bytes = [0; LENGTH];
    bytes[0] = id;
    let meaningful = if id == SHORT { 20 } else { 64 };
    bytes[1..=meaningful].fill(0xff);
    bytes[1] = !cmd;
    for (destination, value) in bytes[2..].iter_mut().zip(payload) {
        *destination = !value;
    }
    bytes
}
fn send(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    bytes: &[u8],
) -> Result<(), ProtocolFailure> {
    active(context)?;
    session
        .send_feature(bytes)
        .map_err(|e| ProtocolFailure::Transport(e.message))?;
    active(context)
}
fn stable_query(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    id: u8,
    cmd: u8,
    length: usize,
) -> Result<Vec<u8>, ProtocolFailure> {
    let mut previous: Option<Vec<u8>> = None;
    for _ in 0..12 {
        send(session, context, &command(id, cmd, &[]))?;
        context.sleep(Duration::from_millis(90));
        active(context)?;
        let raw = session
            .feature(id, LENGTH)
            .map_err(|e| ProtocolFailure::Transport(e.message))?;
        active(context)?;
        let valid_length = if id == SHORT {
            raw.len() == 21 || raw.len() == LENGTH
        } else {
            raw.len() == LENGTH
        };
        if !valid_length || raw[0] != id {
            return Err(ProtocolFailure::InvalidReply);
        }
        if raw[1] != !cmd {
            previous = None;
            continue;
        }
        let payload: Vec<u8> = raw[2..2 + length].iter().map(|byte| !byte).collect();
        if previous.as_ref() == Some(&payload) {
            return Ok(payload);
        }
        previous = Some(payload);
    }
    Err(ProtocolFailure::UnstableReply)
}
fn receiver(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
) -> Result<Vec<u8>, ProtocolFailure> {
    let identity = stable_query(session, context, SHORT, 3, 7)?;
    // Physical reference: bonded, VID 3837, receiver PID 100B, linked.
    if identity[..6] != [1, 0x37, 0x38, 0x0b, 0x10, 1] {
        return Err(ProtocolFailure::UnsupportedIdentity);
    }
    Ok(identity)
}
fn mouse(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
) -> Result<Vec<u8>, ProtocolFailure> {
    let identity = stable_query(session, context, SHORT, 6, 11)?;
    // Do not guess the undocumented mode bit order. 09 is the captured
    // A7 V2 Ultra+ receiver flag; independently require receiver link above.
    if identity[..4] != [0x37, 0x38, 0x21, 0x40]
        || identity[4..8] != FIRMWARE
        || identity[8] != 9
        || identity[9] > 100
        || identity[10] > 1
    {
        return Err(ProtocolFailure::UnsupportedIdentity);
    }
    Ok(identity[..9].to_vec())
}
fn config(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
) -> Result<Vec<u8>, ProtocolFailure> {
    let bytes = stable_query(session, context, LONG, 0x67, CONFIG_LENGTH)?;
    if bytes[0] > 2
        || bytes[1] & 0x0f > 5
        || bytes[2] & 0x0f > 5
        || bytes[1] >> 4 > 5
        || bytes[2] >> 4 > 5
        || !(1..=6).contains(&bytes[16])
        || bytes[18] > 20
    {
        return Err(ProtocolFailure::InvalidReply);
    }
    for pair in bytes[4..16].as_chunks::<2>().0 {
        let dpi = u16::from_le_bytes([pair[0], pair[1]]);
        if !(50..=42000).contains(&dpi) {
            return Err(ProtocolFailure::InvalidReply);
        }
    }
    Ok(bytes)
}
fn rate(bytes: &[u8]) -> u32 {
    RATES[(bytes[2] >> 4) as usize]
}
pub fn execute_rate(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    info: &HidInfo,
    requested: Option<u32>,
) -> PollingResult {
    execute_rate_checked(session, context, info, requested, None)
}
pub fn execute_rate_checked(
    session: &mut dyn HidSession,
    context: &PollContext<'_>,
    info: &HidInfo,
    requested: Option<u32>,
    expected: Option<u32>,
) -> PollingResult {
    let mut result = PollingResult {
        previous_hz: None,
        observed_hz: None,
        supported_hz: vec![],
        may_have_changed: false,
        failure: None,
    };
    if !candidate(info) || requested.is_some_and(|hz| !RATES.contains(&hz)) {
        result.failure = Some(ProtocolFailure::Unsupported);
        return result;
    }
    let operation = (|| -> Result<(), ProtocolFailure> {
        let receiver_before = receiver(session, context)?;
        let mouse_before = mouse(session, context)?;
        let before = config(session, context)?;
        let previous = rate(&before);
        result.previous_hz = Some(previous);
        result.observed_hz = Some(previous);
        result.supported_hz = RATES.to_vec();
        if expected.is_some_and(|rate| result.observed_hz != Some(rate)) {
            return Err(ProtocolFailure::ConfigurationChanged);
        }
        let Some(hz) = requested else {
            return Ok(());
        };
        if hz == previous {
            return Ok(());
        }
        // Recheck receiver, paired model, firmware and full active profile just
        // before committing. Never write an old blob over a concurrent change.
        if receiver(session, context)? != receiver_before
            || mouse(session, context)? != mouse_before
        {
            return Err(ProtocolFailure::UnsupportedIdentity);
        }
        let current = config(session, context)?;
        if current != before {
            return Err(if current[0] != before[0] {
                ProtocolFailure::ProfileChanged
            } else {
                ProtocolFailure::ConfigurationChanged
            });
        }
        let mut expected = before.clone();
        let index = RATES.iter().position(|rate| *rate == hz).unwrap() as u8;
        expected[2] = (expected[2] & 0x0f) | (index << 4);
        let set = command(LONG, 0x57, &expected);
        active(context)?;
        result.may_have_changed = true;
        result.observed_hz = None;
        let send_failure = send(session, context, &set).err();
        // No undocumented SET acknowledgement is assumed. The reference
        // verifies this command through a settled, correlated config read.
        context.sleep(Duration::from_millis(400));
        let verification = (|| -> Result<(), ProtocolFailure> {
            if receiver(session, context)? != receiver_before
                || mouse(session, context)? != mouse_before
            {
                return Err(ProtocolFailure::UnsupportedIdentity);
            }
            let after = config(session, context)?;
            if after[0] != before[0] {
                return Err(ProtocolFailure::ProfileChanged);
            }
            // Every unrelated byte, including button records/unknown tail and
            // the inactive link's rate/stage, must still match the saved blob.
            if after
                .iter()
                .enumerate()
                .any(|(i, byte)| i != 2 && *byte != before[i])
                || (after[2] & 0x0f) != (before[2] & 0x0f)
            {
                return Err(ProtocolFailure::UnrelatedConfigurationChanged);
            }
            result.observed_hz = Some(rate(&after));
            if after != expected {
                return Err(ProtocolFailure::VerificationMismatch);
            }
            Ok(())
        })();
        if let Some(error) = send_failure {
            return Err(error);
        }
        verification
    })();
    result.failure = operation.err();
    if matches!(
        result.failure,
        Some(
            ProtocolFailure::UnsupportedIdentity
                | ProtocolFailure::ConfigurationChanged
                | ProtocolFailure::ProfileChanged
                | ProtocolFailure::UnrelatedConfigurationChanged
        )
    ) {
        result.previous_hz = None;
        result.observed_hz = None;
        result.supported_hz.clear();
    }
    result
}
