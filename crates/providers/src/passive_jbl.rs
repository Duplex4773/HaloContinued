//! Persistent read-only collections, serviced by the existing bounded worker pool.
use hb_core::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub(crate) const MAX_SESSIONS: usize = 8;
const MAX_REPORTS: usize = 32;
const RETRY: Duration = Duration::from_secs(15);
type ListeningResult = Result<(Option<u8>, bool), ProviderError>;
#[derive(Default)]
pub(crate) struct PassiveJbl {
    sessions: BTreeMap<String, Box<dyn HidSession>>,
    retry: BTreeMap<String, (Duration, String)>,
    power: BTreeMap<String, bool>,
    generation: Option<u64>,
    pub probe_listen: bool,
}
impl PassiveJbl {
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.retry.clear();
        self.power.clear();
        self.generation = None;
    }
    pub fn delay(&self) -> Option<Duration> {
        if !self.sessions.is_empty() {
            Some(Duration::from_secs(1))
        } else if !self.retry.is_empty() {
            Some(RETRY)
        } else {
            None
        }
    }
    pub fn prepare(&mut self, generation: u64, paths: &BTreeSet<String>) {
        if self.generation != Some(generation) {
            self.clear();
        }
        self.generation = Some(generation);
        self.sessions.retain(|p, _| paths.contains(p));
        self.retry.retain(|p, _| paths.contains(p));
        self.power.retain(|p, _| paths.contains(p));
    }
    pub fn listen(
        &mut self,
        info: &HidInfo,
        hid: &dyn HidTransport,
        c: &PollContext<'_>,
    ) -> (ListeningResult, Vec<String>) {
        let path = &info.path;
        let mut lines = Vec::new();
        if !c.active() {
            self.clear();
            return (Err(ProviderError::new("JBL collection cancelled")), lines);
        }
        let first = !self.sessions.contains_key(path);
        if first {
            if let Some((at, error)) = self.retry.get(path)
                && *at > c.clock.monotonic()
            {
                return (Err(ProviderError::new(error.clone())), lines);
            }
            if self.sessions.len() >= MAX_SESSIONS {
                return (
                    Err(ProviderError::new("JBL collection capacity reached")),
                    lines,
                );
            }
            match hid.open(info) {
                Ok(s) => {
                    self.sessions.insert(path.clone(), s);
                    self.retry.remove(path);
                }
                Err(e) => {
                    lines.push(format!("open: {e}"));
                    self.retry
                        .insert(path.clone(), (c.clock.monotonic() + RETRY, e.to_string()));
                    return (Err(e), lines);
                }
            }
        }
        let mut level = None;
        let mut fresh_online = false;
        let mut failure = None;
        // Only an explicitly configured one-shot probe may wait for its first report.
        let probe = first && self.probe_listen;
        let probe_deadline = c.clock.monotonic() + Duration::from_secs(10);
        let mut reports = 0;
        for _ in 0..if probe { 40 } else { MAX_REPORTS } {
            if !c.active() {
                self.clear();
                return (Err(ProviderError::new("JBL collection cancelled")), lines);
            }
            if probe && c.clock.monotonic() >= probe_deadline {
                break;
            }
            let timeout = if probe && level.is_none() {
                Duration::from_millis(250)
            } else {
                Duration::ZERO
            };
            match self.sessions.get_mut(path).unwrap().read(64, timeout) {
                Ok(r) if r.is_empty() => {
                    if probe && level.is_none() {
                        continue;
                    }
                    break;
                }
                Ok(r) => {
                    reports += 1;
                    if let Some(b) = crate::protocols::jbl(&r) {
                        level = Some(b.level);
                        fresh_online = true;
                    } else if r.first() == Some(&9)
                        && let Some(on) = r.get(1)
                    {
                        self.power.insert(path.clone(), *on != 0);
                        if *on == 0 {
                            fresh_online = false;
                        }
                        lines.push(format!(
                            "power report: headset {}",
                            if *on == 0 { "OFF" } else { "ON" }
                        ));
                    }
                    if reports >= MAX_REPORTS {
                        break;
                    }
                }
                Err(e) => {
                    lines.push(format!("read: {e}"));
                    failure = Some(e);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.sessions.remove(path);
            self.retry.insert(
                path.clone(),
                (c.clock.monotonic() + RETRY, error.to_string()),
            );
            return (Err(error), lines);
        }
        (Ok((level, fresh_online)), lines)
    }
}
