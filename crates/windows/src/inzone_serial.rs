//! The original Sony INZONE H9 publishes a USB serial interface. Discover it
//! alongside HID endpoints so it shares the existing device cache and worker.
use crate::transport::{container, wide};
use hb_core::{HidInfo, HidSession, ProviderError};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    time::Duration,
};
use windows::{
    Win32::{
        Devices::{Communication::*, DeviceAndDriverInstallation::*},
        Foundation::HANDLE,
    },
    core::{GUID, PCWSTR},
};

const COM_PORT: GUID = GUID::from_u128(0x86e0d1e0_8089_11d0_9ce4_08003e301f73);
pub(super) const VENDOR: u16 = 0x054c;
const PRODUCT: u16 = 0x0e53;

fn inzone_instance(path: &str) -> Option<String> {
    let mut parts = path.strip_prefix(r"\\?\")?.split('#');
    let bus = parts.next()?;
    let hardware = parts.next()?;
    let instance = parts.next()?;
    let interface = parts.next()?;
    let mut ids = hardware.split('&');
    if !bus.eq_ignore_ascii_case("usb")
        || !ids.next()?.eq_ignore_ascii_case("vid_054c")
        || !ids.next()?.eq_ignore_ascii_case("pid_0e53")
        || !interface.eq_ignore_ascii_case("{86e0d1e0-8089-11d0-9ce4-08003e301f73}")
        || parts.next().is_some()
        || instance.is_empty()
    {
        return None;
    }
    Some(format!("{bus}\\{hardware}\\{instance}"))
}

pub(super) fn enumerate() -> Result<Vec<HidInfo>, ProviderError> {
    let flags = CM_GET_DEVICE_INTERFACE_LIST_PRESENT;
    for _ in 0..2 {
        let mut size = 0;
        let status = unsafe {
            CM_Get_Device_Interface_List_SizeW(&mut size, &COM_PORT, PCWSTR::null(), flags)
        };
        if status != CR_SUCCESS || size > 65_536 {
            return Err(ProviderError::new("Could not list USB serial devices"));
        }
        if size <= 1 {
            return Ok(Vec::new());
        }
        let mut buffer = vec![0; size as usize];
        let status =
            unsafe { CM_Get_Device_Interface_ListW(&COM_PORT, PCWSTR::null(), &mut buffer, flags) };
        if status == CR_BUFFER_SMALL {
            continue;
        }
        if status != CR_SUCCESS {
            return Err(ProviderError::new("Could not read USB serial devices"));
        }
        return Ok(buffer
            .split(|&unit| unit == 0)
            .take_while(|part| !part.is_empty())
            .filter_map(|part| {
                let path = String::from_utf16(part).ok()?;
                let instance = wide(&inzone_instance(&path)?);
                let mut node = 0;
                let status = unsafe {
                    CM_Locate_DevNodeW(
                        &mut node,
                        PCWSTR(instance.as_ptr()),
                        CM_LOCATE_DEVNODE_NORMAL,
                    )
                };
                (status == CR_SUCCESS).then(|| HidInfo {
                    path,
                    vendor_id: VENDOR,
                    product_id: PRODUCT,
                    interface: -1,
                    container: container(node),
                    ..HidInfo::default()
                })
            })
            .collect());
    }
    Err(ProviderError::new(
        "USB serial devices changed during discovery",
    ))
}

pub(super) fn is_endpoint(info: &HidInfo) -> bool {
    (info.vendor_id, info.product_id, info.interface) == (VENDOR, PRODUCT, -1)
        && inzone_instance(&info.path).is_some()
}

pub(super) fn open(info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&info.path)?;
    let handle = HANDLE(file.as_raw_handle());
    let mut config = DCB {
        DCBlength: size_of::<DCB>() as u32,
        ..DCB::default()
    };
    unsafe { GetCommState(handle, &mut config) }.map_err(native_error)?;
    config.BaudRate = 115_200;
    config.ByteSize = 8;
    config.Parity = NOPARITY;
    config.StopBits = ONESTOPBIT;
    // Binary, DTR enabled, RTS enabled. No software or hardware flow control.
    config._bitfield = 1 | (1 << 4) | (1 << 12);
    unsafe {
        SetCommState(handle, &config).map_err(native_error)?;
        SetCommTimeouts(handle, &timeouts(Duration::ZERO)).map_err(native_error)?;
    }
    Ok(Box::new(SerialSession(file, Duration::ZERO)))
}

fn native_error(error: windows::core::Error) -> ProviderError {
    ProviderError::new(error.to_string())
}

fn timeouts(read: Duration) -> COMMTIMEOUTS {
    COMMTIMEOUTS {
        ReadIntervalTimeout: u32::MAX,
        ReadTotalTimeoutMultiplier: if read.is_zero() { 0 } else { u32::MAX },
        ReadTotalTimeoutConstant: if read.is_zero() {
            0
        } else {
            read.as_millis().clamp(1, 200) as u32
        },
        WriteTotalTimeoutConstant: 1_000,
        ..COMMTIMEOUTS::default()
    }
}

struct SerialSession(File, Duration);
impl HidSession for SerialSession {
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        // Avoid replaying a partial transaction on a disconnected/busy port.
        if self.0.write(data)? != data.len() {
            return Err(ProviderError::new("Incomplete INZONE serial write"));
        }
        Ok(())
    }
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        if self.1 != timeout {
            unsafe { SetCommTimeouts(HANDLE(self.0.as_raw_handle()), &timeouts(timeout)) }
                .map_err(native_error)?;
            self.1 = timeout;
        }
        let mut data = vec![0; length];
        let count = self.0.read(&mut data)?;
        data.truncate(count);
        Ok(data)
    }
    fn send_feature(&mut self, _: &[u8]) -> Result<(), ProviderError> {
        Err(ProviderError::new(
            "INZONE serial does not accept HID feature reports",
        ))
    }
    fn feature(&mut self, _: u8, _: usize) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::new(
            "INZONE serial does not expose HID feature reports",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_original_inzone_usb_serial_interface() {
        let suffix = "#instance#{86e0d1e0-8089-11d0-9ce4-08003e301f73}";
        for (hardware, supported) in [
            (r"\\?\USB#VID_054C&PID_0E53&MI_00", true),
            (r"\\?\USB#VID_054C&PID_0FA8", false),
            (r"\\?\USB#VID_054C&PID_0E531", false),
            (r"\\?\BTHENUM#VID_054C&PID_0E53", false),
        ] {
            assert_eq!(
                inzone_instance(&format!("{hardware}{suffix}")).is_some(),
                supported
            );
        }
    }
}
