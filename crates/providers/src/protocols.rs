//! Wire facts ported from providers/*.py, upstream 1.13.0. See docs/protocols.md
//! for original captures and authors. Parsers accept bounded slices, never guessed
//! offsets or a percentage from an unrelated report.
#[derive(Clone, Debug, PartialEq)]
pub struct Battery {
    pub level: u8,
    pub charging: Option<bool>,
    pub coarse: bool,
    pub label: Option<String>,
}
pub fn battery(level: u8, charging: Option<bool>) -> Option<Battery> {
    (level <= 100).then_some(Battery {
        level,
        charging,
        coarse: false,
        label: None,
    })
}
pub fn padded(head: &[u8], length: usize) -> Vec<u8> {
    let mut data = vec![0; length.max(head.len())];
    data[..head.len()].copy_from_slice(head);
    data
}
pub fn wl_feature(r: &[u8]) -> Option<Battery> {
    for i in 5..r.len().saturating_sub(2) {
        if r[i] == 0x83
            && r[i - 1] == 0
            && r[i - 2] == 2
            && [0xa1, 0xa2].contains(&r[i - 5])
            && let Some(b) = battery(r[i + 2], Some(r[i + 1] != 0))
        {
            return Some(b);
        }
    }
    None
}
pub fn wl_heartbeat(r: &[u8]) -> Option<Battery> {
    for offset in [0, 1] {
        if r.get(offset..offset + 2) == Some(&[3, 0][..]) && r.len() >= offset + 4 {
            return battery(r[offset + 2], Some(r[offset + 3] != 0));
        }
    }
    None
}
pub fn gwolves_old(r: &[u8]) -> Option<Battery> {
    for offset in [1, 0] {
        if r.get(offset..offset + 3) == Some(&[0xa1, 2, 0x8f][..]) && r.len() > offset + 5 {
            return battery(r[offset + 5], Some(r[offset + 4] != 0));
        }
    }
    None
}
pub fn hyperx3s(r: &[u8]) -> Option<(u8, u8)> {
    if r.len() < 7 {
        return None;
    }
    let (kind, value) = match r[0] {
        0x0c => (r[5], r[6]),
        0x0d => (
            match r[4] {
                1 => 6,
                10 => 0x48,
                _ => return None,
            },
            r[5],
        ),
        _ => return None,
    };
    match kind {
        6 if value <= 100 => Some((kind, value)),
        0x48 => Some((kind, value)),
        _ => None,
    }
}
pub fn steelseries_elite(r: &[u8]) -> Option<(Option<Battery>, Option<u8>)> {
    if r.len() > 15 && r[..2] == [1, 0xb0] {
        return Some((battery(r[6], Some(r[15] == 2)), Some(r[14])));
    }
    if r.len() < 5 || r[0] != 7 {
        return None;
    }
    match r[1] {
        0xb7 => battery(r[2], Some(r[4] == 2)).map(|b| (Some(b), None)),
        0xb5 => Some((None, Some(r[4]))),
        _ => None,
    }
}
pub fn centurion_frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 61);
    let mut r = padded(&[0x51, (payload.len() + 1) as u8, 0], 64);
    r[3..3 + payload.len()].copy_from_slice(payload);
    r
}
pub fn centurion_payload(r: &[u8]) -> Option<&[u8]> {
    if r.len() < 4 || r[0] != 0x51 || r[1] <= 1 || usize::from(r[1]) + 2 > r.len() {
        return None;
    }
    Some(&r[3..2 + usize::from(r[1])])
}
pub fn centurion_battery(data: &[u8]) -> Option<Battery> {
    battery(
        *data.first()?,
        Some(data.get(2).is_some_and(|n| [1, 2, 3].contains(n))),
    )
}
pub fn centurion_legacy(r: &[u8]) -> Option<Battery> {
    if r.len() >= 13 && r[..2] == [0x51, 0x0b] && r[8] == 4 {
        battery(r[10], Some(r[12] == 2))
    } else {
        None
    }
}
pub fn pulsar(r: &[u8]) -> Option<Battery> {
    if r.len() < 17
        || r[..2] != [8, 4]
        || r[..17].iter().fold(0u8, |s, x| s.wrapping_add(*x)) != 0x55
    {
        return None;
    }
    battery(r[6], Some(r[7] != 0))
}
pub fn audeze_echo_only(frames: &[Vec<u8>]) -> bool {
    frames.len() >= 5
        && frames
            .iter()
            .all(|r| r.len() >= 4 && r[0] == 7 && r[3..].iter().all(|b| *b == 0))
}
pub fn pulsar_request() -> Vec<u8> {
    let mut d = padded(&[8, 4], 17);
    d[16] = 0x55u8.wrapping_sub(d[..16].iter().fold(0u8, |s, x| s.wrapping_add(*x)));
    d
}
pub fn razer_request(tid: u8, command: u8) -> Vec<u8> {
    let mut d = vec![0; 91];
    d[2] = tid;
    d[6] = 2;
    d[7] = 7;
    d[8] = command;
    d[89] = d[3..89].iter().fold(0, |crc, b| crc ^ b);
    d
}
pub fn razer_reply(r: &[u8], command: u8) -> Option<(u8, Option<u8>)> {
    let r = if r.len() >= 91 { &r[1..] } else { r };
    if r.len() < 90 {
        return None;
    }
    Some((r[0], (r[6] == 7 && r[7] == command).then_some(r[9])))
}
pub fn pa_request(barracuda: bool, command: u8, remote: Option<bool>) -> Vec<u8> {
    let mut d = vec![0; 64];
    d[0] = if barracuda { 1 } else { 2 };
    d[1] = 0x80;
    d[2] = if remote.is_some() { 7 } else { 8 };
    if barracuda {
        d[3] = 0x50;
        d[4] = 0x41;
        d[5] = if remote.is_some() { 0x0e } else { 8 };
        d[6] = 8;
        d[7] = if remote.is_some() { 2 } else { 3 };
        d[8] = command;
        if let Some(on) = remote {
            d[9] = u8::from(on);
        }
    } else {
        d[5] = 0x50;
        d[6] = 0x41;
        d[7] = if remote.is_some() { 0x0e } else { 8 };
        d[9] = if remote.is_some() { 2 } else { 3 };
        d[10] = command;
        if let Some(on) = remote {
            d[11] = u8::from(on);
        }
    }
    d
}
pub fn pa_reply(r: &[u8], barracuda: bool, command: u8) -> Option<&[u8]> {
    for skip in [0usize, 1] {
        if barracuda && skip == 1 && r.first() == Some(&1) {
            continue;
        }
        if !barracuda && skip == 0 && r.first() != Some(&2) {
            continue;
        }
        let c = (if barracuda { 13 } else { 12 }) - skip;
        if r.len() > c + 3 && r[c] == command && (r[c + 1] == 1 || barracuda && r[c + 1] == 2) {
            let n = usize::from(r[c + 2]);
            if barracuda && (n == 0 || n > 32) {
                continue;
            }
            return r.get(c + 3..(c + 3 + if barracuda { n } else { n.max(1) }).min(r.len()));
        }
    }
    None
}
pub fn asus(r: &[u8], scale: u8) -> Option<Battery> {
    let offset = if r.starts_with(&[0, 0x12, 7]) { 1 } else { 0 };
    let r = r.get(offset..)?;
    if !r.starts_with(&[0x12, 7]) || r.len() < 10 || r[4] == 0 && r[9] == 0 {
        return None;
    }
    let level = if scale == 25 {
        if r[4] > 4 {
            return None;
        }
        r[4] * 25
    } else {
        r[4]
    };
    let mut b = battery(level, Some(r[9] != 0))?;
    b.coarse = scale == 25;
    if b.coarse {
        b.label = Some(format!(
            "about {}%{}",
            b.level,
            if b.charging == Some(true) {
                ", charging"
            } else {
                ""
            }
        ));
    }
    Some(b)
}
pub fn mchose(r: &[u8]) -> Option<Battery> {
    if r.len() < 13 || ![0x11, 0x12].contains(&r[0]) || r[1] != 0xf9 {
        return None;
    }
    let vid = u16::from_le_bytes([r[2] ^ 255, r[3] ^ 255]);
    if ![0x5253, 0x3837].contains(&vid) {
        return None;
    }
    battery(r[11] ^ 255, Some((r[12] ^ 255) != 0))
}
pub fn mchose_g7(r: &[u8]) -> Option<Battery> {
    if r.len() < 10 || !r.starts_with(&[0xaa, 0x30]) {
        None
    } else {
        battery(r[8], Some(r[9] != 0))
    }
}
pub fn astro(r: &[u8]) -> Option<Battery> {
    if r.len() < 7 || r[..2] != [2, 0x0c] || r[4] != 6 {
        None
    } else {
        battery(r[6], r.get(8).map(|b| *b != 0))
    }
}
pub fn hyperx(r: &[u8]) -> Option<Battery> {
    if r.len() < 8 || r[..4] != [6, 0xff, 0xbb, 2] {
        None
    } else {
        battery(r[7], None)
    }
}
pub fn hyperx3(r: &[u8]) -> Option<Battery> {
    if r.len() < 5 || r[0] != 0x66 || ![0x89, 0x0d].contains(&r[1]) || r[2] == 0 && r[3] == 0 {
        None
    } else {
        battery(r[4], None)
    }
}
pub fn hyperx_alpha(r: &[u8]) -> Option<Battery> {
    if r.len() < 7 || r[..2] != [0x51, 2] {
        None
    } else {
        battery(r[2], Some(r[6] & 0x80 != 0))
    }
}
pub fn keychron(r: &[u8]) -> Option<Battery> {
    if r.len() < 21 || r[..2] != [0xb4, 6] {
        None
    } else {
        battery(r[20], None)
    }
}
pub fn jbl(r: &[u8]) -> Option<Battery> {
    if r.len() < 2 || r[0] != 8 {
        None
    } else {
        battery(r[1], None)
    }
}
pub fn am_infinity(r: &[u8]) -> Option<Battery> {
    if r.len() < 4 {
        return None;
    }
    let i = if r[0] == 5 { 3 } else { 2 };
    if r[1..i].iter().any(|b| *b != 0) || r[i] == 0 {
        return None;
    }
    battery(r[i].min(100), None)
}
pub fn corsair(r: &[u8]) -> Option<Battery> {
    if r.len() < 6 {
        return None;
    }
    let level = u16::from_le_bytes([r[4], r[5]]);
    if level == 0 || level > 1000 {
        None
    } else {
        battery((level / 10) as u8, None)
    }
}
pub fn corsair_nxp(r: &[u8]) -> Option<Battery> {
    let r = if r.len() >= 65 { &r[1..] } else { r };
    if r.len() < 6 {
        return None;
    }
    let raw = *r.get(4)?;
    let level = *[0, 15, 30, 50, 100].get(raw as usize)?;
    let mut b = battery(level, None)?;
    b.coarse = true;
    b.label = Some(format!("about {}%", b.level));
    Some(b)
}
pub fn audeze(r: &[u8]) -> Option<Battery> {
    for win in r.windows(5) {
        if win[..4] == [0xd6, 0x0c, 0, 0]
            && let Some(b) = battery(win[4], None)
        {
            return Some(b);
        }
    }
    None
}
pub fn ds4(byte: u8) -> Battery {
    Battery {
        level: (byte & 15).min(10) * 10,
        charging: Some(byte & 16 != 0),
        coarse: true,
        label: None,
    }
}
pub fn dualsense(byte: u8) -> Battery {
    Battery {
        level: (byte & 15).min(10) * 10,
        charging: Some([1, 2].contains(&(byte >> 4))),
        coarse: true,
        label: None,
    }
}
pub fn eightbitdo(byte: u8) -> Option<Battery> {
    let level = byte & 127;
    if level == 0 {
        return None;
    }
    battery(level, Some(byte & 128 != 0))
}
pub fn nintendo(byte: u8) -> Option<Battery> {
    let raw = (byte & 0xe0) >> 4;
    if raw > 8 {
        return None;
    }
    let mut b = battery((u16::from(raw) * 100 / 8) as u8, Some(byte & 16 != 0))?;
    b.coarse = true;
    let word = match raw {
        8 => "full",
        6 => "medium",
        4 => "low",
        2 => "critical",
        _ => "empty",
    };
    b.label = Some(format!(
        "about {}% ({word}){}",
        b.level,
        if b.charging == Some(true) {
            ", charging"
        } else {
            ""
        }
    ));
    Some(b)
}
pub fn steelseries(r: &[u8], variant: &str) -> Option<Battery> {
    match variant {
        "parse_nova7" | "parse_nova7_discrete" => {
            if r.len() < 4 || r[0] != 0xb0 || r[1] != 3 || r[3] == 0 {
                return None;
            }
            let mut b = battery(r[2].min(100), Some([1, 2].contains(&r[3])))?;
            if variant.ends_with("discrete") {
                b.level = r[2].min(4) * 25;
                b.coarse = true
            }
            Some(b)
        }
        "parse_nova5" => {
            if r.len() < 5 || r[0] != 0xb0 || r[1] == 2 {
                return None;
            }
            battery(r[3].min(100), Some(r[4] == 1))
        }
        "parse_arctis7_plus" => {
            if r.len() < 4 || r[0] != 0xb0 || r[1] == 1 {
                return None;
            }
            let mut b = battery(r[2].min(4) * 25, Some(r[3] == 1))?;
            b.coarse = true;
            Some(b)
        }
        "parse_gamebuds" => {
            if r.len() < 7 || r[0] != 0xb0 {
                return None;
            }
            let level = (0..2).filter(|i| r[3 + i] == 3).map(|i| r[5 + i]).min()?;
            battery(level.min(100), Some(false))
        }
        "parse_aerox3" => {
            let r = if r.first() == Some(&0) { &r[1..] } else { r };
            if r.len() < 2 || r[0] != 0xd2 {
                return None;
            }
            let raw = r[1] & 127;
            if raw == 0 {
                return None;
            }
            battery(
                if raw > 21 {
                    raw.min(100)
                } else {
                    (raw - 1) * 5
                },
                Some(r[1] & 128 != 0),
            )
        }
        "parse_rival3" => {
            let r = if r.first() == Some(&0) { &r[1..] } else { r };
            if r.len() >= 4 && r[0] == 0xaa {
                return battery(r[1], Some(r[3] != 0));
            }
            if r.len() >= 3 && r[2] <= 1 {
                battery(r[0], Some(r[2] == 1))
            } else {
                None
            }
        }
        _ => None,
    }
}
pub fn voltage_to_percent(mv: u16) -> u8 {
    const CURVE: &[(u16, u8)] = &[
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    if mv >= CURVE[0].0 {
        return 100;
    }
    for pair in CURVE.windows(2) {
        let (hi, hp) = pair[0];
        let (lo, lp) = pair[1];
        if mv >= lo {
            return (f64::from(lp) + f64::from(mv - lo) * f64::from(hp - lp) / f64::from(hi - lo))
                .round_ties_even() as u8;
        }
    }
    0
}
pub fn logitech(feature: u16, r: &[u8]) -> Option<Battery> {
    if r.len() < 3 {
        return None;
    }
    match feature {
        0x1000 => {
            if r[0] == 0 {
                return None;
            }
            battery(r[0], Some([1, 2, 3, 4].contains(&r[2])))
        }
        0x1004 => {
            let charging = Some([1, 2, 3, 4].contains(&r[2]));
            if r[0] > 0 {
                return battery(r[0], charging);
            }
            let level = match r[1] {
                8 => 90,
                4 => 50,
                2 => 20,
                1 => 5,
                _ => return None,
            };
            let mut b = battery(level, charging)?;
            b.coarse = true;
            let word = match r[1] {
                8 => "full",
                4 => "good",
                2 => "low",
                _ => "critical",
            };
            b.label = Some(format!("about {}% ({word})", b.level));
            Some(b)
        }
        0x1001 | 0x1f20 => {
            let mv = u16::from_be_bytes([r[0], r[1]]);
            if mv < 2500 || feature == 0x1f20 && r[2] & 1 == 0 {
                return None;
            }
            battery(
                voltage_to_percent(mv),
                Some(if feature == 0x1001 {
                    r[2] & 128 != 0
                } else {
                    r[2] & 2 != 0
                }),
            )
        }
        _ => None,
    }
}
