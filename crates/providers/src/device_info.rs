//! Read-only firmware access on reviewed modern Razer mouse collections.
//! Uses standard 00:81 from the published Razer protocol; no mode changes,
//! configuration writes, serial queries, scans of command IDs or retry loops.
use crate::{
    controls::{GuardedSession, connection_stamp, open_connected},
    provider::{is_bluetooth, receiver_key, trusted_identity},
};
use hb_core::*;
use std::time::Duration;

pub fn supported(device: &ConfigurationDevice) -> bool {
    device.kind == "mouse"
        && device.source == "razer"
        && device.via == "usb"
        && device
            .key
            .split(':')
            .nth(1)
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .is_some_and(|pid| crate::dpi::razer_maximum(pid).is_some() || matches!(pid, 0x009f))
}
pub fn firmware_query() -> [u8; 91] {
    let mut report = [0; 91];
    report[2] = 0x1f;
    report[6] = 2;
    report[8] = 0x81;
    report[89] = report[3..89].iter().fold(0, |a, b| a ^ b);
    report
}
pub fn is_firmware_query(report: &[u8]) -> bool {
    report == firmware_query()
}

pub fn parse_firmware(report: &[u8]) -> Result<FirmwareVersion, ProviderError> {
    let invalid = || {
        ProviderError::new(
            "The device returned an incomplete or unrelated firmware response. Try Refresh information.",
        )
    };
    if report.len() != 91
        || report[0] != 0
        || report[2] != 0x1f
        || report[3..6] != [0, 0, 0]
        || !(2..=80).contains(&report[6])
        || report[7..9] != [0, 0x81]
        || report[89] != report[3..89].iter().fold(0, |a, b| a ^ b)
    {
        return Err(invalid());
    }
    match report[1] {
        2 => {}
        1 => {
            return Err(ProviderError::new(
                "The device is busy. Try Refresh information shortly.",
            ));
        }
        5 => {
            return Err(ProviderError::new(
                "This connection does not report its firmware version.",
            ));
        }
        4 => {
            return Err(ProviderError::new(
                "Wake the mouse, then select Refresh information.",
            ));
        }
        _ => {
            return Err(ProviderError::new(
                "Firmware could not be read. Try Refresh information.",
            ));
        }
    }
    if matches!((report[9], report[10]), (0, 0) | (255, 255)) {
        return Err(ProviderError::new(
            "This connection did not provide a firmware version.",
        ));
    }
    Ok(FirmwareVersion {
        major: report[9],
        minor: report[10],
    })
}
pub fn execute(
    request: &DeviceInfoRequest,
    hid: &dyn HidTransport,
    context: &PollContext<'_>,
) -> DeviceInfoOutcome {
    let result = (|| {
        let target = &request.target;
        let device = &target.device;
        if !supported(device) {
            return Err(ProviderError::new(
                "Firmware reading is not available for this device yet.",
            ));
        }
        if !device.online() {
            return Err(ProviderError::new(
                "Connect or wake the device, then select Refresh information.",
            ));
        }
        if !context.active() || hid.generation() != target.generation {
            return Err(ProviderError::new(
                "The connection changed. Open Device info again.",
            ));
        }
        let devices = hid.enumerate(0x1532)?;
        if devices.len() > 512 {
            return Err(ProviderError::new(
                "Device information is unavailable. Try Refresh information.",
            ));
        }
        let matches = |info: &&HidInfo| {
            info.vendor_id == 0x1532
                && info.interface == 0
                && info.feature_length == Some(91)
                && !is_bluetooth(&info.path)
                && device.key == format!("razer:{:04x}:{}", info.product_id, trusted_identity(info))
        };
        let mut matching = devices.iter().filter(matches);
        let info = matching.next().ok_or_else(|| {
            ProviderError::new("The device is temporarily unavailable. Try Refresh information.")
        })?;
        if matching.next().is_some()
            || devices.iter().any(|other| {
                other.vendor_id == info.vendor_id
                    && other.product_id == info.product_id
                    && trusted_identity(other) == trusted_identity(info)
                    && receiver_key(other) != receiver_key(info)
            })
        {
            return Err(ProviderError::new(
                "More than one device matches this connection. Firmware was not read.",
            ));
        }
        if device
            .serial
            .as_ref()
            .is_some_and(|s| !s.trim().eq_ignore_ascii_case(info.serial.trim()))
            || device.container.as_ref().is_some_and(|c| {
                info.container
                    .as_ref()
                    .is_none_or(|v| !c.trim().eq_ignore_ascii_case(v.trim()))
            })
        {
            return Err(ProviderError::new(
                "The selected device changed. Open Device info again.",
            ));
        }
        let stamp = connection_stamp(target, hid, info)?;
        if !context.active() || hid.generation() != target.generation {
            return Err(ProviderError::new(
                "The connection changed. Open Device info again.",
            ));
        }
        let session = open_connected(hid, info, stamp)?;
        let mut session = GuardedSession::new(session, hid, context, target.generation);
        session.send_feature(&firmware_query())?;
        context.sleep(Duration::from_millis(if info.product_id == 0x0094 {
            400
        } else {
            60
        }));
        if !context.active() {
            return Err(ProviderError::new("Firmware reading was cancelled."));
        }
        let response = session.feature(0, 91)?;
        if stamp.is_some() && hid.connection_stamp(info) != stamp {
            return Err(ProviderError::new(
                "The device reconnected. Try Refresh information.",
            ));
        }
        parse_firmware(&response)
    })();
    DeviceInfoOutcome::from_result(request, context.clock.unix(), result)
}
