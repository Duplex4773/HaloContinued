use crate::{
    catalog::{AUDEZE_REQUESTS, DEVICES, Device},
    protocols::{self as p, Battery},
};
use hb_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    time::Duration,
};

pub const FAMILIES: &[(&str, &str)] = &[
    ("razer", "Razer mice, keyboards and headsets"),
    ("audeze", "Audeze Maxwell"),
    ("wlmouse", "WLmouse"),
    ("mchose", "MCHOSE"),
    ("hyperx_alpha2", "HyperX Cloud Alpha 2"),
    ("hyperx_cloud3", "HyperX Cloud III Wireless"),
    ("hyperx_cloud3s", "HyperX Cloud III S Wireless"),
    ("logitech_centurion", "Logitech G PRO X 2 LIGHTSPEED"),
    ("steelseries_elite", "SteelSeries Arctis Nova Elite"),
    ("hyperx", "HyperX Cloud II Wireless"),
    ("keychron", "Keychron"),
    ("pulsar", "Pulsar / ATK / VXE / Hitscan"),
    ("jbl", "JBL Quantum"),
    ("logitech", "Logitech"),
    ("steelseries", "SteelSeries"),
    ("playstation", "PlayStation controllers"),
    ("eightbitdo", "8BitDo controllers"),
    ("barracuda", "Razer Barracuda Pro"),
    ("nintendo", "Nintendo Switch controllers"),
    ("asus", "ASUS ROG / TUF"),
    ("gwolves", "G-Wolves"),
    ("lofree", "Lofree"),
    ("astro", "Astro A50"),
    ("corsair", "Corsair"),
    ("lamzu", "LAMZU"),
    ("am_infinity", "AM Infinity 8K"),
];
pub fn providers() -> Vec<Box<dyn BatteryProvider>> {
    FAMILIES
        .iter()
        .map(|(id, _)| Box::new(HidProvider::new(id)) as Box<dyn BatteryProvider>)
        .collect()
}
pub struct HidProvider {
    jbl: crate::passive_jbl::PassiveJbl,
    id: &'static str,
    // The catalog is static; retain its ordered vendor set across polls.
    vendors: Box<[u16]>,
    diagnostics: Vec<String>,
    last: BTreeMap<String, Reading>,
    first_seen: BTreeMap<String, Duration>,
    last_observed: BTreeMap<String, Duration>,
    stuck: BTreeMap<String, u8>,
    logitech_identity: BTreeMap<String, (String, String, String)>,
    logitech_asleep: BTreeSet<String>,
    logitech_slots: BTreeMap<String, String>,
    logitech_error: Option<u8>,
    logitech_timeout: u64,
    audeze_short_useless: BTreeSet<String>,
    audeze_echo: bool,
    audeze_pending: bool,
    playstation_basic: bool,
    playstation_pending: bool,
    playstation_since: BTreeMap<String, Duration>,
    playstation_budget: BTreeMap<String, Duration>,
    pa_accepted: bool,
    razer_dead: BTreeMap<String, Duration>,
    steel_path: BTreeMap<String, String>,
    family_names: BTreeMap<String, String>,
    mchose_models: BTreeMap<String, u16>,
    last_reply_accepted: bool,
    counter: u8,
    razer_status: u16,
    razer_cache: BTreeMap<String, (String, u8)>,
    centurion_features: BTreeMap<String, (u8, Option<u8>)>,
}
impl HidProvider {
    pub fn new(id: &'static str) -> Self {
        Self {
            jbl: Default::default(),
            id,
            vendors: DEVICES
                .iter()
                .filter(|d| d.provider == id)
                .map(|d| d.vid)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            diagnostics: Vec::new(),
            last: BTreeMap::new(),
            first_seen: BTreeMap::new(),
            last_observed: BTreeMap::new(),
            stuck: BTreeMap::new(),
            logitech_identity: BTreeMap::new(),
            logitech_asleep: BTreeSet::new(),
            logitech_slots: BTreeMap::new(),
            logitech_error: None,
            logitech_timeout: 600,
            audeze_short_useless: BTreeSet::new(),
            audeze_echo: false,
            audeze_pending: false,
            playstation_basic: false,
            playstation_pending: false,
            playstation_since: BTreeMap::new(),
            playstation_budget: BTreeMap::new(),
            pa_accepted: false,
            razer_dead: BTreeMap::new(),
            steel_path: BTreeMap::new(),
            family_names: BTreeMap::new(),
            mchose_models: BTreeMap::new(),
            last_reply_accepted: false,
            counter: 0,
            razer_status: 0,
            razer_cache: BTreeMap::new(),
            centurion_features: BTreeMap::new(),
        }
    }
    /// Enable the bounded first-listen window only for a one-shot diagnostic probe.
    pub fn with_probe_listen(mut self) -> Self {
        self.jbl.probe_listen = true;
        self
    }
    fn poll_jbl(
        &mut self,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        if !c.active() {
            self.jbl.clear();
            return Ok(Vec::new());
        }
        let selected: Vec<_> = infos
            .iter()
            .filter_map(|i| {
                DEVICES
                    .iter()
                    .find(|d| d.provider == "jbl" && d.vid == i.vendor_id && d.pid == i.product_id)
                    .filter(|d| candidate_group("jbl", d, i, infos))
                    .map(|d| (*d, i))
            })
            .collect();
        let paths = selected.iter().map(|(_, i)| i.path.clone()).collect();
        self.jbl.prepare(hid.generation(), &paths);
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut healthy = 0;
        let mut failures = Vec::new();
        for (d, info) in selected.into_iter().take(crate::passive_jbl::MAX_SESSIONS) {
            let key = reading_key("jbl", &d, info);
            if !seen.insert(key.clone()) {
                continue;
            }
            let (result, lines) = self.jbl.listen(info, hid, c);
            for line in lines {
                self.log(line);
            }
            if !c.active() {
                self.jbl.clear();
                break;
            }
            let (level, online) = match result {
                Ok(observation) => {
                    healthy += 1;
                    observation
                }
                Err(error) => {
                    self.log(format!("{}: {error}", info.path));
                    failures.push(error.to_string());
                    if let Some(mut r) = self.last.get(&key).cloned() {
                        r.connection = Connection::Stale;
                        out.push(r);
                    }
                    continue;
                }
            };
            if let Some(level) = level {
                let mut r = Reading::new(&key, d.name, "jbl", c.clock.unix());
                r.kind = "headset".into();
                r.serial = (!info.serial.is_empty()).then(|| info.serial.clone());
                r.container = info.container.clone();
                r.level = Some(level);
                r.charging = Some(false);
                if !online {
                    r.connection = Connection::Sleeping;
                }
                self.last_observed.insert(key.clone(), c.clock.monotonic());
                self.last.insert(key, r.clone());
                out.push(r);
            } else if let Some(r) = self.cached(&key, c.clock.monotonic()) {
                out.push(r);
            } else {
                self.log("nothing heard yet and no earlier level");
            }
        }
        if healthy == 0 && !failures.is_empty() {
            Err(ProviderError::new(format!(
                "JBL collections unavailable: {}",
                failures.join("; ")
            )))
        } else {
            Ok(out)
        }
    }
    /// Called only after all vendor enumerations succeeded. Absence is then
    /// evidence for releasing protocol hints, not a transient enumeration error.
    fn prune_enumerated(&mut self, infos: &[HidInfo], now: Duration) {
        if self.last.is_empty()
            && self.last_observed.is_empty()
            && self.razer_dead.is_empty()
            && self.razer_cache.is_empty()
            && self.steel_path.is_empty()
            && self.family_names.is_empty()
            && self.mchose_models.is_empty()
        {
            return;
        }
        let paths: BTreeSet<_> = infos.iter().map(|i| i.path.as_str()).collect();
        self.razer_dead
            .retain(|path, until| *until > now && paths.contains(path.as_str()));
        self.razer_cache
            .retain(|_, (path, _)| paths.contains(path.as_str()));
        self.steel_path
            .retain(|_, path| paths.contains(path.as_str()));
        self.mchose_models
            .retain(|path, _| paths.contains(path.as_str()));
        if self.last.is_empty() && self.family_names.is_empty() {
            self.last_observed.clear();
            return;
        }
        let live: BTreeSet<_> = infos
            .iter()
            .filter_map(|i| {
                DEVICES
                    .iter()
                    .find(|d| {
                        d.provider == self.id && d.vid == i.vendor_id && d.pid == i.product_id
                    })
                    .map(|d| reading_key(self.id, d, i))
                    .or_else(|| {
                        (self.id == "mchose" && [0x5253, 0x3837].contains(&i.vendor_id))
                            .then(|| format!("mchose:{:04x}:{}", i.vendor_id, trusted_identity(i)))
                    })
            })
            .collect();
        self.family_names.retain(|key, _| live.contains(key));
        self.last.retain(|key, reading| {
            let recent = self
                .last_observed
                .get(key)
                .is_some_and(|at| now.saturating_sub(*at) < Duration::from_secs(300));
            live.contains(key)
                && (recent || self.id == "jbl" || self.id == "razer" && reading.kind == "mouse")
                || self.id == "mchose" && recent
        });
        self.last_observed
            .retain(|key, _| self.last.contains_key(key));
    }

    fn log(&mut self, text: impl Into<String>) {
        if self.diagnostics.len() < 120 {
            self.diagnostics.push(text.into());
        }
    }
    fn log_reply(&mut self, reply: &[u8]) {
        if self.diagnostics.len() >= 120 {
            return;
        }
        let bytes = &reply[..reply.len().min(32)];
        let mut text = String::with_capacity(6 + bytes.len().saturating_mul(3));
        text.push_str("reply ");
        for (index, byte) in bytes.iter().enumerate() {
            if index != 0 {
                text.push(' ');
            }
            write!(text, "{byte:02x}").unwrap();
        }
        self.diagnostics.push(text);
    }
    fn receive(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        attempts: usize,
        ms: u64,
        accept: impl Fn(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        for _ in 0..attempts {
            if !c.active() {
                break;
            }
            let r = s.read(
                64,
                Duration::from_millis(ms).min(c.deadline.saturating_sub(c.clock.monotonic())),
            )?;
            if !r.is_empty() {
                self.log_reply(&r);
                if accept(&r) {
                    self.last_reply_accepted = true;
                    return Ok(Some(r));
                }
            }
        }
        Ok(None)
    }
    fn query(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        packet: &[u8],
        attempts: usize,
        ms: u64,
        accept: impl Fn(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        if !c.active() {
            return Ok(None);
        }
        s.write(packet)?;
        self.receive(s, c, attempts, ms, accept)
    }
    fn razer_query(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        tid: u8,
        cmd: u8,
    ) -> Result<Option<u8>, ProviderError> {
        self.razer_status = 0;
        let request = p::razer_request(tid, cmd);
        s.send_feature(&request)?;
        let deadline = c.clock.monotonic() + Duration::from_secs(1);
        c.sleep(Duration::from_millis(60));
        let mut foreign = 0;
        loop {
            if !c.active() {
                break;
            }
            let response = s.feature(0, 91)?;
            let Some((status, value)) = p::razer_reply(&response, cmd) else {
                return Ok(None);
            };
            self.log(format!(
                "Razer tid={tid:02x} cmd=07:{cmd:02x} status={status:02x}"
            ));
            self.razer_status = u16::from(status);
            if status == 2 && value.is_some() {
                return Ok(value);
            }
            if status == 4 {
                return Ok(None);
            }
            if status != 1 && status != 2 {
                return Ok(None);
            }
            if c.clock.monotonic() >= deadline {
                break;
            }
            if status == 2 {
                foreign += 1;
                if foreign % 4 == 0
                    && foreign <= 8
                    && let Err(e) = s.send_feature(&request)
                {
                    if self.id != "wlmouse" {
                        return Err(e);
                    }
                    self.log(e.to_string());
                }
            }
            c.sleep(Duration::from_millis(80));
        }
        if foreign > 0 {
            self.razer_status = 0x100;
        }
        Ok(None)
    }
    fn pa_drain(&mut self, s: &mut dyn HidSession, c: &PollContext<'_>, barracuda: bool) {
        for _ in 0..if barracuda { 8 } else { 32 } {
            if !c.active() {
                break;
            }
            match s.read(64, Duration::from_millis(if barracuda { 20 } else { 5 })) {
                Ok(r) if !r.is_empty() => {}
                _ => break,
            }
        }
    }
    fn pa_remote(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        barracuda: bool,
        on: bool,
    ) -> bool {
        if barracuda {
            self.pa_drain(s, c, true)
        }
        if !c.active() {
            return false;
        }
        let result = match s.write(&p::pa_request(barracuda, 0xe1, Some(on))) {
            Ok(_) => true,
            Err(error) => {
                self.log(format!("write remote: {error}"));
                false
            }
        };
        if !barracuda {
            c.sleep(Duration::from_millis(35))
        }
        result
    }
    fn pa_query(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        barracuda: bool,
        command: u8,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        if !barracuda {
            self.pa_drain(s, c, false);
            self.pa_remote(s, c, false, true);
        }
        let mut result = None;
        for _ in 0..if barracuda { 4 } else { 3 } {
            if !c.active() {
                break;
            }
            if barracuda {
                self.pa_drain(s, c, true)
            } else {
                self.pa_remote(s, c, false, true);
            }
            if let Err(error) = s.write(&p::pa_request(barracuda, command, None)) {
                self.log(format!("write query {command:02x}: {error}"));
                break;
            }
            let reply_deadline =
                c.clock.monotonic() + Duration::from_millis(if barracuda { 900 } else { 150 });
            let mut reads = 0;
            while c.clock.monotonic() < reply_deadline && (!barracuda || reads < 6) {
                if !c.active() {
                    break;
                }
                reads += 1;
                let before = c.clock.monotonic();
                let reply = s.read(
                    64,
                    Duration::from_millis(if barracuda { 150 } else { 50 })
                        .min(reply_deadline.saturating_sub(before))
                        .min(c.deadline.saturating_sub(c.clock.monotonic())),
                );
                if let Ok(reply) = &reply
                    && let Some(payload) = p::pa_reply(reply, barracuda, command)
                {
                    result = Some(payload.to_vec());
                    break;
                }
                if !barracuda && c.clock.monotonic() == before {
                    c.sleep(
                        Duration::from_millis(if reply.as_ref().is_ok_and(|r| !r.is_empty()) {
                            1
                        } else {
                            50
                        })
                        .min(reply_deadline.saturating_sub(before)),
                    );
                }
            }
            if result.is_some() {
                break;
            }
        }
        if !barracuda {
            self.pa_remote(s, c, false, false);
        }
        Ok(result)
    }
    fn read_razer(
        &mut self,
        d: &Device,
        info: &HidInfo,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        let cache_key = format!("{:04x}:{}", d.pid, trusted_identity(info));
        let cached = self.razer_cache.get(&cache_key).cloned();
        let mut tids = vec![
            cached
                .as_ref()
                .filter(|(path, _)| path == &info.path)
                .map_or(d.parameter, |(_, tid)| *tid),
        ];
        for tid in [0x1f, 0x3f, 0xff, 0x9f, 8] {
            if !tids.contains(&tid) {
                tids.push(tid)
            }
        }
        for tid in tids {
            if !c.active() {
                break;
            }
            if let Some(raw) = self.razer_query(s, c, tid, 0x80)? {
                self.razer_cache
                    .insert(cache_key.clone(), (info.path.clone(), tid));
                let charging = match self.razer_query(s, c, tid, 0x84) {
                    Ok(value) => Some(value.is_some_and(|b| b != 0)),
                    Err(error) => {
                        self.log(format!("charging query: {error}"));
                        Some(false)
                    }
                };
                return Ok(p::battery(
                    (f64::from(raw) * 100.0 / 255.0).round_ties_even() as u8,
                    charging,
                ));
            }
            if [4, 0x100].contains(&self.razer_status) {
                self.razer_cache.insert(cache_key, (info.path.clone(), tid));
                break;
            }
        }
        Ok(None)
    }
    fn read(
        &mut self,
        d: &Device,
        info: &HidInfo,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
    ) -> Result<Option<Battery>, ProviderError> {
        match self.id {
            "razer" if [0x555, 0x556].contains(&d.pid) => self.read_pa(s, c, false),
            "razer" => self.read_razer(d, info, s, c),
            "barracuda" => self.read_pa(s, c, true),
            "wlmouse" if info.usage_page != 0xffff => {
                for _ in 0..8 {
                    if !c.active() {
                        break;
                    }
                    if let Ok(reply) = s.read(64, Duration::from_millis(40)) {
                        if reply.is_empty() {
                            break;
                        }
                        if let Some(b) = p::wl_heartbeat(&reply) {
                            return Ok(Some(b));
                        }
                    }
                }
                Ok(None)
            }
            "wlmouse" | "lamzu" | "gwolves" => {
                let old = self.id == "gwolves" && d.variant.starts_with("old:");
                let request = if old {
                    p::padded(
                        &[0, 0, 2, 0x8f, u8::from(!d.variant.contains(":wired:"))],
                        65,
                    )
                } else {
                    p::padded(&[0, 0, 0, 2, 2, 0, 0x83], 65)
                };
                if let Err(error) = s.send_feature(&request) {
                    if self.id == "wlmouse" {
                        self.log(format!(
                            "feature request: {error}; still checking readable status"
                        ));
                    } else {
                        return Err(error);
                    }
                }
                for _ in 0..if self.id == "lamzu" { 10 } else { 15 } {
                    if !c.active() {
                        break;
                    }
                    c.sleep(Duration::from_millis(50));
                    for length in if self.id == "wlmouse" {
                        vec![65, 64]
                    } else {
                        vec![65]
                    } {
                        if let Ok(reply) = s.feature(0, length)
                            && let Some(b) = if old {
                                p::gwolves_old(&reply)
                            } else {
                                p::wl_feature(&reply)
                            }
                        {
                            return Ok(Some(b));
                        }
                    }
                }
                if self.id == "wlmouse" {
                    for _ in 0..4 {
                        let r = s.read(64, Duration::from_millis(50))?;
                        if let Some(b) = p::wl_heartbeat(&r) {
                            return Ok(Some(b));
                        }
                    }
                }
                Ok(None)
            }
            "asus" => {
                for _ in 0..16 {
                    if !c.active() {
                        return Ok(None);
                    }
                    match s.read(65, Duration::ZERO) {
                        Ok(reply) if !reply.is_empty() => {}
                        _ => break,
                    }
                }
                for _ in 0..3 {
                    if !c.active() {
                        break;
                    }
                    s.write(&p::padded(&[0, 0x12, 7], 65))?;
                    for _ in 0..8 {
                        if !c.active() {
                            break;
                        }
                        let reply = s.read(65, Duration::from_millis(300))?;
                        if reply.is_empty() {
                            break;
                        }
                        let shape = if reply.first() == Some(&0) {
                            &reply[1..]
                        } else {
                            &reply[..]
                        };
                        if shape.starts_with(&[0xff, 0xaa]) || reply.iter().all(|b| *b == 0) {
                            return Ok(None);
                        }
                        if shape.starts_with(&[0x12, 7]) {
                            return Ok(p::asus(&reply, d.parameter));
                        }
                    }
                }
                Ok(None)
            }
            "pulsar" => {
                for _ in 0..4 {
                    if !c.active() {
                        return Ok(None);
                    }
                    if s.read(17, Duration::from_millis(30))?.is_empty() {
                        break;
                    }
                }
                s.write(&p::pulsar_request())?;
                for _ in 0..4 {
                    if !c.active() {
                        break;
                    }
                    let reply = s.read(17, Duration::from_millis(250))?;
                    if reply.is_empty() {
                        break;
                    }
                    if let Some(b) = p::pulsar(&reply) {
                        let voltage = u16::from_be_bytes([reply[8], reply[9]]);
                        if voltage != 0 {
                            self.log(format!("{voltage} mV"));
                        }
                        return Ok(Some(b));
                    }
                    c.sleep(Duration::from_millis(20));
                }
                self.log("no power reply (events, short frames and invalid checksums are ignored)");
                Ok(None)
            }
            "mchose" if d.vid == 0xa8a5 => {
                s.write(&p::padded(&[0, 0x55, 0x30, 0xa5, 0x0b, 0x2e, 1, 1, 1], 65))?;
                for _ in 0..25 {
                    if !c.active() {
                        break;
                    }
                    c.sleep(Duration::from_millis(20));
                    if let Ok(reply) = s.read(64, Duration::ZERO)
                        && let Some(b) = p::mchose_g7(&reply)
                    {
                        return Ok(Some(b));
                    }
                }
                Ok(None)
            }
            "mchose" => {
                for (id, length) in [(0x11, 20), (0x12, 64)] {
                    for _ in 0..4 {
                        if !c.active() {
                            break;
                        }
                        let mut packet = vec![255; length + 1];
                        packet[0] = id;
                        packet[1] = 6 ^ 255;
                        if s.send_feature(&packet).is_err() {
                            break;
                        }
                        c.sleep(Duration::from_millis(120));
                        if let Ok(reply) = s.feature(id, length + 1)
                            && let Some(b) = p::mchose(&reply)
                        {
                            self.mchose_models.insert(
                                info.path.clone(),
                                u16::from_le_bytes([reply[4] ^ 255, reply[5] ^ 255]),
                            );
                            return Ok(Some(b));
                        }
                    }
                }
                Ok(None)
            }
            "astro" => {
                let r = self.query(
                    s,
                    c,
                    &p::padded(&[2, 0x0c, 3, 0, 6, 0x0c], 64),
                    8,
                    300,
                    |r| r.len() > 4 && r[4] == 6,
                )?;
                Ok(r.as_deref().and_then(p::astro))
            }
            "hyperx" => {
                s.write(&p::padded(&[6, 0xff, 0xbb, 2, 0], 52))?;
                c.sleep(Duration::from_millis(100));
                let reply = s.read(20, Duration::from_millis(1000))?;
                let mut battery = p::hyperx(&reply);
                if let Some(b) = &mut battery
                    && c.active()
                {
                    s.write(&p::padded(&[6, 0xff, 0xbb, 3, 0], 52))?;
                    c.sleep(Duration::from_millis(100));
                    let reply = s.read(20, Duration::from_millis(1000))?;
                    b.charging = if reply.len() > 4 && reply[..4] == [6, 0xff, 0xbb, 3] {
                        Some(reply[4] == 1)
                    } else {
                        Some(false)
                    };
                }
                Ok(battery)
            }
            "hyperx_cloud3" => {
                let mut out = None;
                for cmd in [0x89, 0x8a] {
                    let packet = p::padded(&[0x66, cmd], 62);
                    if let Err(e) = s.write(&packet) {
                        if e.message.to_lowercase().contains("incorrect function")
                            || e.message.contains("0x00000001")
                        {
                            self.log("retrying as a feature report");
                            if let Err(error) = s.send_feature(&packet) {
                                self.log(format!("feature report -> {error}"));
                                return Err(error);
                            }
                            self.log("feature report accepted");
                        } else {
                            return Err(e);
                        }
                    }
                    c.sleep(Duration::from_millis(100));
                    let r = self.receive(s, c, 6, 200, |r| {
                        r.len() > 2
                            && r[0] == 0x66
                            && [cmd, if cmd == 0x89 { 0x0d } else { 0x0c }].contains(&r[1])
                    })?;
                    if cmd == 0x89 {
                        out = r.as_deref().and_then(p::hyperx3)
                    } else if let (Some(b), Some(r)) = (&mut out, r) {
                        b.charging = match r[2] {
                            0 => Some(false),
                            1 | 2 => Some(true),
                            _ => None,
                        }
                    }
                }
                Ok(out)
            }
            "hyperx_alpha2" => {
                for _ in 0..64 {
                    if !c.active() {
                        return Ok(None);
                    }
                    if s.read(64, Duration::from_millis(5))?.is_empty() {
                        break;
                    }
                }
                s.write(&p::padded(&[0x50, 2], 64))?;
                let deadline = (c.clock.monotonic() + Duration::from_millis(1500)).min(c.deadline);
                while c.active() && c.clock.monotonic() < deadline {
                    let reply = s.read(64, Duration::from_millis(150))?;
                    if let Some(b) = p::hyperx_alpha(&reply) {
                        return Ok(Some(b));
                    }
                    if reply.is_empty() {
                        c.sleep(Duration::from_millis(1));
                    }
                }
                Ok(None)
            }
            "keychron" => {
                for attempt in 0..3 {
                    if !c.active() {
                        break;
                    }
                    if attempt > 0 {
                        c.sleep(Duration::from_millis(100));
                    }
                    if s.send_feature(&p::padded(&[0xb3, 6], 64)).is_err() {
                        continue;
                    }
                    let reply = s.read(64, Duration::from_millis(500))?;
                    if let Some(b) = p::keychron(&reply) {
                        return Ok(Some(b));
                    }
                }
                Ok(None)
            }
            "am_infinity" => {
                for length in [65, 67] {
                    if !c.active() {
                        break;
                    }
                    if s.send_feature(&p::padded(&[0, 0xf7], length)).is_err() {
                        continue;
                    }
                    c.sleep(Duration::from_millis(30));
                    if let Ok(reply) = s.feature(5, 65)
                        && let Some(b) = p::am_infinity(&reply)
                    {
                        return Ok(Some(b));
                    }
                }
                self.log("no charge in the status report (mouse off or asleep, or the 2.4G link is not up)");
                Ok(None)
            }
            "corsair" if d.variant == "nxp" => {
                s.write(&p::padded(&[0, 0x0e, 0x50], 65))?;
                let reply = s.read(65, Duration::from_millis(500))?;
                Ok(p::corsair_nxp(&reply))
            }
            "corsair" => {
                s.write(&p::padded(&[0, 2, 8, 2, 0x13], 65))?;
                s.write(&p::padded(&[0, 2, 8, 2, 0x12], 65))?;
                for _ in 0..4 {
                    if s.read(64, Duration::from_millis(30))?.is_empty() {
                        break;
                    }
                }
                s.write(&p::padded(&[0, 2, 9, 2, 0x12], 65))?;
                if s.read(64, Duration::from_millis(500))?.is_empty() {
                    return Ok(None);
                }
                for _ in 0..4 {
                    if s.read(64, Duration::from_millis(30))?.is_empty() {
                        break;
                    }
                }
                for _ in 0..3 {
                    if !c.active() {
                        break;
                    }
                    s.write(&p::padded(&[0, 2, 9, 2, 0x0f], 65))?;
                    let reply = s.read(64, Duration::from_millis(500))?;
                    if let Some(b) = p::corsair(&reply) {
                        return Ok(Some(b));
                    }
                }
                Ok(None)
            }
            "audeze" => {
                let state_key = format!("{:04x}:{}", d.pid, trusted_identity(info));
                self.audeze_echo = false;
                let single = &[6, 7, 0x80, 5, 0x5a, 3, 0, 0xd6, 0x0c];
                let mut frames = Vec::new();
                let mut answered = true;
                let mut level = None;
                if !self.audeze_short_useless.contains(&state_key) {
                    let (got, active) = self.audeze_sequence(s, c, &[single])?;
                    answered = active;
                    level = got.iter().rev().find_map(|r| p::audeze(r));
                    frames.extend(got);
                }
                if level.is_none() && answered {
                    let (got, _) = self.audeze_sequence(s, c, AUDEZE_REQUESTS)?;
                    level = got.iter().rev().find_map(|r| p::audeze(r));
                    frames.extend(got);
                }
                if level.is_some() {
                    self.audeze_short_useless.remove(&state_key);
                } else {
                    self.audeze_short_useless.insert(state_key);
                }
                self.audeze_echo = p::audeze_echo_only(&frames);
                if self.audeze_echo {
                    self.log(
                        "empty echoes: headset did not answer; replug the dongle if persistent",
                    );
                }
                Ok(level)
            }
            "playstation" => {
                self.playstation_basic = false;
                let bt = is_bluetooth(&info.path);
                let group = format!("{:04x}:{bt}:{}", d.pid, trusted_identity(info));
                let budget = *self
                    .playstation_budget
                    .entry(group)
                    .or_insert(c.clock.monotonic() + Duration::from_millis(2500));
                let deadline = (c.clock.monotonic() + Duration::from_millis(1500))
                    .min(c.deadline)
                    .min(budget);
                if c.clock.monotonic() >= deadline {
                    return Ok(None);
                }
                if !bt || c.playstation_full_mode {
                    let _ = s.feature(if d.parameter == 1 { 5 } else { 2 }, 64);
                }
                while c.active() && c.clock.monotonic() < deadline {
                    let reply = match s.read(78, Duration::ZERO) {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    if reply.is_empty() {
                        c.sleep(Duration::from_millis(5));
                        continue;
                    }
                    if bt && !c.playstation_full_mode && reply[0] == 1 {
                        self.playstation_basic = true;
                        self.log("basic Bluetooth mode: no battery report; full mode not enabled");
                        return Ok(None);
                    }
                    let (rid, offset) = if bt {
                        if d.parameter == 1 {
                            (0x31, 54)
                        } else {
                            (0x11, 32)
                        }
                    } else {
                        (1, if d.parameter == 1 { 53 } else { 30 })
                    };
                    if reply[0] != rid || reply.len() <= offset {
                        continue;
                    }
                    if d.pid == 0x0ba0 && reply.len() > 31 && reply[31] & 4 != 0 {
                        return Ok(None);
                    }
                    return Ok(Some(if d.parameter == 1 {
                        p::dualsense(reply[offset])
                    } else {
                        p::ds4(reply[offset])
                    }));
                }
                Ok(None)
            }
            "eightbitdo" => {
                let mut seen_reports = BTreeMap::new();
                let rid = if is_bluetooth(&info.path) { 1 } else { 4 };
                let deadline = (c.clock.monotonic() + Duration::from_millis(400)).min(c.deadline);
                let mut reports = 0;
                while c.active() && c.clock.monotonic() < deadline && reports < 16 {
                    let reply = match s.read(64, Duration::ZERO) {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    if reply.is_empty() {
                        c.sleep(Duration::from_millis(5));
                        continue;
                    }
                    reports += 1;
                    seen_reports
                        .entry(reply[0])
                        .or_insert_with(|| reply.clone());
                    if reply[0] == rid
                        && reply.len() > 14
                        && let Some(b) = p::eightbitdo(reply[14])
                    {
                        return Ok(Some(b));
                    }
                }
                if seen_reports.is_empty() {
                    self.log("nothing received: ordinary mode (not switched by Steam or a game)");
                }
                for (id, reply) in seen_reports {
                    self.log(format!(
                        "report {id:#04x} len={}, no level in byte 14: {}",
                        reply.len(),
                        reply
                            .iter()
                            .take(32)
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                Ok(None)
            }
            "nintendo" => {
                let start = c.clock.monotonic();
                let mut sent = Vec::new();
                while c.active() {
                    let now = c.clock.monotonic();
                    if sent.is_empty() && now - start >= Duration::from_millis(100)
                        || sent.len() == 1 && now - sent[0] >= Duration::from_millis(300)
                    {
                        let packet = p::padded(
                            &[1, self.counter & 15, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 2],
                            49,
                        );
                        self.counter = self.counter.wrapping_add(1);
                        s.write(&packet)?;
                        sent.push(now);
                    }
                    if sent
                        .first()
                        .is_some_and(|first| now - *first >= Duration::from_millis(600))
                    {
                        break;
                    }
                    let reply = s.read(64, Duration::ZERO)?;
                    if reply.is_empty() {
                        c.sleep(Duration::from_millis(5));
                        continue;
                    }
                    if reply.len() >= 3 && [0x21, 0x30, 0x31].contains(&reply[0]) {
                        return Ok(p::nintendo(reply[2]));
                    }
                }
                Ok(None)
            }
            "lofree" => {
                let online = self.lofree_command(s, c, 0xaa)?;
                if online.is_none_or(|value| value == 0) {
                    return Ok(None);
                }
                let level = self.lofree_command(s, c, 0x1a)?;
                Ok(level.and_then(|v| p::battery(v, None)))
            }
            "steelseries" => self.steelseries(s, c, d),
            _ => Err(ProviderError::new("unregistered protocol")),
        }
    }
    fn audeze_sequence(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        requests: &[&[u8]],
    ) -> Result<(Vec<Vec<u8>>, bool), ProviderError> {
        let mut frames = Vec::new();
        let mut unanswered = 0;
        for packet in requests {
            if !c.active() {
                return Ok((frames, false));
            }
            if s.write(&p::padded(packet, 62)).is_err() {
                break;
            }
            c.sleep(Duration::from_millis(60));
            if let Ok(reply) = s.input_report(7, 62) {
                if !reply.is_empty() {
                    frames.push(reply);
                } else if frames.is_empty() {
                    unanswered += 1;
                }
            } else if frames.is_empty() {
                unanswered += 1;
            }
            if unanswered >= 3 {
                return Ok((frames, false));
            }
        }
        for _ in 0..2 {
            if !c.active() {
                break;
            }
            c.sleep(Duration::from_millis(60));
            if let Ok(reply) = s.input_report(7, 62)
                && !reply.is_empty()
            {
                frames.push(reply);
            }
        }
        Ok((frames, true))
    }
    fn read_pa(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        barracuda: bool,
    ) -> Result<Option<Battery>, ProviderError> {
        self.pa_accepted = false;
        let mut woke = false;
        for attempt in 0..if barracuda { 3 } else { 4 } {
            if !c.active() {
                return Ok(None);
            }
            let accepted = if barracuda {
                self.pa_remote(s, c, true, true)
            } else {
                s.write(&p::pa_request(false, 0xe1, Some(true))).is_ok()
            };
            if accepted {
                woke = true;
                break;
            }
            c.sleep(Duration::from_millis(150 * (attempt + 1)));
        }
        if !woke {
            self.log("receiver does not accept commands");
            return if barracuda {
                Ok(None)
            } else {
                Err(ProviderError::new("PA receiver needs reopen"))
            };
        }
        self.pa_accepted = true;
        if barracuda {
            c.sleep(Duration::from_millis(50));
        } else {
            c.sleep(Duration::from_millis(35));
        }
        let result = (|| {
            let level = self
                .pa_query(s, c, barracuda, 0x21)?
                .and_then(|r| r.first().copied());
            let Some(level) = level else {
                self.log("no battery reply");
                return Ok(None);
            };
            let charging = self
                .pa_query(s, c, barracuda, 0x2a)?
                .and_then(|r| r.first().copied())
                .is_some_and(|b| b != 0);
            Ok(p::battery(
                if barracuda { level } else { level.min(100) },
                Some(charging),
            ))
        })();
        if barracuda {
            self.pa_remote(s, c, true, false);
        }
        result
    }
    fn lofree_wait(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        deadline: Duration,
        accept: impl Fn(&[u8]) -> bool,
    ) -> Option<Vec<u8>> {
        while c.active() && c.clock.monotonic() < deadline {
            let before = c.clock.monotonic();
            let reply = match s.read(
                64,
                Duration::from_millis(100).min(deadline.saturating_sub(before)),
            ) {
                Ok(r) => r,
                Err(e) => {
                    self.log(e.to_string());
                    return None;
                }
            };
            if accept(&reply) {
                return Some(reply);
            }
            if reply.is_empty() && c.clock.monotonic() == before {
                c.sleep(Duration::from_millis(100).min(deadline.saturating_sub(before)));
            }
        }
        None
    }
    fn lofree_command(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        cmd: u8,
    ) -> Result<Option<u8>, ProviderError> {
        let deadline = (c.clock.monotonic() + Duration::from_secs(3)).min(c.deadline);
        s.write(&p::padded(&[4, 0, 0, 1], 32))?;
        if self
            .lofree_wait(s, c, deadline, |r| r.len() > 3 && r[0] == 4 && r[3] == 1)
            .is_none()
        {
            return Ok(None);
        }
        s.write(&p::padded(&[4, 0, 0, cmd, 0, 0, 0, 0], 32))?;
        let reply = self.lofree_wait(s, c, deadline, |r| {
            r.len() > 8 && r[0] == 4 && [0, cmd].contains(&r[3]) && r[5] == 0 && r[6] == 0
        });
        let _ = s.write(&p::padded(&[4, 0, 0, 2], 32));
        let stop_deadline = deadline.min(c.clock.monotonic() + Duration::from_millis(500));
        let _ = self.lofree_wait(s, c, stop_deadline, |r| {
            r.len() > 3 && r[0] == 4 && r[3] == 2
        });
        Ok(reply.map(|r| r[8]))
    }
    fn steelseries(
        &mut self,
        s: &mut dyn HidSession,
        c: &PollContext<'_>,
        d: &Device,
    ) -> Result<Option<Battery>, ProviderError> {
        let r = match d.variant {
            "exchange_arctis1" => self
                .query(s, c, &[6, 0x12], 10, 100, |r| {
                    r.len() >= 4 && r[..2] == [6, 0x12]
                })?
                .and_then(|r| {
                    if r[2] == 1 {
                        None
                    } else {
                        p::battery(r[3].min(100), None)
                    }
                }),
            "exchange_arctis7_2018" => {
                let link = self.query(s, c, &[6, 0x14], 10, 100, |r| {
                    r.len() >= 4 && r[..2] == [6, 0x14]
                })?;
                if link.is_none_or(|r| r[2] != 3) {
                    return Ok(None);
                }
                self.query(s, c, &[6, 0x18], 10, 100, |r| {
                    r.len() >= 4 && r[..2] == [6, 0x18]
                })?
                .and_then(|r| p::battery(r[2].min(100), None))
            }
            "exchange_arctis7" => self
                .query(s, c, &[6, 0x18], 10, 100, |r| {
                    r.len() >= 4 && r[..2] == [6, 0x18]
                })?
                .and_then(|r| {
                    if r[2] == 0 {
                        None
                    } else {
                        p::battery(r[2].min(100), None)
                    }
                }),
            "exchange_arctis9" => self
                .query(s, c, &[0, 0x20], 10, 100, |r| {
                    r.len() >= 5 && [0xaa, 0x55].contains(&r[0])
                })?
                .and_then(|r| {
                    if r[0] != 0xaa || r[1] != 1 {
                        None
                    } else {
                        p::battery(
                            ((u16::from(r[3].clamp(0x64, 0x9a)) - 0x64) * 100 / (0x9a - 0x64))
                                as u8,
                            Some(r[4] == 1),
                        )
                    }
                }),
            "exchange_pro_wireless" => {
                let state = self.query(s, c, &p::padded(&[0x41, 0xaa], 31), 10, 100, |r| {
                    !r.is_empty() && [2, 4].contains(&r[0])
                })?;
                if state.is_none_or(|r| r[0] == 2) {
                    return Ok(None);
                }
                self.query(s, c, &p::padded(&[0x40, 0xaa], 31), 10, 100, |r| {
                    !r.is_empty() && r[0] <= 4
                })?
                .and_then(|r| {
                    let mut b = p::battery(r[0] * 25, None)?;
                    b.coarse = true;
                    Some(b)
                })
            }
            "exchange_nova_pro" => self
                .query(s, c, &[6, 0xb0], 10, 100, |r| {
                    r.len() >= 16 && r[0] == 0xb0 && [1, 2, 8].contains(&r[15])
                })?
                .and_then(|r| {
                    if r[15] == 1 || r[6] > 8 {
                        None
                    } else {
                        let mut b =
                            p::battery((u16::from(r[6]) * 100 / 8) as u8, Some(r[15] == 2))?;
                        b.coarse = true;
                        Some(b)
                    }
                }),
            variant if ["parse_rival3", "parse_aerox3"].contains(&variant) => {
                let (request, echo) = if variant == "parse_aerox3" {
                    (p::padded(&[0, 0xd2], 64), 0xd2)
                } else {
                    (p::padded(&[0, 0xaa, 1], 64), 0xaa)
                };
                for _ in 0..3 {
                    if !c.active() {
                        break;
                    }
                    if s.write(&request).is_err() {
                        continue;
                    }
                    for _ in 0..6 {
                        if !c.active() {
                            break;
                        }
                        let reply = s.read(64, Duration::from_millis(100))?;
                        if reply.is_empty() {
                            continue;
                        }
                        if reply[0] == echo
                            || reply.first() == Some(&0) && reply.get(1) == Some(&echo)
                        {
                            return Ok(p::steelseries(&reply, variant));
                        }
                        if let Some(b) = p::steelseries(&reply, variant) {
                            return Ok(Some(b));
                        }
                        break;
                    }
                }
                None
            }
            variant => self
                .query(s, c, &[0, 0xb0], 10, 100, |r| r.first() == Some(&0xb0))?
                .as_deref()
                .and_then(|r| p::steelseries(r, variant)),
        };
        Ok(r)
    }
    // HID++ addresses and two response channels are distinct protocol inputs.
    #[allow(clippy::too_many_arguments)]
    fn logitech_request(
        &mut self,
        long: &mut dyn HidSession,
        short: &mut Option<Box<dyn HidSession>>,
        c: &PollContext<'_>,
        slot: u8,
        feature: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        self.logitech_error = None;
        if !c.active() {
            return Ok(None);
        }
        self.counter = self.counter.wrapping_add(1);
        let token = (function << 4) | (10 + self.counter % 6);
        let mut packet = p::padded(&[0x11, slot, feature, token], 20);
        packet[4..4 + params.len()].copy_from_slice(params);
        long.write(&packet)?;
        let deadline =
            (c.clock.monotonic() + Duration::from_millis(self.logitech_timeout)).min(c.deadline);
        while c.active() && c.clock.monotonic() < deadline {
            for channel in 0..2 {
                let r = if channel == 0 {
                    long.read(64, Duration::ZERO)?
                } else if let Some(s) = short {
                    s.read(64, Duration::ZERO)?
                } else {
                    continue;
                };
                if r.len() < 4 || r[1] != slot {
                    continue;
                }
                if [0x8f, 0xff].contains(&r[2]) && r.len() >= 6 && r[3] == feature && r[4] == token
                {
                    self.logitech_error = Some(r[5]);
                    return Ok(None);
                }
                if r[2] == feature && r[3] == token {
                    let mut payload = r[4..].to_vec();
                    payload.resize(payload.len().max(16), 0);
                    return Ok(Some(payload));
                }
            }
            c.sleep(Duration::from_millis(5));
        }
        Ok(None)
    }
    fn logitech_identity_read(
        &mut self,
        long: &mut dyn HidSession,
        short: &mut Option<Box<dyn HidSession>>,
        c: &PollContext<'_>,
        slot: u8,
    ) -> Result<(String, String, String), ProviderError> {
        let (mut name, mut kind, mut unit) = (String::new(), String::new(), String::new());
        if let Some(feature) = self
            .logitech_request(long, short, c, slot, 0, 0, &[0, 5])?
            .map(|r| r[0])
            .filter(|f| *f != 0)
        {
            let length = self
                .logitech_request(long, short, c, slot, feature, 0, &[])?
                .map_or(0, |r| r[0]);
            let mut bytes = Vec::new();
            while bytes.len() < usize::from(length) && c.active() {
                let Some(chars) =
                    self.logitech_request(long, short, c, slot, feature, 1, &[bytes.len() as u8])?
                else {
                    break;
                };
                bytes.extend_from_slice(&chars[..16]);
            }
            name = String::from_utf8_lossy(&bytes[..bytes.len().min(length as usize)])
                .trim()
                .into();
            if let Some(r) = self.logitech_request(long, short, c, slot, feature, 2, &[])? {
                kind = match r[0] {
                    0 | 2 => "keyboard",
                    3..=5 => "mouse",
                    _ => "",
                }
                .into();
            }
        }
        if let Some(feature) = self
            .logitech_request(long, short, c, slot, 0, 0, &[0, 3])?
            .map(|r| r[0])
            .filter(|f| *f != 0)
            && let Some(r) = self.logitech_request(long, short, c, slot, feature, 0, &[])?
            && r[1..5].iter().any(|b| *b != 0)
        {
            unit = r[1..5].iter().map(|b| format!("{b:02X}")).collect();
        }
        Ok((name, kind, unit))
    }
    fn logitech_poll(
        &mut self,
        infos: &[HidInfo],
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> PollResult {
        let mut groups: BTreeMap<(u16, String), BTreeMap<u8, &HidInfo>> = BTreeMap::new();
        let mut headsets = BTreeMap::new();
        for d in infos {
            let key = (d.product_id, receiver_key(d));
            if d.usage_page == 0xff00 && [1, 2].contains(&d.usage) {
                groups.entry(key).or_default().insert(d.usage as u8, d);
            } else if DEVICES.iter().any(|known| {
                known.provider == "logitech"
                    && known.pid == d.product_id
                    && known.variant == "headset"
            }) && (d.usage_page, d.usage)
                == if d.product_id == 0x0ac4 {
                    (0x0c, 1)
                } else {
                    (0xff43, 0x202)
                }
            {
                headsets.insert(key, d);
            }
        }
        for (key, info) in headsets {
            groups.entry(key).or_default().entry(2).or_insert(info);
        }
        let active_slots: BTreeSet<_> = groups
            .keys()
            .flat_map(|(pid, group)| {
                [1, 2, 3, 4, 5, 6, 255]
                    .into_iter()
                    .map(move |slot| format!("{pid:04x}:{group}:{slot}"))
            })
            .collect();
        self.logitech_identity
            .retain(|slot, _| active_slots.contains(slot));
        self.logitech_asleep
            .retain(|slot| active_slots.contains(slot));
        self.logitech_slots
            .retain(|slot, _| active_slots.contains(slot));
        let mut found: BTreeMap<String, Reading> = BTreeMap::new();
        for ((pid, group), paths) in &groups {
            if !c.active() {
                break;
            }
            let Some(long_info) = paths.get(&2) else {
                continue;
            };
            let mut long = match hid.open(long_info) {
                Ok(s) => s,
                Err(e) => {
                    self.log(format!("open: {e}"));
                    continue;
                }
            };
            let mut short = paths.get(&1).and_then(|i| hid.open(i).ok());
            self.counter = 0;
            let receiver = DEVICES
                .iter()
                .any(|d| d.provider == "logitech" && d.pid == *pid && d.variant == "receiver")
                || long_info.product.to_lowercase().contains("receiver");
            let result: Result<(), ProviderError> = (|| {
                for slot in if receiver {
                    vec![1, 2, 3, 4, 5, 6]
                } else {
                    vec![255]
                } {
                    if !c.active() {
                        break;
                    }
                    let slot_key = format!("{pid:04x}:{group}:{slot}");
                    self.logitech_timeout = if self.logitech_asleep.contains(&slot_key) {
                        600
                    } else {
                        2000
                    };
                    let ping = self.logitech_request(&mut *long, &mut short, c, slot, 0, 1, &[])?;
                    self.logitech_timeout = 600;
                    if ping.is_none() {
                        if let Some(error) = self.logitech_error {
                            self.log(match error {
                                1 => format!("slot {slot}: HID++ 1.0 device, not read yet"),
                                8 => format!("slot {slot}: empty receiver slot"),
                                9 => format!("slot {slot}: paired device switched off"),
                                _ => format!("slot {slot}: error {error:02x}"),
                            });
                            self.logitech_asleep.remove(&slot_key);
                            if error == 8 {
                                self.logitech_identity.remove(&slot_key);
                                if let Some(old) = self.logitech_slots.remove(&slot_key) {
                                    self.last.remove(&old);
                                    self.last_observed.remove(&old);
                                }
                            }
                        } else {
                            self.log(format!("slot {slot}: no reply, device asleep"));
                            self.logitech_asleep.insert(slot_key);
                        }
                        continue;
                    }
                    self.logitech_asleep.remove(&slot_key);
                    let (name, mut kind, unit) =
                        if let Some(identity) = self.logitech_identity.get(&slot_key) {
                            identity.clone()
                        } else {
                            let identity =
                                self.logitech_identity_read(&mut *long, &mut short, c, slot)?;
                            if !identity.0.is_empty() || !identity.2.is_empty() {
                                self.logitech_identity
                                    .insert(slot_key.clone(), identity.clone());
                            }
                            identity
                        };
                    let headset = DEVICES.iter().find(|d| {
                        d.provider == "logitech" && d.pid == *pid && d.variant == "headset"
                    });
                    if headset.is_some() {
                        kind = "headset".into();
                    }
                    let name = if name.is_empty() {
                        headset.map_or("Logitech device", |d| d.name).into()
                    } else {
                        name
                    };
                    for feature in [0x1004u16, 0x1000, 0x1001, 0x1f20] {
                        let index = self
                            .logitech_request(
                                &mut *long,
                                &mut short,
                                c,
                                slot,
                                0,
                                0,
                                &feature.to_be_bytes(),
                            )?
                            .map_or(0, |r| r[0]);
                        if index == 0 {
                            continue;
                        }
                        let params = self.logitech_request(
                            &mut *long,
                            &mut short,
                            c,
                            slot,
                            index,
                            u8::from(feature == 0x1004),
                            &[],
                        )?;
                        if params.is_none() && feature == 0x1f20 && self.logitech_error.is_some() {
                            self.log(format!(
                                "slot {slot} '{name}': headset connected but inactive"
                            ));
                        }
                        let Some(battery) = params.as_deref().and_then(|r| p::logitech(feature, r))
                        else {
                            continue;
                        };
                        let key = if unit.is_empty() {
                            format!("logitech:{group}:{pid:04x}:{slot}")
                        } else {
                            format!("logitech:{unit}")
                        };
                        if let Some(old) = self.logitech_slots.insert(slot_key.clone(), key.clone())
                            && old != key
                        {
                            self.last.remove(&old);
                        }
                        let mut reading = Reading::new(&key, &name, "logitech", c.clock.unix());
                        reading.kind = kind.clone();
                        reading.serial = (!unit.is_empty()).then(|| unit.clone());
                        reading.container = long_info.container.clone();
                        set_battery(&mut reading, battery);
                        if !found.contains_key(&key) || reading.charging == Some(true) {
                            found.insert(key, reading);
                        }
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                self.log(format!("channel error: {e}"));
            }
            self.logitech_timeout = 600;
        }
        for (key, reading) in &found {
            self.last.insert(key.clone(), reading.clone());
            self.last_observed.insert(key.clone(), c.clock.monotonic());
        }
        let mut out: Vec<_> = found.values().cloned().collect();
        let now = c.clock.monotonic();
        self.last.retain(|key, reading| {
            if found.contains_key(key) {
                true
            } else if !groups.is_empty()
                && self
                    .last_observed
                    .get(key)
                    .is_some_and(|at| now.saturating_sub(*at) < Duration::from_secs(300))
            {
                let mut stale = reading.clone();
                stale.connection = Connection::Sleeping;
                out.push(stale);
                true
            } else {
                false
            }
        });
        self.last_observed
            .retain(|key, _| self.last.contains_key(key));
        Ok(out)
    }
    fn cached(&self, key: &str, now: Duration) -> Option<Reading> {
        let r = self.last.get(key)?;
        if ![
            "razer",
            "wlmouse",
            "mchose",
            "gwolves",
            "lamzu",
            "am_infinity",
            "nintendo",
            "jbl",
            "logitech",
        ]
        .contains(&self.id)
        {
            return None;
        }
        if self.id == "razer"
            && (r.kind == "headset" || r.name.to_lowercase().contains("blackshark"))
        {
            return None;
        }
        let expired = self.id != "jbl"
            && self
                .last_observed
                .get(key)
                .is_none_or(|at| now.saturating_sub(*at) >= Duration::from_secs(300));
        if expired && !(self.id == "razer" && r.kind == "mouse") {
            return None;
        }
        let mut r = r.clone();
        if expired {
            // Presence is separate from battery freshness. Keep the established
            // mouse identity, but never display its expired percentage.
            r.level = None;
            r.charging = None;
            r.charging_inferred = false;
            r.approx = None;
        }
        r.connection = Connection::Sleeping;
        if self.id == "nintendo" {
            r.charging = Some(false);
            r.approx = Some(format!(
                "{} (last known value)",
                r.approx
                    .as_deref()
                    .unwrap_or("battery level")
                    .split(',')
                    .next()
                    .unwrap_or("battery level")
            ));
        }
        Some(r)
    }
}
include!("upstream114.rs");
fn set_battery(r: &mut Reading, b: Battery) {
    r.level = Some(b.level);
    r.charging = b.charging.or(Some(false));
    if b.coarse {
        r.precision = Precision::Coarse;
        r.approx = b.label.or_else(|| Some(format!("about {}%", b.level)));
    }
}
pub fn is_bluetooth(path: &str) -> bool {
    let p = path.to_lowercase();
    p.contains("vid&") || p.contains("00001124-0000-1000-8000-00805f9b34fb")
}
fn reading_key(id: &str, d: &Device, i: &HidInfo) -> String {
    let identity = trusted_identity(i);
    if id == "gwolves" && !d.variant.is_empty() {
        format!(
            "gwolves:{}:{identity}",
            d.variant.rsplit(':').next().unwrap_or("")
        )
    } else if ["wlmouse", "gwolves", "lamzu", "am_infinity", "lofree"].contains(&id) {
        format!("{}:{identity}", id)
    } else if id == "mchose" {
        format!("mchose:{:04x}:{identity}", d.vid)
    } else if id == "asus" {
        format!(
            "asus:{}:{identity}",
            d.name.to_lowercase().replace(' ', "-")
        )
    } else if id == "pulsar" {
        format!("pulsar:{:04x}{:04x}:{identity}", d.vid, d.pid)
    } else if id == "audeze" {
        format!("audeze:{identity}")
    } else if id == "playstation" {
        format!("ps:{:04x}:{identity}", d.pid)
    } else if id == "eightbitdo" {
        format!("8bitdo:{:04x}:{identity}", d.pid)
    } else if id == "razer" {
        format!("razer:{:04x}:{identity}", d.pid)
    } else if id == "nintendo" {
        format!("switch:{:04x}:{identity}", d.pid)
    } else if ["hyperx_cloud3", "hyperx_alpha2"].contains(&id) {
        format!("hyperx:{:04x}:{identity}", d.pid)
    } else {
        format!("{}:{:04x}:{identity}", id, d.pid)
    }
}
fn razer_keyboard_interface(pid: u16) -> Option<i32> {
    match pid {
        0x0290 | 0x0296 | 0x02d5 => Some(2),
        0x0292 | 0x0298 | 0x02d7 | 0x0271 | 0x0258 | 0x02ba | 0x02b9 => Some(3),
        _ => None,
    }
}
pub fn receiver_key(info: &HidInfo) -> String {
    info.container
        .as_deref()
        .and_then(valid_identity_value)
        .unwrap_or_else(|| {
            if let Some(segment) = info.path.split('#').nth(2) {
                segment
                    .rsplit_once('&')
                    .map_or(segment, |(prefix, _)| prefix)
                    .to_lowercase()
            } else {
                info.path.to_ascii_lowercase()
            }
        })
}
pub(crate) fn trusted_identity(info: &HidInfo) -> String {
    valid_identity_value(&info.serial).unwrap_or_else(|| receiver_key(info))
}
fn valid_identity_value(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_uppercase();
    if normalized.is_empty()
        || matches!(normalized.as_str(), "UNKNOWN" | "NONE" | "N/A" | "NULL")
        || normalized
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .all(|c| c == '0')
    {
        None
    } else {
        Some(normalized)
    }
}
fn candidate(id: &str, d: &Device, i: &HidInfo) -> bool {
    let usage = (i.usage_page, i.usage);
    match id {
        "razer" => d.pid != 0x053a,
        "barracuda" => i.usage_page == 0xff00,
        "audeze" => usage == (0xff13, 1),
        "hyperx" => usage == (0xff90, 0x0303),
        "hyperx_cloud3" | "jbl" => usage == (0xff13, 1),
        "hyperx_alpha2" => usage == (0xff13, 0xff00),
        "astro" => usage == (0xff32, 0x74),
        "lamzu" => i.interface == 2 && i.usage_page == 0xffff,
        "gwolves" => i.feature_length == Some(65),
        "lofree" => usage == (0xff1c, 0x92),
        "am_infinity" => usage == (0xffff, 2),
        "asus" => i.interface == 0 && i.usage_page >= 0xff00,
        "mchose" => {
            if d.vid == 0xa8a5 {
                i.usage_page == 0xff01
            } else {
                i.usage_page >= 0xff00
            }
        }
        "keychron" => i.interface == 4,
        "pulsar" => usage == (0xff02, 2) || (i.interface == 1 && i.usage_page >= 0xff00),
        "corsair" => {
            if d.variant == "nxp" {
                i.usage_page == 0xff42
            } else {
                i.interface == 4 && i.usage_page >= 0xff00
            }
        }
        "playstation" => true,
        "eightbitdo" => true,
        "nintendo" => is_bluetooth(&i.path),
        "steelseries" => match d.variant {
            "exchange_arctis1" => i.interface == 3 && i.usage_page == 0xff43,
            "exchange_arctis7" | "exchange_arctis7_2018" => {
                i.interface == 5 && i.usage_page >= 0xff00
            }
            "exchange_arctis9" | "exchange_pro_wireless" => {
                i.interface == 0 && i.usage_page >= 0xff00
            }
            "exchange_nova_pro" => [3, 4].contains(&i.interface) && i.usage_page >= 0xff00,
            _ => usage == (0xffc0, 1),
        },
        _ => i.usage_page >= 0xff00,
    }
}
fn candidate_group(id: &str, d: &Device, i: &HidInfo, infos: &[HidInfo]) -> bool {
    if !matches!(
        id,
        "astro" | "keychron" | "jbl" | "corsair" | "pulsar" | "audeze"
    ) && !(id == "steelseries" && !d.variant.starts_with("exchange_"))
    {
        return candidate(id, d, i);
    }
    let mine: Vec<_> = infos
        .iter()
        .filter(|other| other.vendor_id == i.vendor_id && other.product_id == i.product_id)
        .filter(|other| trusted_identity(other) == trusted_identity(i))
        .collect();
    match id {
        "astro" | "keychron" | "jbl" => {
            let picked = mine
                .iter()
                .find(|other| candidate(id, d, other))
                .or_else(|| mine.first());
            picked.is_some_and(|picked| picked.path == i.path)
        }
        "corsair" if d.variant == "nxp" => {
            if mine.iter().any(|other| other.usage_page == 0xff42) {
                i.usage_page == 0xff42
            } else {
                true
            }
        }
        "corsair" => mine
            .iter()
            .find(|other| other.interface == 4)
            .or_else(|| mine.first())
            .is_some_and(|picked| picked.path == i.path),
        "pulsar" => {
            let control: Vec<_> = mine
                .iter()
                .copied()
                .filter(|other| (other.usage_page, other.usage) == (0xff02, 2))
                .collect();
            let picked = control
                .iter()
                .chain(mine.iter())
                .find(|other| {
                    other.output_length.is_none_or(|n| n == 17)
                        && (candidate(id, d, other) || other.output_length == Some(17))
                })
                .or_else(|| control.first())
                .or_else(|| mine.iter().find(|other| candidate(id, d, other)));
            picked.is_some_and(|picked| picked.path == i.path)
        }
        "audeze" => {
            let preferred = mine
                .iter()
                .any(|other| (other.usage_page, other.usage) == (0xff13, 1));
            let page = mine.iter().any(|other| other.usage_page == 0xff13);
            if preferred {
                (i.usage_page, i.usage) == (0xff13, 1)
            } else if page {
                i.usage_page == 0xff13
            } else {
                i.usage_page >= 0xff00
            }
        }
        "steelseries" if !d.variant.starts_with("exchange_") => mine
            .iter()
            .filter(|other| (other.usage_page, other.usage) == (0xffc0, 1))
            .min_by_key(|other| other.interface != 3)
            .is_some_and(|picked| picked.path == i.path),
        _ => candidate(id, d, i),
    }
}
impl BatteryProvider for HidProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn diagnostics(&self) -> Vec<String> {
        self.diagnostics.clone()
    }
    fn next_poll_delay(&self) -> Option<Duration> {
        if self.id == "jbl" {
            return self.jbl.delay();
        }
        (self.audeze_pending || self.playstation_pending).then_some(Duration::from_secs(3))
    }
    fn invalidate(&mut self) {
        self.jbl.clear();
        self.centurion_features.clear();
    }
    fn poll(&mut self, hid: &dyn HidTransport, c: &PollContext<'_>) -> PollResult {
        if self.id == "jbl" && !c.active() {
            self.jbl.clear();
            return Ok(Vec::new());
        }
        self.diagnostics.clear();
        self.audeze_pending = false;
        self.playstation_pending = false;
        self.playstation_budget.clear();
        let mut infos = Vec::new();
        for &vendor in &self.vendors {
            infos.extend(hid.enumerate(vendor)?);
        }
        if self.id != "logitech" {
            self.prune_enumerated(&infos, c.clock.monotonic());
        }
        if self.id == "jbl" {
            return self.poll_jbl(&infos, hid, c);
        }
        if self.id == "pulsar" {
            for info in &infos {
                if DEVICES.iter().any(|d| {
                    d.provider == self.id && d.vid == info.vendor_id && d.pid == info.product_id
                }) {
                    self.log(format!("{:04x}:{:04x} output={}{}", info.usage_page, info.usage,
                        info.output_length.map_or_else(|| "unknown".into(), |n| n.to_string()),
                        if info.output_length.is_some_and(|n| n != 17) { " cannot take a 17-byte frame; skipped when a compatible collection exists" } else { "" }));
                }
            }
        }
        if self.id == "logitech" {
            return self.logitech_poll(&infos, hid, c);
        }
        if ["hyperx_cloud3s", "steelseries_elite", "logitech_centurion"].contains(&self.id) {
            return self.poll_114_headsets(&infos, hid, c);
        }
        if self.id == "audeze" {
            self.first_seen.retain(|key, _| {
                infos
                    .iter()
                    .any(|i| key == &format!("audeze:{}", trusted_identity(i)))
            });
            self.stuck.retain(|key, _| {
                infos
                    .iter()
                    .any(|i| key == &format!("audeze:{}", trusted_identity(i)))
            });
            self.audeze_short_useless.retain(|key| {
                infos
                    .iter()
                    .any(|i| key == &format!("{:04x}:{}", i.product_id, trusted_identity(i)))
            });
        }
        let mut selected: Vec<_> = infos
            .iter()
            .filter_map(|i| {
                DEVICES
                    .iter()
                    .find(|d| {
                        d.provider == self.id && d.vid == i.vendor_id && d.pid == i.product_id
                    })
                    .copied()
                    .or_else(|| {
                        if self.id == "mchose" && [0x5253, 0x3837].contains(&i.vendor_id) {
                            Some(Device {
                                provider: "mchose",
                                vid: i.vendor_id,
                                pid: i.product_id,
                                name: "MCHOSE mouse",
                                variant: "",
                                parameter: 0,
                            })
                        } else {
                            None
                        }
                    })
                    .map(|d| (d, i))
            })
            .filter(|(d, i)| candidate_group(self.id, d, i, &infos))
            .collect();
        if self.id == "steelseries" {
            selected.retain(|(d, i)| {
                !d.variant.starts_with("exchange_")
                    || self
                        .steel_path
                        .get(&format!("{:04x}:{}", d.pid, trusted_identity(i)))
                        .is_none_or(|path| {
                            !infos.iter().any(|other| &other.path == path) || &i.path == path
                        })
            });
        }
        selected.sort_by_key(|(d, i)| {
            (
                !([0x4b1a, 0x4b1e, 0x001c].contains(&i.product_id)
                    || self.id == "gwolves"
                        && (d.variant.contains(":wired:")
                            || d.variant.is_empty() && i.product_id != 0x3854)),
                if self.id == "razer" {
                    let key = format!("{:04x}:{}", i.product_id, trusted_identity(i));
                    if self
                        .razer_cache
                        .get(&key)
                        .is_some_and(|(path, _)| path == &i.path)
                    {
                        0
                    } else if razer_keyboard_interface(i.product_id) == Some(i.interface) {
                        1
                    } else if [1, 0xff00].contains(&i.usage_page) {
                        2
                    } else {
                        3
                    }
                } else if self.id == "corsair" {
                    i32::from(i.usage != 1)
                } else if self.id == "wlmouse" {
                    if i.usage_page == 0xffff { 0 } else { 1 }
                } else if self.id == "mchose" {
                    i32::from(i.usage_page != 0xff01)
                } else if ["playstation", "eightbitdo"].contains(&self.id) {
                    i32::from(!(i.usage_page == 1 && [4, 5].contains(&i.usage)))
                } else {
                    0
                },
                if i.interface < 0 { 99 } else { i.interface },
                i.path.clone(),
            )
        });
        let mut output = Vec::new();
        let mut seen = BTreeSet::new();
        let mut errors = Vec::new();
        if self.id == "audeze" {
            for info in &infos {
                if !DEVICES.iter().any(|d| {
                    d.provider == self.id && d.vid == info.vendor_id && d.pid == info.product_id
                }) {
                    continue;
                }
                let identity = trusted_identity(info);
                let key = format!("audeze:{identity}");
                if !seen.contains(&key)
                    && !infos.iter().any(|other| {
                        trusted_identity(other) == identity && other.usage_page >= 0xff00
                    })
                {
                    self.log("no vendor collection: not writing to standard collections");
                    let mut reading = Reading::new(&key, "Audeze Maxwell", self.id, c.clock.unix());
                    reading.kind = "headset".into();
                    reading.charging = Some(false);
                    output.push(reading);
                    seen.insert(key);
                }
            }
        }
        for (d, i) in selected {
            if !c.active() {
                break;
            }
            if self.id == "audeze"
                && ![0x4b1a, 0x4b1e].contains(&d.pid)
                && infos.iter().any(|other| {
                    trusted_identity(other) == trusted_identity(i)
                        && other
                            .product
                            .trim()
                            .eq_ignore_ascii_case("Audeze Maxwell Dongle")
                })
            {
                self.first_seen
                    .remove(&format!("audeze:{}", trusted_identity(i)));
                self.stuck
                    .remove(&format!("audeze:{}", trusted_identity(i)));
                continue;
            }
            if self.id == "lofree"
                && infos.iter().any(|other| {
                    other.product_id == 0x24 && trusted_identity(other) == trusted_identity(i)
                })
            {
                continue;
            }
            let key = reading_key(self.id, &d, i);
            if ["wlmouse", "gwolves", "lamzu"].contains(&self.id)
                && output.iter().any(|r: &Reading| {
                    r.key == key
                        && (r.charging == Some(true)
                            || self.id == "gwolves" && !d.variant.is_empty())
                })
            {
                continue;
            }
            if ![
                "mchose",
                "wlmouse",
                "gwolves",
                "lamzu",
                "asus",
                "playstation",
            ]
            .contains(&self.id)
                && seen.contains(&key)
            {
                continue;
            }
            self.log(format!(
                "{} {:04x}:{:04x} interface {} usage {:04x}:{:04x}",
                d.name, i.vendor_id, i.product_id, i.interface, i.usage_page, i.usage
            ));
            if self.id == "razer"
                && self
                    .razer_dead
                    .get(&i.path)
                    .is_some_and(|until| *until > c.clock.monotonic())
            {
                continue;
            }
            let mut session = match hid.open(i) {
                Ok(s) => s,
                Err(e) => {
                    self.log(e.to_string());
                    if self.id == "razer" && ![0x555, 0x556].contains(&d.pid) {
                        self.razer_dead.insert(
                            i.path.clone(),
                            c.clock.monotonic() + Duration::from_secs(300),
                        );
                    }
                    if ["playstation", "eightbitdo"].contains(&self.id) {
                        let mut reading = Reading::new(&key, d.name, self.id, c.clock.unix());
                        if self.id == "eightbitdo" {
                            reading.approx = Some(
                                "level shown only while Steam or a game uses the controller".into(),
                            );
                        }
                        if self.id == "playstation" {
                            let since = *self
                                .playstation_since
                                .entry(key.clone())
                                .or_insert(c.clock.monotonic());
                            if c.clock.monotonic().saturating_sub(since) < Duration::from_secs(120)
                            {
                                reading.approx =
                                    Some("connected, battery level not reported yet".into());
                            } else {
                                reading.approx = Some(
                                    "connected, battery not readable (another app may hold it)"
                                        .into(),
                                );
                            }
                        }
                        reading.kind = "gamepad".into();
                        reading.charging = Some(false);
                        seen.insert(key);
                        output.push(reading);
                    } else {
                        errors.push(e);
                    }
                    continue;
                }
            };
            let razer_cached = self.id == "razer"
                && self
                    .razer_cache
                    .get(&format!("{:04x}:{}", d.pid, trusted_identity(i)))
                    .is_some_and(|(path, _)| path == &i.path);
            self.last_reply_accepted = false;
            let mut result = self.read(&d, i, &mut *session, c);
            if self.id == "steelseries"
                && d.variant.starts_with("exchange_")
                && self.last_reply_accepted
            {
                self.steel_path.insert(
                    format!("{:04x}:{}", d.pid, trusted_identity(i)),
                    i.path.clone(),
                );
                seen.insert(key.clone());
            }
            if self.id == "razer"
                && ![0x555, 0x556].contains(&d.pid)
                && d.name.to_lowercase().contains("blackshark")
                && result.as_ref().is_ok_and(|r| r.is_none())
            {
                result = self.read_pa(&mut *session, c, false);
            }
            if self.id == "razer"
                && (self.razer_status == 0x100 || razer_cached && self.razer_status == 4)
            {
                seen.insert(key.clone());
            }
            if result
                .as_ref()
                .is_err_and(|e| e.message == "PA receiver needs reopen")
                && c.active()
            {
                drop(session);
                c.sleep(Duration::from_millis(300));
                result = match hid.open(i) {
                    Ok(mut reopened) => {
                        let result = self.read(&d, i, &mut *reopened, c);
                        if result
                            .as_ref()
                            .is_err_and(|e| e.message == "PA receiver needs reopen")
                        {
                            self.log(
                                "receiver does not accept commands (headset off or USB asleep)",
                            );
                            Ok(None)
                        } else {
                            result
                        }
                    }
                    Err(e) => {
                        self.log(format!("reopen: {e}"));
                        Ok(None)
                    }
                };
            }
            if self.id == "razer" && [0x555, 0x556].contains(&d.pid) && self.pa_accepted {
                self.razer_cache.insert(
                    format!("{:04x}:{}", d.pid, trusted_identity(i)),
                    (i.path.clone(), 0),
                );
                if result.as_ref().is_ok_and(|r| r.is_none()) {
                    seen.insert(key.clone());
                }
            }
            if self.id == "razer" && ![0x555, 0x556].contains(&d.pid) && result.is_err() {
                self.razer_dead.insert(
                    i.path.clone(),
                    c.clock.monotonic() + Duration::from_secs(300),
                );
            }
            match result {
                Ok(Some(b)) => {
                    let mut r = Reading::new(&key, d.name, self.id, c.clock.unix());
                    if self.id == "lofree"
                        && let Some(product) = i
                            .product
                            .split('@')
                            .next()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    {
                        r.name = format!("Lofree {product}");
                    }
                    if d.name == "Razer wireless device" && !i.product.trim().is_empty() {
                        r.name = i.product.trim().into();
                    }
                    r.serial = (!i.serial.is_empty()).then(|| i.serial.clone());
                    r.container = i.container.clone();
                    r.kind = if ["playstation", "eightbitdo", "nintendo"].contains(&self.id) {
                        "gamepad"
                    } else if self.id == "lofree"
                        || self.id == "razer"
                            && ([0x25a, 0x25c].contains(&d.pid)
                                || razer_keyboard_interface(d.pid).is_some())
                    {
                        "keyboard"
                    } else if [
                        "audeze",
                        "barracuda",
                        "astro",
                        "hyperx",
                        "hyperx_cloud3",
                        "hyperx_alpha2",
                        "jbl",
                        "corsair",
                        "steelseries",
                    ]
                    .contains(&self.id)
                        && !(d.variant == "nxp"
                            || d.variant.contains("rival")
                            || d.variant.contains("aerox"))
                    {
                        "headset"
                    } else {
                        "mouse"
                    }
                    .into();
                    r.via = if is_bluetooth(&i.path) {
                        "bluetooth"
                    } else {
                        "usb"
                    }
                    .into();
                    set_battery(&mut r, b);
                    if self.id == "audeze" {
                        self.stuck.remove(&key);
                        r.charging = Some([0x4b1a, 0x4b1e].contains(&d.pid));
                        r.charging_inferred = true;
                        let first = *self
                            .first_seen
                            .entry(key.clone())
                            .or_insert(c.clock.monotonic());
                        if r.level == Some(0)
                            && c.clock.monotonic().saturating_sub(first) < Duration::from_secs(90)
                        {
                            r.level = None;
                            r.charging = Some(false);
                            self.audeze_pending = true;
                            r.approx = Some("connected, battery level not reported yet".into());
                        }
                    }
                    if self.id == "playstation" {
                        self.playstation_since.remove(&key);
                    }
                    if [
                        "mchose",
                        "wlmouse",
                        "gwolves",
                        "lamzu",
                        "asus",
                        "playstation",
                    ]
                    .contains(&self.id)
                    {
                        if self.id == "mchose" {
                            r.name = if self.mchose_models.get(&i.path) == Some(&0x31) {
                                "MCHOSE M7 Ultra".into()
                            } else if !i.product.trim().is_empty() {
                                i.product.trim().into()
                            } else if let Some(model) = self.mchose_models.get(&i.path) {
                                format!("MCHOSE mouse (0x{model:04x})")
                            } else {
                                d.name.into()
                            };
                        } else if !["asus", "playstation"].contains(&self.id) {
                            let generic = d.name.to_lowercase().contains("receiver")
                                || ["WLmouse", "G-Wolves mouse", "LAMZU Maya X"].contains(&d.name)
                                    && self.family_names.contains_key(&key);
                            if !generic {
                                self.family_names.insert(key.clone(), r.name.clone());
                            }
                            if let Some(name) = self.family_names.get(&key) {
                                r.name = name.clone();
                            }
                        }
                        if let Some(existing) =
                            output.iter().position(|old: &Reading| old.key == key)
                        {
                            if r.charging == Some(true) || output[existing].level.is_none() {
                                output[existing] = r.clone();
                                self.last.insert(key.clone(), r);
                                self.last_observed.insert(key.clone(), c.clock.monotonic());
                            }
                            seen.insert(key);
                            continue;
                        }
                    }
                    self.last.insert(key.clone(), r.clone());
                    self.last_observed.insert(key.clone(), c.clock.monotonic());
                    seen.insert(key);
                    output.push(r);
                }
                Ok(None) => {
                    if let Some(r) = self.cached(&key, c.clock.monotonic()) {
                        seen.insert(key);
                        output.push(r)
                    } else if ["playstation", "eightbitdo"].contains(&self.id) {
                        if output.iter().any(|r: &Reading| r.key == key) {
                            continue;
                        }
                        let mut r = Reading::new(&key, d.name, self.id, c.clock.unix());
                        if self.id == "eightbitdo" {
                            r.approx = Some(
                                "level shown only while Steam or a game uses the controller".into(),
                            );
                        }
                        if self.id == "playstation" && self.playstation_basic {
                            self.playstation_since.remove(&key);
                            r.approx = Some(
                                "level shown over Bluetooth only while Steam or a game uses it"
                                    .into(),
                            );
                        }
                        if self.id == "playstation" && !self.playstation_basic {
                            let since = *self
                                .playstation_since
                                .entry(key.clone())
                                .or_insert(c.clock.monotonic());
                            if c.clock.monotonic().saturating_sub(since) < Duration::from_secs(120)
                            {
                                self.playstation_pending = true;
                                r.approx = Some("connected, battery level not reported yet".into());
                            } else {
                                r.approx = Some(
                                    "connected, battery not readable (another app may hold it)"
                                        .into(),
                                );
                            }
                        }
                        r.kind = "gamepad".into();
                        r.via = if is_bluetooth(&i.path) {
                            "bluetooth"
                        } else {
                            "usb"
                        }
                        .into();
                        seen.insert(key);
                        output.push(r);
                    } else if self.id == "barracuda" {
                        let mut r = Reading::new(&key, d.name, self.id, c.clock.unix());
                        r.connection = Connection::Sleeping;
                        r.charging = Some(false);
                        r.kind = "headset".into();
                        seen.insert(key);
                        output.push(r);
                    } else if self.id == "audeze" {
                        self.first_seen.remove(&key);
                        let count = self.stuck.entry(key.clone()).or_default();
                        *count = if self.audeze_echo {
                            count.saturating_add(1)
                        } else {
                            0
                        };
                        if *count >= 2 {
                            let mut r = Reading::new(&key, d.name, self.id, c.clock.unix());
                            r.connection = Connection::Online;
                            r.kind = "headset".into();
                            r.approx = Some("no answer from the headset - unplug the dongle and plug it back in".into());
                            self.log("no answer from headset: unplug and replug dongle");
                            output.push(r);
                        }
                    }
                }
                Err(e) => {
                    self.log(e.to_string());
                    errors.push(e)
                }
            }
        }
        if self.id == "playstation" {
            self.playstation_since.retain(|key, _| {
                infos
                    .iter()
                    .any(|i| key == &format!("ps:{:04x}:{}", i.product_id, trusted_identity(i)))
            });
            self.playstation_pending = output.iter().any(|r| {
                r.level.is_none()
                    && r.approx.as_deref() == Some("connected, battery level not reported yet")
            });
        }
        if self.id == "razer" {
            // Failed/bounded-suppressed exchanges may not reach Ok(None). Only
            // enumerated allowlisted collections can retain a known mouse.
            for info in &infos {
                if let Some(device) = DEVICES.iter().find(|d| {
                    d.provider == "razer" && d.vid == info.vendor_id && d.pid == info.product_id
                }) && candidate_group("razer", device, info, &infos)
                {
                    let key = format!("razer:{:04x}:{}", device.pid, trusted_identity(info));
                    if (errors.is_empty() || !output.is_empty())
                        && !output.iter().any(|r| r.key == key)
                        && self.last.get(&key).is_some_and(|r| r.kind == "mouse")
                        && let Some(reading) = self.cached(&key, c.clock.monotonic())
                    {
                        output.push(reading);
                    }
                }
            }
            let live: Vec<_> = output.iter().filter(|r| r.online()).cloned().collect();
            output.retain(|r| {
                r.online()
                    || !live.iter().any(|online| {
                        let shared_serial = online
                            .serial
                            .as_ref()
                            .zip(r.serial.as_ref())
                            .is_some_and(|(a, b)| {
                                a.eq_ignore_ascii_case(b) && valid_identity_value(a).is_some()
                            });
                        let shared_container = online
                            .container
                            .as_ref()
                            .zip(r.container.as_ref())
                            .is_some_and(|(a, b)| {
                                a.eq_ignore_ascii_case(b) && valid_identity_value(a).is_some()
                            });
                        online.name == r.name
                            && online.key.split(':').nth(1) != r.key.split(':').nth(1)
                            && (shared_serial || shared_container)
                    })
            });
        }
        if self.id == "mchose" {
            for key in self.last.keys() {
                if !seen.contains(key)
                    && let Some(r) = self.cached(key, c.clock.monotonic())
                {
                    output.push(r)
                }
            }
        }
        if output.is_empty() && !errors.is_empty() {
            Err(errors.remove(0))
        } else {
            Ok(output)
        }
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };
    thread_local! { static COUNT: Cell<Option<usize>> = const { Cell::new(None) }; }
    struct Allocator;
    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;
    fn counted() {
        let _ = COUNT.try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
    }
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            counted();
            unsafe { System.alloc(l) }
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            counted();
            unsafe { System.alloc_zeroed(l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            counted();
            unsafe { System.realloc(p, l, n) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
    }
    fn allocations(f: impl FnOnce()) -> usize {
        COUNT.with(|c| c.set(Some(0)));
        f();
        COUNT.with(|c| c.replace(None).unwrap())
    }
    #[test]
    fn bounded_reply_diagnostics_keep_exact_hex_with_one_allocation_then_none_at_cap() {
        let mut provider = HidProvider::new("razer");
        provider.diagnostics.reserve_exact(120);
        let reply: Vec<u8> = (0..64).collect();
        assert_eq!(allocations(|| provider.log_reply(&reply)), 1);
        assert_eq!(
            provider.diagnostics[0],
            "reply 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f 10 11 12 13 14 15 16 17 18 19 1a 1b 1c 1d 1e 1f"
        );
        provider.log_reply(&[0, 0xab, 0xff]);
        assert_eq!(provider.diagnostics[1], "reply 00 ab ff");
        provider.log_reply(&[]);
        assert_eq!(provider.diagnostics[2], "reply ");
        provider.diagnostics.resize(120, String::new());
        assert_eq!(allocations(|| provider.log_reply(&reply)), 0);
        assert_eq!(provider.diagnostics.len(), 120);
        let before = allocations(|| {
            let text = format!(
                "reply {}",
                reply
                    .iter()
                    .take(32)
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            std::hint::black_box(text);
        });
        assert!(before >= 34, "old hex formatting allocated {before} times");
    }
    #[test]
    fn ordinary_collections_skip_unused_group_identity_work_without_allocations() {
        for id in FAMILIES.iter().map(|(id, _)| *id).filter(|id| {
            !matches!(
                *id,
                "astro" | "keychron" | "jbl" | "corsair" | "pulsar" | "audeze" | "steelseries"
            )
        }) {
            let d = DEVICES.iter().find(|d| d.provider == id).unwrap();
            let info = HidInfo {
                vendor_id: d.vid,
                product_id: d.pid,
                serial: "SYNTHETIC-LONG-SERIAL".into(),
                path: "synthetic-path".into(),
                usage_page: 0xffff,
                interface: 4,
                ..Default::default()
            };
            let infos = vec![info.clone(); 64];
            let expected = candidate(id, d, &info);
            let baseline = allocations(|| {
                std::hint::black_box(candidate(id, d, &info));
            });
            assert_eq!(
                allocations(|| assert_eq!(candidate_group(id, d, &info, &infos), expected)),
                baseline
            );
        }
        let d = DEVICES
            .iter()
            .find(|d| d.provider == "steelseries" && d.variant.starts_with("exchange_"))
            .unwrap();
        let info = HidInfo {
            vendor_id: d.vid,
            product_id: d.pid,
            usage_page: 0xff43,
            interface: 3,
            ..Default::default()
        };
        assert_eq!(
            allocations(|| assert_eq!(
                candidate_group("steelseries", d, &info, std::slice::from_ref(&info)),
                candidate("steelseries", d, &info)
            )),
            0
        );
    }
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod cache_test_support;

#[cfg(test)]
mod cache_retention_tests {
    use super::cache_test_support::{FakeClock, FakeHid, context, info};
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct EnumerationFailure;
    impl HidTransport for EnumerationFailure {
        fn enumerate(&self, _: u16) -> Result<Vec<HidInfo>, ProviderError> {
            Err(ProviderError::new("synthetic enumeration failure"))
        }
        fn open(&self, _: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
            panic!("enumeration failure must not open hardware")
        }
    }

    #[test]
    fn confirmed_hotplug_absence_releases_protocol_hints_but_enumeration_errors_do_not() {
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        for id in ["razer", "steelseries", "wlmouse", "mchose"] {
            let mut provider = HidProvider::new(id);
            for index in 0..1000 {
                let key = format!("invented-{index}");
                provider
                    .last
                    .insert(key.clone(), Reading::new(&key, "Invented", id, 0));
                provider.last_observed.insert(key.clone(), Duration::ZERO);
                provider
                    .razer_dead
                    .insert(key.clone(), Duration::from_secs(300));
                provider.razer_cache.insert(key.clone(), (key.clone(), 0));
                provider.steel_path.insert(key.clone(), key.clone());
                provider.family_names.insert(key.clone(), "Invented".into());
                provider.mchose_models.insert(key, 0x31);
            }
            assert!(
                provider
                    .poll(&EnumerationFailure, &context(&clock, &cancel))
                    .is_err()
            );
            assert_eq!(provider.last.len(), 1000);
            assert_eq!(provider.razer_dead.len(), 1000);
            // After the existing finite MCHOSE absent-receiver grace, all rows
            // must be released; errors above did not establish absence.
            clock.0.store(301_000, std::sync::atomic::Ordering::Relaxed);
            assert!(
                provider
                    .poll(&FakeHid::new(vec![], vec![]), &context(&clock, &cancel))
                    .unwrap()
                    .is_empty()
            );
            assert!(provider.last.is_empty());
            assert!(provider.last_observed.is_empty());
            assert!(provider.razer_dead.is_empty());
            assert!(provider.razer_cache.is_empty());
            assert!(provider.steel_path.is_empty());
            assert!(provider.family_names.is_empty());
            assert!(provider.mchose_models.is_empty());
            clock.0.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn centurion_feature_cache_is_released_on_connection_invalidation() {
        let mut provider = HidProvider::new("logitech_centurion");
        provider
            .centurion_features
            .insert("reconnected-receiver".into(), (3, Some(7)));
        provider.invalidate();
        assert!(provider.centurion_features.is_empty());
    }

    #[test]
    fn present_razer_mouse_keeps_long_sleep_identity_without_expired_dead_paths() {
        let mut provider = HidProvider::new("razer");
        let device = info(0x1532, 0x00b9, 1);
        let d = DEVICES
            .iter()
            .find(|d| d.provider == "razer" && d.pid == device.product_id)
            .unwrap();
        let key = reading_key("razer", d, &device);
        let mut reading = Reading::new(&key, "Invented mouse", "razer", 0);
        reading.kind = "mouse".into();
        reading.level = Some(71);
        provider.last.insert(key.clone(), reading);
        provider.last_observed.insert(key.clone(), Duration::ZERO);
        provider
            .razer_dead
            .insert(device.path.clone(), Duration::from_secs(300));
        provider
            .razer_cache
            .insert("hint".into(), (device.path.clone(), 0x1f));
        provider.prune_enumerated(std::slice::from_ref(&device), Duration::from_secs(3600));
        let sleeping = provider.cached(&key, Duration::from_secs(3600)).unwrap();
        assert_eq!(sleeping.connection, Connection::Sleeping);
        assert_eq!((sleeping.level, sleeping.charging), (None, None));
        assert_eq!(provider.last_observed.len(), 1);
        assert_eq!(provider.razer_cache.len(), 1);
        assert!(provider.razer_dead.is_empty());
        provider.prune_enumerated(&[], Duration::from_secs(3601));
        assert!(provider.last.is_empty());
        assert!(provider.last_observed.is_empty());
        assert!(provider.razer_cache.is_empty());
    }

    #[test]
    fn absent_mchose_grace_is_preserved_then_released_and_logitech_slots_do_not_accumulate() {
        let clock = FakeClock::default();
        let cancel = AtomicBool::new(false);
        let mut provider = HidProvider::new("mchose");
        let mut reading = Reading::new("invented", "Invented", "mchose", 0);
        reading.level = Some(80);
        provider.last.insert("invented".into(), reading);
        provider
            .last_observed
            .insert("invented".into(), Duration::ZERO);
        let empty = FakeHid::new(vec![], vec![]);
        clock.0.store(299_000, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            provider
                .poll(&empty, &context(&clock, &cancel))
                .unwrap()
                .len(),
            1
        );
        clock.0.store(300_000, std::sync::atomic::Ordering::Relaxed);
        assert!(
            provider
                .poll(&empty, &context(&clock, &cancel))
                .unwrap()
                .is_empty()
        );
        assert!(provider.last.is_empty());
        assert!(provider.last_observed.is_empty());
        let mut provider = HidProvider::new("logitech");
        for index in 0..1000 {
            let key = format!("invented-{index}");
            provider.logitech_identity.insert(
                key.clone(),
                ("Invented".into(), "mouse".into(), key.clone()),
            );
            provider.logitech_asleep.insert(key.clone());
            provider.logitech_slots.insert(key.clone(), key.clone());
            provider
                .last
                .insert(key.clone(), Reading::new(&key, "Invented", "logitech", 0));
            provider.last_observed.insert(key, Duration::ZERO);
        }
        assert!(
            provider
                .poll(&empty, &context(&clock, &cancel))
                .unwrap()
                .is_empty()
        );
        assert!(provider.logitech_identity.is_empty());
        assert!(provider.logitech_asleep.is_empty());
        assert!(provider.logitech_slots.is_empty());
        assert!(provider.last.is_empty());
        assert!(provider.last_observed.is_empty());
    }
}
