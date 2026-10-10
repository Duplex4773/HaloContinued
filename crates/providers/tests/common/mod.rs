#![allow(dead_code)]
use hb_core::*;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Debug)]
pub enum Step {
    Write(Vec<u8>),
    WriteError(Vec<u8>, &'static str),
    Send(Vec<u8>),
    SendError(Vec<u8>, &'static str),
    ReadSized(usize, Duration, Result<Vec<u8>, &'static str>),
    Read(Result<Vec<u8>, &'static str>),
    Feature(u8, usize, Result<Vec<u8>, &'static str>),
    Input(u8, usize, Result<Vec<u8>, &'static str>),
}
pub struct FakeSession(Arc<Mutex<VecDeque<Step>>>);
impl HidSession for FakeSession {
    fn input_report(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        match self.0.lock().unwrap().pop_front().unwrap() {
            Step::Input(want_id, want_length, result) => {
                assert_eq!((id, length), (want_id, want_length));
                result.map_err(ProviderError::new)
            }
            step => panic!("unexpected input report {step:?}"),
        }
    }
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        match self.0.lock().unwrap().pop_front().unwrap() {
            Step::WriteError(want, error) => {
                assert_eq!(data, want);
                Err(ProviderError::new(error))
            }
            Step::Write(want) => {
                assert_eq!(data, want);
                Ok(())
            }
            step => panic!("unexpected write {step:?}"),
        }
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        match self.0.lock().unwrap().pop_front().unwrap() {
            Step::SendError(want, error) => {
                assert_eq!(data, want);
                Err(ProviderError::new(error))
            }
            Step::Send(want) => {
                assert_eq!(data, want);
                Ok(())
            }
            step => panic!("unexpected feature write {step:?}"),
        }
    }
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        match self.0.lock().unwrap().pop_front().unwrap() {
            Step::ReadSized(want_length, want_timeout, result) => {
                assert_eq!((length, timeout), (want_length, want_timeout));
                result.map_err(ProviderError::new)
            }
            Step::Read(result) => result.map_err(ProviderError::new),
            step => panic!("unexpected read {step:?}"),
        }
    }
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        match self.0.lock().unwrap().pop_front().unwrap() {
            Step::Feature(want_id, want_length, result) => {
                assert_eq!((id, length), (want_id, want_length));
                result.map_err(ProviderError::new)
            }
            step => panic!("unexpected feature read {step:?}"),
        }
    }
}
pub struct FakeHid {
    pub infos: Vec<HidInfo>,
    pub steps: Arc<Mutex<VecDeque<Step>>>,
    pub opened: Mutex<Vec<String>>,
    pub open_errors: std::collections::BTreeMap<String, &'static str>,
}
impl FakeHid {
    pub fn new(infos: Vec<HidInfo>, steps: Vec<Step>) -> Self {
        Self {
            infos,
            steps: Arc::new(Mutex::new(steps.into())),
            opened: Mutex::new(vec![]),
            open_errors: Default::default(),
        }
    }
    pub fn done(&self) {
        assert!(self.steps.lock().unwrap().is_empty());
    }
    pub fn fail_open(mut self, path: impl Into<String>, error: &'static str) -> Self {
        self.open_errors.insert(path.into(), error);
        self
    }
}
impl HidTransport for FakeHid {
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        Ok(self
            .infos
            .iter()
            .filter(|i| i.vendor_id == vendor)
            .cloned()
            .collect())
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        self.opened.lock().unwrap().push(info.path.clone());
        if let Some(error) = self.open_errors.get(&info.path) {
            return Err(ProviderError::new(*error));
        }
        Ok(Box::new(FakeSession(self.steps.clone())))
    }
}
#[derive(Default)]
pub struct FakeClock(pub AtomicU64);
impl Clock for FakeClock {
    fn unix(&self) -> i64 {
        (self.0.load(Ordering::Relaxed) / 1000) as i64
    }
    fn monotonic(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
    fn sleep(&self, duration: Duration) {
        self.0
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
    }
}
pub fn info(vid: u16, pid: u16, page: u16) -> HidInfo {
    HidInfo {
        path: format!("device-{vid:04x}-{pid:04x}"),
        vendor_id: vid,
        product_id: pid,
        usage_page: page,
        usage: 1,
        interface: 0,
        ..Default::default()
    }
}
pub fn context<'a>(clock: &'a FakeClock, cancel: &'a AtomicBool) -> PollContext<'a> {
    PollContext {
        clock,
        cancelled: cancel,
        deadline: clock.monotonic() + Duration::from_secs(10),
        playstation_full_mode: false,
    }
}
