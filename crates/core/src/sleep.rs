//! Explicit hardware idle timers, independent of polling observations/history.
use crate::ControlTarget;

#[derive(Clone, Debug)]
pub struct SleepRequest {
    pub request: u64,
    pub target: ControlTarget,
    /// None reads the timer; Some applies it once. Zero is allowed only where documented.
    pub seconds: Option<u16>,
    /// Refuse a write if another application or the user changed the timer.
    pub expected: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct SleepOutcome {
    pub request: u64,
    pub key: String,
    pub generation: u64,
    pub connection_stamp: Option<crate::ConnectionStamp>,
    pub seconds: Option<u16>,
    pub may_have_changed: bool,
    pub failure: Option<String>,
}
impl SleepOutcome {
    pub fn failed(request: &SleepRequest, message: impl Into<String>) -> Self {
        Self {
            request: request.request,
            key: request.target.device.key.clone(),
            generation: request.target.generation,
            connection_stamp: None,
            seconds: None,
            may_have_changed: false,
            failure: Some(message.into()),
        }
    }
}
