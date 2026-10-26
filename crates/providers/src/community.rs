//! Battery protocols from hardware-confirmed parent pull requests.
//!
//! Pinned revisions and exact identities live in ../reviewed-devices.json.
//! Credits: HaloBattery contributors; Linux HID/SDL, G-Helper, HeadsetControl,
//! HyperHeadset, Headroom, MousePower, XPanel/voiddog30 and vendor captures.
//! This module performs battery/identity queries only, never configuration writes.
use crate::{
    catalog::{DEVICES, Device},
    protocols::{self as p, Battery},
    provider::{is_bluetooth, trusted_identity},
};
use hb_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const MAX_DEVICES: usize = 64;
type Endpoint = (u16, u16, String);

#[derive(Default)]
pub(crate) struct Community {
    last: BTreeMap<String, (Reading, Duration)>,
    hints: BTreeMap<Endpoint, Hint>,
    sequence: u16,
    generation: Option<u64>,
    pub diagnostics: Vec<String>,
}
#[derive(Default)]
struct Hint {
    path: Option<String>,
    swept: Option<Duration>,
}

fn legacy_gwolves_collection(i: &HidInfo, infos: &[HidInfo]) -> bool {
    infos.iter().any(|other| {
        other.vendor_id == i.vendor_id
            && other.product_id == i.product_id
            && other.feature_length == Some(65)
            && trusted_identity(other) == trusted_identity(i)
    })
}
pub(crate) fn handles(d: &Device, i: &HidInfo, infos: &[HidInfo]) -> bool {
    d.variant.starts_with("pr:")
        || d.provider == "gwolves"
            && d.pid == 0x3854
            && !legacy_gwolves_collection(i, infos)
            && infos.iter().any(|other| {
                other.vendor_id == i.vendor_id
                    && other.product_id == i.product_id
                    && (other.usage_page, other.usage) == (0xff02, 2)
                    && other.output_length == Some(17)
                    && trusted_identity(other) == trusted_identity(i)
            })
}
fn protocol(d: &Device) -> &str {
    if d.provider == "gwolves" {
        "compx"
    } else {
        d.variant.strip_prefix("pr:").unwrap_or("")
    }
}
fn kind(d: &Device) -> &'static str {
    match protocol(d) {
        "kingston" | "k65" | "strix" | "turtle" | "skullcandy" | "v9" | "turbo" => {
            if protocol(d) == "k65" {
                "keyboard"
            } else {
                "headset"
            }
        }
        "flow" | "atk-keyboard" | "aula-keyboard" => "keyboard",
        "rapoo" if [0x1924, 0x4824].contains(&d.pid) => "keyboard",
        "shield" => "gamepad",
        _ => "mouse",
    }
}
fn family(d: &Device) -> u16 {
    match protocol(d) {
        "nearlink" => 0x108c,
        "ajazz" => 0x5c2f,
        "aula-mouse" => 0x22a0,
        "turbo" => 0x6008,
        "rapoo" => {
            if [0x1413, 0x4613].contains(&d.pid) {
                0x1413
            } else {
                0x1924
            }
        }
        "turtle" => {
            if [0x229b, 0x229e].contains(&d.pid) {
                0x229b
            } else {
                0x2251
            }
        }
        _ => d.pid,
    }
}
fn key(d: &Device, i: &HidInfo) -> String {
    format!("{}:{:04x}:{}", d.provider, family(d), trusted_identity(i))
}
fn candidate(d: &Device, i: &HidInfo) -> bool {
    let usage = (i.usage_page, i.usage);
    let out = |length| i.output_length.is_none_or(|n| n == length);
    match protocol(d) {
        "flow" | "compx" => usage == (0xff02, 2) && out(17),
        "kingston" | "turtle" | "skullcandy" => usage == (0xff13, 1),
        "k65" => usage == (0xff42, 1),
        "strix" => i.feature_length.is_some_and(|n| n >= 64),
        "nearlink" => usage == (if d.pid == 0x108c { 0xffb3 } else { 0xffb2 }, 1) && out(64),
        "atk-keyboard" => {
            usage == (0xff1c, 0x92)
                && i.product == "ATK V75X 2.4G Dongle"
                && i.output_length.is_none_or(|n| n >= 32)
        }
        "aula-mouse" => usage == (0xff1c, 0x92) && i.output_length.is_none_or(|n| n >= 32),
        "aula-keyboard" => usage == (0xff80, 1) && i.output_length.is_none_or(|n| n >= 33),
        "sonix" => i.interface == 3 && i.usage_page >= 0xff00 && i.feature_length == Some(65),
        "inphic" => usage == (0x000a, 0) || usage == (0xff00, 1),
        "rapoo" => i.usage_page >= 0xff00,
        "dareu" => usage == (0xff02, 2),
        "ulx" => i.usage_page == 0xff00 && out(64),
        "ajazz" => {
            usage != (1, 2) && (i.output_length == Some(33) || i.usage_page >= 0xff00 && out(33))
        }
        "v9" | "turbo" => i.usage_page >= 0xff00,
        "shield" => {
            i.output_length.is_none_or(|n| n >= 33) && (i.usage_page >= 0xff00 || usage == (1, 5))
        }
        _ => false,
    }
}
pub(crate) fn queryable(d: &Device, i: &HidInfo, infos: &[HidInfo]) -> bool {
    handles(d, i, infos)
        && if protocol(d) == "omni" {
            i.interface == 2 && i.usage_page == 0xff02
        } else {
            candidate(d, i)
        }
}
fn retain_level(d: &Device) -> bool {
    matches!(protocol(d), "compx" | "sonix" | "dareu" | "nearlink")
}
fn keep_sleeping(d: &Device) -> bool {
    !matches!(
        protocol(d),
        "shield" | "kingston" | "aula-keyboard" | "atk-keyboard" | "skullcandy" | "flow" | "strix"
    )
}

impl Community {
    pub fn is_empty(&self) -> bool {
        self.last.is_empty() && self.hints.is_empty()
    }
    pub fn invalidate(&mut self) {
        self.hints.clear();
        self.generation = None;
    }
    fn log(&mut self, text: impl Into<String>) {
        if self.diagnostics.len() < 100 {
            self.diagnostics.push(text.into());
        }
    }
    pub fn poll(
        &mut self,
        source: &'static str,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        self.diagnostics.clear();
        if !c.active() {
            return Ok(Vec::new());
        }
        if self.generation != Some(hid.generation()) {
            self.hints.clear();
            self.generation = Some(hid.generation());
        }
        let mut groups: BTreeMap<Endpoint, (Device, Vec<&HidInfo>)> = BTreeMap::new();
        for i in infos {
            if let Some(d) = DEVICES
                .iter()
                .find(|d| d.provider == source && d.vid == i.vendor_id && d.pid == i.product_id)
                && queryable(d, i, infos)
                && protocol(d) != "omni"
            {
                let group = (d.vid, d.pid, trusted_identity(i));
                if groups.len() >= MAX_DEVICES && !groups.contains_key(&group) {
                    continue;
                }
                groups
                    .entry(group)
                    .or_insert_with(|| (*d, Vec::new()))
                    .1
                    .push(i);
            }
        }
        let mut result = Vec::new();
        let mut present: BTreeSet<_> = groups.values().map(|(d, mine)| key(d, mine[0])).collect();
        self.hints
            .retain(|endpoint, _| groups.contains_key(endpoint));
        // Enumeration can still prove a receiver is present while its control
        // collection/caps are temporarily unavailable. Retain only previously
        // observed devices; never relax the collection filters to query them.
        // The normal queryable path skips this recovery scan entirely.
        for (cached_key, (last, at)) in &self.last {
            if present.contains(cached_key) {
                continue;
            }
            let device = infos.iter().find_map(|i| {
                DEVICES
                    .iter()
                    .find(|d| d.provider == source && d.vid == i.vendor_id && d.pid == i.product_id)
                    .filter(|d| {
                        d.variant.starts_with("pr:")
                            || d.provider == "gwolves"
                                && d.pid == 0x3854
                                && !legacy_gwolves_collection(i, infos)
                    })
                    .filter(|d| key(d, i) == *cached_key)
            });
            if let Some(d) = device
                && let Some(r) = cached_reading(d, last, *at, c.clock.monotonic())
            {
                present.insert(cached_key.clone());
                result.push(r);
            }
        }
        self.last.retain(|k, _| present.contains(k));
        let mut failures = Vec::new();
        let mut successful = false;
        for (endpoint, (d, mut mine)) in groups.into_iter().take(MAX_DEVICES) {
            if !c.active() {
                break;
            }
            let k = key(&d, mine[0]);
            let now = c.clock.monotonic();
            let passive = matches!(protocol(&d), "inphic" | "rapoo");
            if !self.hints.contains_key(&endpoint) {
                self.hints.insert(endpoint.clone(), Hint::default());
            }
            let hint = self.hints.get_mut(&endpoint).unwrap();
            let known = hint
                .path
                .as_ref()
                .is_some_and(|p| mine.iter().any(|i| &i.path == p));
            let heard = self.last.get(&k).map(|(_, t)| *t);
            let resweep =
                !known || heard.is_none_or(|t| now.saturating_sub(t) >= Duration::from_secs(600));
            let sweep_due = hint.swept.is_none_or(|t| {
                now.checked_sub(t).is_none_or(|elapsed| {
                    elapsed >= Duration::from_secs(if known { 600 } else { 60 })
                })
            });
            mine.sort_by_key(|i| {
                (
                    hint.path.as_ref() != Some(&i.path),
                    if (i.usage_page, i.usage) == (0x000a, 0) {
                        0
                    } else {
                        1
                    },
                    i.interface,
                    i.path.as_str(),
                )
            });
            if passive && resweep && sweep_due {
                hint.swept = Some(now);
            }
            let attempts = if known && !(passive && resweep && sweep_due) {
                1
            } else if passive && !sweep_due {
                0
            } else {
                4
            };
            let mut found = None;
            for (index, i) in mine.iter().take(attempts).enumerate() {
                if !c.active() {
                    break;
                }
                match self.read(&d, i, infos, hid, c, index == 0) {
                    Ok(Some(mut r)) => {
                        successful = true;
                        self.log(format!(
                            "{} {:04x}:{:04x}: {:?}%, charging {:?}",
                            d.name, d.vid, d.pid, r.level, r.charging
                        ));
                        self.hints.get_mut(&endpoint).unwrap().path = Some(i.path.clone());
                        if r.key.is_empty() {
                            r.key = k.clone();
                        }
                        found = Some(r);
                        break;
                    }
                    Ok(None) => {
                        successful = true;
                    }
                    Err(e) => {
                        self.log(format!("{}: {}", d.name, e));
                        failures.push(e);
                    }
                }
            }
            if !c.active() {
                break;
            }
            if let Some(r) = found {
                if let Some(chosen) = merge(&mut result, r)
                    && keep_sleeping(&d)
                {
                    self.last.insert(k, (chosen.clone(), c.clock.monotonic()));
                }
            } else if keep_sleeping(&d)
                && let Some((last, at)) = self.last.get(&k)
                && let Some(r) = cached_reading(&d, last, *at, now)
            {
                let _ = merge(&mut result, r);
            }
        }
        if source == "asus" {
            match self.omni(infos, hid, c) {
                Ok(readings) => {
                    successful |= !readings.is_empty();
                    result.extend(readings);
                }
                Err(e) => {
                    self.log(format!("OMNI: {e}"));
                    failures.push(e);
                }
            }
        }
        if !successful && !failures.is_empty() {
            Err(failures.remove(0))
        } else {
            Ok(result)
        }
    }
    fn read(
        &mut self,
        d: &Device,
        i: &HidInfo,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
        first: bool,
    ) -> Result<Option<Reading>, ProviderError> {
        let mut s = hid.open(i)?;
        if !c.active() {
            return Ok(None);
        }
        let s = &mut *s;
        let b = match protocol(d) {
            "compx" => {
                if !drain(s, c, 17, 4)? {
                    return Ok(None);
                }
                s.write(&p::pulsar_request())?;
                receive(s, c, 4, 250, 17, p::pulsar)?
            }
            "flow" => {
                let mut online = None;
                for cmd in [3, 4] {
                    if !c.active() {
                        return Ok(None);
                    }
                    let mut packet = p::pulsar_request();
                    packet[1] = cmd;
                    packet[5] = 0x80;
                    packet[16] = 0x55u8
                        .wrapping_sub(packet[..16].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
                    s.write(&packet)?;
                    let answer = receive(s, c, 4, 250, 17, |r| compx(r, 8, cmd))?;
                    if cmd == 3 {
                        if answer.as_ref().is_none_or(|b| b.level != 1) {
                            return Ok(None);
                        }
                    } else {
                        online = answer;
                    }
                }
                online
            }
            "sonix" => {
                let mut packet = p::padded(&[0, 0, 8, 0x81, 1], 65);
                packet[63] = 0x8a;
                s.send_feature(&packet)?;
                if !pause(c, 50) {
                    return Ok(None);
                }
                let r = s.feature(0, 65)?;
                if r.len() >= 8 && r[..5] == [0, 1, 8, 0x81, 1] {
                    p::battery(r[7], None)
                } else {
                    None
                }
            }
            "kingston" => {
                let mut b = None;
                for cmd in [2, 3] {
                    if !c.active() {
                        return Ok(None);
                    }
                    let _ = s.input_report(6, 62);
                    if !c.active() {
                        return Ok(None);
                    }
                    s.write(&p::padded(
                        &[
                            6, 0, 2, 0, 0x9a, 0, 0, 0x68, 0x4a, 0x8e, 0x0a, 0, 0, 0, 0xbb, cmd, 0,
                        ],
                        62,
                    ))?;
                    if !pause(c, 100) {
                        return Ok(None);
                    }
                    let r = receive(s, c, 1, 1000, 64, |r| {
                        (r.len() >= 8 && r[0] == 0x0b && r[2..4] == [0xbb, cmd]).then(|| r.to_vec())
                    })?;
                    if cmd == 2 {
                        b = r.as_deref().and_then(|r| p::battery(r[7], None));
                        if b.is_none() {
                            break;
                        }
                    } else if let (Some(b), Some(r)) = (&mut b, r) {
                        b.charging = charge(r[4], &[0], &[1, 2]);
                    }
                }
                b
            }
            "k65" => {
                if !drain(s, c, 65, 4)? {
                    return Ok(None);
                }
                let mut values = Vec::new();
                for _ in 0..2 {
                    if !c.active() {
                        return Ok(None);
                    }
                    s.write(&p::padded(&[0, 9, 2, 0x0f], 65))?;
                    let value = receive(s, c, 6, 500, 65, |r| {
                        crate::provider::bragi_raw(r).filter(|v| (1..=1000).contains(v))
                    })?;
                    if let Some(v) = value {
                        values.push(v / 10);
                    } else {
                        break;
                    }
                }
                if values.len() == 2 && values[0] != values[1] {
                    None
                } else {
                    values.first().and_then(|v| p::battery(*v as u8, None))
                }
            }
            "strix" => {
                let mut b = None;
                for _ in 0..3 {
                    if !c.active() {
                        break;
                    }
                    s.send_feature(&p::padded(
                        &[0xff, 8, 0, 0xfd, 4, 0x12, 0xf1, 3, 0x52, 1],
                        64,
                    ))?;
                    if !pause(c, 35) {
                        break;
                    }
                    b = strix(&s.feature(0xff, 64)?);
                    if b.is_some() {
                        break;
                    }
                }
                b
            }
            "nearlink" | "atk-keyboard" => {
                self.sequence = self.sequence.wrapping_add(1).max(1);
                let seq = self.sequence;
                let report = if protocol(d) == "nearlink" && d.pid == 0x108c {
                    5
                } else {
                    4
                };
                if protocol(d) == "nearlink" && !drain(s, c, 64, 4)? {
                    return Ok(None);
                }
                s.write(&p::padded(
                    &[report, seq as u8, (seq >> 8) as u8, 0x1a],
                    if protocol(d) == "nearlink" { 64 } else { 32 },
                ))?;
                receive(
                    s,
                    c,
                    if protocol(d) == "nearlink" { 4 } else { 10 },
                    if protocol(d) == "nearlink" { 250 } else { 100 },
                    64,
                    |r| sequence_battery(r, report, seq, protocol(d) == "nearlink"),
                )?
            }
            "aula-keyboard" => {
                s.write(&p::padded(&[0, 0xaa, 0x10], 33))?;
                receive(s, c, 15, 100, 33, aula_keyboard)?
            }
            "aula-mouse" => self.aula_mouse(s, c)?,
            "dareu" => {
                let reply = infos.iter().find(|other| {
                    other.vendor_id == i.vendor_id
                        && other.product_id == i.product_id
                        && trusted_identity(other) == trusted_identity(i)
                        && (other.usage_page, other.usage) == (0xff01, 0)
                });
                // The input and command handles must belong to this same receiver.
                let Some(reply) = reply else {
                    return Ok(None);
                };
                let mut input = hid.open(reply)?;
                if !drain(&mut *input, c, 17, 4)? {
                    return Ok(None);
                }
                s.send_feature(&p::pulsar_request())?;
                receive(&mut *input, c, 4, 250, 17, |r| compx(r, 9, 4))?.map(|mut b| {
                    b.charging = None;
                    b
                }) // Owner saw the same flag while charging.
            }
            "inphic" => {
                let mut pending = false;
                let mut answer = None;
                for _ in 0..if first { 5 } else { 2 } {
                    if !c.active() {
                        break;
                    }
                    let r = read(s, c, 100, 250)?;
                    if let Some((level, charging, coarse)) = inphic(&r, d.pid) {
                        pending |= charging == Some(true);
                        if let Some(level) = level {
                            let mut b =
                                p::battery(level, if pending { Some(true) } else { charging })
                                    .unwrap();
                            b.coarse = coarse;
                            answer = Some(b);
                            break;
                        }
                    }
                }
                if answer.is_none()
                    && pending
                    && let Some((last, _)) = self.last.get(&key(d, i))
                {
                    answer = last.level.and_then(|level| p::battery(level, Some(true)));
                }
                answer
            }
            "rapoo" => receive(s, c, if first { 12 } else { 4 }, 250, 100, rapoo)?,
            "v9" | "turbo" => self.v9(d, s, c)?,
            "ulx" => self.ulx(s, c)?,
            "ajazz" => self.ajazz(s, c, i.vendor_id == 0x248a)?,
            "skullcandy" => {
                let before = s.input_report(7, 62).unwrap_or_default();
                if !c.active() {
                    return Ok(None);
                }
                s.write(&p::padded(&[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c, 0], 62))?;
                let mut b = None;
                for attempt in 0..12 {
                    if !pause(c, 60) {
                        break;
                    }
                    let r = s.input_report(7, 62)?;
                    if r == before && attempt < 11 {
                        continue;
                    }
                    b = skullcandy(&r);
                    if b.is_some() {
                        break;
                    }
                }
                b
            }
            "turtle" => self.turtle(d, s, c)?,
            "shield" => {
                let Some(level) = shield_query(s, c, 7)?.as_deref().and_then(shield_level) else {
                    return Ok(None);
                };
                let charging = shield_query(s, c, 0x3a)
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(shield_charging);
                let serial = shield_query(s, c, 0x10)
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(shield_serial);
                let mut r = reading(d, i, p::battery(level, charging).unwrap(), c);
                if let Some(serial) = serial {
                    r.key = format!("shield:board:{serial}");
                    r.serial = Some(serial);
                }
                return Ok(Some(r));
            }
            _ => None,
        };
        Ok(b.map(|b| reading(d, i, b, c)))
    }
    fn aula_mouse(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let mut down = false;
        let mut command = |s: &mut dyn HidSession, cmd| -> Result<Option<Vec<u8>>, ProviderError> {
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(&[4, 0, 0, cmd], 32))?;
            receive(s, c, 10, 100, 64, |r| {
                if r.len() > 8 && r[..4] == [4, 0xaa, 0, 0xaa] {
                    down = r[8] != 0xff;
                    return None;
                }
                (r.len() >= 4
                    && r[0] == 4
                    && (r[3] == cmd || cmd == 0x1a && r[3] == 0)
                    && (cmd != 0x1a || r.len() > 9 && r[5..7] == [0, 0]))
                .then(|| r.to_vec())
            })
        };
        if command(s, 1)?.is_none() {
            return Ok(None);
        }
        let answer = command(s, 0x1a);
        // END closes the battery transaction even when its answer is absent.
        if c.active() {
            let _ = command(s, 2)?;
        }
        let r = answer?;
        if down {
            return Ok(None);
        }
        Ok(r.and_then(|r| {
            (r[8] > 0)
                .then(|| p::battery(r[8], charge(r[9], &[0], &[1, 2])))
                .flatten()
        }))
    }
    fn v9(
        &mut self,
        d: &Device,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let turbo = protocol(d) == "turbo";
        let requests: &[&[u8]] = if turbo {
            &[&[0x55, 0xaa, 1, 0, 1, 0, 1, 0x0b, 0], &[0x55, 0x65, 1]]
        } else {
            &[&[0x55, 0x65, 1]]
        };
        for &head in requests {
            if !c.active() {
                break;
            }
            let packet = p::padded(head, 64);
            if let Err(e) = s.write(&packet) {
                if !c.active() {
                    return Ok(None);
                }
                if e.message
                    .to_ascii_lowercase()
                    .contains("incorrect function")
                    || e.message.contains("0x00000001")
                {
                    s.send_feature(&packet)?;
                } else {
                    return Err(e);
                }
            }
            if !pause(c, 50) {
                break;
            }
            if let Some(b) = receive(s, c, 3, 500, 64, |r| v9(r, turbo))? {
                return Ok(b);
            }
        }
        Ok(None)
    }
    fn ulx(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let ask = |s: &mut dyn HidSession, cmd| -> Result<Option<Vec<u8>>, ProviderError> {
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(&[4, 2, 0x80 | cmd, 0], 64))?;
            receive(s, c, 10, 50, 64, |r| {
                ulx_payload(r, cmd).map(<[u8]>::to_vec)
            })
        };
        if ask(s, 36)?.as_deref().and_then(|p| p.first()).copied() != Some(1) {
            return Ok(None);
        }
        let direct = ask(s, 38)?;
        let mut inferred = false;
        let level = if let Some(level) = direct
            .as_deref()
            .filter(|r| r.len() >= 3 && r[0] <= 100)
            .map(|r| r[0])
        {
            Some(level)
        } else {
            inferred = true;
            ask(s, 5)?
                .as_deref()
                .filter(|r| r.len() >= 2)
                .and_then(|r| voltage(u16::from_le_bytes([r[0], r[1]])))
        };
        let Some(level) = level else {
            return Ok(None);
        };
        let charging = ask(s, 37)?
            .as_deref()
            .and_then(|r| r.first())
            .and_then(|v| charge(*v, &[0], &[1]));
        let mut b = p::battery(level, charging).unwrap();
        b.coarse = inferred;
        if inferred {
            b.label = Some(format!(
                "about {level}% (from voltage; may rise while charging)"
            ));
        }
        Ok(Some(b))
    }
    fn ajazz(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        wired: bool,
    ) -> Result<Option<Battery>, ProviderError> {
        s.write(&p::padded(&[0, 0x10], 33))?;
        let mut announcement = None;
        for _ in 0..5 {
            if !c.active() {
                break;
            }
            let r = read(s, c, 33, 250)?;
            if let Some((mut b, info)) = ajazz(&r) {
                b.charging = if wired {
                    b.charging.map(|v| v && b.level < 100)
                } else {
                    Some(false)
                };
                if info {
                    return Ok(Some(b));
                }
                announcement = Some(b);
            }
        }
        Ok(announcement)
    }
    fn turtle(
        &mut self,
        d: &Device,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let coap = [0x2251, 0x2253].contains(&d.pid);
        let mut blob = Vec::new();
        if coap {
            for _ in 0..5 {
                if !c.active() {
                    return Ok(None);
                }
                let _ = s.input_report(7, 62);
            }
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(&[6, 6, 0, 5, 0x5a, 2, 0, 0x42, 0x99], 62))?;
            if !pause(c, 300) {
                return Ok(None);
            }
            for _ in 0..5 {
                if !c.active() {
                    return Ok(None);
                }
                let _ = s.input_report(7, 62);
            }
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(
                &[
                    6, 14, 0, 5, 0x5a, 10, 0, 2, 0x99, 0x40, 1, 0, 1, 0xb3, b'G', b'S', b'I',
                ],
                62,
            ))?;
            for _ in 0..30 {
                if !pause(c, 30) {
                    break;
                }
                append_turtle(&mut blob, &s.input_report(7, 62)?);
                if let Some(mut b) = turtle_documents(&blob) {
                    b.charging = None;
                    return Ok(Some(b));
                }
            }
        } else {
            let mut budget = 32;
            let mut payload_seen = false;
            for (token, info, limit) in [(false, true, 10), (false, false, 16), (true, false, 16)] {
                if !c.active() {
                    break;
                }
                let mut packet = p::padded(&[6, 0x18, 0, 5, 0x5a, 0x14, 0, 2, 0x99, 0x48, 1], 62);
                packet[11..14].copy_from_slice(if info {
                    &[0xc0, 0x4a, 1]
                } else if token {
                    &[0x44, 0x11, 1]
                } else {
                    &[0x44, 0x14, 4]
                });
                packet[21..27].copy_from_slice(if info { b"a\0SInf" } else { b"a\0SGSI" });
                s.write(&packet)?;
                for _ in 0..if token && !payload_seen { 5 } else { limit } {
                    if budget == 0 || !pause(c, 60) {
                        break;
                    }
                    budget -= 1;
                    let r = s.input_report(7, 62)?;
                    payload_seen |= r.len() > 3 && r[1] > 0;
                    append_turtle(&mut blob, &r);
                    if let Some(b) = turtle_documents(&blob) {
                        return Ok(Some(b));
                    }
                }
            }
        }
        Ok(None)
    }
    fn omni(
        &mut self,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        let mut groups: BTreeMap<String, Vec<&HidInfo>> = BTreeMap::new();
        for i in infos
            .iter()
            .filter(|i| i.vendor_id == 0x0b05 && i.product_id == 0x1ace && i.interface == 2)
        {
            groups
                .entry(crate::provider::receiver_key(i))
                .or_default()
                .push(i);
        }
        let mut out = Vec::new();
        for (id, mine) in groups.into_iter().take(MAX_DEVICES) {
            if !c.active() {
                break;
            }
            let Some(control) = mine.iter().find(|i| i.usage_page == 0xff02) else {
                continue;
            };
            let ask = |i: &HidInfo,
                       head: &[u8],
                       accept: fn(&[u8]) -> bool|
             -> Result<Option<Vec<u8>>, ProviderError> {
                let mut s = hid.open(i)?;
                if !drain(&mut *s, c, 64, 16)? {
                    return Ok(None);
                }
                s.write(&p::padded(head, 64))?;
                receive(&mut *s, c, 8, 300, 64, |r| accept(r).then(|| r.to_vec()))
            };
            let Some(pairs) = ask(control, &[1, 0xa0, 0, 0], |r| {
                r.len() >= 7 && r[..2] == [1, 0xa0]
            })?
            else {
                continue;
            };
            // Only the two models with successful owner tests in parent #185.
            for (model, page, head, name, device_kind, at) in [
                (
                    0x1b65,
                    0xff01,
                    &[3, 0x12, 7][..],
                    "ROG Harpe Ace Mini",
                    "mouse",
                    5,
                ),
                (
                    0x1b06,
                    0xff00,
                    &[2, 0x12, 1][..],
                    "ROG Falchion RX Low Profile",
                    "keyboard",
                    11,
                ),
            ] {
                if !pairs
                    .get(5..)
                    .unwrap_or_default()
                    .chunks(4)
                    .take_while(|p| p.len() >= 2 && p[..2] != [0, 0])
                    .any(|p| u16::from_le_bytes([p[0], p[1]]) == model)
                {
                    continue;
                }
                let Some(i) = mine.iter().find(|i| i.usage_page == page) else {
                    continue;
                };
                let r = ask(i, head, |r| {
                    r.len() > 11 && matches!(r.get(..3), Some([3, 0x12, 7] | [2, 0x12, 1]))
                })?;
                if let Some(r) = r
                    && r.starts_with(head)
                    && r[at] <= 100
                    && (r[at] > 0 || r[9] != 0)
                {
                    let mut reading = Reading::new(
                        format!("asus:omni:{id}:{model:04x}"),
                        name,
                        "asus",
                        c.clock.unix(),
                    );
                    reading.level = Some(r[at]);
                    reading.charging = Some(r[9] != 0);
                    reading.kind = device_kind.into();
                    reading.via = "usb".into();
                    // A receiver container identifies the receiver, not its two children.
                    out.push(reading);
                }
            }
        }
        Ok(out)
    }
}

fn cached_reading(d: &Device, last: &Reading, at: Duration, now: Duration) -> Option<Reading> {
    let age = now.saturating_sub(at);
    if !retain_level(d) && age >= Duration::from_secs(300) {
        return None;
    }
    let mut r = last.clone();
    r.connection = if protocol(d) == "inphic" && age < Duration::from_secs(90) {
        Connection::Online
    } else {
        Connection::Sleeping
    };
    Some(r)
}
fn merge(out: &mut Vec<Reading>, r: Reading) -> Option<&Reading> {
    let index = if let Some(index) = out.iter().position(|old| old.key == r.key) {
        let old = &out[index];
        let replace = if r.online() != old.online() {
            r.online()
        } else if r.source == "shield" && old.via != r.via {
            r.via == "usb"
        } else {
            r.charging == Some(true) || old.via == "bluetooth" && r.via == "usb"
        };
        if !replace {
            return None;
        }
        out[index] = r;
        index
    } else {
        out.push(r);
        out.len() - 1
    };
    Some(&out[index])
}
fn reading(d: &Device, i: &HidInfo, b: Battery, c: &PollContext<'_>) -> Reading {
    let mut r = Reading::new(key(d, i), d.name, d.provider, c.clock.unix());
    r.kind = kind(d).into();
    r.level = Some(b.level);
    r.charging = b.charging;
    r.serial = (!i.serial.is_empty()).then(|| i.serial.clone());
    r.container = i.container.clone();
    r.via = if is_bluetooth(&i.path) {
        "bluetooth"
    } else {
        "usb"
    }
    .into();
    if b.coarse {
        r.precision = Precision::Coarse;
        r.approx = b.label.or_else(|| Some(format!("about {}%", b.level)));
    }
    r
}
fn pause(c: &PollContext<'_>, ms: u64) -> bool {
    c.sleep(Duration::from_millis(ms));
    c.active()
}
fn charge(value: u8, no: &[u8], yes: &[u8]) -> Option<bool> {
    if no.contains(&value) {
        Some(false)
    } else if yes.contains(&value) {
        Some(true)
    } else {
        None
    }
}
fn read(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    size: usize,
    ms: u64,
) -> Result<Vec<u8>, ProviderError> {
    if !c.active() {
        return Ok(Vec::new());
    }
    let before = c.clock.monotonic();
    let r = s.read(
        size,
        Duration::from_millis(ms).min(c.deadline.saturating_sub(before)),
    )?;
    if c.clock.monotonic() == before && ms > 0 {
        c.sleep(Duration::from_millis(1));
    }
    Ok(r)
}
fn receive<T>(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    attempts: usize,
    ms: u64,
    size: usize,
    mut parse: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Option<T>, ProviderError> {
    for _ in 0..attempts {
        if !c.active() {
            break;
        }
        let r = read(s, c, size, ms)?;
        if !c.active() {
            break;
        }
        if let Some(v) = parse(&r) {
            return Ok(Some(v));
        }
    }
    Ok(None)
}
fn drain(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    size: usize,
    n: usize,
) -> Result<bool, ProviderError> {
    for _ in 0..n {
        if !c.active() || s.read(size, Duration::ZERO)?.is_empty() {
            break;
        }
    }
    Ok(c.active())
}
fn compx(r: &[u8], header: u8, cmd: u8) -> Option<Battery> {
    if r.len() < 17
        || r[..3] != [header, cmd, 0]
        || r[..17].iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0x55
    {
        return None;
    }
    p::battery(r[6], charge(r[7], &[0], &[1]))
}
fn sequence_battery(r: &[u8], report: u8, seq: u16, nearlink: bool) -> Option<Battery> {
    if r.len() < if nearlink { 64 } else { 10 }
        || r[..4] != [report, seq as u8, (seq >> 8) as u8, 0x1a]
        || r[5..7] != [0, 0]
    {
        return None;
    }
    let charging = charge(r[9], &[0], &[1, 2])?;
    p::battery(r[8], Some(charging))
}
fn aula_keyboard(r: &[u8]) -> Option<Battery> {
    let r = if r.first() == Some(&0) { &r[1..] } else { r };
    if r.len() < 26 || r[..2] != [0x55, 0x10] || ![0, 0x18].contains(&r[2]) || r[25] == 0 {
        return None;
    }
    p::battery(r[25], None)
}
fn strix(r: &[u8]) -> Option<Battery> {
    let r = if r.first() == Some(&0xff) {
        r.get(1..)?
    } else {
        r
    };
    if r.len() < 12 || r[0] != 0x1b {
        return None;
    }
    let charging = charge(r[8], &[3], &[0x0a]);
    let mut mv = u16::from_le_bytes([r[10], r[11]]);
    if mv == 0 || mv == u16::MAX {
        return None;
    }
    if charging == Some(true) && mv < 4100 {
        mv = mv.saturating_sub(160);
    }
    let level = if mv >> 8 >= 0x10 || mv >= 4100 {
        100
    } else if mv >> 8 >= 0x0f || mv >= 3800 {
        75
    } else if mv >= 3700 {
        50
    } else if mv >> 8 >= 0x0e || mv >= 3500 {
        25
    } else {
        10
    };
    let mut b = p::battery(level, charging)?;
    b.coarse = true;
    Some(b)
}
type PassiveBattery = (Option<u8>, Option<bool>, bool);
fn inphic(r: &[u8], pid: u16) -> Option<PassiveBattery> {
    let r = if r.first() == Some(&3) { &r[1..] } else { r };
    if r.len() < 4
        || r[1] != 0x40
        || !(if pid == 0xfa65 {
            &[0x95, 0x90, 0x93, 0x99][..]
        } else {
            &[0x55, 0x10, 0x4e][..]
        })
        .contains(&r[0])
    {
        return None;
    }
    if r[0] == 0x10 {
        let level = r[3].checked_mul(10)?;
        return (level > 0 && level <= 100).then_some((Some(level), None, true));
    }
    match r[2] {
        3 => Some((None, Some(true), false)),
        2 => Some((Some(100), Some(true), false)),
        1 if (1..=100).contains(&r[3]) => Some((Some(r[3]), Some(false), false)),
        _ => None,
    }
}
fn rapoo(r: &[u8]) -> Option<Battery> {
    if r.len() < 9 || !(r[0] == 7 || r[..2] == [0xbd, 1]) {
        return None;
    }
    p::battery(r[8], Some(charge(r[7], &[1], &[2])?))
}
fn v9(r: &[u8], turbo: bool) -> Option<Option<Battery>> {
    if r.len() < 4 || r[0] != 0x55 {
        return None;
    }
    if r[1] == 0x65 {
        if turbo && r[3] == 26 {
            return Some(None);
        }
        if turbo && ![2, 3, 4].contains(&r[3]) {
            return None;
        }
        return p::battery(
            r[2],
            if turbo {
                None
            } else {
                charge(r[3], &[2], &[0])
            },
        )
        .map(Some);
    }
    if !turbo || r.len() < 11 || r[1] != 0xaa || r[5..8] != [0, 1, 0x0b] {
        return None;
    }
    let len = r[4] as usize;
    if len < 3 || r.len() < 8 + len || ![0, 1].contains(&r[3]) {
        return None;
    }
    let expected = if r[3] == 1 {
        r[3..8 + len - 1].iter().fold(0, |a, b| a ^ b)
    } else {
        0
    };
    if r[8 + len - 1] != expected {
        return None;
    }
    if r[8] == 0 {
        return Some(None);
    }
    p::battery(r[9] & 0x7f, None).map(Some)
}
fn ulx_payload(r: &[u8], cmd: u8) -> Option<&[u8]> {
    if r.len() < 4 || r[0] != 5 || r[2] & 0x7f != cmd || r[1] < 2 {
        return None;
    }
    r.get(4..4 + usize::from(r[3]))
}
fn voltage(mv: u16) -> Option<u8> {
    if mv == 0 || mv == u16::MAX {
        return None;
    }
    let v = f64::from(mv);
    let points = [
        (3000., 0.2),
        (3620., 5.),
        (3660., 10.),
        (3740., 25.),
        (3880., 50.),
        (4170., 75.),
        (4380., 100.),
    ];
    if v < points[0].0 {
        return Some(0);
    }
    for pair in points.windows(2) {
        let [(lo, a), (hi, b)] = pair else {
            unreachable!()
        };
        if v <= *hi {
            return Some((a + (b - a) * (v - lo) / (hi - lo)).round() as u8);
        }
    }
    Some(100)
}
fn ajazz(r: &[u8]) -> Option<(Battery, bool)> {
    let r = if r.len() == 33 && r[0] == 0 {
        &r[1..]
    } else {
        r
    };
    if r.len() < 32 {
        return None;
    }
    if r[..2] == [0xc0, 1] && r[2] > 0 {
        return p::battery(r[2], None).map(|b| (b, false));
    }
    if r[..4] != [0x10, 0, 1, 0x0b]
        || r[4..31].iter().fold(0u8, |a, b| a.wrapping_add(*b)) != r[31]
        || r[13] == 0
    {
        return None;
    }
    p::battery(r[13], charge(r[14], &[1], &[0])).map(|b| (b, true))
}
fn skullcandy(r: &[u8]) -> Option<Battery> {
    if r.len() < 12 || r[0] != 7 {
        return None;
    }
    let end = 3 + usize::from(r[1]);
    let r = r.get(..end)?;
    let mut i = 3;
    while i + 6 <= r.len() {
        if r[i] != 5 {
            i += 1;
            continue;
        }
        let len = usize::from(u16::from_le_bytes([r[i + 2], r[i + 3]]));
        if len < 2 || i + 4 + len > r.len() {
            return None;
        }
        if r[i + 1] == 0x5d
            && r[i + 4..i + 6] == [0xd6, 0x0c]
            && len >= 5
            && r[i + 6] == 0
            && r[i + 7] == 0
        {
            return p::battery(r[i + 8], None);
        }
        i += 4 + len;
    }
    None
}
fn append_turtle(blob: &mut Vec<u8>, r: &[u8]) {
    if r.len() < 3 || r[0] != 7 {
        return;
    }
    let n = usize::from(u16::from_le_bytes([r[1], r[2]]));
    if n <= r.len() - 3 && blob.len() + n <= 4096 {
        blob.extend_from_slice(&r[3..3 + n]);
    }
}
fn turtle_documents(blob: &[u8]) -> Option<Battery> {
    let mut offset = 0;
    let mut level = None;
    let mut charging = None;
    while let Some(begin) = blob.get(offset..)?.iter().position(|b| *b == b'{') {
        let begin = offset + begin;
        let mut stream =
            serde_json::Deserializer::from_slice(&blob[begin..]).into_iter::<serde_json::Value>();
        match stream.next() {
            Some(Ok(doc)) => {
                offset = begin + stream.byte_offset();
                if doc
                    .get("UP")
                    .or_else(|| doc.get("OR"))
                    .and_then(|v| v.as_str())
                    != Some("GSI")
                {
                    continue;
                }
                if let Some(v) = doc.get("KVP") {
                    let number = |key| {
                        v.get(key).and_then(|v: &serde_json::Value| {
                            v.as_u64().or_else(|| v.as_str()?.parse::<u64>().ok())
                        })
                    };
                    if let Some(n) = number("240").filter(|n| *n <= 100) {
                        level = Some(n as u8);
                    }
                    if let Some(n) = number("250").filter(|n| *n <= 1) {
                        charging = Some(n == 1);
                    }
                }
            }
            _ => {
                offset = begin + 1;
            }
        }
    }
    p::battery(level?, charging)
}
fn shield_query(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
    cmd: u8,
) -> Result<Option<Vec<u8>>, ProviderError> {
    if !c.active() {
        return Ok(None);
    }
    s.write(&p::padded(&[4, cmd, 0], 33))?;
    receive(s, c, 10, 100, 65, |r| {
        ((r.len() == 33 || r.len() == 65) && r[..3] == [3, cmd, 0]).then(|| r.to_vec())
    })
}
fn shield_level(r: &[u8]) -> Option<u8> {
    (r.len() > 14 && r[14] <= 100).then(|| r[14])
}
fn shield_charging(r: &[u8]) -> Option<bool> {
    if r.len() < 6
        || r[3] > 1
        || r[4] > 2
        || ![0, 1, 2, 3, 8].contains(&r[5])
        || r[3] == 1 && r[5] == 0
    {
        return None;
    }
    Some(r[3] == 1 && r[5] == 2)
}
fn shield_serial(r: &[u8]) -> Option<String> {
    let bytes = r.get(5..19)?;
    bytes
        .iter()
        .any(|b| *b != 0)
        .then(|| bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The WGI owner calls this only for a proven FlyDigi WGI device. No XInput
/// slot is renamed or given another controller's battery. Ambiguity does no I/O.
pub fn flydigi_battery(
    hid: &dyn HidTransport,
    c: &PollContext<'_>,
) -> Result<Option<Battery>, ProviderError> {
    let infos = hid.enumerate(0x37d7)?;
    let candidates: Vec<_> = infos
        .iter()
        .filter(|i| i.product_id == 0x2401 && i.usage_page == 0xffa0)
        .collect();
    if candidates.len() != 1 || !c.active() {
        return Ok(None);
    }
    let mut s = hid.open(candidates[0])?;
    if !c.active() {
        return Ok(None);
    }
    s.write(&p::padded(&[0, 0x5a, 0xa5, 7, 0, 0], 6))?;
    if !c.active() {
        return Ok(None);
    }
    s.write(&p::padded(&[0, 0x5a, 0xa5, 1, 2, 0], 6))?;
    receive(&mut *s, c, 10, 100, 32, |r| {
        if r.len() < 12 || r[..3] != [0x5a, 0xa5, 1] || r[11] >> 4 > 2 || r[11] & 15 > 5 {
            return None;
        }
        let mut b = p::battery((r[11] & 15) * 20, Some(r[11] >> 4 > 0))?;
        b.coarse = true;
        Some(b)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_captures_are_rejected_without_guessing_percentages() {
        let compx_capture = [9, 4, 0, 0, 0, 2, 90, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xec];
        assert_eq!(compx(&compx_capture, 9, 4).unwrap().level, 90);
        for length in 0..17 {
            assert!(compx(&compx_capture[..length], 9, 4).is_none());
        }
        for (at, value) in [(0, 8), (1, 3), (2, 1), (6, 255), (16, 0)] {
            let mut r = compx_capture;
            r[at] = value;
            assert!(compx(&r, 9, 4).is_none());
        }
        let seq = [4, 1, 0, 0x1a, 0, 0, 0, 0, 60, 1];
        for (at, value) in [
            (0, 5),
            (1, 2),
            (2, 1),
            (3, 0x19),
            (5, 1),
            (6, 1),
            (8, 255),
            (9, 3),
        ] {
            let mut r = seq;
            r[at] = value;
            assert!(sequence_battery(&r, 4, 1, false).is_none());
        }
        assert!(inphic(&[3, 0x10, 0x50, 1, 9], 0xfa60).is_none());
        assert!(inphic(&[3, 0x55, 0x40, 1, 30], 0xfa65).is_none());
        assert!(inphic(&[3, 0x95, 0x40, 0xff, 75], 0xfa65).is_none());
        assert!(rapoo(&[0xbd, 1, 0, 7, 0, 0x46, 0, 0, 84]).is_none());
        assert_eq!(v9(&[0x55, 0x65, 50, 26], true), Some(None));
        assert!(v9(&[0x55, 0x65, 255, 2], false).is_none());
        assert!(ajazz(&[0; 32]).is_none());
        assert!(skullcandy(&[7, 255, 0, 5, 0x5d, 5, 0, 0xd6, 0xc, 0, 0, 80]).is_none());
        assert!(turtle_documents(b"{\"OR\":\"GSI\",\"KVP\":{\"240\":101}}").is_none());
        assert!(turtle_documents(b"{\"OR\":\"Other\",\"KVP\":{\"240\":80}}").is_none());
    }
    #[test]
    fn bounded_json_fragments_ignore_envelopes_and_preserve_partial_updates() {
        let b=turtle_documents(b"\x01{\"OR\":\"GSI\",\"KVP\":{\"250\":1}}\x02{\"UP\":\"GSI\",\"KVP\":{\"240\":77}}junk").unwrap();
        assert_eq!(b.level, 77);
        assert_eq!(b.charging, Some(true));
        let mut blob = Vec::new();
        append_turtle(&mut blob, &[7, 5, 0, 1]);
        assert!(blob.is_empty());
        for _ in 0..1000 {
            append_turtle(&mut blob, &[7, 2, 0, 1, 2]);
        }
        assert_eq!(blob.len(), 2000);
        for _ in 0..1000 {
            append_turtle(&mut blob, &[7, 3, 0, 1, 2, 3]);
        }
        assert!(blob.len() <= 4096);
    }
    #[test]
    fn arbitrary_short_reports_never_panic_or_create_out_of_range_batteries() {
        for n in 0..=128 {
            let r: Vec<_> = (0..n).map(|i| (i * 73 + n * 13) as u8).collect();
            let values = [
                compx(&r, 9, 4),
                sequence_battery(&r, 4, 1, false),
                aula_keyboard(&r),
                strix(&r),
                rapoo(&r),
                v9(&r, true).flatten(),
                v9(&r, false).flatten(),
                ajazz(&r).map(|v| v.0),
                skullcandy(&r),
                turtle_documents(&r),
            ];
            assert!(values.iter().flatten().all(|b| b.level <= 100));
            let _ = inphic(&r, 0xfa60);
            let _ = shield_level(&r);
            let _ = shield_charging(&r);
            let _ = shield_serial(&r);
            let _ = ulx_payload(&r, 5);
        }
    }
}
