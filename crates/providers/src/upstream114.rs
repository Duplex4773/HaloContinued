// Read-only exchanges ported from upstream 1.14.0. Protocol credits and capture
// provenance remain in docs/protocols.md; no settings commands are sent here.
impl HidProvider {
    fn poll_114_headsets(
        &mut self,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        self.centurion_features
            .retain(|path, _| infos.iter().any(|i| &i.path == path));
        let mut groups: BTreeMap<String, (Device, Vec<&HidInfo>)> = BTreeMap::new();
        for i in infos {
            let Some(d) = DEVICES
                .iter()
                .find(|d| d.provider == self.id && d.vid == i.vendor_id && d.pid == i.product_id)
            else {
                continue;
            };
            if self.id == "steelseries_elite" && i.interface != 3
                || self.id == "logitech_centurion" && (i.usage_page, i.usage) != (0xffa0, 1)
            {
                continue;
            }
            groups
                .entry(reading_key(self.id, d, i))
                .or_insert_with(|| (*d, vec![]))
                .1
                .push(i);
        }
        let mut out = vec![];
        let mut errors = vec![];
        for (key, (d, mut mine)) in groups {
            if !c.active() {
                break;
            }
            mine.sort_by_key(|i| {
                (
                    if self.id == "steelseries_elite" && i.usage_page == 0xffc0 {
                        0
                    } else if i.usage_page >= 0xff00 {
                        1
                    } else {
                        2
                    },
                    i.usage_page,
                )
            });
            let mut handles: Vec<(&HidInfo, Box<dyn HidSession>)> = mine
                .into_iter()
                .take(16)
                .filter_map(|i| {
                    if !c.active() {
                        return None;
                    }
                    match hid.open(i) {
                        Ok(s) => Some((i, s)),
                        Err(e) => {
                            self.log(format!("open collection: {e}"));
                            errors.push(e);
                            None
                        }
                    }
                })
                .collect();
            let result = if self.id == "logitech_centurion" {
                match handles.first_mut() {
                    Some((i, s)) => self.centurion_read(i, &mut **s, c),
                    None => Ok(None),
                }
            } else {
                self.split_headset_read(&key, &mut handles, c)
            };
            match result {
                Ok(Some(b)) => {
                    let mut r = Reading::new(key, d.name, self.id, c.clock.unix());
                    if let Some((i, _)) = handles.first() {
                        r.serial = (!i.serial.is_empty()).then(|| i.serial.clone());
                        r.container = i.container.clone();
                    }
                    r.kind = "headset".into();
                    r.via = "usb".into();
                    let charging = b.charging;
                    set_battery(&mut r, b);
                    r.charging = charging;
                    out.push(r);
                }
                Ok(None) => {}
                Err(e) => {
                    self.log(format!("battery exchange: {e}"));
                    errors.push(e);
                }
            }
        }
        if out.is_empty() && !errors.is_empty() {
            return Err(errors.remove(0));
        }
        Ok(out)
    }
    fn split_send(
        &mut self,
        key: &str,
        handles: &mut [(&HidInfo, Box<dyn HidSession>)],
        packet: &[u8],
        c: &PollContext<'_>,
    ) -> bool {
        let cached = self.steel_path.get(key).cloned();
        let mut order: Vec<usize> = (0..handles.len()).collect();
        order.sort_by_key(|&n| cached.as_deref() != Some(handles[n].0.path.as_str()));
        for n in order {
            if !c.active() {
                return false;
            }
            let (i, s) = &mut handles[n];
            if self.id == "steelseries_elite" && i.usage_page < 0xff00
                || i.output_length.is_some_and(|len| len != packet.len())
            {
                continue;
            }
            match s.write(packet) {
                Ok(()) => {
                    self.steel_path.insert(key.into(), i.path.clone());
                    return true;
                }
                Err(e) => self.log(format!("output report refused: {e}")),
            }
        }
        self.steel_path.remove(key);
        false
    }
    fn split_headset_read(
        &mut self,
        key: &str,
        handles: &mut [(&HidInfo, Box<dyn HidSession>)],
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        if handles.is_empty() {
            return Ok(None);
        }
        if self.id == "hyperx_cloud3s" {
            let mut level = None;
            let mut charging = None;
            for cmd in [6, 0x48] {
                if !self.split_send(key, handles, &p::padded(&[0x0c, 2, 3, 1, 0, cmd], 64), c) {
                    if !c.active() {
                        return Ok(None);
                    }
                    if cmd == 6 {
                        return Err(ProviderError::new(
                            "no collection accepted battery output report",
                        ));
                    }
                    break;
                }
                let end = (c.clock.monotonic() + Duration::from_secs(1)).min(c.deadline);
                let mut answered = false;
                let mut readable = false;
                while c.active() && c.clock.monotonic() < end && !answered {
                    for (_, s) in handles.iter_mut() {
                        if !c.active() {
                            break;
                        }
                        let Ok(r) = s.read(64, Duration::ZERO) else {
                            continue;
                        };
                        readable = true;
                        if let Some((kind, value)) = p::hyperx3s(&r) {
                            if kind == 6 {
                                level = Some(value);
                            } else {
                                charging = match value {
                                    0 => Some(false),
                                    1 | 2 => Some(true),
                                    _ => None,
                                };
                            }
                            if kind == cmd {
                                answered = true;
                                break;
                            }
                        }
                    }
                    if !answered {
                        c.sleep(Duration::from_millis(10));
                    }
                }
                if !readable && c.active() {
                    return Err(ProviderError::new("all input collections failed"));
                }
                if cmd == 6 && level.is_none() {
                    return Ok(None);
                }
            }
            return Ok(level.and_then(|n| p::battery(n, charging)));
        }
        // A base station may push status even when it refuses the output report.
        let sent = self.split_send(key, handles, &p::padded(&[1, 0xb0], 64), c);
        let end = (c.clock.monotonic() + Duration::from_secs(1)).min(c.deadline);
        let mut level = None;
        let mut charging = false;
        let mut power = None;
        let mut readable = false;
        while c.active() && c.clock.monotonic() < end && (level.is_none() || power.is_none()) {
            for (_, s) in handles.iter_mut() {
                if !c.active() {
                    break;
                }
                let Ok(r) = s.read(64, Duration::ZERO) else {
                    continue;
                };
                readable = true;
                if let Some((battery, state)) = p::steelseries_elite(&r) {
                    if let Some(b) = battery {
                        level = Some(b.level);
                        charging = b.charging == Some(true);
                    }
                    if state.is_some() {
                        power = state;
                    }
                    if power == Some(1) {
                        return Ok(None);
                    }
                }
            }
            if level.is_none() || power.is_none() {
                c.sleep(Duration::from_millis(10));
            }
        }
        if !readable && c.active() {
            return Err(ProviderError::new("all input collections failed"));
        }
        if !sent && level.is_none() && c.active() {
            return Err(ProviderError::new(
                "no collection accepted status output report",
            ));
        }
        Ok(level.and_then(|n| p::battery(n, Some(charging || power == Some(2)))))
    }
    fn centurion_exchange(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        bridge: Option<u8>,
        index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Vec<u8>, ProviderError> {
        if !c.active() {
            return Err(ProviderError::new("poll cancelled or expired"));
        }
        let fn_sw = (function & 0xf0) | 1;
        let mut payload = vec![index, fn_sw];
        payload.extend_from_slice(params);
        if let Some(b) = bridge {
            let mut sub = vec![0];
            sub.extend(payload);
            payload = vec![b, 0x11, 0, sub.len() as u8];
            payload.extend(sub);
        }
        s.write(&p::centurion_frame(&payload))?;
        let end = (c.clock.monotonic() + Duration::from_millis(1500)).min(c.deadline);
        let mut reads = 0;
        let mut acknowledged = false;
        while c.active() && c.clock.monotonic() < end && reads < 150 {
            reads += 1;
            let r = s.read(
                64,
                Duration::from_millis(100).min(end.saturating_sub(c.clock.monotonic())),
            )?;
            if let Some(data) = p::centurion_payload(&r) {
                if let Some(b) = bridge {
                    if data.len() >= 2 && data[..2] == [b, 0x11] {
                        acknowledged = true;
                    }
                    if data.len() >= 7 && data[..2] == [b, 0x10] && data[4..7] == [0, index, fn_sw]
                    {
                        let size = usize::from(u16::from_be_bytes([data[2], data[3]]));
                        if size >= 3 && size <= data.len() - 4 {
                            return Ok(data[7..4 + size].to_vec());
                        }
                    }
                    if data.len() >= 7
                        && data[..2] == [b, 0x10]
                        && data[5] == 0xff
                        && data[6] == index
                    {
                        return Err(ProviderError::new("headset rejected feature"));
                    }
                } else if data.len() >= 2 && data[..2] == [index, fn_sw] {
                    return Ok(data[2..].to_vec());
                } else if data.len() >= 3 && data[..3] == [0xff, index, fn_sw] {
                    return Err(ProviderError::new("receiver rejected feature"));
                }
            }
            // Keep finite progress with transports that return an empty read early.
            if r.is_empty() {
                c.sleep(Duration::from_millis(10));
            }
        }
        Err(ProviderError::new(if acknowledged {
            "Centurion headset offline"
        } else {
            "Centurion response timed out"
        }))
    }
    fn centurion_discover(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<(u8, Option<u8>), ProviderError> {
        let fs = self
            .centurion_exchange(s, c, None, 0, 0, &[0, 1])?
            .first()
            .copied()
            .filter(|n| *n != 0)
            .ok_or_else(|| ProviderError::new("receiver has no FeatureSet"))?;
        let count = self
            .centurion_exchange(s, c, None, fs, 0, &[])?
            .first()
            .copied()
            .ok_or_else(|| ProviderError::new("missing receiver feature count"))?;
        let mut bridge = None;
        for n in 0..count {
            let d = self.centurion_exchange(s, c, None, fs, 0x10, &[n])?;
            if d.get(1..3) == Some(&[0, 3][..]) {
                bridge = Some(n);
                break;
            }
        }
        let bridge =
            bridge.ok_or_else(|| ProviderError::new("receiver has no Centurion bridge"))?;
        let fs = self
            .centurion_exchange(s, c, Some(bridge), 0, 0, &[0, 1])?
            .first()
            .copied()
            .filter(|n| *n != 0)
            .ok_or_else(|| ProviderError::new("headset has no FeatureSet"))?;
        let count = self
            .centurion_exchange(s, c, Some(bridge), fs, 0, &[])?
            .first()
            .copied()
            .ok_or_else(|| ProviderError::new("missing headset feature count"))?;
        let mut battery = None;
        for n in 0..count {
            let d = self.centurion_exchange(s, c, Some(bridge), fs, 0x10, &[n])?;
            if d.get(1..3) == Some(&[1, 4][..]) {
                battery = Some(n);
            }
        }
        Ok((bridge, battery))
    }
    fn centurion_read(
        &mut self,
        i: &HidInfo,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let result: Result<Option<Battery>, ProviderError> = (|| {
            let features = if let Some(v) = self.centurion_features.get(&i.path) {
                *v
            } else {
                let v = self.centurion_discover(s, c)?;
                self.centurion_features.insert(i.path.clone(), v);
                v
            };
            if let Some(index) = features.1 {
                return Ok(p::centurion_battery(&self.centurion_exchange(
                    s,
                    c,
                    Some(features.0),
                    index,
                    0,
                    &[],
                )?));
            }
            if !c.active() {
                return Ok(None);
            }
            s.write(&p::padded(&[0x51, 8, 0, 3, 0x1a, 0, 3, 0, 4, 0x0a], 64))?;
            let end = (c.clock.monotonic() + Duration::from_millis(1500)).min(c.deadline);
            let mut seen = 0;
            while c.active() && c.clock.monotonic() < end && seen < 4 {
                let r = s.read(
                    64,
                    Duration::from_millis(100).min(end.saturating_sub(c.clock.monotonic())),
                )?;
                if r.first() == Some(&0x51) {
                    seen += 1;
                }
                if r.len() >= 7 && r[..2] == [0x51, 5] && r[6] == 0 {
                    return Ok(None);
                }
                if let Some(b) = p::centurion_legacy(&r) {
                    return Ok(Some(b));
                }
                if r.is_empty() {
                    c.sleep(Duration::from_millis(10));
                }
            }
            Ok(None)
        })();
        if result
            .as_ref()
            .is_err_and(|e| e.message == "Centurion headset offline")
        {
            return Ok(None);
        }
        if result.is_err() {
            self.centurion_features.remove(&i.path);
        }
        result
    }
}
