//! Deterministic passive collection tests; no native devices or threads.
mod common;
use common::{FakeClock, context};
use hb_core::*;
use hb_providers::HidProvider;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct State {
    reports: VecDeque<Result<Vec<u8>, &'static str>>,
    opens: usize,
    drops: usize,
    reads: usize,
    fail_open: bool,
    fail_second: bool,
    timeouts: Vec<Duration>,
}
struct Session(Arc<Mutex<State>>);
impl Drop for Session {
    fn drop(&mut self) {
        self.0.lock().unwrap().drops += 1;
    }
}
impl HidSession for Session {
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        assert_eq!(length, 64);
        let mut s = self.0.lock().unwrap();
        s.reads += 1;
        s.timeouts.push(timeout);
        s.reports
            .pop_front()
            .unwrap_or(Ok(vec![]))
            .map_err(ProviderError::new)
    }
    fn write(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("JBL must never write")
    }
    fn send_feature(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        panic!("JBL must never send features")
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        panic!("JBL must never query features")
    }
}
struct Transport {
    infos: Vec<HidInfo>,
    state: Arc<Mutex<State>>,
    generation: AtomicU64,
}
impl Transport {
    fn new(count: usize) -> Self {
        Self {
            infos: (0..count)
                .map(|i| HidInfo {
                    vendor_id: 0x0ecb,
                    product_id: 0x2088,
                    usage_page: 0xff13,
                    usage: 1,
                    path: format!("jbl-{i}"),
                    serial: format!("serial-{i}"),
                    container: Some(format!("container-{i}")),
                    ..Default::default()
                })
                .collect(),
            state: Arc::new(Mutex::new(State::default())),
            generation: AtomicU64::new(0),
        }
    }
    fn push(&self, reports: &[&[u8]]) {
        self.state
            .lock()
            .unwrap()
            .reports
            .extend(reports.iter().map(|r| Ok(r.to_vec())));
    }
}
impl HidTransport for Transport {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .infos
            .iter()
            .filter(|i| i.vendor_id == vendor)
            .cloned()
            .collect())
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        let mut s = self.state.lock().unwrap();
        s.opens += 1;
        if s.fail_open || s.fail_second && info.path == "jbl-1" {
            Err(ProviderError::new("denied"))
        } else {
            Ok(Box::new(Session(self.state.clone())))
        }
    }
}
#[test]
fn persistent_collection_captures_late_reports_newest_wins_and_quiet_keeps_timestamp() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    assert_eq!(p.next_poll_delay(), Some(Duration::from_secs(1)));
    h.push(&[&[0x2f, 1], &[9, 1], &[8, 40], &[8, 73], &[8, 101]]);
    clock.0.store(1000, Ordering::Relaxed);
    let live = p.poll(&h, &context(&clock, &cancel)).unwrap().remove(0);
    assert_eq!(
        (live.level, live.charging, live.timestamp),
        (Some(73), Some(false), 1)
    );
    assert!(live.online());
    assert_eq!(live.container.as_deref(), Some("container-0"));
    clock.0.store(900000, Ordering::Relaxed);
    let quiet = p.poll(&h, &context(&clock, &cancel)).unwrap().remove(0);
    assert_eq!(
        (quiet.key, quiet.level, quiet.timestamp, quiet.connection),
        (live.key, Some(73), 1, Connection::Sleeping)
    );
    let s = h.state.lock().unwrap();
    assert_eq!(s.opens, 1);
    assert!(s.timeouts.iter().all(|t| *t == Duration::ZERO));
}
#[test]
fn power_off_after_level_is_sleeping_and_power_on_without_level_never_refreshes() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.push(&[&[8, 95], &[9, 0]]);
    let off = p.poll(&h, &context(&clock, &cancel)).unwrap().remove(0);
    assert_eq!(
        (off.level, off.connection),
        (Some(95), Connection::Sleeping)
    );
    assert!(p.diagnostics().iter().any(|l| l.contains("OFF")));
    h.push(&[&[9, 1]]);
    clock.0.store(1000, Ordering::Relaxed);
    let on = p.poll(&h, &context(&clock, &cancel)).unwrap().remove(0);
    assert_eq!(
        (on.level, on.timestamp, on.connection),
        (Some(95), 0, Connection::Sleeping)
    );
}
#[test]
fn report_order_revives_only_when_a_valid_level_follows_power_off() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.push(&[&[8, 80], &[9, 0], &[9, 1]]);
    assert_eq!(
        p.poll(&h, &context(&clock, &cancel)).unwrap()[0].connection,
        Connection::Sleeping
    );
    h.push(&[&[9, 0], &[8, 101], &[0x2f, 1]]);
    assert_eq!(
        p.poll(&h, &context(&clock, &cancel)).unwrap()[0].connection,
        Connection::Sleeping
    );
    clock.0.store(2000, Ordering::Relaxed);
    h.push(&[&[8, 79]]);
    let live = p.poll(&h, &context(&clock, &cancel)).unwrap().remove(0);
    assert_eq!(
        (live.level, live.timestamp, live.connection),
        (Some(79), 2, Connection::Online)
    );
    h.push(&[&[9, 0], &[8, 78]]);
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap()[0].online());
}
#[test]
fn removal_generation_invalidate_cancel_and_drop_close_sessions() {
    let mut h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.push(&[&[8, 70]]);
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    h.generation.store(1, Ordering::Relaxed);
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(h.state.lock().unwrap().drops, 1);
    p.invalidate();
    assert_eq!(h.state.lock().unwrap().drops, 2);
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    assert_eq!(h.state.lock().unwrap().drops, 3);
    cancel.store(false, Ordering::Relaxed);
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    h.infos.clear();
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    assert_eq!(h.state.lock().unwrap().drops, 4);
    h.infos = Transport::new(1).infos;
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    drop(p);
    let s = h.state.lock().unwrap();
    assert_eq!(s.opens, 5);
    assert_eq!(s.drops, 5);
}
#[test]
fn open_and_read_failure_use_fifteen_second_backoff_then_recover() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.state.lock().unwrap().fail_open = true;
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    assert_eq!(h.state.lock().unwrap().opens, 1);
    assert_eq!(p.next_poll_delay(), Some(Duration::from_secs(15)));
    h.state.lock().unwrap().fail_open = false;
    clock.0.store(15000, Ordering::Relaxed);
    h.state.lock().unwrap().reports.push_back(Err("unplugged"));
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    assert_eq!(h.state.lock().unwrap().drops, 1);
    assert!(p.poll(&h, &context(&clock, &cancel)).is_err());
    assert_eq!(h.state.lock().unwrap().opens, 2);
    clock.0.store(30000, Ordering::Relaxed);
    h.push(&[&[8, 60]]);
    assert_eq!(
        p.poll(&h, &context(&clock, &cancel)).unwrap()[0].level,
        Some(60)
    );
    assert_eq!(h.state.lock().unwrap().opens, 3);
}
#[test]
fn failed_read_discards_uncertain_new_level_and_engine_retains_stale_cache_and_preferences() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.push(&[&[8, 75]]);
    let initial = p.poll(&h, &context(&clock, &cancel)).unwrap();
    let key = initial[0].key.clone();
    let mut settings = Settings::default();
    settings.devices.entry(key.clone()).or_default().name = Some("My headphones".into());
    settings.devices.get_mut(&key).unwrap().hidden = true;
    let mut engine = Engine::new(settings, Estimator::default());
    engine.apply("jbl", Ok(initial), 0.0, false);
    h.push(&[&[8, 5]]);
    h.state
        .lock()
        .unwrap()
        .reports
        .push_back(Err("communication lost"));
    let failed = p.poll(&h, &context(&clock, &cancel));
    assert!(failed.is_err());
    assert!(engine.apply("jbl", failed, 1.0, false).is_empty());
    let stale = engine.readings().remove(0);
    assert_eq!(
        (stale.level, stale.timestamp, stale.connection),
        (Some(75), 0, Connection::Stale)
    );
    let cooldown = p.poll(&h, &context(&clock, &cancel));
    assert!(cooldown.is_err());
    engine.apply("jbl", cooldown, 2.0, false);
    assert_eq!(engine.readings()[0].level, Some(75));
    assert!(engine.settings.devices[&key].hidden);
    assert_eq!(
        engine.settings.devices[&key].name.as_deref(),
        Some("My headphones")
    );
    clock.0.store(15000, Ordering::Relaxed);
    // Successfully reading silence is healthy, and returns the original sleeping value.
    let quiet = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(
        (quiet[0].level, quiet[0].connection.clone()),
        (Some(75), Connection::Sleeping)
    );
    engine.apply("jbl", Ok(quiet), 15.0, false);
    assert!(engine.snapshot(15).errors.is_empty());
}
#[test]
fn mixed_healthy_and_failed_collections_keep_results_and_silent_health_is_success() {
    let h = Transport::new(2);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.state.lock().unwrap().fail_second = true;
    // A successful silent sibling is sufficient even though another collection fails.
    assert!(p.poll(&h, &context(&clock, &cancel)).unwrap().is_empty());
    assert!(p.diagnostics().iter().any(|line| line.contains("denied")));
    h.push(&[&[8, 65]]);
    let live = p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].level, Some(65));
    assert!(live[0].online());
    assert_eq!(h.state.lock().unwrap().opens, 2);
    assert!(p.diagnostics().iter().any(|line| line.contains("denied")));
}
#[test]
fn reports_sessions_and_diagnostics_are_bounded() {
    let h = Transport::new(10);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl");
    h.state
        .lock()
        .unwrap()
        .reports
        .extend((0..1000).map(|_| Ok(vec![9, 1])));
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    let s = h.state.lock().unwrap();
    assert_eq!(s.opens, 8);
    assert_eq!(s.reads, 8 * 32);
    assert!(p.diagnostics().len() <= 120);
}
#[test]
fn explicit_probe_waits_only_on_first_open() {
    let h = Transport::new(1);
    let clock = FakeClock::default();
    let cancel = AtomicBool::new(false);
    let mut p = HidProvider::new("jbl").with_probe_listen();
    h.push(&[&[], &[], &[8, 75]]);
    assert_eq!(
        p.poll(&h, &context(&clock, &cancel)).unwrap()[0].level,
        Some(75)
    );
    let first = h.state.lock().unwrap().timeouts.clone();
    assert_eq!(
        first,
        vec![Duration::from_millis(250); 3]
            .into_iter()
            .chain([Duration::ZERO])
            .collect::<Vec<_>>()
    );
    p.poll(&h, &context(&clock, &cancel)).unwrap();
    assert_eq!(
        h.state.lock().unwrap().timeouts.last(),
        Some(&Duration::ZERO)
    );
}
