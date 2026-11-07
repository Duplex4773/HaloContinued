//! Battery reads from HaloBattery PRs #337, #349, #352 and #359.
use super::*;

pub(super) fn receiver_battery(
    protocol: &str,
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
) -> Result<Option<Battery>, ProviderError> {
    let mut request = [0; 33];
    let (request, size, window) = if protocol == "eweadn" {
        request[1] = 0x30;
        (&request[..], 33, 180)
    } else {
        request[..6].copy_from_slice(&[0x13, 0x4a, 1, 0, 0, 2]);
        (&request[..19], 20, 150)
    };
    for _ in 0..3 {
        if !c.active() {
            break;
        }
        s.write(request)?;
        let until = c.clock.monotonic() + Duration::from_millis(window);
        while c.active() && c.clock.monotonic() < until {
            let r = read(s, c, size, 100)?;
            if protocol == "eweadn" && r.strip_prefix(&[0]).unwrap_or(&r).starts_with(&[0x30, 2]) {
                return Ok(None);
            }
            if let Some(battery) = receiver_reply(protocol, &r) {
                return Ok(Some(battery));
            }
        }
    }
    Ok(None)
}

fn receiver_reply(protocol: &str, r: &[u8]) -> Option<Battery> {
    if protocol == "eweadn" {
        let r = r.strip_prefix(&[0]).unwrap_or(r); // Windows can include report ID zero.
        if r.len() >= 15 && r.starts_with(&[0x30, 0]) && &r[4..8] == b"M156" {
            return p::battery(r[13], charge(r[12], &[0], &[1]));
        }
    } else if r.len() == 20
        && r.starts_with(&[0x13, 0x4a, 1])
        && r[..19].iter().copied().fold(0u8, u8::wrapping_add) == r[19]
    {
        // The low nibble is also set at full charge with no cable attached.
        return p::battery(r[5], None);
    }
    None
}

pub(super) fn monsgeek(
    s: &mut dyn HidSession,
    c: &PollContext<'_>,
) -> Result<Option<Battery>, ProviderError> {
    let request = p::padded(&[0, 0xf7], 65);
    for _ in 0..3 {
        if !c.active() {
            break;
        }
        s.send_feature(&request)?;
        if !pause(c, 20) {
            break;
        }
        let r = s.feature(0, 65)?;
        if let Some(battery) = monsgeek_reply(&r) {
            return Ok(Some(battery));
        }
    }
    Ok(None)
}

fn monsgeek_reply(r: &[u8]) -> Option<Battery> {
    (r.len() >= 9 && r[0] == 0 && r[1] != 0xf7 && matches!(r[7], 1 | 3))
        .then(|| p::battery(r[2], None))
        .flatten()
}

fn jabra_payload(r: &[u8], sequence: u8, class: u8, op: u8) -> Option<&[u8]> {
    if r.len() < 6 || !r.starts_with(&[5, 0, 4, sequence]) || r[4] & 0xc0 != 0xc0 {
        return None;
    }
    if r[5] == 0xfe {
        return Some(&[]);
    }
    let end = usize::from(r[4] & 0x3f) + 1;
    if !r.get(5..)?.starts_with(&[class, op]) {
        return None;
    }
    r.get(7..end)
}

impl Community {
    pub(super) fn jabra(
        &mut self,
        d: &Device,
        i: &HidInfo,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Reading>, ProviderError> {
        let mut query = |class, op| {
            self.sequence = self.sequence.wrapping_add(1);
            let seq = self.sequence as u8;
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(&[5, 4, 0, seq, 0x46, class, op], 64))?;
            receive(s, c, 6, 300, 64, |r| {
                jabra_payload(r, seq, class, op).map(<[u8]>::to_vec)
            })
        };
        let endpoint = (d.vid, d.pid, trusted_identity(i));
        let battery = query(0x12, 2)?.and_then(|r| p::battery(*r.get(1)?, Some(r[0] & 1 != 0)));
        let Some(battery) = battery else {
            self.hints.get_mut(&endpoint).unwrap().headset_name = None;
            return Ok(None);
        };
        let hint = self.hints.get_mut(&endpoint).unwrap();
        if hint.headset_name.is_none() {
            hint.headset_name = query(2, 0).ok().flatten().and_then(|r| {
                let length = usize::from(*r.first()?);
                let name = std::str::from_utf8(r.get(1..1 + length)?).ok()?.trim();
                (!name.is_empty()).then(|| name.to_owned())
            });
        }
        let mut r = reading(d, i, battery, c);
        if let Some(name) = &hint.headset_name {
            r.name.clone_from(name);
        }
        Ok(Some(r))
    }

    pub(super) fn hs80(
        &self,
        d: &Device,
        i: &HidInfo,
        infos: &[&HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> Result<Option<Reading>, ProviderError> {
        let wireless = d.pid == 0x0a6b;
        let cable = !wireless
            || infos.iter().any(|other| {
                other.vendor_id == d.vid
                    && matches!(other.product_id, 0x0a69 | 0x0a6a)
                    && trusted_identity(other) == trusted_identity(i)
            });
        let level = if d.pid == 0x0a6a {
            None // Power-only USB identity has no readable battery collection.
        } else {
            let mut s = hid.open(i)?;
            if !drain(&mut *s, c, 64, 4)? {
                return Ok(None);
            }
            let target = if wireless { 9 } else { 8 };
            s.write(&p::padded(&[2, target, 2, 0x0f], 64))?;
            receive(&mut *s, c, 3, 500, 64, |r| hs80_level(r, target))?
        };
        if level.is_none() && !cable {
            return Ok(None);
        }
        let mut r = reading(
            d,
            i,
            p::battery(level.unwrap_or(0), Some(cable)).unwrap(),
            c,
        );
        r.level = level;
        r.charging_inferred = cable;
        if wireless {
            r.via = "wireless".into();
        }
        Ok(Some(r))
    }

    pub(super) fn blackshark(
        &mut self,
        d: &Device,
        i: &HidInfo,
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> Result<Option<Reading>, ProviderError> {
        let endpoint = (d.vid, d.pid, trusted_identity(i));
        let state = self.blackshark.entry(endpoint).or_default();
        if state.path != i.path {
            state.close();
            state.level = None;
            state.path.clone_from(&i.path);
        }
        let result = state.poll(i, hid, c);
        if result.is_err() {
            state.close();
        }
        Ok(result?.map(|(battery, timestamp, fresh)| {
            let mut r = reading(d, i, battery, c);
            r.timestamp = timestamp;
            if !fresh {
                r.connection = Connection::Sleeping;
            }
            r
        }))
    }
}

fn hs80_level(r: &[u8], target: u8) -> Option<u8> {
    let data = r.strip_prefix(&[1, target - 8, 2, 0])?;
    let tenths = u16::from_le_bytes(data.get(..2)?.try_into().ok()?);
    (tenths <= 1000).then_some((tenths / 10) as u8)
}

const REFRESH: Duration = Duration::from_secs(300);

#[derive(Default)]
pub(super) struct BlackShark {
    path: String,
    session: Option<Box<dyn HidSession>>,
    level: Option<(u8, i64)>,
    charging: Option<bool>,
    woken: bool,
    next_query: Duration,
    settle_until: Duration,
}

impl BlackShark {
    pub(super) fn close(&mut self) {
        self.session = None;
        self.charging = None;
        self.woken = false;
        // Keep the quiet deadline across Refresh, suspension and failed handles.
    }

    fn poll(
        &mut self,
        info: &HidInfo,
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> Result<Option<(Battery, i64, bool)>, ProviderError> {
        if !c.active() {
            return Ok(None);
        }
        let mut s = match self.session.take() {
            Some(session) => session,
            None => hid.open(info)?,
        };
        let result = self.exchange(&mut *s, c);
        self.session = Some(s);
        result?;
        Ok(self
            .level
            .map(|(level, at)| (p::battery(level, self.charging).unwrap(), at, self.woken)))
    }

    fn exchange(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<(), ProviderError> {
        for _ in 0..128 {
            let r = read(s, c, 64, 1)?;
            if r.is_empty() {
                break;
            }
            self.observe(&r, c);
        }
        let now = c.clock.monotonic();
        if !c.active() || now < self.next_query {
            return Ok(());
        }
        c.sleep(self.settle_until.saturating_sub(now));
        if !c.active() {
            return Ok(());
        }
        self.level = None;
        self.next_query = c.clock.monotonic() + REFRESH;
        for attempt in 0..3 {
            if attempt > 0 && !pause(c, 700) {
                break;
            }
            for (class, query, wait) in [
                (2, false, 50),
                (0x2a, false, 50),
                (0x2a, true, 50),
                (0x21, true, 300),
            ] {
                if !c.active() {
                    return Ok(());
                }
                if !query && self.woken {
                    continue;
                }
                s.write(&blackshark_frame(class, query))?;
                let until = c.clock.monotonic() + Duration::from_millis(wait);
                while c.active() && c.clock.monotonic() < until {
                    let timeout = until
                        .saturating_sub(c.clock.monotonic())
                        .as_millis()
                        .clamp(1, 50) as u64;
                    let r = read(s, c, 64, timeout)?;
                    self.observe(&r, c);
                    if self.level.is_some() {
                        return Ok(());
                    }
                }
            }
        }
        self.woken = false;
        self.next_query = c.clock.monotonic() + REFRESH;
        Ok(())
    }

    fn observe(&mut self, r: &[u8], c: &PollContext<'_>) {
        if r.len() < 63
            || r[0] != 2
            || !matches!(r[11], 1 | 2)
            || r[12] == 0
            || xor(&r[..62]) != r[62]
        {
            return;
        }
        match (r[10], r[13]) {
            (0x21, level @ 0..=100) => {
                self.level = Some((level, c.clock.unix()));
                self.woken = true;
                self.next_query = c.clock.monotonic() + REFRESH;
            }
            (0x2a, value @ 0..=1) => {
                self.charging = Some(value == 1);
                if self.woken
                    && let Some((_, at)) = &mut self.level
                {
                    *at = c.clock.unix();
                }
            }
            (0x20, 1) if r[11] == 2 => {
                self.level = None;
                self.charging = None;
                self.woken = false;
                self.settle_until = c.clock.monotonic() + Duration::from_millis(600);
                self.next_query = self.settle_until;
            }
            _ => {}
        }
    }
}

fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}

fn blackshark_frame(class: u8, query: bool) -> [u8; 64] {
    let mut r = [0; 64];
    r[0] = 2;
    r[2] = 0x60;
    r[6] = 4;
    r[9] = if query { 0x80 } else { 0 };
    r[10] = class;
    r[62] = xor(&r[..62]);
    r
}
