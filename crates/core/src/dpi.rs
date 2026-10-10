//! On-demand hardware sensitivity, independent of battery and polling policy.
use crate::ControlTarget;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpiValue {
    pub x: u16,
    pub y: u16,
}
impl DpiValue {
    pub fn uniform(value: u16) -> Self {
        Self { x: value, y: value }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpiState {
    pub value: DpiValue,
    pub profile: Option<u16>,
    pub stage: Option<u8>,
}
#[derive(Clone, Debug)]
pub struct DpiRequest {
    pub request: u64,
    pub target: ControlTarget,
    pub value: Option<DpiValue>,
    /// Required for writes. Refuse a change if the hardware changed since the UI read.
    pub expected: Option<DpiState>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DpiBackup {
    pub device: String,
    pub timestamp: i64,
    pub original: DpiState,
    pub requested: DpiValue,
    /// Original stage report or complete onboard sector, including checksum.
    pub profile_bytes: Vec<u8>,
    pub profile_format: Option<u8>,
}
#[derive(Clone, Debug)]
pub struct DpiLimits {
    /// Inclusive minimum, maximum and increment. Separate axes can differ.
    pub x: Vec<(u16, u16, u16)>,
    pub y: Vec<(u16, u16, u16)>,
    /// Catalog bounds have no hardware-advertised increment.
    pub from_model: bool,
}
#[derive(Clone, Debug)]
pub struct DpiOutcome {
    pub request: u64,
    pub key: String,
    pub generation: u64,
    pub current: Option<DpiState>,
    pub previous: Option<DpiState>,
    pub may_have_changed: bool,
    pub failure: Option<String>,
    pub limits: Option<DpiLimits>,
}
impl DpiOutcome {
    pub fn failed(request: &DpiRequest, message: impl Into<String>) -> Self {
        Self {
            request: request.request,
            key: request.target.device.key.clone(),
            generation: request.target.generation,
            current: None,
            previous: None,
            may_have_changed: false,
            failure: Some(message.into()),
            limits: None,
        }
    }
}
