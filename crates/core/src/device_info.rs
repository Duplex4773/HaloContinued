//! On-demand information requests. Firmware reads never authorize configuration.
use crate::{ControlTarget, ProviderError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareVersion {
    pub major: u8,
    pub minor: u8,
}
impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfoRequest {
    pub request: u64,
    pub target: ControlTarget,
}

#[derive(Clone, Debug)]
pub struct DeviceInfoOutcome {
    pub request: u64,
    pub target: ControlTarget,
    pub checked_at: i64,
    /// Version reported by the selected connection. The command does not
    /// distinguish a receiver's own firmware from a paired mouse's firmware.
    pub firmware: Option<FirmwareVersion>,
    pub failure: Option<String>,
}
impl DeviceInfoOutcome {
    pub fn failed(request: &DeviceInfoRequest, error: impl Into<String>) -> Self {
        Self {
            request: request.request,
            target: request.target.clone(),
            checked_at: 0,
            firmware: None,
            failure: Some(error.into()),
        }
    }
    pub fn from_result(
        request: &DeviceInfoRequest,
        checked_at: i64,
        result: Result<FirmwareVersion, ProviderError>,
    ) -> Self {
        let mut value = Self::failed(request, "");
        value.checked_at = checked_at;
        match result {
            Ok(version) => {
                value.firmware = Some(version);
                value.failure = None;
            }
            Err(error) => value.failure = Some(error.message),
        }
        value
    }
}
