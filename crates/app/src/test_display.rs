//! Native fixtures prefer a secondary display. Nothing here ships in the app.
use std::sync::OnceLock;
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::*,
    UI::HiDpi::*,
};
use windows::core::BOOL;

struct Display {
    name: String,
    work: RECT,
    primary: bool,
}
static DISPLAY: OnceLock<Display> = OnceLock::new();

unsafe extern "system" fn enumerate(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            let len = info
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szDevice.len());
            let displays = &mut *(data.0 as *mut Vec<Display>);
            displays.push(Display {
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                work: info.monitorInfo.rcWork,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
    }
    BOOL(1)
}
fn selected() -> &'static Display {
    DISPLAY.get_or_init(|| {
        let mut displays = Vec::<Display>::new();
        unsafe {
            assert!(EnumDisplayMonitors(None, None, Some(enumerate), LPARAM((&mut displays as *mut Vec<Display>) as isize)).as_bool());
        }
        let requested = std::env::var("HALO_TEST_DISPLAY").ok();
        let index = if let Some(name) = requested {
            displays.iter().position(|d| d.name.eq_ignore_ascii_case(&name))
                .expect("Requested native-test display is unavailable; refusing to show test windows elsewhere")
        } else {
            displays.iter().position(|d| !d.primary).unwrap_or(0)
        };
        assert!(!displays.is_empty(), "No display available for native fixtures");
        let display = displays.swap_remove(index);
        eprintln!("Native test display: {} ({}, {})", display.name, display.work.left, display.work.top);
        display
    })
}
pub fn origin() -> (i32, i32) {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let display = selected();
    (display.work.left + 64, display.work.top + 64)
}
pub fn assert_placement(hwnd: HWND) {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        assert!(
            GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut info.monitorInfo
            )
            .as_bool()
        );
        let len = info
            .szDevice
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.szDevice.len());
        assert_eq!(
            String::from_utf16_lossy(&info.szDevice[..len]),
            selected().name
        );
    }
}
pub fn tray_guid(shipping: u128) -> u128 {
    // Fixtures bypass the app's normal single-instance guard. Keep both the
    // production icon and independent test processes outside their namespace.
    shipping ^ 0x31bc207fd5514ab6b68cdb59060f9701 ^ (u128::from(std::process::id()) << 64)
}
#[test]
fn fixture_tray_identity_is_stable_and_separate_from_the_installed_application() {
    for key in ["application", "simulated:mouse"] {
        let shipping = hb_core::stable_guid(key);
        assert_ne!(tray_guid(shipping), shipping);
        assert_eq!(tray_guid(shipping), tray_guid(shipping));
    }
    assert_ne!(
        tray_guid(hb_core::stable_guid("application")),
        tray_guid(hb_core::stable_guid("simulated:mouse"))
    );
}
