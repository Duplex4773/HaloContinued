//! On-demand per-monitor HDR. No watcher, timer, settings file or global shortcut.
use hb_core::ProviderError;
use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::Win32::{Devices::Display::*, Foundation::*};

pub const MAX_DISPLAYS: usize = 64;
const LAYOUT_CHANGED: &str = "Displays changed. Reopen the Displays menu and try again.";
const UNAVAILABLE: &str = "HDR control unavailable";
const MIRRORED: &str = "HDR unavailable while displays are duplicated";
const POLICY: &str = "HDR unavailable in this Windows session";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrState {
    Available { enabled: bool, active: bool },
    Unsupported,
    Unavailable(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayId {
    adapter: i64,
    target: u32,
    source_adapter: i64,
    source: u32,
    monitor_path: String,
    source_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub name: String,
    pub primary: bool,
    pub hdr: HdrState,
    id: DisplayId,
}
impl Display {
    pub fn label(&self) -> String {
        let source = self.id.source_name.strip_prefix(r"\\.\DISPLAY");
        let label = match source.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            Some(number) => format!("Display {number} · {}", self.name),
            None => self.name.clone(),
        };
        if self.primary {
            format!("{label} (Main)")
        } else {
            label
        }
    }
    pub fn hdr_request(&self) -> Option<HdrRequest> {
        match self.hdr {
            HdrState::Available { enabled, .. } => Some(HdrRequest {
                target: self.id.clone(),
                enabled: !enabled,
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HdrRequest {
    target: DisplayId,
    enabled: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrOutcome {
    Verified,
    Accepted,
}

// These Windows SDK 26100 packets are absent from the pinned windows bindings.
// Keep the SDK's UINT32 bitfields as one u32, not Rust/C++ bitfield wrappers.
// Source: Microsoft win32metadata, RecompiledIdlHeaders/um/wingdi.h.
#[repr(C)]
#[derive(Default)]
struct AdvancedColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    color_encoding: u32,
    bits_per_channel: u32,
    active_mode: u32,
}
#[repr(C)]
#[derive(Default)]
struct SetHdrState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
}
const GET_ADVANCED_COLOR_INFO_2: DISPLAYCONFIG_DEVICE_INFO_TYPE =
    DISPLAYCONFIG_DEVICE_INFO_TYPE(15);
const SET_HDR_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = DISPLAYCONFIG_DEVICE_INFO_TYPE(16);

fn header<T>(
    kind: DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter: i64,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size_of::<T>() as u32,
        adapterId: LUID {
            LowPart: adapter as u32,
            HighPart: (adapter >> 32) as i32,
        },
        id,
    }
}
fn adapter(luid: LUID) -> i64 {
    ((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)) as i64
}
fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}
fn color_state(flags: u32, active_mode: u32) -> HdrState {
    if active_mode > 2 {
        HdrState::Unavailable(UNAVAILABLE)
    } else if flags & (1 << 4) == 0 {
        // WCG/automatic colour management is not HDR.
        HdrState::Unsupported
    } else if flags & (1 << 3) != 0 {
        HdrState::Unavailable(POLICY)
    } else {
        HdrState::Available {
            enabled: flags & (1 << 5) != 0,
            active: active_mode == 2,
        }
    }
}
fn native_error() -> ProviderError {
    ProviderError::new(
        "Windows couldn't change HDR on this display. Check Settings > System > Display, then try again.",
    )
}

trait DisplayApi {
    fn inventory(&self) -> Result<Vec<Display>, ProviderError>;
    fn set(&self, target: &DisplayId, enabled: bool) -> Result<(), ProviderError>;
}
struct WindowsDisplayApi;

fn active_paths()
-> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), ProviderError> {
    // A hotplug can change buffer sizes between the two calls. Bound both
    // retries and allocations; opening a menu never creates a discovery loop.
    for _ in 0..3 {
        let (mut count, mut mode_count) = (0, 0);
        let result = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut count, &mut mode_count)
        };
        if result != ERROR_SUCCESS {
            return Err(ProviderError::new(
                "Windows couldn't read the connected displays.",
            ));
        }
        if count > 256 || mode_count > 4096 {
            return Err(ProviderError::new(
                "Windows reported an unsupported display layout.",
            ));
        }
        if count == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS {
            return Err(ProviderError::new(
                "Windows couldn't read the connected displays.",
            ));
        }
        if count as usize > paths.len() || mode_count as usize > modes.len() {
            return Err(ProviderError::new(LAYOUT_CHANGED));
        }
        paths.truncate(count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }
    Err(ProviderError::new(LAYOUT_CHANGED))
}
fn target_name(id: &DisplayId) -> Result<DISPLAYCONFIG_TARGET_DEVICE_NAME, ProviderError> {
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: header::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            id.adapter,
            id.target,
        ),
        ..Default::default()
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
        return Err(ProviderError::new(LAYOUT_CHANGED));
    }
    Ok(target)
}
impl DisplayApi for WindowsDisplayApi {
    fn inventory(&self) -> Result<Vec<Display>, ProviderError> {
        let (paths, modes) = active_paths()?;
        let mut displays = Vec::new();
        for path in paths
            .iter()
            .filter(|p| p.targetInfo.targetAvailable.as_bool())
        {
            let mut id = DisplayId {
                adapter: adapter(path.targetInfo.adapterId),
                target: path.targetInfo.id,
                source_adapter: adapter(path.sourceInfo.adapterId),
                source: path.sourceInfo.id,
                monitor_path: String::new(),
                source_name: String::new(),
            };
            let target = target_name(&id).ok();
            if let Some(target) = &target {
                id.monitor_path = text(&target.monitorDevicePath);
            }
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    id.source_adapter,
                    id.source,
                ),
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0 {
                id.source_name = text(&source.viewGdiDeviceName);
            }
            let mut name = target
                .as_ref()
                .map(|t| text(&t.monitorFriendlyDeviceName))
                .unwrap_or_default();
            if name.is_empty() {
                name = "Connected display".into();
            }
            let source_mode = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
            let primary = modes.get(source_mode as usize).is_some_and(|m| {
                m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                    && unsafe {
                        m.Anonymous.sourceMode.position.x == 0
                            && m.Anonymous.sourceMode.position.y == 0
                    }
            });
            let cloned = paths
                .iter()
                .filter(|p| {
                    p.sourceInfo.adapterId == path.sourceInfo.adapterId
                        && p.sourceInfo.id == path.sourceInfo.id
                })
                .count()
                != 1;
            let ambiguous = paths
                .iter()
                .filter(|p| {
                    p.targetInfo.adapterId == path.targetInfo.adapterId
                        && p.targetInfo.id == path.targetInfo.id
                })
                .count()
                != 1;
            let mut info = AdvancedColorInfo2 {
                header: header::<AdvancedColorInfo2>(
                    GET_ADVANCED_COLOR_INFO_2,
                    id.adapter,
                    id.target,
                ),
                ..Default::default()
            };
            let hdr = if cloned {
                HdrState::Unavailable(MIRRORED)
            } else if ambiguous || id.monitor_path.is_empty() || id.source_name.is_empty() {
                HdrState::Unavailable(UNAVAILABLE)
            } else if unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0 {
                color_state(info.flags, info.active_mode)
            } else {
                // The legacy advanced-colour API cannot reliably distinguish
                // HDR from WCG. Never fall back to a WCG/global toggle.
                HdrState::Unavailable(UNAVAILABLE)
            };
            if displays
                .iter()
                .any(|d: &Display| d.id.adapter == id.adapter && d.id.target == id.target)
            {
                continue;
            }
            displays.push(Display {
                name,
                primary,
                hdr,
                id,
            });
            if displays.len() >= MAX_DISPLAYS {
                break;
            }
        }
        displays.sort_by(|a, b| {
            b.primary
                .cmp(&a.primary)
                .then(a.id.source_name.cmp(&b.id.source_name))
                .then(a.name.cmp(&b.name))
        });
        Ok(displays)
    }
    fn set(&self, target: &DisplayId, enabled: bool) -> Result<(), ProviderError> {
        // Recheck the monitor's device path immediately before the target-only
        // SET, so a recycled adapter/target ID cannot silently retarget it.
        if text(&target_name(target)?.monitorDevicePath) != target.monitor_path {
            return Err(ProviderError::new(LAYOUT_CHANGED));
        }
        let packet = SetHdrState {
            header: header::<SetHdrState>(SET_HDR_STATE, target.adapter, target.target),
            flags: u32::from(enabled),
        };
        if unsafe { DisplayConfigSetDeviceInfo(&packet.header) } != 0 {
            return Err(native_error());
        }
        Ok(())
    }
}

pub fn enumerate() -> Result<Vec<Display>, ProviderError> {
    WindowsDisplayApi.inventory()
}
pub fn set_hdr(request: &HdrRequest, cancelled: &AtomicBool) -> Result<HdrOutcome, ProviderError> {
    apply(&WindowsDisplayApi, request, cancelled)
}
fn apply(
    api: &impl DisplayApi,
    request: &HdrRequest,
    cancelled: &AtomicBool,
) -> Result<HdrOutcome, ProviderError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(ProviderError::new("HDR change cancelled."));
    }
    let displays = api.inventory()?;
    let mut matching = displays.iter().filter(|d| d.id == request.target);
    let display = matching
        .next()
        .ok_or_else(|| ProviderError::new(LAYOUT_CHANGED))?;
    if matching.next().is_some() {
        return Err(ProviderError::new(LAYOUT_CHANGED));
    }
    let enabled = match display.hdr {
        HdrState::Available { enabled, .. } => enabled,
        HdrState::Unsupported => {
            return Err(ProviderError::new("This display doesn't support HDR."));
        }
        HdrState::Unavailable(reason) => return Err(ProviderError::new(reason)),
    };
    if enabled == request.enabled {
        return Ok(HdrOutcome::Verified);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(ProviderError::new("HDR change cancelled."));
    }
    api.set(&display.id, request.enabled)?;
    // No repeated writes or sleeps while Windows completes a mode transition.
    let verified = api.inventory().ok().is_some_and(|displays| displays.iter().any(|d| {
        d.id == request.target && matches!(d.hdr, HdrState::Available { enabled, .. } if enabled == request.enabled)
    }));
    Ok(if verified {
        HdrOutcome::Verified
    } else {
        HdrOutcome::Accepted
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    fn display(target: u32, enabled: bool) -> Display {
        Display {
            name: "Test & panel".into(),
            primary: target == 1,
            hdr: HdrState::Available {
                enabled,
                active: enabled,
            },
            id: DisplayId {
                adapter: 1,
                target,
                source_adapter: 1,
                source: target,
                monitor_path: format!("test-monitor-{target}"),
                source_name: format!(r"\\.\DISPLAY{target}"),
            },
        }
    }
    struct FakeApi {
        displays: RefCell<Vec<Display>>,
        writes: RefCell<Vec<(DisplayId, bool)>>,
        fail_write: Cell<bool>,
        fail_readback: Cell<bool>,
    }
    impl FakeApi {
        fn new() -> Self {
            Self {
                displays: RefCell::new(vec![display(1, false), display(2, true)]),
                writes: RefCell::new(Vec::new()),
                fail_write: Cell::new(false),
                fail_readback: Cell::new(false),
            }
        }
    }
    impl DisplayApi for FakeApi {
        fn inventory(&self) -> Result<Vec<Display>, ProviderError> {
            if self.fail_readback.get() && !self.writes.borrow().is_empty() {
                return Err(native_error());
            }
            Ok(self.displays.borrow().clone())
        }
        fn set(&self, target: &DisplayId, enabled: bool) -> Result<(), ProviderError> {
            self.writes.borrow_mut().push((target.clone(), enabled));
            if self.fail_write.get() {
                return Err(native_error());
            }
            self.displays
                .borrow_mut()
                .iter_mut()
                .find(|d| d.id == *target)
                .unwrap()
                .hdr = HdrState::Available {
                enabled,
                active: enabled,
            };
            Ok(())
        }
    }
    #[test]
    fn packets_match_the_microsoft_sdk_layout() {
        assert_eq!(size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>(), 20);
        assert_eq!(size_of::<AdvancedColorInfo2>(), 36);
        assert_eq!(size_of::<SetHdrState>(), 24);
        assert_eq!(std::mem::offset_of!(AdvancedColorInfo2, flags), 20);
        assert_eq!(std::mem::offset_of!(AdvancedColorInfo2, active_mode), 32);
    }
    #[test]
    fn hdr_is_distinct_from_wide_colour_and_policy_restrictions() {
        assert_eq!(color_state(1 | 2 | 64 | 128, 1), HdrState::Unsupported);
        assert_eq!(
            color_state(16 | 32, 2),
            HdrState::Available {
                enabled: true,
                active: true
            }
        );
        assert_eq!(
            color_state(16, 0),
            HdrState::Available {
                enabled: false,
                active: false
            }
        );
        assert_eq!(
            color_state(16 | 32, 0),
            HdrState::Available {
                enabled: true,
                active: false
            }
        );
        assert_eq!(color_state(16 | 8, 0), HdrState::Unavailable(POLICY));
        assert_eq!(color_state(16, 99), HdrState::Unavailable(UNAVAILABLE));
    }
    #[test]
    fn only_the_selected_monitor_changes_even_with_identical_names() {
        let api = FakeApi::new();
        for enabled in [true, false] {
            let request = api.displays.borrow()[0].hdr_request().unwrap();
            assert_eq!(
                apply(&api, &request, &AtomicBool::new(false)).unwrap(),
                HdrOutcome::Verified
            );
            assert_eq!(
                api.displays.borrow()[0].hdr,
                HdrState::Available {
                    enabled,
                    active: enabled
                }
            );
            assert_eq!(api.displays.borrow()[1], display(2, true));
        }
        assert_eq!(api.writes.borrow().len(), 2);
        assert!(
            api.writes
                .borrow()
                .iter()
                .all(|(target, _)| target.target == 1)
        );
    }
    #[test]
    fn already_applied_request_sends_no_set() {
        let api = FakeApi::new();
        let request = api.displays.borrow()[0].hdr_request().unwrap();
        api.displays.borrow_mut()[0].hdr = HdrState::Available {
            enabled: true,
            active: true,
        };
        assert_eq!(
            apply(&api, &request, &AtomicBool::new(false)).unwrap(),
            HdrOutcome::Verified
        );
        assert!(api.writes.borrow().is_empty());
    }
    #[test]
    fn hotplug_ambiguous_mirrored_unsupported_and_cancelled_requests_send_nothing() {
        for case in 0..7 {
            let api = FakeApi::new();
            let request = api.displays.borrow()[0].hdr_request().unwrap();
            match case {
                0 => api.displays.borrow_mut()[0].id.monitor_path = "replacement".into(),
                1 => api.displays.borrow_mut()[0].id.source += 1,
                2 => {
                    let copy = api.displays.borrow()[0].clone();
                    api.displays.borrow_mut().push(copy);
                }
                3 => api.displays.borrow_mut()[0].hdr = HdrState::Unavailable(MIRRORED),
                4 => api.displays.borrow_mut()[0].hdr = HdrState::Unsupported,
                5 => api.displays.borrow_mut().clear(),
                _ => {}
            }
            assert!(apply(&api, &request, &AtomicBool::new(case == 6)).is_err());
            assert!(api.writes.borrow().is_empty());
        }
    }
    #[test]
    fn failed_or_unconfirmed_changes_never_retry_a_write() {
        for failure in [false, true] {
            let api = FakeApi::new();
            let request = api.displays.borrow()[0].hdr_request().unwrap();
            api.fail_write.set(failure);
            api.fail_readback.set(!failure);
            let result = apply(&api, &request, &AtomicBool::new(false));
            if failure {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), HdrOutcome::Accepted);
            }
            assert_eq!(api.writes.borrow().len(), 1);
        }
    }
    #[test]
    #[ignore = "Read-only native monitor discovery; no HDR setting is changed"]
    fn native_hdr_inventory_is_read_only() {
        let displays = enumerate().unwrap();
        for display in &displays {
            println!("{}: {:?}", display.label(), display.hdr);
        }
        assert!(displays.len() <= MAX_DISPLAYS);
    }
}
