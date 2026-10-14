// Battery-only protocols reviewed through HaloBattery 2251f05 (1.15.0).
// Credits: HaloBattery contributors, Solaar, SDL, HeadsetControl, ckb-next,
// OpenLinkHub and the community captures in issues #56, #58, #148, #149, #155,
// #159, #164 and #204. Device configuration allowlists are separate.

// Level, charging evidence, and explicit disconnection.
type LinkPower = (Option<u8>, Option<bool>, bool);

pub fn nzxt_telemetry(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 27 || r[..3] != [0x4e, 2, 0x97] {
        return None;
    }
    let level = u16::from_le_bytes([r[17], r[18]]);
    (level <= 100).then_some((level as u8, r[22] & 1 != 0))
}
pub fn steam_battery(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 15 || r[0] != 0x43 || r[2] > 100 {
        return None;
    }
    match r[1] {
        1 => Some((r[2], false)),
        2 | 4 => Some((r[2], true)),
        _ => None,
    }
}
pub fn mchose_a5(r: &[u8]) -> Option<Battery> {
    if r.len() < 9 {
        return None;
    }
    let at = if r[0] == 0xa1 {
        0
    } else if r[1] == 0xa1 {
        1
    } else {
        return None;
    };
    if r.len() < at + 8 || r[at + 4] != 0 || r[at + 5] != 0x83 {
        return None;
    }
    p::battery(r[at + 7].min(100), Some(r[at + 6] > 0))
}
pub fn bragi_raw(r: &[u8]) -> Option<u16> {
    let r = if r.len() >= 65 { &r[1..] } else { r };
    if r.len() < 5 || r[0] != 1 {
        return None;
    }
    if r[1..3] == [2, 0] {
        Some(u16::from_le_bytes([r[3], r[4]]))
    } else if r.len() >= 6 && r[1..4] == [1, 2, 0] {
        Some(u16::from_le_bytes([r[4], r[5]]))
    } else {
        None
    }
}
pub fn logitech_register_battery(register: u8, r: &[u8]) -> Option<Battery> {
    match register {
        0x0d if r.len() >= 3 && (1..=100).contains(&r[0]) => {
            p::battery(r[0], Some(matches!(r[2] & 0xf0, 0x50 | 0x90)))
        }
        7 if r.len() >= 2 => {
            let (level, word) = match r[0] {
                7 => (90, "full"),
                5 => (50, "good"),
                3 => (20, "low"),
                1 => (5, "critical"),
                _ => return None,
            };
            let mut b = p::battery(level, Some(r[1] & 0x21 == 0x21 || r[1] & 0x22 == 0x22))?;
            b.coarse = true;
            b.label = Some(format!("about {level}% ({word})"));
            Some(b)
        }
        _ => None,
    }
}
impl HidProvider {
    fn query_blackshark2020(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        self.blackshark_reading = None;
        s.send_feature(&p::padded(&[0xff, 0x0a, 0, 0xfd, 4, 0x12, 0xf1, 2, 5], 64))?;
        for _ in 0..5 {
            if !c.active() {
                break;
            }
            c.sleep(Duration::from_millis(100));
            let r = s.feature(0xff, 64)?;
            self.log_reply(&r);
            if r.len() != 64 || !r.starts_with(&[0xff, 0x0f, 5, 0xfe, 0x12, 4, 0x1f, 8, 5]) {
                continue;
            }
            if r[12..14] == [0, 0] {
                return Ok(None);
            }
            let charging = match r[11] {
                9 => Some(true),
                1 | 2 | 3 | 6 => Some(false),
                _ => None,
            };
            let level = (r[14] <= 100 && charging != Some(true)).then_some(r[14]);
            self.log(format!(
                "2020 power={:02x} voltage={}mV reported={}; charging percentage withheld",
                r[11],
                u16::from_be_bytes([r[12], r[13]]),
                r[14]
            ));
            self.blackshark_reading = Some((level, charging));
            let mut b = p::battery(level.unwrap_or(0), charging).unwrap();
            b.coarse = true;
            b.label = Some(level.map_or_else(
                || {
                    if charging == Some(true) {
                        "Percentage unavailable while charging".into()
                    } else {
                        "Battery percentage unavailable".into()
                    }
                },
                |level| format!("about {level}% (device-reported steps)"),
            ));
            return Ok(Some(b));
        }
        self.log("2020: no fresh battery reply; stale headers and request echoes rejected");
        Ok(None)
    }
    fn query_mchose_a5(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let packet = p::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65);
        s.send_feature(&packet)?;
        c.sleep(Duration::from_millis(34));
        for attempt in 0..8 {
            if !c.active() {
                break;
            }
            if attempt == 3 {
                s.send_feature(&packet)?;
                c.sleep(Duration::from_millis(34));
            }
            if let Ok(r) = s.feature(0, 65)
                && let Some(b) = mchose_a5(&r)
            {
                return Ok(Some(b));
            }
            c.sleep(Duration::from_millis(30));
        }
        Ok(None)
    }
    fn bragi_write(
        s: &mut dyn HidSession,
        pid: u16,
        route: u8,
        property: u8,
    ) -> Result<(), ProviderError> {
        if pid == 0x1b7f {
            let request = p::padded(&[0, route, 2, property], 65);
            s.write(&request).or_else(|_| s.write(&request[1..]))
        } else {
            // Only these allowlisted Virtuoso identities accept this bare shape.
            s.write(&p::padded(&[2, route, 2, property], 64))
        }
    }
    fn bragi_read(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        deadline: Duration,
        battery: bool,
    ) -> Result<Option<u16>, ProviderError> {
        for _ in 0..96 {
            if !c.active() || c.clock.monotonic() >= deadline {
                break;
            }
            let r = s.read(
                65,
                Duration::from_millis(500).min(deadline.saturating_sub(c.clock.monotonic())),
            )?;
            self.log_reply(&r);
            if let Some(value) = bragi_raw(&r)
                && if battery {
                    (1..=1000).contains(&value)
                } else {
                    [1, 2].contains(&value)
                }
            {
                return Ok(Some(value));
            }
            c.sleep(Duration::from_millis(5));
        }
        Ok(None)
    }
    fn query_bragi(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        info: &HidInfo,
    ) -> Result<Option<Battery>, ProviderError> {
        let now = c.clock.monotonic();
        let deadline = *self
            .corsair_budget
            .entry(format!(
                "{:04x}:{}",
                info.product_id,
                trusted_identity(info)
            ))
            .or_insert(now + Duration::from_secs(7));
        let deadline = deadline.min(c.deadline);
        if now >= deadline {
            return Ok(None);
        }
        for _ in 0..4 {
            if !c.active() || s.read(64, Duration::from_millis(30))?.is_empty() {
                break;
            }
        }
        let pid = info.product_id;
        if pid == 0x0a40 {
            for (property, route) in [(0x13, 8), (0x12, 8), (0x12, 9)] {
                if !c.active() || c.clock.monotonic() >= deadline {
                    return Ok(None);
                }
                Self::bragi_write(s, pid, route, property)?;
                let end = (c.clock.monotonic() + Duration::from_millis(800)).min(deadline);
                for _ in 0..96 {
                    if !c.active() || c.clock.monotonic() >= end {
                        break;
                    }
                    let r = s.read(
                        65,
                        Duration::from_millis(500).min(end.saturating_sub(c.clock.monotonic())),
                    )?;
                    self.log_reply(&r);
                    c.sleep(Duration::from_millis(5));
                }
            }
        }
        let routes: &[u8] = match pid {
            0x0a3d | 0x0a62 => &[8, 9],
            0x0a3e | 0x0a64 => &[9, 8],
            _ => &[9],
        };
        for &route in routes {
            if !c.active() || c.clock.monotonic() >= deadline {
                break;
            }
            Self::bragi_write(s, pid, route, 0x0f)?;
            let Some(first) = self.bragi_read(
                s,
                c,
                (c.clock.monotonic() + Duration::from_secs(6)).min(deadline),
                true,
            )?
            else {
                continue;
            };
            if !c.active() || c.clock.monotonic() >= deadline {
                return Ok(None);
            }
            Self::bragi_write(s, pid, route, 0x0f)?;
            let second = self.bragi_read(
                s,
                c,
                (c.clock.monotonic() + Duration::from_secs(3)).min(deadline),
                true,
            )?;
            if second.is_some_and(|second| second / 10 != first / 10) {
                self.log("Corsair battery confirmations disagree; ignored");
                return Ok(None);
            }
            if second.is_none() {
                self.log("Corsair confirming read silent; retaining first reading");
            }
            let charge = if pid != 0x1b7f && c.active() && c.clock.monotonic() < deadline {
                Self::bragi_write(s, pid, route, 0x10)?;
                self.bragi_read(
                    s,
                    c,
                    (c.clock.monotonic() + Duration::from_secs(3)).min(deadline),
                    false,
                )?
                .map(|v| v == 1)
            } else {
                None
            };
            return Ok(p::battery((first / 10) as u8, charge));
        }
        Ok(None)
    }
    fn logitech_registers(
        &mut self,
        long: &mut dyn HidSession,
        short: &mut Option<Box<dyn HidSession>>,
        c: &PollContext<'_>,
        slot: u8,
    ) -> Result<Option<Battery>, ProviderError> {
        for register in [0x0d, 7] {
            let Some(channel) = short.as_mut() else {
                return Ok(None);
            };
            if !c.active() {
                break;
            }
            channel.write(&[0x10, slot, 0x81, register, 0, 0, 0])?;
            let deadline = (c.clock.monotonic() + Duration::from_millis(600)).min(c.deadline);
            let mut answered = false;
            while c.active() && c.clock.monotonic() < deadline && !answered {
                for channel in 0..2 {
                    let r = if channel == 0 {
                        long.read(64, Duration::ZERO)?
                    } else {
                        short.as_mut().unwrap().read(64, Duration::ZERO)?
                    };
                    if r.len() < 4 || r[1] != slot {
                        continue;
                    }
                    if r.len() >= 6 && [0x8f, 0xff].contains(&r[2]) && r[3..5] == [0x81, register] {
                        answered = true;
                        break;
                    }
                    if r[2..4] == [0x81, register] {
                        if let Some(b) = logitech_register_battery(register, &r[4..]) {
                            return Ok(Some(b));
                        }
                        answered = true;
                        break;
                    }
                }
                if !answered {
                    c.sleep(Duration::from_millis(5));
                }
            }
        }
        Ok(None)
    }
    fn keep_logitech_register(
        &mut self,
        b: Battery,
        info: &HidInfo,
        group: &str,
        slot: u8,
        c: &PollContext<'_>,
        output: &mut BTreeMap<String, Reading>,
    ) {
        let slot_key = format!("{:04x}:{group}:{slot}", info.product_id);
        let (name, kind, unit) = self
            .logitech_identity
            .get(&slot_key)
            .cloned()
            .unwrap_or_else(|| ("Logitech device".into(), String::new(), String::new()));
        let key = if unit.is_empty() {
            format!("logitech:{group}:{:04x}:{slot}", info.product_id)
        } else {
            format!("logitech:{unit}")
        };
        if let Some(old) = self.logitech_slots.insert(slot_key, key.clone())
            && old != key
        {
            self.last.remove(&old);
            self.last_observed.remove(&old);
        }
        let mut reading = Reading::new(&key, name, "logitech", c.clock.unix());
        reading.kind = kind;
        reading.serial = (!unit.is_empty()).then_some(unit);
        reading.container = info.container.clone();
        set_battery(&mut reading, b);
        self.last_observed.insert(key.clone(), c.clock.monotonic());
        self.last.insert(key, reading.clone());
        output.insert(reading.key.clone(), reading);
    }
    fn poll_added115(
        &mut self,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        let mut groups: BTreeMap<String, Vec<(&Device, &HidInfo)>> = BTreeMap::new();
        for info in infos {
            let Some(d) = DEVICES.iter().find(|d| {
                d.provider == self.id && d.vid == info.vendor_id && d.pid == info.product_id
            }) else {
                continue;
            };
            if self.id == "nzxt" && (info.usage_page, info.usage) != (0xffca, 1) {
                continue;
            }
            if self.id == "steam"
                && (!(2..=5).contains(&info.interface)
                    || !(info.usage_page >= 0xff00 || (info.usage_page, info.usage) == (1, 5)))
            {
                continue;
            }
            let key = if self.id == "steam" {
                format!(
                    "steam:{:04x}:{}:{}",
                    d.pid,
                    trusted_identity(info),
                    info.interface
                )
            } else {
                format!("nzxt:2101:{}", trusted_identity(info))
            };
            groups.entry(key).or_default().push((d, info));
        }
        self.steel_path
            .retain(|_, path| infos.iter().any(|i| &i.path == path));
        self.last.retain(|key, _| groups.contains_key(key));
        self.last_observed
            .retain(|key, _| self.last.contains_key(key));
        if self.id == "steam" && groups.is_empty() {
            for info in infos
                .iter()
                .filter(|i| [0x1304, 0x1305].contains(&i.product_id))
            {
                self.log(format!(
                    "Steam puck has no usable slot; interface {} usage {:04x}:{:04x}",
                    info.interface, info.usage_page, info.usage
                ));
            }
        }
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for (key, mut collections) in groups.into_iter().take(128) {
            if !c.active() {
                break;
            }
            collections.sort_by_key(|(_, i)| {
                (
                    if (i.usage_page, i.usage) == (0xff00, 1) {
                        0
                    } else if i.usage_page >= 0xff00 {
                        1
                    } else {
                        2
                    },
                    i.product_id,
                    self.id == "nzxt"
                        && !self.steel_path.is_empty()
                        && self
                            .steel_path
                            .get(&format!(
                                "nzxt:{:04x}:{}",
                                i.product_id,
                                trusted_identity(i)
                            ))
                            .is_none_or(|path| path != &i.path),
                    &i.path,
                )
            });
            let mut current: Option<Reading> = None;
            let mut disconnected = false;
            let count = if self.id == "steam" {
                1
            } else {
                collections.len()
            };
            let mut per_pid: BTreeMap<u16, usize> = BTreeMap::new();
            for (d, info) in collections.into_iter().take(count) {
                if !c.active() {
                    break;
                }
                let used = per_pid.entry(d.pid).or_default();
                if *used >= 2 {
                    continue;
                }
                *used += 1;
                if self.id == "steam" && (info.usage_page, info.usage) != (0xff00, 1) {
                    self.log(format!(
                        "Steam slot {}: fallback collection {:04x}:{:04x}",
                        info.interface, info.usage_page, info.usage
                    ));
                }
                let mut s = match hid.open(info) {
                    Ok(s) => s,
                    Err(e) => {
                        self.log(format!("{}: {e}", info.path));
                        errors.push(e);
                        continue;
                    }
                };
                let result: Result<Option<LinkPower>, ProviderError> = (|| {
                    if self.id == "nzxt" {
                        for _ in 0..3 {
                            if !c.active() || s.read(64, Duration::from_millis(1))?.is_empty() {
                                break;
                            }
                        }
                        if !c.active() {
                            return Ok(None);
                        }
                        s.write(&p::padded(&[0x4e, 2, 0x81, 0, 0xb0], 64))?;
                        for _ in 0..10 {
                            if !c.active() {
                                break;
                            }
                            let r = s.read(64, Duration::from_millis(150))?;
                            self.log_reply(&r);
                            if let Some((level, charging)) = nzxt_telemetry(&r) {
                                return Ok(Some((Some(level), Some(charging), false)));
                            }
                            if !r.is_empty() && !r.starts_with(&[0x4e, 0xe5]) {
                                c.sleep(Duration::from_millis(20));
                            }
                        }
                        return Ok(None);
                    }
                    let start = c.clock.monotonic();
                    let mut end = (start + Duration::from_millis(400)).min(c.deadline);
                    let mut battery = None;
                    let mut live = false;
                    for _ in 0..4096 {
                        if !c.active() || c.clock.monotonic() >= end {
                            break;
                        }
                        let r = s.read(
                            64,
                            Duration::from_millis(20).min(end.saturating_sub(c.clock.monotonic())),
                        )?;
                        if r.is_empty() {
                            c.sleep(Duration::from_millis(5));
                            continue;
                        }
                        if matches!(r[0], 0x79 | 0x46) && r.get(1) == Some(&1) {
                            return Ok(Some((None, None, true)));
                        }
                        if matches!(r[0], 0x42 | 0x45 | 0x47)
                            || matches!(r[0], 0x79 | 0x46) && r.get(1) == Some(&2)
                        {
                            live = true;
                            end = (start + Duration::from_secs(5)).min(c.deadline);
                        }
                        if r[0] == 0x43 && r.len() >= 15 {
                            live = true;
                        }
                        if let Some(b) = steam_battery(&r) {
                            battery = Some(b);
                            live = true;
                        }
                    }
                    Ok(battery
                        .map(|(l, c)| (Some(l), Some(c), false))
                        .or_else(|| live.then_some((None, None, false))))
                })();
                match result {
                    Ok(Some((_, _, true))) => {
                        disconnected = true;
                        self.last.remove(&key);
                        self.last_observed.remove(&key);
                        break;
                    }
                    Ok(Some((level, charging, false))) => {
                        if self.id == "nzxt" {
                            per_pid.insert(d.pid, 2);
                            self.steel_path.insert(
                                format!("nzxt:{:04x}:{}", d.pid, trusted_identity(info)),
                                info.path.clone(),
                            );
                        }
                        let mut r = Reading::new(&key, d.name, self.id, c.clock.unix());
                        r.level = level;
                        r.charging = charging;
                        r.kind = if self.id == "steam" {
                            "gamepad"
                        } else {
                            "mouse"
                        }
                        .into();
                        r.via = "wireless".into();
                        r.serial = (!info.serial.is_empty()).then(|| info.serial.clone());
                        r.container = info.container.clone();
                        if self.id == "nzxt" && d.pid == 0x2129 {
                            r.via = "usb".into();
                        }
                        if self.id == "steam" && r.level.is_none() {
                            if let Some(last) = self.last.get(&key) {
                                r.level = last.level;
                                r.charging = last.charging;
                                r.timestamp = last.timestamp;
                            } else {
                                r.approx = Some("Connected; waiting for battery reading".into());
                            }
                        }
                        if current.as_ref().is_none_or(|kept| {
                            kept.charging != Some(true) && r.charging == Some(true)
                        }) {
                            current = Some(r);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.log(format!("{}: {e}", info.path));
                        errors.push(e);
                    }
                }
            }
            if let Some(r) = current {
                self.last_observed.insert(key.clone(), c.clock.monotonic());
                self.last.insert(key, r.clone());
                out.push(r);
            } else if !disconnected
                && let Some(last) = self.last.get(&key)
                && (self.id == "steam"
                    || self.last_observed.get(&key).is_some_and(|at| {
                        c.clock.monotonic().saturating_sub(*at) < Duration::from_secs(300)
                    }))
            {
                let mut r = last.clone();
                r.connection = Connection::Sleeping;
                out.push(r);
            }
        }
        if out.is_empty() && !errors.is_empty() {
            Err(errors.remove(0))
        } else {
            Ok(out)
        }
    }
}
