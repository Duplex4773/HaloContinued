//! Read-only battery protocols from the pinned HaloBattery fork sources in
//! reviewed-devices.json. No device settings are written by these providers.
use super::{drain, pause, read as input, reading, receive};
use crate::{
    catalog::Device,
    protocols::{self as p, Battery},
    provider::{receiver_key, valid_identity_value},
};
use hb_core::*;
use std::time::Duration;

pub(super) fn candidate(protocol: &str, i: &HidInfo) -> bool {
    let usage = (i.usage_page, i.usage);
    let output = |length| i.output_length.is_none_or(|n| n == length);
    match protocol {
        "endgame-we" | "irocks" => usage == (0xff02, 2) && i.feature_length == Some(17),
        "glorious" => {
            matches!(usage, (0xffa0, 1) | (0xffff, 0 | 1)) && i.feature_length == Some(65)
        }
        "vaxee" => i.usage_page == 0xff05 && matches!(i.feature_length, Some(64 | 65)),
        "alienware" => usage == (0xff00, 1) && i.interface == 1 && i.feature_length == Some(65),
        "urx" => {
            usage == (0xff60, 0x61)
                && i.interface == 1
                && i.output_length.is_some_and(|n| matches!(n, 32 | 33))
        }
        "f87-pro" => usage == (0xff02, 2) && i.output_length == Some(20),
        "bt600" => usage == (0xffa2, 3) && i.interface == 3 && output(64),
        "barracuda-x" => i.usage_page == 0xff00 && i.output_length == Some(64),
        "inzone-h9" => i.interface == -1 && usage == (0, 0),
        _ => false,
    }
}

pub(super) fn glorious_cable_matches(receiver: &HidInfo, cable: &HidInfo) -> bool {
    if cable.vendor_id != receiver.vendor_id || cable.product_id != 0x2011 {
        return false;
    }
    let matches = |a: &str, b: &str| {
        valid_identity_value(a)
            .zip(valid_identity_value(b))
            .is_some_and(|(a, b)| a == b)
    };
    receiver
        .container
        .as_deref()
        .zip(cable.container.as_deref())
        .is_some_and(|(a, b)| matches(a, b))
        || matches(&receiver.serial, &cable.serial)
}

pub(super) fn split_mouse(
    d: &Device,
    command: &HidInfo,
    infos: &[&HidInfo],
    hid: &dyn HidTransport,
    c: &PollContext<'_>,
) -> Result<Option<Reading>, ProviderError> {
    let identity = receiver_key(command);
    let mut replies = infos.iter().filter(|i| {
        (i.vendor_id, i.product_id, i.usage_page, i.usage) == (d.vid, d.pid, 0xff01, 0)
            && receiver_key(i) == identity
    });
    let Some(reply) = replies.next() else {
        return Ok(None);
    };
    if replies.next().is_some() || !c.active() {
        return Ok(None);
    }
    let mut reader = hid.open(reply)?;
    let mut writer = hid.open(command)?;
    if !drain(&mut *reader, c, 64, 4)? {
        return Ok(None);
    }
    let endgame = d.pid == 0x1961;
    if d.pid == 0xc009
        && split_query(&mut *writer, &mut *reader, c, 3, false)?
            .is_none_or(|(online, _)| online != 1)
    {
        return Ok(None);
    }
    let Some((level, flag)) = split_query(&mut *writer, &mut *reader, c, 4, endgame)? else {
        return Ok(None);
    };
    let charging = flag.and_then(|f| match f {
        0 => Some(false),
        1 => Some(true),
        _ if !endgame => Some(true),
        _ => None,
    });
    let Some(mut b) = p::battery(level, charging) else {
        return Ok(None);
    };
    if endgame {
        b.coarse = true;
    }
    let mut r = reading(d, command, b, c);
    if endgame && charging != Some(true) {
        r.connection = match split_query(&mut *writer, &mut *reader, c, 3, true)? {
            Some((1, _)) => Connection::Online,
            Some((0, _)) => Connection::Sleeping,
            _ => Connection::Stale,
        };
    }
    Ok(Some(r))
}

fn split_query(
    writer: &mut dyn HidSession,
    reader: &mut dyn HidSession,
    c: &PollContext<'_>,
    command: u8,
    endgame: bool,
) -> Result<Option<(u8, Option<u8>)>, ProviderError> {
    if !c.active() {
        return Ok(None);
    }
    let mut request = [0; 17];
    request[0] = 8;
    request[1] = command;
    request[16] = 0x55 - 8 - command;
    writer.send_feature(&request)?;
    let timeout = if endgame {
        400
    } else if command == 3 {
        500
    } else {
        250
    };
    let deadline = c
        .deadline
        .min(c.clock.monotonic() + Duration::from_millis(timeout));
    let context = PollContext { deadline, ..*c };
    receive(reader, &context, 8, timeout, 64, |r| {
        let r = r.get(..17)?;
        if (endgame && r[0] != 9 || !endgame && !matches!(r[0], 8 | 9))
            || r[1] != command
            || r[2] != 0
            || wrapping_sum(r) != 0x55
            || endgame && (r[3..5] != [0, 0] || !(1..=10).contains(&r[5]))
        {
            return None;
        }
        Some((r[6], (!endgame || r[5] >= 2).then_some(r[7])))
    })
}

fn wrapping_sum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte))
}

pub(super) fn read(
    protocol: &str,
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    i: &HidInfo,
) -> Result<Option<Battery>, ProviderError> {
    match protocol {
        "glorious" => glorious(s, c),
        "vaxee" => vaxee(s, c, i.feature_length.unwrap_or(65)),
        "alienware" => {
            if !drain(s, c, 65, 4)? {
                return Ok(None);
            }
            s.send_feature(&p::padded(&[0, 0x0e, 0xa1], 65))?;
            receive(s, c, 6, 250, 65, |r| {
                let r = r
                    .strip_prefix(&[0])
                    .unwrap_or(r)
                    .strip_prefix(&[0x0e, 0xa1])?;
                let [level @ 1..=100, state, ..] = *r else {
                    return None;
                };
                p::battery(level, super::charge(state, &[0], &[4]))
            })
        }
        "urx" => {
            if !drain(s, c, 33, 4)? {
                return Ok(None);
            }
            s.write(&p::padded(&[0, 8, 0, 7], 32))?;
            receive(s, c, 6, 250, 33, |r| {
                let r = r.strip_prefix(&[0]).unwrap_or(r).strip_prefix(&[8, 0, 7])?;
                let [level @ 1..=100, ..] = *r else {
                    return None;
                };
                p::battery(level, None)
            })
        }
        "f87-pro" => f87_pro(s, c),
        "bt600" => bt600(s, c),
        "inzone-h9" => inzone(s, c),
        _ => Ok(None),
    }
}

fn glorious(s: &mut dyn HidSession, c: &PollContext<'_>) -> Result<Option<Battery>, ProviderError> {
    let request = p::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65);
    for _ in 0..2 {
        if !c.active() {
            break;
        }
        s.send_feature(&request)?;
        if !pause(c, 100) {
            break;
        }
        let r = s.feature(0, 65)?;
        if let [0, 0xa1, _, _, _, _, 0x83, charging, level, ..] = *r
            && let Some(b) = p::battery(level, Some(charging != 0))
        {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

fn vaxee(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    length: usize,
) -> Result<Option<Battery>, ProviderError> {
    for attempt in 0..2 {
        if !c.active() || attempt != 0 && !pause(c, 300) {
            break;
        }
        let mut query = |cmd| {
            if !c.active() {
                return Ok(None);
            }
            s.send_feature(&p::padded(&[0x0e, 0xa5, cmd, 1, 1], length))?;
            if !pause(c, 100) {
                return Ok(None);
            }
            let r = s.feature(0x0e, length)?;
            Ok::<_, ProviderError>(match *r {
                [0x0e, 0xa5, echo, _, _, value, ..] if echo == cmd => Some(value),
                _ => None,
            })
        };
        let Some(level) = query(0x0b)? else {
            continue;
        };
        if level > 20 {
            continue;
        }
        let charging = query(0x10)?.map(|value| value != 0);
        let mut b = p::battery(level * 5, charging).unwrap();
        b.coarse = true;
        return Ok(Some(b));
    }
    Ok(None)
}

fn f87_pro(s: &mut dyn HidSession, c: &PollContext<'_>) -> Result<Option<Battery>, ProviderError> {
    let mut query = |command| {
        // On this shared receiver, do not assume an unmatched model response
        // is an F87 Pro. The six-byte hardware model payload must match.
        for _ in 0..16 {
            if !c.active() {
                return Ok(None);
            }
            if s.read(20, Duration::ZERO)?.is_empty() {
                if !c.active() {
                    return Ok(None);
                }
                let mut frame = [0; 20];
                frame[..3].copy_from_slice(&[0x13, command, 1]);
                frame[19] = wrapping_sum(&frame[..19]);
                s.write(&frame)?;
                return receive(s, c, 15, 100, 20, |r| {
                    let r: [u8; 20] = r.try_into().ok()?;
                    (r[..4] == [0x13, command, 1, 0] && wrapping_sum(&r[..19]) == r[19])
                        .then_some(r)
                });
            }
        }
        Ok(None)
    };
    if !query(5)?.is_some_and(|r| matches!(r[4], 6 | 10) && r[5..11] == [3, 0, 0, 0, 1, 0x0b]) {
        return Ok(None);
    }
    Ok(query(0x4a)?
        .filter(|r| r[4] == 2)
        .and_then(|r| p::battery(r[5], None)))
}

fn bt600(s: &mut dyn HidSession, c: &PollContext<'_>) -> Result<Option<Battery>, ProviderError> {
    if !drain(s, c, 64, 4)? {
        return Ok(None);
    }
    // Windows attach nudge. The headset HELLO must succeed before the read.
    let _ = s.write(&[6, 1]);
    let mut exchange = |target, address, op, reply, group, item| {
        if !c.active() {
            return Ok(None);
        }
        let len = if op == 2 { 6 } else { 7 };
        s.write(&p::padded(
            &[7, 1, 1, 0x10, len, target, 0, 0, op, group, item],
            64,
        ))?;
        let deadline = c.deadline.min(c.clock.monotonic() + Duration::from_secs(1));
        let context = PollContext { deadline, ..*c };
        for _ in 0..16 {
            let r = input(s, &context, 64, 1000)?;
            if r.is_empty() || !context.active() {
                break;
            }
            if r.starts_with(&[7, 1, 1, 0x10])
                && r.get(5) == Some(&address)
                && r.get(8..11) == Some(&[reply, group, item])
            {
                return Ok(Some(r));
            }
        }
        Ok::<_, ProviderError>(None)
    };
    let _ = exchange(0, 0, 1, 8, 1, 2)?;
    if exchange(0x20, 2, 1, 8, 1, 2)?.is_none() {
        return Ok(None);
    }
    Ok(exchange(0x20, 2, 2, 3, 0x0a, 0x1a)?.and_then(|r| {
        let [level, count, charging, ..] = *r.get(11..)? else {
            return None;
        };
        if count < 2 || level >= count {
            return None;
        }
        let mut b = p::battery(
            (u16::from(level) * 100 / u16::from(count - 1)) as u8,
            Some(charging != 0),
        )?;
        b.coarse = count < 101;
        Some(b)
    }))
}

fn race_packet(group: u8, sequence: u8, body: [u8; 3]) -> [u8; 10] {
    [
        1, 0x80, 7, 0x50, 0x41, group, sequence, body[0], body[1], body[2],
    ]
}

fn race_payload(mut r: &[u8], group: u8, sequence: u8) -> Option<[u8; 3]> {
    if let [1, 0x80, length, ..] = *r {
        r = r.get(3..3 + usize::from(length))?;
    }
    while r.len() >= 10 && r[..2] == [0x50, 0x49] {
        let length = usize::from(u16::from_le_bytes([r[8], r[9]]));
        let payload = r.get(10..10 + length)?;
        if payload.starts_with(&[group, 0x80 | sequence]) {
            let [_, _, status, lo, hi, ..] = *payload else {
                return (group == 0x0e).then(|| [payload.get(2).copied().unwrap_or(0xff), 0, 0]);
            };
            return Some([status, lo, hi]);
        }
        r = &r[10 + length..];
    }
    None
}

pub(super) fn barracuda_x(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    sequence: u8,
) -> Result<Option<Battery>, ProviderError> {
    if !drain(s, c, 64, 4)? {
        return Ok(None);
    }
    s.write(&[1, 0x40])?;
    if receive(s, c, 4, 200, 64, |r| {
        r.starts_with(&[1, 0x40, 1, 1]).then_some(())
    })?
    .is_none()
        || !c.active()
    {
        return Ok(None);
    }
    // Sequence changes each poll so a late packet from an earlier exchange
    // cannot masquerade as this reading. The headset provides voltage only.
    s.write(&race_packet(0x0e, sequence, [2, 0xe1, 1]))?;
    let result = (|| -> Result<Option<Battery>, ProviderError> {
        let ready = receive(s, c, 4, 200, 64, |r| race_payload(r, 0x0e, sequence))?;
        if !matches!(ready, Some([0, ..])) || !c.active() {
            return Ok(None);
        }
        s.write(&race_packet(6, sequence + 1, [1, 0, 0x31]))?;
        Ok(receive(s, c, 4, 200, 64, |r| {
            let [0, lo, hi] = race_payload(r, 6, sequence + 1)? else {
                return None;
            };
            let mv = u16::from_le_bytes([lo, hi]);
            (2500..=4500).contains(&mv).then_some(mv)
        })?
        .and_then(|mv| {
            let mut b = p::battery(barracuda_percentage(mv), None)?;
            b.coarse = true;
            Some(b)
        }))
    })();
    // Unwind the dongle's query routing on the same handle, even on timeout or
    // cancellation after entering remote mode. This does not change settings.
    let cleanup = s.write(&race_packet(0x0e, sequence + 2, [2, 0xe1, 0]));
    let reading = result?;
    cleanup?;
    if c.active() {
        let _ = receive(s, c, 4, 200, 64, |r| race_payload(r, 0x0e, sequence + 2))?;
    }
    Ok(reading)
}

fn barracuda_percentage(mv: u16) -> u8 {
    const CURVE: [u16; 11] = [
        3300, 3680, 3750, 3790, 3830, 3870, 3910, 3960, 4020, 4080, 4150,
    ];
    if mv <= CURVE[0] {
        return 0;
    }
    CURVE
        .windows(2)
        .enumerate()
        .find_map(|(index, range)| {
            (mv < range[1])
                .then(|| (index as u16 * 10 + (mv - range[0]) * 10 / (range[1] - range[0])) as u8)
        })
        .unwrap_or(100)
}

fn inzone(s: &mut dyn HidSession, c: &PollContext<'_>) -> Result<Option<Battery>, ProviderError> {
    if !pause(c, 500) || !drain(s, c, 64, 4)? {
        return Ok(None);
    }
    for command in [
        [1, 0, 0xfc, 8, 0x96, 0xc3, 0x21, 1, 1, 1, 0, 0x7d],
        [1, 0, 0xfc, 8, 0x96, 0xc3, 0x41, 2, 1, 1, 0, 0x9e],
        [1, 0, 0xfc, 8, 0x96, 0xc3, 0x41, 4, 1, 1, 0, 0xa0],
    ] {
        if !c.active() {
            return Ok(None);
        }
        s.write(&command)?;
        if !pause(c, 250) {
            return Ok(None);
        }
    }
    let mut received = Vec::with_capacity(77);
    let deadline = c
        .deadline
        .min(c.clock.monotonic() + Duration::from_millis(800));
    let context = PollContext { deadline, ..*c };
    receive(s, &context, 16, 200, 64, |chunk| {
        received.extend_from_slice(chunk);
        let found = received.windows(14).find_map(|r| {
            if r.starts_with(&[4, 0xff, 0x0b, 0, 0x96, 0xc3, 0x14, 4])
                && r[11] <= 1
                && wrapping_sum(&r[4..13]) == r[13]
            {
                p::battery(r[12], Some(r[11] == 1))
            } else {
                None
            }
        });
        let excess = received.len().saturating_sub(13);
        received.drain(..excess);
        found
    })
}
