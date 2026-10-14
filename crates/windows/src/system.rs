//! Owned Windows lifecycle resources and application-specific registration.
use crate::transport::wide;
use hb_core::ProviderError;
use windows::{
    Win32::{
        Foundation::*,
        System::{Registry::*, Threading::*},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::w,
};
// Mutex, Run value and notification IDs remain stable across the Halo Continued
// rename: avoid duplicate instances/startup entries and retain notification preferences.
pub struct Instance(HANDLE);
pub enum InstanceAcquisition {
    Acquired(Instance),
    AlreadyRunning,
}
impl Instance {
    pub fn acquire() -> Result<Self, ProviderError> {
        match Self::acquire_state()? {
            InstanceAcquisition::Acquired(instance) => Ok(instance),
            InstanceAcquisition::AlreadyRunning => {
                Err(ProviderError::new("Halo Continued is already running"))
            }
        }
    }
    pub fn acquire_state() -> Result<InstanceAcquisition, ProviderError> {
        Self::acquire_named(w!("Local\\HaloBatteryNext"))
    }
    fn acquire_named(name: windows::core::PCWSTR) -> Result<InstanceAcquisition, ProviderError> {
        let handle = unsafe { CreateMutexW(None, false, name) }
            .map_err(|e| ProviderError::new(e.to_string()))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(InstanceAcquisition::AlreadyRunning);
        }
        Ok(InstanceAcquisition::Acquired(Self(handle)))
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub fn startup(enabled: bool) -> Result<(), ProviderError> {
    let result = if enabled {
        let exe = std::env::current_exe()?;
        if executable_in_temp(
            &exe,
            &[
                std::env::temp_dir(),
                std::env::var_os("TEMP")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default(),
                std::env::var_os("TMP")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default(),
            ],
        ) {
            return Err(ProviderError::new(
                "Move Halo Continued out of the temporary folder before enabling Start with Windows",
            ));
        }
        let value = wide(&format!("\"{}\" --background", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                w!("HaloBatteryNext"),
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            )
        }
    } else {
        unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                w!("HaloBatteryNext"),
            )
        }
    };
    if result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(ProviderError::new(format!(
            "Startup registry error {}",
            result.0
        )))
    }
}

fn canonical_components(path: &std::path::Path) -> Option<Vec<String>> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let path = path
        .canonicalize()
        .or_else(|_| std::path::absolute(path))
        .ok()?;
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Prefix(prefix) => {
                // canonicalize produces a verbatim prefix; compare the same
                // normalized drive/UNC prefix as a path that does not exist yet.
                let value = match prefix.kind() {
                    std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) => {
                        format!("{}:", char::from(drive).to_ascii_lowercase())
                    }
                    std::path::Prefix::UNC(server, share)
                    | std::path::Prefix::VerbatimUNC(server, share) => format!(
                        "\\\\{}\\{}",
                        server.to_string_lossy().to_lowercase(),
                        share.to_string_lossy().to_lowercase()
                    ),
                    _ => prefix.as_os_str().to_string_lossy().to_lowercase(),
                };
                parts.push(value);
            }
            other => parts.push(other.as_os_str().to_string_lossy().to_lowercase()),
        }
    }
    Some(parts)
}
fn executable_in_temp(exe: &std::path::Path, temp_roots: &[std::path::PathBuf]) -> bool {
    let Some(exe) = canonical_components(exe) else {
        return false;
    };
    temp_roots
        .iter()
        .filter_map(|root| canonical_components(root))
        .any(|root| exe.len() > root.len() && exe.starts_with(&root))
}
pub fn is_startup() -> bool {
    let mut size = 0;
    (unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            w!("HaloBatteryNext"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    }) == ERROR_SUCCESS
}
pub fn dark_theme() -> bool {
    theme_value(SYSTEM_LIGHT_THEME)
}
const SYSTEM_LIGHT_THEME: windows::core::PCWSTR = w!("SystemUsesLightTheme");
const APPS_LIGHT_THEME: windows::core::PCWSTR = w!("AppsUseLightTheme");
/// Windows app appearance can differ from the taskbar/system appearance.
pub fn dashboard_dark_theme() -> bool {
    !high_contrast() && theme_value(APPS_LIGHT_THEME)
}
pub fn high_contrast() -> bool {
    let mut contrast = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            size_of::<HIGHCONTRASTW>() as u32,
            Some((&mut contrast as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && contrast.dwFlags.contains(HCF_HIGHCONTRASTON)
    }
}
fn theme_value(name: windows::core::PCWSTR) -> bool {
    theme_value_at(
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        name,
    )
}
fn theme_value_at(subkey: windows::core::PCWSTR, name: windows::core::PCWSTR) -> bool {
    let mut data = 1u32;
    let mut size = 4;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    result == ERROR_SUCCESS && data == 0
}
fn gaming_notification_state(state: QUERY_USER_NOTIFICATION_STATE) -> bool {
    matches!(
        state,
        QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE | QUNS_BUSY
    )
}
pub fn gaming() -> bool {
    unsafe { SHQueryUserNotificationState() }.is_ok_and(gaming_notification_state)
}

/// Use the same Windows media files and fallback aliases as upstream 1.14.
/// WinMM handles asynchronous playback; no playback worker or timer is needed.
pub fn play_low_battery_sound(level: u8) -> Result<(), ProviderError> {
    use windows::Win32::Media::Audio::PlaySoundW;
    let windows_dir = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    play_low_battery_sound_with(
        level,
        &windows_dir,
        |path| path.is_file(),
        |name, flags| {
            let name = wide(name);
            unsafe { PlaySoundW(windows::core::PCWSTR(name.as_ptr()), None, flags) }.as_bool()
        },
    )
}

fn play_low_battery_sound_with(
    level: u8,
    windows_dir: &std::path::Path,
    exists: impl FnOnce(&std::path::Path) -> bool,
    mut play: impl FnMut(&str, windows::Win32::Media::Audio::SND_FLAGS) -> bool,
) -> Result<(), ProviderError> {
    use windows::Win32::Media::Audio::{SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    let (filename, alias) = if level <= 5 {
        ("Windows Battery Critical.wav", "SystemHand")
    } else {
        ("Windows Battery Low.wav", "SystemExclamation")
    };
    let path = windows_dir.join("Media").join(filename);
    let played = if exists(&path) {
        play(
            &path.to_string_lossy(),
            SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
        )
    } else {
        play(alias, SND_ALIAS | SND_ASYNC)
    };
    if played {
        Ok(())
    } else {
        Err(ProviderError::new("Windows battery sound unavailable"))
    }
}

#[cfg(test)]
mod battery_sound_tests {
    use super::*;
    use windows::Win32::Media::Audio::{SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};

    #[test]
    fn production_playback_uses_windows_files_and_asynchronous_fallback_aliases() {
        for (level, filename, alias) in [
            (6, "Windows Battery Low.wav", "SystemExclamation"),
            (5, "Windows Battery Critical.wav", "SystemHand"),
            (0, "Windows Battery Critical.wav", "SystemHand"),
        ] {
            for present in [false, true] {
                let root = std::path::Path::new(r"C:\Windows");
                let expected = root.join("Media").join(filename);
                let mut calls = 0;
                play_low_battery_sound_with(
                    level,
                    root,
                    |path| {
                        assert_eq!(path, expected);
                        present
                    },
                    |name, flags| {
                        calls += 1;
                        assert_eq!(
                            name,
                            if present {
                                expected.to_str().unwrap()
                            } else {
                                alias
                            }
                        );
                        assert_eq!(
                            flags,
                            if present {
                                SND_FILENAME | SND_ASYNC | SND_NODEFAULT
                            } else {
                                SND_ALIAS | SND_ASYNC
                            }
                        );
                        true
                    },
                )
                .unwrap();
                assert_eq!(calls, 1);
            }
        }
        assert!(
            play_low_battery_sound_with(
                10,
                std::path::Path::new(r"C:\Windows"),
                |_| false,
                |_, _| false
            )
            .is_err()
        );
    }
}

fn polling_notification_state_blocked(state: Option<QUERY_USER_NOTIFICATION_STATE>) -> bool {
    !matches!(
        state,
        Some(QUNS_NOT_PRESENT | QUNS_ACCEPTS_NOTIFICATIONS | QUNS_QUIET_TIME)
    )
}
/// Configuration requires a known non-gaming Shell state. Failure is not permission.
pub fn polling_apply_blocked() -> bool {
    polling_notification_state_blocked(unsafe { SHQueryUserNotificationState() }.ok())
}
/// Conservative automatic polling trigger. Unknown, locked, busy and presentation
/// states authorize neither boosting nor restoration.
pub fn fullscreen_polling_state() -> Option<bool> {
    fullscreen_polling_notification_state(unsafe { SHQueryUserNotificationState() }.ok())
}
fn fullscreen_polling_notification_state(
    state: Option<QUERY_USER_NOTIFICATION_STATE>,
) -> Option<bool> {
    match state? {
        QUNS_RUNNING_D3D_FULL_SCREEN => Some(true),
        QUNS_ACCEPTS_NOTIFICATIONS | QUNS_QUIET_TIME => Some(false),
        _ => None,
    }
}
#[test]
fn automatic_polling_requires_conservative_shell_state() {
    assert_eq!(
        fullscreen_polling_notification_state(Some(QUNS_RUNNING_D3D_FULL_SCREEN)),
        Some(true)
    );
    for state in [QUNS_ACCEPTS_NOTIFICATIONS, QUNS_QUIET_TIME] {
        assert_eq!(
            fullscreen_polling_notification_state(Some(state)),
            Some(false)
        );
    }
    for state in [
        None,
        Some(QUNS_NOT_PRESENT),
        Some(QUNS_BUSY),
        Some(QUNS_PRESENTATION_MODE),
        Some(QUNS_APP),
        Some(QUERY_USER_NOTIFICATION_STATE(99)),
    ] {
        assert_eq!(fullscreen_polling_notification_state(state), None);
    }
}
pub const APP_USER_MODEL_ID: &str = "HaloBatteryNext.Desktop";
pub const APP_DISPLAY_NAME: &str = "Halo Continued";

struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn registry_result(result: WIN32_ERROR) -> Result<(), ProviderError> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(ProviderError::new(format!(
            "Application identity registry error {}",
            result.0
        )))
    }
}
fn register_application_identity(subkey: &str, icon_uri: &str) -> Result<(), ProviderError> {
    let subkey = wide(subkey);
    let mut handle = HKEY::default();
    registry_result(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
    })?;
    let key = RegistryKey(handle);
    for (name, value) in [("DisplayName", APP_DISPLAY_NAME), ("IconUri", icon_uri)] {
        let name = wide(name);
        let value = wide(value);
        registry_result(unsafe {
            RegSetKeyValueW(
                key.0,
                None,
                windows::core::PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            )
        })?;
    }
    Ok(())
}
/// Register a real notification image path and display name in HKCU.
/// IconUri is a file path, not an executable resource reference (exe,0).
pub fn identify_registered_with_icon(icon: &std::path::Path) -> Result<(), ProviderError> {
    set_process_identity()?;
    let icon = notification_icon_path(icon)?;
    register_application_identity(
        &format!("Software\\Classes\\AppUserModelId\\{APP_USER_MODEL_ID}"),
        &icon,
    )
}
fn notification_icon_path(icon: &std::path::Path) -> Result<String, ProviderError> {
    let supported = icon
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("ico"));
    if !supported || !icon.is_file() {
        return Err(ProviderError::new(
            "Notification icon must be an existing PNG or ICO image file",
        ));
    }
    let value = icon.canonicalize()?.to_string_lossy().into_owned();
    Ok(if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    })
}
fn set_process_identity() -> Result<(), ProviderError> {
    unsafe { SetCurrentProcessExplicitAppUserModelID(w!("HaloBatteryNext.Desktop")) }
        .map_err(|e| ProviderError::new(e.to_string()))
}
pub fn identify() {
    let _ = set_process_identity();
}

#[derive(Default)]
struct DockCache(Option<(std::time::Instant, bool)>);
impl DockCache {
    fn read(&mut self, now: std::time::Instant, force: bool, query: impl FnOnce() -> bool) -> bool {
        if !force
            && let Some((at, present)) = self.0
            && now.saturating_duration_since(at)
                < std::time::Duration::from_secs(if present { 10 } else { 30 })
        {
            return present;
        }
        let present = query();
        self.0 = Some((now, present));
        present
    }
}
/// Native process enumeration, cached for ten seconds while a dock is running
/// and thirty seconds when absent. Explicit UI/theme refreshes bypass the cache.
/// This does not spawn PowerShell or require process query privileges.
pub fn mydockfinder_running(force: bool) -> bool {
    use std::{
        sync::{Mutex, OnceLock},
        time::Instant,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    static CACHE: OnceLock<Mutex<DockCache>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(DockCache::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    cache.read(Instant::now(), force, || {
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .ok()
            .map(|h| {
                let snapshot = Snapshot(h);
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };
                let mut present = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
                while present {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|x| *x == 0)
                        .unwrap_or(entry.szExeFile.len());
                    if is_mydockfinder_wide(&entry.szExeFile[..end]) {
                        return true;
                    }
                    present = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
                }
                false
            })
            .unwrap_or(false)
    })
}
fn is_mydockfinder_wide(name: &[u16]) -> bool {
    let matches = |part: &[u16], text: &[u8]| {
        part.len() == text.len()
            && part
                .iter()
                .zip(text)
                .all(|(&unit, byte)| unit <= 0x7f && (unit as u8).eq_ignore_ascii_case(byte))
    };
    matches(name, b"dock_64.exe")
        || matches(name, b"dock_32.exe")
        || name.windows(6).any(|part| matches(part, b"mydock"))
}
#[cfg(test)]
fn is_mydockfinder(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "dock_64.exe" | "dock_32.exe" | "mydockfinder.exe"
    ) || name.contains("mydock")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dock_detection_backs_off_when_absent_but_refreshes_explicit_changes() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut cache = DockCache::default();
        assert!(!cache.read(start, false, || false));
        for seconds in [5, 10, 20, 29] {
            assert!(
                !cache.read(start + Duration::from_secs(seconds), false, || panic!(
                    "cached absent dock"
                ))
            );
        }
        assert!(cache.read(start + Duration::from_secs(30), false, || true));
        assert!(
            cache.read(start + Duration::from_secs(39), false, || panic!(
                "cached dock"
            ))
        );
        assert!(!cache.read(start + Duration::from_secs(40), false, || false));
        assert!(cache.read(start + Duration::from_secs(41), true, || true));
    }
    #[test]
    fn borrowed_dock_names_preserve_detection_without_string_allocations() {
        for name in [
            "",
            "explorer.exe",
            "Dock_64.EXE",
            "dock_32.exe",
            "MYDOCKFINDER.exe",
            "MyDockBeta.exe",
            "µmydock.exe",
            "mydöck.exe",
            "mydoc.exe",
        ] {
            assert_eq!(
                is_mydockfinder_wide(&name.encode_utf16().collect::<Vec<_>>()),
                is_mydockfinder(name)
            );
        }
        let mut invalid = vec![0xd800];
        invalid.extend("MYDOCK.exe".encode_utf16());
        assert!(is_mydockfinder_wide(&invalid));
    }
    #[test]
    fn app_and_system_theme_preferences_are_independent_and_default_light() {
        use windows::core::PCWSTR;
        struct TestKey(Vec<u16>);
        impl Drop for TestKey {
            fn drop(&mut self) {
                unsafe {
                    let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
                }
            }
        }
        let cleanup = TestKey(wide(&format!(
            "Software\\HaloBatteryNext.Tests\\Theme.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        let subkey = PCWSTR(cleanup.0.as_ptr());
        let mut handle = HKEY::default();
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey,
                    None,
                    None,
                    REG_OPTION_VOLATILE,
                    KEY_SET_VALUE,
                    None,
                    &mut handle,
                    None,
                )
            },
            ERROR_SUCCESS
        );
        let key = RegistryKey(handle);
        assert!(!theme_value_at(subkey, APPS_LIGHT_THEME));
        assert!(!theme_value_at(subkey, SYSTEM_LIGHT_THEME));
        for (apps, system) in [(0u32, 1u32), (1, 0), (0, 0), (1, 1)] {
            // Fixture names are independent of the production selectors, so a
            // spelling mistake cannot silently pass by writing the same typo.
            for (name, value) in [
                (w!("AppsUseLightTheme"), apps),
                (w!("SystemUsesLightTheme"), system),
            ] {
                assert_eq!(
                    unsafe {
                        RegSetKeyValueW(
                            key.0,
                            None,
                            name,
                            REG_DWORD.0,
                            Some((&value as *const u32).cast()),
                            size_of::<u32>() as u32,
                        )
                    },
                    ERROR_SUCCESS
                );
            }
            assert_eq!(theme_value_at(subkey, APPS_LIGHT_THEME), apps == 0);
            assert_eq!(theme_value_at(subkey, SYSTEM_LIGHT_THEME), system == 0);
        }
        drop(key);
        assert_eq!(
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, subkey) },
            ERROR_SUCCESS
        );
        assert!(!theme_value_at(subkey, APPS_LIGHT_THEME));
        assert!(!theme_value_at(subkey, SYSTEM_LIGHT_THEME));
    }
    #[test]
    fn singleton_reports_already_running_and_releases_owned_handle() {
        use windows::core::PCWSTR;
        let name = wide(&format!(
            "Local\\HaloBatteryNext.Acquisition.Test.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let InstanceAcquisition::Acquired(first) =
            Instance::acquire_named(PCWSTR(name.as_ptr())).unwrap()
        else {
            panic!("unique instance was not acquired")
        };
        assert!(matches!(
            Instance::acquire_named(PCWSTR(name.as_ptr())).unwrap(),
            InstanceAcquisition::AlreadyRunning
        ));
        drop(first);
        assert!(matches!(
            Instance::acquire_named(PCWSTR(name.as_ptr())).unwrap(),
            InstanceAcquisition::Acquired(_)
        ));
    }
    #[test]
    fn application_identity_registry_roundtrip() {
        use windows::core::PCWSTR;
        // Production registration and the test use the same native writer. This
        // unique key is removed even if an assertion unwinds.
        struct TestKey(Vec<u16>);
        impl Drop for TestKey {
            fn drop(&mut self) {
                unsafe {
                    let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
                }
            }
        }
        let name = format!(
            "Software\\Classes\\AppUserModelId\\HaloBatteryNext.Test.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let cleanup = TestKey(wide(&name));
        let icon = notification_icon_path(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../app/app.ico"
        )))
        .unwrap();
        register_application_identity(&name, &icon).unwrap();
        let read = |name: &str| {
            let name = wide(name);
            let mut value = [0u16; 512];
            let mut size = std::mem::size_of_val(&value) as u32;
            let mut kind = REG_VALUE_TYPE::default();
            assert_eq!(
                unsafe {
                    RegGetValueW(
                        HKEY_CURRENT_USER,
                        PCWSTR(cleanup.0.as_ptr()),
                        PCWSTR(name.as_ptr()),
                        RRF_RT_REG_SZ,
                        Some(&mut kind),
                        Some(value.as_mut_ptr().cast()),
                        Some(&mut size),
                    )
                },
                ERROR_SUCCESS
            );
            assert_eq!(kind, REG_SZ);
            assert_eq!(value[size as usize / 2 - 1], 0);
            String::from_utf16(&value[..size as usize / 2 - 1]).unwrap()
        };
        assert_eq!(read("DisplayName"), "Halo Continued");
        assert_eq!(read("IconUri"), icon);
        assert!(notification_icon_path(std::path::Path::new("HaloContinued.exe,0")).is_err());
        assert!(notification_icon_path(std::path::Path::new("missing.png")).is_err());
        assert!(std::path::Path::new(&icon).is_file());
        // Re-registration preserves a valid canonical image path.
        let moved_icon = icon.replace('\\', "/");
        assert_ne!(icon, moved_icon);
        assert!(std::path::Path::new(&moved_icon).is_file());
        register_application_identity(&name, &moved_icon).unwrap();
        assert_eq!(read("IconUri"), moved_icon);
    }
    #[test]
    fn native_process_application_identity_is_set() {
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(w!("HaloBatteryNext.Desktop")).unwrap();
            let id = GetCurrentProcessExplicitAppUserModelID().unwrap();
            let value = id.to_string().unwrap();
            windows::Win32::System::Com::CoTaskMemFree(Some(id.0.cast()));
            assert_eq!(value, APP_USER_MODEL_ID);
        }
    }
    #[test]
    fn polling_apply_requires_known_non_gaming_notification_state() {
        for state in [
            QUNS_NOT_PRESENT,
            QUNS_ACCEPTS_NOTIFICATIONS,
            QUNS_QUIET_TIME,
        ] {
            assert!(!polling_notification_state_blocked(Some(state)));
        }
        for state in [
            QUNS_APP,
            QUNS_BUSY,
            QUNS_RUNNING_D3D_FULL_SCREEN,
            QUNS_PRESENTATION_MODE,
            QUERY_USER_NOTIFICATION_STATE(0),
            QUERY_USER_NOTIFICATION_STATE(99),
        ] {
            assert!(polling_notification_state_blocked(Some(state)));
        }
        // A failed native query has no state and must never grant Apply permission.
        assert!(polling_notification_state_blocked(None));
    }
    #[test]
    fn gaming_notification_states_and_native_query() {
        for state in [
            QUNS_RUNNING_D3D_FULL_SCREEN,
            QUNS_PRESENTATION_MODE,
            QUNS_BUSY,
        ] {
            assert!(gaming_notification_state(state));
        }
        for state in [
            QUNS_NOT_PRESENT,
            QUNS_ACCEPTS_NOTIFICATIONS,
            QUNS_QUIET_TIME,
            QUNS_APP,
        ] {
            assert!(!gaming_notification_state(state));
        }
        // Native invocation must yield a bool even where Shell is unavailable;
        // classification above covers all documented values deterministically.
        let _: bool = gaming();
    }
    #[test]
    fn bundled_application_icon_is_native_and_has_expected_dimensions() {
        use windows::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::*};
        let path = wide(concat!(env!("CARGO_MANIFEST_DIR"), "/../app/app.ico"));
        unsafe {
            let handle = LoadImageW(
                None,
                windows::core::PCWSTR(path.as_ptr()),
                IMAGE_ICON,
                64,
                64,
                LR_LOADFROMFILE,
            )
            .unwrap();
            let icon = HICON(handle.0);
            let mut info = ICONINFO::default();
            let result = GetIconInfo(icon, &mut info);
            let _ = DestroyIcon(icon);
            result.unwrap();
            let mut bitmap = BITMAP::default();
            let size = GetObjectW(
                info.hbmColor.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut bitmap as *mut BITMAP).cast()),
            );
            let _ = DeleteObject(info.hbmColor.into());
            let _ = DeleteObject(info.hbmMask.into());
            assert_eq!(size as usize, std::mem::size_of::<BITMAP>());
            assert_eq!((bitmap.bmWidth, bitmap.bmHeight), (64, 64));
        }
        let bytes = include_bytes!("../../app/app.ico");
        assert!(
            bytes[62..62 + 64 * 64 * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] > 0)
        );
        assert!(
            bytes[62..62 + 64 * 64 * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] == 0)
        );
    }
    #[test]
    fn shell_process_names() {
        assert!(is_mydockfinder("Dock_64.EXE"));
        assert!(is_mydockfinder("MyDockBeta.exe"));
        assert!(!is_mydockfinder("explorer.exe"));
    }
    #[test]
    fn temporary_executable_copy_and_directory_boundaries() {
        use std::path::{Path, PathBuf};
        let roots = [
            PathBuf::from("C:\\Users\\Tester\\Temp"),
            PathBuf::from("D:\\Tmp"),
        ];
        assert!(executable_in_temp(
            Path::new("C:\\Users\\Tester\\Temp\\Rar$EXa1234\\HaloContinued.exe"),
            &roots
        ));
        assert!(executable_in_temp(
            Path::new("d:\\TMP\\extract\\HaloContinued.exe"),
            &roots
        ));
        assert!(!executable_in_temp(
            Path::new("C:\\Users\\Tester\\TempTools\\HaloContinued.exe"),
            &roots
        ));
        assert!(!executable_in_temp(
            Path::new("C:\\Tools\\HaloContinued.exe"),
            &roots
        ));
        assert!(!executable_in_temp(
            Path::new("C:\\Users\\Tester\\Temp\\..\\Tools\\HaloContinued.exe"),
            &roots
        ));
        assert!(!executable_in_temp(
            Path::new("C:\\Tools\\HaloContinued.exe"),
            &[PathBuf::new()]
        ));
    }
}

/// Timestamp only: no input contents, hooks, interception or synthetic input.
pub fn last_input_tick() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetLastInputInfo(&mut info).as_bool().then_some(info.dwTime) }
}
