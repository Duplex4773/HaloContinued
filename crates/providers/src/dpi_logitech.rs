//! HID++ DPI and narrowly scoped onboard-profile updates. See docs/dpi-controls.md.
use crate::{
    controls::GuardedSession,
    dpi::{BackupWriter, error, prepare},
    provider::{is_bluetooth, receiver_key},
};
use hb_core::*;
use std::{collections::BTreeMap, time::Duration};

fn receiver(pid: u16) -> bool {
    matches!(pid, 0xc539 | 0xc53a | 0xc53f | 0xc547 | 0xc548 | 0xc54d)
}
fn collection(i: &HidInfo) -> bool {
    i.vendor_id == 0x046d
        && i.usage_page == 0xff00
        && matches!(i.usage, 1 | 2)
        && !is_bluetooth(&i.path)
        && (receiver(i.product_id) || matches!(i.product_id, 0xc094 | 0xc098 | 0xc09b | 0xc0a0))
}
struct Link<'a> {
    long: GuardedSession<'a>,
    short: Option<GuardedSession<'a>>,
    context: &'a PollContext<'a>,
    slot: u8,
    counter: u8,
}
impl Link<'_> {
    fn call(
        &mut self,
        feature: u8,
        function: u8,
        params: &[u8],
        minimum: usize,
    ) -> Result<Vec<u8>, ProviderError> {
        if !self.context.active() || params.len() > 16 {
            return Err(error("DPI request cancelled or invalid."));
        }
        self.counter = self.counter % 15 + 1;
        let token = (function << 4) | self.counter;
        let mut p = [0; 20];
        p[..4].copy_from_slice(&[0x11, self.slot, feature, token]);
        p[4..4 + params.len()].copy_from_slice(params);
        self.long.write(&p)?;
        let end = (self.context.clock.monotonic() + Duration::from_millis(600))
            .min(self.context.deadline);
        while self.context.active() && self.context.clock.monotonic() < end {
            for channel in 0..2 {
                let r = if channel == 0 {
                    self.long.read(64, Duration::ZERO)?
                } else if let Some(short) = &mut self.short {
                    short.read(64, Duration::ZERO)?
                } else {
                    continue;
                };
                if r.len() < 4 || r[1] != self.slot {
                    continue;
                }
                if matches!(r[2], 0x8f | 0xff) && r.len() >= 6 && r[3] == feature && r[4] == token {
                    return Err(error(&format!(
                        "The mouse rejected the DPI request (device error {}). No retry was made.",
                        r[5]
                    )));
                }
                if r[2] != feature || r[3] != token {
                    continue;
                }
                if !((r[0] == 0x11 && r.len() == 20) || (r[0] == 0x10 && r.len() == 7))
                    || r.len() < 4 + minimum
                {
                    return Err(error("Incomplete DPI response."));
                }
                return Ok(r[4..].to_vec());
            }
            self.context.sleep(Duration::from_millis(5));
        }
        Err(error(
            "No matching DPI reply. Wake the mouse and read again.",
        ))
    }
    fn feature(&mut self, id: u16) -> Result<Option<u8>, ProviderError> {
        let r = self.call(0, 0, &id.to_be_bytes(), 1)?;
        Ok((r[0] != 0).then_some(r[0]))
    }
    fn identity(&mut self) -> Result<Option<(String, u16)>, ProviderError> {
        let Some(f) = self.feature(0x0003)? else {
            return Ok(None);
        };
        let r = self.call(f, 0, &[], 13)?;
        if r[1..5].iter().all(|b| *b == 0) {
            return Ok(None);
        }
        let mut offset = 7;
        let (mut wireless, mut usb) = (None, None);
        for bit in [1, 2, 4, 8] {
            if r[6] & bit != 0 {
                if offset + 2 > 13 {
                    return Err(error("Unknown mouse identity layout."));
                }
                let id = u16::from_be_bytes([r[offset], r[offset + 1]]);
                offset += 2;
                if bit == 4 {
                    wireless = Some(id)
                } else if bit == 8 {
                    usb = Some(id)
                }
            }
        }
        // Do not derive authorization from a receiver PID or display name.
        let pairs = [
            (0x4074, None),
            (0x4093, Some(0xc094)),
            (0x409f, Some(0xc098)),
            (0x40a9, Some(0xc09b)),
            (0x40b8, Some(0xc0a0)),
        ];
        let Some((model, _)) = pairs
            .iter()
            .find(|(w, u)| wireless == Some(*w) && (usb.is_none() || usb == *u))
        else {
            return Ok(None);
        };
        Ok(Some((
            r[1..5].iter().map(|b| format!("{b:02X}")).collect(),
            *model,
        )))
    }
}

pub(crate) fn execute<'a>(
    request: &DpiRequest,
    hid: &'a dyn HidTransport,
    context: &'a PollContext<'a>,
    backup: &mut BackupWriter<'_>,
    out: &mut DpiOutcome,
) -> Result<(), ProviderError> {
    let d = &request.target.device;
    let unit = d
        .serial
        .as_deref()
        .ok_or_else(|| error("Verified mouse identity required."))?;
    let infos = hid.enumerate(0x046d)?;
    if infos.len() > 512 {
        return Err(error("Too many mouse collections."));
    }
    let mut groups: BTreeMap<(u16, String), Vec<&HidInfo>> = BTreeMap::new();
    for i in infos.iter().filter(|i| collection(i)) {
        groups
            .entry((i.product_id, receiver_key(i)))
            .or_default()
            .push(i);
    }
    let mut found = None;
    for entries in groups.values() {
        let longs: Vec<_> = entries.iter().copied().filter(|i| i.usage == 2).collect();
        if longs.is_empty() {
            continue;
        }
        if longs.len() != 1 {
            return Err(error("Ambiguous mouse control collection."));
        }
        let i = longs[0];
        if d.container.as_ref().is_some_and(|c| {
            i.container
                .as_ref()
                .is_none_or(|o| !c.eq_ignore_ascii_case(o))
        }) {
            continue;
        }
        let shorts: Vec<_> = entries.iter().copied().filter(|i| i.usage == 1).collect();
        if shorts.len() > 1 {
            return Err(error("Ambiguous mouse reply collection."));
        }
        let mut link = Link {
            long: GuardedSession::new(hid.open(i)?, hid, context, request.target.generation),
            short: shorts
                .first()
                .map(|s| {
                    hid.open(s).map(|session| {
                        GuardedSession::new(session, hid, context, request.target.generation)
                    })
                })
                .transpose()?,
            context,
            slot: 255,
            counter: 0,
        };
        // Receiver routing is resolved from hardware unit IDs, not from names.
        let slots: Vec<_> = if receiver(i.product_id) {
            (1..=6).collect()
        } else {
            vec![255]
        };
        let mut matching_slot = None;
        for slot in slots {
            link.slot = slot;
            match link.identity() {
                Ok(Some((actual, model))) if actual.eq_ignore_ascii_case(unit) => {
                    if matching_slot.is_some() {
                        return Err(error("Duplicate paired mouse identity."));
                    }
                    matching_slot = Some((slot, model));
                }
                Ok(_) => {}
                // An empty receiver slot reports HID++ unknown device (8).
                Err(e) if e.message.contains("(device error 8)") => {}
                Err(e) => return Err(e),
            }
        }
        if let Some((slot, model)) = matching_slot {
            if found.is_some() {
                return Err(error(
                    "Multiple connections match this mouse. Disconnect the extra connection first.",
                ));
            }
            link.slot = slot;
            found = Some((link, model));
        }
    }
    let (mut link, model) =
        found.ok_or_else(|| error("No supported mouse matches this hardware identity."))?;
    transaction(&mut link, model, unit, request, backup, out)
}

/// Decode ranges without expanding tens of thousands of individual DPI choices.
fn ranges(raw: &[u8]) -> Result<Vec<(u16, u16, u16)>, ProviderError> {
    let words: Vec<_> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    let mut result = Vec::new();
    let mut n = 0;
    while n < words.len() {
        let v = words[n];
        n += 1;
        if v == 0 {
            return if result.is_empty() {
                Err(error("Empty DPI range."))
            } else {
                Ok(result)
            };
        }
        if v & 0xe000 == 0xe000 {
            let step = v & 0x1fff;
            let start = result
                .last()
                .map(|r: &(u16, u16, u16)| r.1)
                .ok_or_else(|| error("Invalid DPI range."))?;
            let end = *words.get(n).ok_or_else(|| error("Truncated DPI range."))?;
            n += 1;
            if step == 0 || end <= start {
                return Err(error("Invalid DPI range."));
            }
            result.push((start, end, step));
        } else {
            result.push((v, v, 1));
        }
    }
    Err(error("Unterminated DPI range."))
}
fn accepts(ranges: &[(u16, u16, u16)], value: u16) -> bool {
    ranges
        .iter()
        .any(|&(lo, hi, step)| value >= lo && value <= hi && (value - lo).is_multiple_of(step))
}

struct Sensor {
    feature: u8,
    extended: bool,
    y: bool,
    lod: bool,
    x_ranges: Vec<(u16, u16, u16)>,
    y_ranges: Vec<(u16, u16, u16)>,
}
impl Sensor {
    fn discover(link: &mut Link<'_>) -> Result<Self, ProviderError> {
        let extended = link.feature(0x2202)?;
        let (feature, extended) = if let Some(f) = extended {
            (f, true)
        } else {
            (
                link.feature(0x2201)?
                    .ok_or_else(|| error("DPI adjustment is unavailable on this mouse."))?,
                false,
            )
        };
        if link.call(feature, 0, &[], 1)?[0] != 1 {
            return Err(error("Unsupported mouse sensor layout."));
        }
        let (y, lod) = if extended {
            let caps = link.call(feature, 1, &[0], 3)?;
            if caps[0] != 0 {
                return Err(error("Unexpected DPI sensor."));
            }
            (caps[2] & 1 != 0, caps[2] & 2 != 0)
        } else {
            (false, false)
        };
        let mut read_ranges = |axis| -> Result<_, ProviderError> {
            if !extended {
                let r = link.call(feature, 1, &[0], 3)?;
                return ranges(&r[1..]);
            }
            let mut raw = Vec::new();
            for page in 0..16 {
                let r = link.call(feature, 2, &[0, axis, page], 16)?;
                if r[..3] != [0, axis, page] {
                    return Err(error("Unrelated DPI range response."));
                }
                raw.extend_from_slice(&r[3..]);
                if r[r.len() - 2..] == [0, 0] {
                    return ranges(&raw);
                }
            }
            Err(error("DPI range response exceeded its limit."))
        };
        let x_ranges = read_ranges(0)?;
        let y_ranges = if y { read_ranges(1)? } else { x_ranges.clone() };
        Ok(Self {
            feature,
            extended,
            y,
            lod,
            x_ranges,
            y_ranges,
        })
    }
    fn read(&self, link: &mut Link<'_>) -> Result<(DpiValue, u8), ProviderError> {
        let r = link.call(
            self.feature,
            if self.extended { 5 } else { 2 },
            &[0],
            if self.extended { 10 } else { 5 },
        )?;
        if r[0] != 0 {
            return Err(error("Unexpected DPI sensor."));
        }
        let word = |n| u16::from_be_bytes([r[n], r[n + 1]]);
        let x = if word(1) == 0 { word(3) } else { word(1) };
        let y = if self.y {
            if word(5) == 0 { word(7) } else { word(5) }
        } else {
            x
        };
        if !accepts(&self.x_ranges, x) || !accepts(&self.y_ranges, y) {
            return Err(error("Mouse DPI does not match its advertised range."));
        }
        if self.lod && r[9] > 2 {
            return Err(error("Unknown lift-off distance. Nothing was changed."));
        }
        Ok((DpiValue { x, y }, if self.lod { r[9] } else { 0 }))
    }
}
fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}
fn read_sector(
    link: &mut Link<'_>,
    feature: u8,
    sector: u16,
    size: usize,
) -> Result<Vec<u8>, ProviderError> {
    if !(32..=512).contains(&size) {
        return Err(error("Unsupported onboard profile size."));
    }
    let mut raw = vec![0; size];
    let mut start = 0;
    while start < size {
        let offset = start.min(size - 16);
        let mut p = [0; 4];
        p[..2].copy_from_slice(&sector.to_be_bytes());
        p[2..].copy_from_slice(&(offset as u16).to_be_bytes());
        let r = link.call(feature, 5, &p, 16)?;
        raw[offset..offset + 16].copy_from_slice(&r[..16]);
        start = offset + 16;
    }
    if crc(&raw[..size - 2]) != u16::from_be_bytes([raw[size - 2], raw[size - 1]]) {
        return Err(error(
            "The onboard profile checksum is invalid. Nothing was changed.",
        ));
    }
    Ok(raw)
}
fn active(link: &mut Link<'_>, feature: u8) -> Result<(u8, u16, u8), ProviderError> {
    let mode = link.call(feature, 2, &[], 1)?[0];
    if !matches!(mode, 1 | 2) {
        return Err(error("Mouse profile mode is unknown."));
    }
    let r = link.call(feature, 4, &[], 2)?;
    let profile = u16::from_be_bytes([r[0], r[1]]);
    let stage = link.call(feature, 11, &[], 1)?[0];
    if stage >= 5 {
        return Err(error("Unknown active DPI stage."));
    }
    Ok((mode, profile, stage))
}
fn transaction(
    link: &mut Link<'_>,
    model: u16,
    unit: &str,
    request: &DpiRequest,
    backup: &mut BackupWriter<'_>,
    out: &mut DpiOutcome,
) -> Result<(), ProviderError> {
    let sensor = Sensor::discover(link)?;
    out.limits = Some(DpiLimits {
        x: sensor.x_ranges.clone(),
        y: sensor.y_ranges.clone(),
        from_model: false,
    });
    let feature = link
        .feature(0x8100)?
        .ok_or_else(|| error("The active mouse profile cannot be verified."))?;
    let (mode, profile, stage) = active(link, feature)?;
    let (value, lod) = sensor.read(link)?;
    let before = DpiState {
        value,
        profile: Some(profile),
        stage: Some(stage),
    };
    out.current = Some(before.clone());
    let Some(wanted) = request.value else {
        return Ok(());
    };
    if !accepts(&sensor.x_ranges, wanted.x)
        || !accepts(&sensor.y_ranges, wanted.y)
        || (!sensor.y && wanted.x != wanted.y)
    {
        return Err(error("Choose a DPI value supported by this mouse."));
    }
    let mut bytes = Vec::new();
    let mut format = None;
    let mut offset = 0;
    if mode == 1 {
        let descriptor = link.call(feature, 0, &[], 9)?;
        let f = descriptor[1];
        let size = usize::from(u16::from_be_bytes([descriptor[7], descriptor[8]]));
        // Known classic layouts and the documented HERO 2 layout only, never future formats.
        if descriptor[0] != 1
            || !(matches!(model, 0x40a9 | 0x40b8) && f == 7
                || !matches!(model, 0x40a9 | 0x40b8) && (1..=5).contains(&f))
            || profile == 0
            || profile >= 256
            || profile > u16::from(descriptor[6])
        {
            return Err(error(
                "This onboard profile format is not supported yet. Nothing was changed.",
            ));
        }
        bytes = read_sector(link, feature, profile, size)?;
        format = Some(f);
        offset = if f == 7 {
            4 + usize::from(stage) * 5
        } else {
            3 + usize::from(stage) * 2
        };
        let x = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let y = if f == 7 {
            u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]])
        } else {
            x
        };
        if (DpiValue { x, y }) != value {
            return Err(error(
                "Current DPI differs from the saved stage. Release DPI-shift buttons and read again.",
            ));
        }
    }
    let Some(wanted) = prepare(
        request,
        &before,
        bytes.clone(),
        format,
        link.context,
        backup,
        out,
    )?
    else {
        return Ok(());
    };
    if link
        .identity()?
        .is_none_or(|(actual, m)| !actual.eq_ignore_ascii_case(unit) || m != model)
        || active(link, feature)? != (mode, profile, stage)
        || sensor.read(link)? != (value, lod)
    {
        return Err(error(
            "Mouse DPI or identity changed while preparing the update.",
        ));
    }
    if mode == 1 {
        if read_sector(link, feature, profile, bytes.len())? != bytes {
            return Err(error(
                "The onboard profile changed elsewhere. Read DPI again.",
            ));
        }
        bytes[offset..offset + 2].copy_from_slice(&wanted.x.to_le_bytes());
        if format == Some(7) {
            bytes[offset + 2..offset + 4].copy_from_slice(&wanted.y.to_le_bytes());
        }
        let len = bytes.len();
        let checksum = crc(&bytes[..len - 2]);
        bytes[len - 2..].copy_from_slice(&checksum.to_be_bytes());
        let mut p = [0; 6];
        p[..2].copy_from_slice(&profile.to_be_bytes());
        p[4..].copy_from_slice(&(len as u16).to_be_bytes());
        out.current = None;
        out.may_have_changed = true;
        link.call(feature, 6, &p, 0)?;
        for chunk in bytes.chunks(16) {
            link.call(feature, 7, chunk, 0)?;
        }
        link.call(feature, 8, &[], 0)?;
        if read_sector(link, feature, profile, len)? != bytes {
            return Err(error(
                "The mouse did not confirm the complete DPI profile. No retry was made.",
            ));
        }
        // Re-select only the existing stage; never switch mode or another profile.
        if active(link, feature)? != (mode, profile, stage) {
            return Err(error("The active profile changed during the DPI update."));
        }
        link.call(feature, 12, &[stage], 0)?;
    } else {
        let mut p = vec![0];
        p.extend_from_slice(&wanted.x.to_be_bytes());
        if sensor.extended {
            p.extend_from_slice(&if sensor.y { wanted.y } else { 0 }.to_be_bytes());
            p.push(lod);
        }
        out.current = None;
        out.may_have_changed = true;
        link.call(sensor.feature, if sensor.extended { 6 } else { 3 }, &p, 0)?;
    }
    let (value, after_lod) = sensor.read(link)?;
    if value != wanted || lod != after_lod || active(link, feature)? != (mode, profile, stage) {
        return Err(error(
            "Current DPI could not be verified after the change. Read current DPI before retrying.",
        ));
    }
    out.current = Some(DpiState {
        value,
        profile: Some(profile),
        stage: Some(stage),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_ranges_reject_malformed_steps_and_preserve_exact_choices() {
        for words in [
            vec![0],
            vec![0xe032, 1000, 0],
            vec![100, 0xe000, 1000, 0],
            vec![100, 0xe032],
            vec![100, 0xe032, 50, 0],
            vec![100, 200],
        ] {
            let bytes: Vec<_> = words.into_iter().flat_map(u16::to_be_bytes).collect();
            assert!(ranges(&bytes).is_err());
        }
        let ranges = ranges(&[0, 100, 0xe0, 50, 0x27, 0x10, 0, 0]).unwrap();
        assert!(accepts(&ranges, 100));
        assert!(accepts(&ranges, 10000));
        assert!(!accepts(&ranges, 101));
        assert!(!accepts(&ranges, 10050));
        assert_eq!(ranges.len(), 2);
        assert_eq!(crc(b"123456789"), 0x29b1);
    }
}
