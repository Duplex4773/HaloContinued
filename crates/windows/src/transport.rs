//! HIDAPI's Windows C backend. Enumeration is shared and invalidated by device events.
use hb_core::*;
use hidapi::{HidApi, HidDevice};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Devices::{DeviceAndDriverInstallation::*, HumanInterfaceDevice::*, Properties::*},
        Foundation::*,
        Storage::FileSystem::*,
    },
    core::PCWSTR,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn property(dev: u32, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut kind = DEVPROPTYPE::default();
    let mut size = 0;
    unsafe {
        CM_Get_DevNode_PropertyW(dev, key, &mut kind, None, &mut size, 0);
    }
    if size == 0 || size > 65536 {
        return None;
    }
    let mut bytes = vec![0; size as usize];
    let result = unsafe {
        CM_Get_DevNode_PropertyW(dev, key, &mut kind, Some(bytes.as_mut_ptr()), &mut size, 0)
    };
    (result == CR_SUCCESS).then_some(bytes)
}
pub fn container(dev: u32) -> Option<String> {
    property(dev, &DEVPKEY_Device_ContainerId)
        .filter(|b| b.len() == 16)
        .map(|b| format!("{:032x}", u128::from_le_bytes(b.try_into().unwrap())))
}
struct NativeHandle(HANDLE);
impl Drop for NativeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Preparsed(PHIDP_PREPARSED_DATA);
impl Drop for Preparsed {
    fn drop(&mut self) {
        unsafe {
            let _ = HidD_FreePreparsedData(self.0);
        }
    }
}
struct Cached {
    at: Duration,
    generation: u64,
    devices: Vec<HidInfo>,
    short: bool,
    short_retry_at: Option<Duration>,
}
const CACHE_TTL: Duration = Duration::from_secs(30);
const SHORT_RETRY: Duration = Duration::from_secs(30);
// Share the native interface census across a burst of vendor polls. This is
// deliberately much shorter than metadata caching: a new vendor poll or short
// collection retry must still recover when Windows misses a connection event.
const CENSUS_TTL: Duration = Duration::from_secs(1);
#[derive(Default)]
struct CensusCache {
    sample: Option<(Duration, u64, BTreeMap<u16, usize>)>,
    queried: BTreeSet<u16>,
}
impl CensusCache {
    fn count(
        &mut self,
        vendor: u16,
        generation: u64,
        now: Duration,
        refresh: impl FnOnce() -> Result<BTreeMap<u16, usize>, ProviderError>,
    ) -> Result<usize, ProviderError> {
        if let Some((at, epoch, counts)) = &self.sample
            && *epoch == generation
            && now >= *at
            && now - *at < CENSUS_TTL
            && self.queried.insert(vendor)
        {
            return Ok(counts.get(&vendor).copied().unwrap_or(0));
        }
        // Do not reuse a stale/partial result if the native scan fails.
        self.sample = None;
        self.queried.clear();
        let counts = refresh()?;
        let count = counts.get(&vendor).copied().unwrap_or(0);
        self.sample = Some((now, generation, counts));
        self.queried.insert(vendor);
        Ok(count)
    }
}
#[derive(Default)]
struct EnumerationCache(BTreeMap<u16, Cached>);
impl EnumerationCache {
    fn after_open<T>(
        &mut self,
        vendor: u16,
        result: Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        if result.is_err() {
            // Access failure can mean an exclusive owner, not a connection
            // change. Re-enumerate this vendor without revoking unrelated
            // in-flight battery results or successful sibling observations.
            self.0.remove(&vendor);
        }
        result
    }
    fn enumerate(
        &mut self,
        vendor: u16,
        generation: u64,
        now: Duration,
        mut refresh: impl FnMut() -> Result<(Vec<HidInfo>, usize), ProviderError>,
    ) -> Result<Vec<HidInfo>, ProviderError> {
        let previous = self.0.get(&vendor).filter(|c| c.generation == generation);
        if let Some(c) = previous {
            let immediate_or_due = c.short
                && c.short_retry_at
                    .is_none_or(|at| now.saturating_sub(at) >= SHORT_RETRY);
            if now.saturating_sub(c.at) < CACHE_TTL && !immediate_or_due {
                return Ok(c.devices.clone());
            }
        }
        let short_retry_at = previous.filter(|c| c.short).map(|_| now);
        let (devices, present) = refresh()?;
        let short = devices.len() < present;
        self.0.insert(
            vendor,
            Cached {
                at: now,
                generation,
                devices: devices.clone(),
                short,
                short_retry_at: if short { short_retry_at } else { None },
            },
        );
        Ok(devices)
    }
}
pub struct WindowsHid {
    api: Mutex<HidApi>,
    cache: Mutex<EnumerationCache>,
    census: Mutex<CensusCache>,
    generation: AtomicU64,
    epoch: Instant,
}
impl WindowsHid {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            api: Mutex::new({
                HidApi::disable_device_discovery();
                HidApi::new().map_err(error)?
            }),
            cache: Mutex::new(EnumerationCache::default()),
            census: Mutex::new(CensusCache::default()),
            generation: AtomicU64::new(0),
            epoch: Instant::now(),
        })
    }
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}
struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
#[cfg(test)]
fn path_vendor(path: &str) -> Option<u16> {
    let bytes = path.as_bytes();
    for marker in [b"vid_", b"vid&"] {
        if let Some(at) = bytes
            .windows(4)
            .position(|part| part.eq_ignore_ascii_case(marker))
        {
            let digits = &bytes[at + 4..];
            let end = digits.iter().take_while(|b| b.is_ascii_hexdigit()).count();
            let digits = &digits[..end];
            if digits.len() >= 4 {
                // Only ASCII hex bytes reached this slice; no allocation or
                // Unicode conversion is needed for a USB/Bluetooth vendor ID.
                return u16::from_str_radix(
                    std::str::from_utf8(&digits[digits.len() - 4..]).ok()?,
                    16,
                )
                .ok();
            }
        }
    }
    None
}
// Inspect borrowed native UTF-16 before decoding. ASCII marker precedence and
// the trailing four hex digits match path_vendor, including Bluetooth VID forms.
fn path_vendor_wide(path: &[u16]) -> Option<u16> {
    for marker in [b"vid_", b"vid&"] {
        if let Some(at) = path.windows(4).position(|part| {
            part.iter()
                .zip(marker)
                .all(|(&unit, byte)| unit <= 0x7f && (unit as u8).eq_ignore_ascii_case(byte))
        }) {
            let mut value = 0u16;
            let mut count = 0usize;
            for &unit in &path[at + 4..] {
                let digit = match unit {
                    0x30..=0x39 => unit - 0x30,
                    0x41..=0x46 => unit - 0x41 + 10,
                    0x61..=0x66 => unit - 0x61 + 10,
                    _ => break,
                };
                value = value.wrapping_mul(16).wrapping_add(digit);
                count += 1;
            }
            if count >= 4 {
                return Some(value);
            }
        }
    }
    None
}
/// Count paths without opening devices. HIDAPI may temporarily omit a collection
/// whose zero-access metadata probe fails; this census makes that omission retryable.
fn present_counts() -> Result<BTreeMap<u16, usize>, ProviderError> {
    let guid = unsafe { HidD_GetHidGuid() };
    let set = DeviceSet(
        unsafe {
            SetupDiGetClassDevsW(
                Some(&guid),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(|e| ProviderError::new(e.to_string()))?,
    );
    let mut paths: BTreeMap<u16, BTreeSet<String>> = BTreeMap::new();
    // The census only needs one variable-length native detail buffer at a time.
    let mut storage = Vec::<u64>::new();
    for index in 0..65536 {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInterfaces(set.0, None, &guid, index, &mut interface) }
            .is_err()
        {
            if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(ProviderError::new("HID interface census failed"));
        }
        let mut size = 0;
        unsafe {
            let _ =
                SetupDiGetDeviceInterfaceDetailW(set.0, &interface, None, 0, Some(&mut size), None);
        }
        if !(8..=65536).contains(&size) {
            continue;
        }
        // u64 storage provides the alignment required by the variable-length
        // native structure. DevicePath begins at the documented member offset.
        storage.resize((size as usize).div_ceil(8), 0);
        let detail = storage
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(set.0, &interface, Some(detail), size, None, None)
        }
        .is_err()
        {
            continue;
        }
        let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let wide = unsafe {
            std::slice::from_raw_parts(
                storage.as_ptr().cast::<u8>().add(offset).cast::<u16>(),
                (size as usize - offset) / 2,
            )
        };
        let end = wide.iter().position(|v| *v == 0).unwrap_or(wide.len());
        let wide = &wide[..end];
        if let Some(vendor) = path_vendor_wide(wide) {
            let mut path = String::from_utf16_lossy(wide);
            path.make_ascii_lowercase();
            paths.entry(vendor).or_default().insert(path);
        }
    }
    // Only retain counts between polls, not paths or hardware identifiers.
    Ok(paths
        .into_iter()
        .map(|(vendor, paths)| (vendor, paths.len()))
        .collect())
}
fn error(e: hidapi::HidError) -> ProviderError {
    ProviderError::new(e.to_string())
}
fn metadata(info: &mut HidInfo) {
    let path = wide(&info.path);
    let instance = info
        .path
        .trim_start_matches("\\\\?\\")
        .split("#{")
        .next()
        .unwrap_or("")
        .replace('#', "\\");
    let name = wide(&instance);
    let mut dev = 0;
    if unsafe { CM_Locate_DevNodeW(&mut dev, PCWSTR(name.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) }
        == CR_SUCCESS
    {
        info.container = container(dev)
    }
    // Zero desired access preserves access to otherwise exclusive collections.
    if let Ok(handle) = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    } {
        let handle = NativeHandle(handle);
        let mut data = PHIDP_PREPARSED_DATA::default();
        unsafe {
            if HidD_GetPreparsedData(handle.0, &mut data) {
                let data = Preparsed(data);
                let mut caps = HIDP_CAPS::default();
                if HidP_GetCaps(data.0, &mut caps) == HIDP_STATUS_SUCCESS {
                    info.output_length = Some(caps.OutputReportByteLength as usize);
                    info.feature_length = Some(caps.FeatureReportByteLength as usize);
                }
            }
        }
    }
}
impl HidTransport for WindowsHid {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
    fn enumerate(&self, vendor: u16) -> Result<Vec<HidInfo>, ProviderError> {
        let generation = self.generation.load(Ordering::Relaxed);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.enumerate(vendor, generation, self.epoch.elapsed(), || {
            let present = self
                .census
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .count(vendor, generation, self.epoch.elapsed(), present_counts)?;
            // add_devices targets a vendor; never enumerate unrelated vendor collections.
            let mut api = self.api.lock().unwrap_or_else(|p| p.into_inner());
            api.reset_devices().map_err(error)?;
            api.add_devices(vendor, 0).map_err(error)?;
            let devices = api
                .device_list()
                .map(|d| {
                    let mut info = HidInfo {
                        path: d.path().to_string_lossy().into(),
                        vendor_id: d.vendor_id(),
                        product_id: d.product_id(),
                        usage_page: d.usage_page(),
                        usage: d.usage(),
                        interface: d.interface_number(),
                        product: d.product_string().unwrap_or("").into(),
                        serial: d.serial_number().unwrap_or("").into(),
                        ..Default::default()
                    };
                    metadata(&mut info);
                    info
                })
                .collect::<Vec<_>>();
            Ok((devices, present))
        })
    }
    fn open(&self, info: &HidInfo) -> Result<Box<dyn HidSession>, ProviderError> {
        let path =
            CString::new(info.path.as_bytes()).map_err(|e| ProviderError::new(e.to_string()))?;
        let result = {
            // Enumeration locks cache before API. Release API before taking
            // cache here, so failed opens cannot invert that lock order.
            self.api
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .open_path(&path)
                .map_err(error)
        };
        let handle = match result {
            Ok(handle) => handle,
            Err(error) => self
                .cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .after_open(info.vendor_id, Err(error))?,
        };
        Ok(Box::new(Session(handle)))
    }
}
struct Session(HidDevice);
impl HidSession for Session {
    fn input_report(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        if length == 0 {
            return Err(ProviderError::new("empty input report buffer"));
        }
        let mut data = vec![0; length];
        data[0] = id;
        let n = self.0.get_input_report(&mut data).map_err(error)?;
        data.truncate(n);
        Ok(data)
    }
    fn write(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let n = self.0.write(data).map_err(error)?;
        if n != data.len() {
            return Err(ProviderError::new("short HID write"));
        }
        Ok(())
    }
    fn read(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        let mut data = vec![0; length];
        let n = self
            .0
            .read_timeout(&mut data, timeout.as_millis().min(i32::MAX as u128) as i32)
            .map_err(error)?;
        data.truncate(n);
        Ok(data)
    }
    fn send_feature(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        self.0.send_feature_report(data).map_err(error)
    }
    fn feature(&mut self, id: u8, length: usize) -> Result<Vec<u8>, ProviderError> {
        let mut data = vec![0; length];
        if data.is_empty() {
            return Err(ProviderError::new("empty feature buffer"));
        }
        data[0] = id;
        let n = self.0.get_feature_report(&mut data).map_err(error)?;
        data.truncate(n);
        Ok(data)
    }
}
#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn census_shares_vendor_bursts_but_refreshes_on_expiry_and_connection_changes() {
        let mut cache = CensusCache::default();
        let mut scans = 0;
        for generation in [0, 1] {
            for (millis, vendor) in [(0, 0x1532), (999, 0x046d), (1000, 0x9999), (1000, 0x1532)] {
                let count = cache
                    .count(vendor, generation, Duration::from_millis(millis), || {
                        scans += 1;
                        Ok(BTreeMap::from([(0x1532, 3), (0x046d, 2)]))
                    })
                    .unwrap();
                assert_eq!(
                    count,
                    match vendor {
                        0x1532 => 3,
                        0x046d => 2,
                        _ => 0,
                    }
                );
            }
        }
        assert_eq!(scans, 4);
    }
    #[test]
    fn repeated_vendor_recovery_always_rechecks_native_census() {
        let mut cache = CensusCache::default();
        for count in [2, 3, 4] {
            assert_eq!(
                cache
                    .count(0x1532, 0, Duration::ZERO, || Ok(BTreeMap::from([(
                        0x1532, count
                    )])))
                    .unwrap(),
                count
            );
        }
    }
    #[test]
    fn census_failure_never_becomes_empty_success_or_stale_recovery() {
        let mut cache = CensusCache::default();
        cache
            .count(0x1532, 0, Duration::ZERO, || {
                Ok(BTreeMap::from([(0x1532, 3)]))
            })
            .unwrap();
        assert!(
            cache
                .count(0x1532, 1, Duration::ZERO, || Err(ProviderError::new(
                    "unavailable"
                )))
                .is_err()
        );
        assert!(cache.sample.is_none());
        assert_eq!(
            cache
                .count(0x1532, 1, Duration::ZERO, || Ok(BTreeMap::from([(
                    0x1532, 4
                )])))
                .unwrap(),
            4
        );
        // A regressed injected clock cannot keep an entry cached indefinitely.
        cache
            .count(0x1532, 1, Duration::from_secs(2), || Ok(BTreeMap::new()))
            .unwrap();
        assert_eq!(
            cache
                .count(0x1532, 1, Duration::from_secs(1), || Ok(BTreeMap::from([
                    (0x1532, 2)
                ])))
                .unwrap(),
            2
        );
    }
    fn infos(n: usize) -> Vec<HidInfo> {
        (0..n)
            .map(|i| HidInfo {
                path: format!("collection-{i}"),
                ..Default::default()
            })
            .collect()
    }
    #[test]
    fn failed_open_recovers_only_affected_vendor_without_connection_epoch_change() {
        let mut cache = EnumerationCache::default();
        let generation = 7;
        for vendor in [0x1532, 0x046d] {
            cache
                .enumerate(vendor, generation, Duration::ZERO, || Ok((infos(2), 2)))
                .unwrap();
        }
        let error = cache.after_open::<()>(0x1532, Err(ProviderError::new("exclusive owner")));
        assert!(error.is_err());
        assert!(!cache.0.contains_key(&0x1532));
        let mut refreshed = false;
        cache
            .enumerate(0x1532, generation, Duration::from_secs(1), || {
                refreshed = true;
                Ok((infos(1), 1))
            })
            .unwrap();
        assert!(refreshed);
        assert_eq!(cache.0[&0x1532].generation, generation);
        cache
            .enumerate(0x046d, generation, Duration::from_secs(1), || {
                panic!("unrelated provider must retain its cache")
            })
            .unwrap();
        cache.after_open(0x1532, Ok(())).unwrap();
        cache
            .enumerate(0x1532, generation, Duration::from_secs(2), || {
                panic!("successful access must not expire metadata")
            })
            .unwrap();
    }
    #[test]
    fn short_list_immediate_retry_recovers_missing_collection() {
        let mut cache = EnumerationCache::default();
        let mut calls = 0;
        for expected in [9, 10, 10] {
            let devices = cache
                .enumerate(0x1532, 0, Duration::ZERO, || {
                    calls += 1;
                    Ok((infos(if calls == 1 { 9 } else { 10 }), 10))
                })
                .unwrap();
            assert_eq!(devices.len(), expected);
        }
        assert_eq!(calls, 2);
    }
    #[test]
    fn permanently_short_list_retries_once_then_rate_limited() {
        let mut cache = EnumerationCache::default();
        let mut calls = 0;
        for secs in [0, 0, 1, 10, 29] {
            cache
                .enumerate(0x1532, 0, Duration::from_secs(secs), || {
                    calls += 1;
                    Ok((infos(9), 10))
                })
                .unwrap();
        }
        assert_eq!(calls, 2);
        cache
            .enumerate(0x1532, 0, Duration::from_secs(30), || {
                calls += 1;
                Ok((infos(9), 10))
            })
            .unwrap();
        assert_eq!(calls, 3);
        assert!(SHORT_RETRY >= Duration::from_secs(10));
    }
    #[test]
    fn complete_list_is_cached_then_ttl_expires() {
        let mut cache = EnumerationCache::default();
        let mut calls = 0;
        for secs in [0, 1, 29, 30] {
            cache
                .enumerate(0x1532, 0, Duration::from_secs(secs), || {
                    calls += 1;
                    Ok((infos(10), 10))
                })
                .unwrap();
        }
        assert_eq!(calls, 2);
    }
    #[test]
    fn cached_collection_preserves_capabilities_without_reprobing() {
        let mut cache = EnumerationCache::default();
        let mut probes = 0;
        for seconds in [0, 1, 29] {
            let entries = cache
                .enumerate(0x372e, 0, Duration::from_secs(seconds), || {
                    probes += 1;
                    Ok((
                        vec![HidInfo {
                            path: "selected-feature-collection".into(),
                            feature_length: Some(65),
                            output_length: Some(33),
                            usage_page: 0xff00,
                            usage: 1,
                            ..Default::default()
                        }],
                        1,
                    ))
                })
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(
                (
                    &*entries[0].path,
                    entries[0].feature_length,
                    entries[0].output_length,
                    entries[0].usage_page,
                    entries[0].usage
                ),
                ("selected-feature-collection", Some(65), Some(33), 0xff00, 1)
            );
        }
        assert_eq!(probes, 1);
    }
    #[test]
    fn vendor_path_census_keeps_usb_and_bluetooth_vendor_boundaries() {
        let paths = [
            "\\\\?\\hid#vid_1532&pid_0000&col1#{abcd}",
            "\\\\?\\hid#VID_1532&pid_0000&col2#{abcd}",
            "\\\\?\\hid#vid_054c&pid_0000#{abcd}",
            "bth#vid&0002045e_pid&02fd",
            "hid#vid_153&pid_0000",
        ];
        assert_eq!(
            paths
                .iter()
                .filter(|p| path_vendor(p) == Some(0x1532))
                .count(),
            2
        );
        assert_eq!(path_vendor(paths[3]), Some(0x045e));
        assert_eq!(path_vendor(paths[4]), None);
        assert_eq!(path_vendor("µHID#ViD_1532&pid_0000"), Some(0x1532));
        assert_eq!(path_vendor("BTH#VID&0002045E_PID&02FD"), Some(0x045e));
        assert_eq!(path_vendor("hid#vid_153&vid&0002045e"), Some(0x045e));
        assert_eq!(path_vendor("hid#vid_1532FFFF&pid_0000"), Some(0xffff));
    }
    #[test]
    fn borrowed_utf16_vendor_filter_matches_decoded_paths_including_invalid_unicode() {
        for path in [
            "",
            "hid#VID_1532&pid_0000",
            "µHID#ViD_1532&pid_0000",
            "BTH#VID&0002045E_PID&02FD",
            "hid#vid_153&vid&0002045e",
            "hid#vid_1532FFFF&pid_0000",
            "vid&1234_vid_5678",
            "vid_12z4_vid&ABCD",
            "vid_１２３４",
            "vid_ABCDEF0123456789",
        ] {
            let wide: Vec<u16> = path.encode_utf16().collect();
            assert_eq!(path_vendor_wide(&wide), path_vendor(path), "{path}");
        }
        let prefixes: &[&[u16]] = &[&[], &[0xd800], &[0xdc00], &[0xd800, 0xdc00], &[0x00b5]];
        for prefix in prefixes {
            for marker in ["vid_", "VID&", "ViD_"] {
                for digits in ["123", "1532", "0002045e", "ABCDEF0123456789", "12z4"] {
                    let mut wide = prefix.to_vec();
                    wide.extend(marker.encode_utf16());
                    wide.extend(digits.encode_utf16());
                    wide.push(0xd800);
                    wide.extend("&pid_0000".encode_utf16());
                    let decoded = String::from_utf16_lossy(&wide);
                    assert_eq!(path_vendor_wide(&wide), path_vendor(&decoded), "{decoded}");
                }
            }
        }
    }
    #[test]
    fn connection_generation_change_forces_rediscovery() {
        let mut cache = EnumerationCache::default();
        let mut calls = 0;
        for generation in [0, 0, 1, 1, 2] {
            cache
                .enumerate(0x1532, generation, Duration::ZERO, || {
                    calls += 1;
                    Ok((infos(1), 1))
                })
                .unwrap();
        }
        assert_eq!(calls, 3);
    }
    #[test]
    fn failed_enumeration_is_not_cached_as_empty_or_complete() {
        let mut cache = EnumerationCache::default();
        assert!(
            cache
                .enumerate(0x1532, 0, Duration::ZERO, || Err(ProviderError::new(
                    "unavailable"
                )))
                .is_err()
        );
        assert!(cache.0.is_empty());
        let devices = cache
            .enumerate(0x1532, 0, Duration::ZERO, || Ok((infos(1), 1)))
            .unwrap();
        assert_eq!(devices.len(), 1);
    }
}
