#![windows_subsystem = "windows"]
#[cfg(test)]
#[path = "../../core/tests/support/allocation_counter.rs"]
mod allocation_counter;
#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: allocation_counter::TrackingAllocator =
    allocation_counter::TrackingAllocator;
mod battery_saver;
mod boost;
mod chart;
mod dashboard_theme;
mod dashboard_widgets;
mod icons;
mod runtime;
mod storage_worker;
mod ui;
use hb_core::*;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
fn application_data_directory(args: &[String]) -> hb_storage::DataDirectory {
    let explicit = args
        .iter()
        .position(|arg| arg == "--data-dir")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from);
    let executable = std::env::current_exe().ok();
    hb_storage::resolve_data_directory(executable.as_deref(), explicit)
}
fn main() {
    if let Err(e) = entry() {
        if std::env::args().any(|a| a == "--probe" || a == "--polling-probe") {
            eprintln!("Halo Continued: {e}");
            std::process::exit(1);
        }
        let args: Vec<_> = std::env::args().collect();
        let directory = application_data_directory(&args).path;
        let saved = std::fs::create_dir_all(&directory).is_ok()
            && hb_storage::atomic_write(
                &directory.join("startup-issue.txt"),
                e.to_string().as_bytes(),
            )
            .is_ok();
        let message = hb_windows::transport::wide(if saved {
            "Halo Continued couldn't start. Try opening the app again.\n\nIf this keeps happening, share startup-issue.txt from the app's data folder with support."
        } else {
            "Halo Continued couldn't start. Try opening the app again. If this keeps happening, check that the app can save files in its data folder."
        });
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(message.as_ptr()),
                windows::core::w!("Halo Continued"),
                windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}
fn entry() -> Result<(), ProviderError> {
    let args: Vec<String> = std::env::args().collect();
    let option = |name: &str| {
        args.iter()
            .position(|s| s == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if args.iter().any(|a| a == "--simulate-keyboards") && !args.iter().any(|a| a == "--simulate") {
        return Err(ProviderError::new(
            "--simulate-keyboards requires --simulate",
        ));
    }
    let directory = application_data_directory(&args);
    let dir = directory.path;
    if directory.portable_requested && !directory.portable {
        let _ = hb_storage::atomic_write(&dir.join("portable-issue.txt"),
            b"Portable settings were requested, but the app folder could not be written. Settings and history use the usual HaloBatteryNext data folder instead.");
    }
    if args.iter().any(|a| a == "--polling-probe") {
        // Diagnostic access is exclusive with the running app. Writes always
        // require an explicit rate and exact stable key; no default mouse SET.
        let _instance = hb_windows::system::Instance::acquire()?;
        let selected_provider = option("--provider").unwrap_or_else(|| "razer".into());
        if !hb_providers::controls::POLLING_PROVIDERS.contains(&selected_provider.as_str())
            && selected_provider != "corsair"
        {
            return Err(ProviderError::new(
                "Polling controls support Razer, Logitech and MCHOSE; Corsair keyboards are recognition only",
            ));
        }
        let action = match option("--polling-hz") {
            Some(hz) => {
                if option("--device-key").is_none() {
                    return Err(ProviderError::new(
                        "An explicit --device-key is required for a rate change",
                    ));
                }
                let hz = hz
                    .parse::<u32>()
                    .map_err(|_| ProviderError::new("Invalid polling rate"))?;
                ControlAction::Apply(PollingRate::try_from(hz).map_err(ProviderError::new)?)
            }
            None => ControlAction::Read,
        };
        if matches!(action, ControlAction::Apply(_)) && hb_windows::system::polling_apply_blocked()
        {
            return Err(ProviderError::new(
                "Close the game or presentation and verify the Windows notification state before changing the rate",
            ));
        }
        let hid = hb_windows::WindowsHid::new()?;
        let clock = SystemClock::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let context = PollContext {
            clock: &clock,
            cancelled: &cancel,
            deadline: clock.monotonic() + Duration::from_secs(25),
            playstation_full_mode: false,
        };
        let kind = option("--device-kind").unwrap_or_else(|| "all".into());
        if !["all", "mouse", "keyboard"].contains(&kind.as_str()) {
            return Err(ProviderError::new(
                "Use --device-kind all, mouse or keyboard",
            ));
        }
        let key = option("--device-key");
        let mut devices = Vec::new();
        if kind != "mouse" && matches!(selected_provider.as_str(), "razer" | "corsair") {
            devices = hb_providers::configuration::discover_keyboards(&hid, &context)?
                .into_iter()
                .filter(|d| d.source == selected_provider)
                .collect();
        }
        let selected_keyboard = key
            .as_ref()
            .is_some_and(|key| devices.iter().any(|d| &d.key == key));
        if kind != "keyboard" && !selected_keyboard && selected_provider != "corsair" {
            let mut provider = hb_providers::providers()
                .into_iter()
                .find(|p| p.id() == selected_provider)
                .ok_or_else(|| ProviderError::new("Provider unavailable"))?;
            devices.extend(
                provider
                    .poll(&hid, &context)?
                    .iter()
                    .map(ConfigurationDevice::from_reading),
            );
        }
        let mut output = Vec::new();
        for device in devices
            .into_iter()
            .filter(|r| option("--device-key").is_none_or(|key| key == r.key))
        {
            let request = ControlRequest {
                request: 1,
                target: ControlTarget {
                    device,
                    generation: hid.generation(),
                    connection_stamp: None,
                },
                action,
            };
            let result = runtime::execute_control(
                &request,
                &hid,
                &clock,
                &cancel,
                &cancel,
                hb_windows::system::polling_apply_blocked(),
                None,
            );
            output.push(serde_json::json!({ "key": result.key,
                "name": request.target.device.name,
                "previous_hz": result.previous.map(PollingRate::hz),
                "observed_hz": result.observation.as_ref().and_then(|o| o.rate).map(PollingRate::hz),
                "supported_hz": result.observation.as_ref().map(|o| o.supported.iter().map(|r| r.hz()).collect::<Vec<_>>()),
                "timestamp": result.observation.as_ref().map(|o| o.timestamp),
                "may_have_changed": result.may_have_changed,
                "failure": result.failure }));
        }
        if output.is_empty() {
            return Err(ProviderError::new("No matching online device"));
        }
        let path = option("--output")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("polling-probe.json"));
        hb_storage::atomic_write(&path, &serde_json::to_vec_pretty(&output).unwrap())?;
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe") {
        // Battery probes also send vendor requests. Share the same exclusion
        // as polling probes so they cannot interrupt the running app's exchange.
        let _instance = hb_windows::system::Instance::acquire()?;
        let hid = hb_windows::WindowsHid::new()?;
        let clock = SystemClock::default();
        let cancel = AtomicBool::new(false);
        let mut providers = hb_providers::providers();
        if let Some(jbl) = providers.iter_mut().find(|provider| provider.id() == "jbl") {
            *jbl = Box::new(hb_providers::provider::HidProvider::new("jbl").with_probe_listen());
        }
        providers.push(Box::new(hb_windows::BluetoothProvider::default()));
        providers.push(Box::new(hb_windows::ControllerProvider));
        if let Some(selected) = option("--provider")
            && !providers.iter().any(|p| p.id() == selected)
        {
            return Err(ProviderError::new(format!("Unknown provider: {selected}")));
        }
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        let mut output = serde_json::Map::new();
        for mut p in providers {
            if option("--provider").is_some_and(|id| id != p.id()) {
                continue;
            }
            let c = PollContext {
                clock: &clock,
                cancelled: &cancel,
                deadline: clock.monotonic() + Duration::from_secs(25),
                playstation_full_mode: false,
            };
            let result = p.poll(&hid, &c);
            output.insert(
                p.id().into(),
                match result {
                    Ok(r) => serde_json::json!({"devices":r,"diagnostics":p.diagnostics()}),
                    Err(e) => {
                        serde_json::json!({"error":e.to_string(),"diagnostics":p.diagnostics()})
                    }
                },
            );
        }
        let path = option("--output")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("probe.json"));
        hb_storage::atomic_write(&path, &serde_json::to_vec_pretty(&output).unwrap())?;
        return Ok(());
    }
    let _instance = match hb_windows::system::Instance::acquire_state()? {
        hb_windows::system::InstanceAcquisition::Acquired(instance) => instance,
        hb_windows::system::InstanceAcquisition::AlreadyRunning => {
            // A second normal launch opens the existing dashboard. The mutex
            // still owns lifecycle exclusion; this lookup never starts workers.
            if args.iter().any(|a| a == "--background") {
                return Ok(());
            }
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::*;
                if let Ok(hwnd) = FindWindowW(
                    windows::core::w!("HaloBatteryNext.Native"),
                    // Keep the hidden discovery title compatible with older running builds.
                    windows::core::w!("Halo Battery Next monitor"),
                ) {
                    if !args.iter().any(|a| a == "--background") {
                        PostMessageW(
                            Some(hwnd),
                            WM_APP + 8,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        )
                        .map_err(|e| ProviderError::new(e.to_string()))?;
                    }
                    return Ok(());
                }
            }
            return Err(ProviderError::new(
                "Halo Continued is already starting; try opening it again shortly",
            ));
        }
    };
    let icon_path = dir.join("application.png");
    let icon: &[u8] = include_bytes!("../application.png");
    let registration = (|| {
        hb_windows::system::identify();
        if std::fs::read(&icon_path).ok().as_deref() != Some(icon) {
            hb_storage::atomic_write(&icon_path, icon)?;
        }
        hb_windows::system::identify_registered_with_icon(&icon_path)
    })();
    let identity_error = registration.err().map(|e| e.to_string());
    let settings = hb_storage::load_settings(&dir.join("config.json"));
    let runtime = runtime::Runtime::start(
        dir.clone(),
        settings.clone(),
        args.iter().any(|a| a == "--simulate"),
    )?;
    ui::run(
        runtime,
        settings,
        dir,
        args.iter().any(|a| a == "--background"),
        identity_error,
    )
}

#[cfg(test)]
mod performance_fixture {
    /// Run the production monitoring/tray path with synthetic readings. This
    /// deliberately omits global instance/notification registration so a normal
    /// app can stay running. An external sampler owns timing and graceful exit.
    #[test]
    #[ignore = "requires an explicit isolated directory and external sampler"]
    fn background_monitoring() {
        let directory = std::path::PathBuf::from(
            std::env::var_os("HALO_BENCHMARK_DIRECTORY")
                .expect("HALO_BENCHMARK_DIRECTORY must name a fresh benchmark fixture"),
        )
        .canonicalize()
        .unwrap();
        let private = std::env::current_dir()
            .unwrap()
            .join("validation-local")
            .canonicalize()
            .unwrap();
        assert!(directory.starts_with(&private) && directory != private);
        assert!(!directory.join("history.db").exists());
        let settings = hb_storage::load_settings(&directory.join("config.json"));
        assert!(!settings.polling_controls && !settings.restore_polling_on_startup);
        assert!(!settings.notify && !settings.full_alert && !settings.low_sound);
        assert!(!settings.bluetooth && !settings.quiet_fullscreen && !settings.status_file);
        assert!(settings.devices.is_empty());
        let runtime = crate::runtime::Runtime::start(directory.clone(), settings.clone(), true)
            .expect("synthetic runtime must start");
        crate::ui::run(runtime, settings, directory, true, None)
            .expect("native benchmark must exit gracefully");
    }
}
