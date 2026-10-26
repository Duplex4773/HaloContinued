//! Device information is queried only by opening this view or pressing Refresh.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(super) struct DeviceInfoUi {
    next: u64,
    pending: Option<(DeviceInfoRequest, Arc<AtomicBool>)>,
    outcome: Option<Box<DeviceInfoOutcome>>,
}
impl DeviceInfoUi {
    #[cfg(test)]
    pub(super) fn pending_sequence(&self) -> Option<u64> {
        self.pending.as_ref().map(|(r, _)| r.request)
    }
    pub(super) fn clear(&mut self) {
        if let Some((_, cancel)) = self.pending.take() {
            cancel.store(true, Ordering::Release);
        }
        self.outcome = None;
    }
    pub(super) fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"pending":self.pending.is_some(),"firmware":self.outcome.as_ref().and_then(|o|o.firmware).map(|v|v.to_string()),
            "checked_at":self.outcome.as_ref().map(|o|o.checked_at),"error":self.outcome.as_ref().and_then(|o|o.failure.as_deref())})
    }
    fn matches(&self, device: &ConfigurationDevice, generation: u64) -> bool {
        let target = self
            .pending
            .as_ref()
            .map(|(r, _)| &r.target)
            .or_else(|| self.outcome.as_ref().map(|r| &r.target));
        target.is_some_and(|t| {
            t.generation == generation
                && DeviceDescriptorRef::from(&t.device)
                    .same_identity(DeviceDescriptorRef::from(device))
        })
    }
    fn accept(
        &mut self,
        outcome: Box<DeviceInfoOutcome>,
        device: &ConfigurationDevice,
        generation: u64,
    ) -> bool {
        if !self.matches(device, generation)
            || !self.pending.as_ref().is_some_and(|(r, c)| {
                !c.load(Ordering::Acquire)
                    && r.request == outcome.request
                    && r.target.generation == outcome.target.generation
                    && DeviceDescriptorRef::from(&r.target.device)
                        .same_identity(DeviceDescriptorRef::from(&outcome.target.device))
            })
        {
            return false;
        }
        self.pending = None;
        self.outcome = Some(outcome);
        true
    }
}
fn firmware_message(message: &str) -> &str {
    if [
        "Firmware reading ",
        "Firmware could ",
        "Firmware is not read ",
        "The device ",
        "The selected device ",
        "The connection ",
        "This connection ",
        "Connect or wake ",
        "Wake the mouse",
        "More than one device ",
        "Device workers are busy",
        "Open Devices",
        "The device returned",
        "This connection did not",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
    {
        message
    } else {
        "Couldn't read the firmware version. Close other device apps and try Refresh information."
    }
}
fn connection_text(device: &ConfigurationDevice) -> String {
    let transport = hb_providers::device_metadata::connection_label(device);
    let state = match device.connection {
        Connection::Online => "Connected",
        Connection::Sleeping => "Sleeping",
        Connection::Stale => "Disconnected",
    };
    format!("{transport} · {state}")
}
fn verification_text(device: &ConfigurationDevice, battery: bool) -> String {
    if device.source == "simulation" {
        return "Simulated device. No hardware verification is claimed.".into();
    }
    if device.source == "razer" && device.key.starts_with("razer:00bf:") {
        return "Wireless polling and fullscreen boost: User-tested.\r\nSleep: User-tested at the selected timeout.\r\nDPI and firmware reading: Not yet verified.".into();
    }
    if device.source == "razer" && device.key.starts_with("razer:00be:") {
        return "The wireless model has user-tested polling, boost and sleep.\r\nThis USB connection and firmware reading: Not yet verified.".into();
    }
    if battery && hb_providers::device_metadata::parent_battery_verified(device) {
        return "Battery: User-verified in HaloBattery.\r\nDevice settings and firmware reading: Not hardware-verified.".into();
    }
    if battery {
        "Hardware verification is not recorded for this model.\r\nBattery support is inherited from HaloBattery; this does not verify its settings controls.".into()
    } else {
        "Settings controls and firmware reading: Not hardware-verified for this model.".into()
    }
}

#[cfg(test)]
pub(super) fn show_fixture_information(state: &mut State) {
    state.information.clear();
    state.device_section = 4;
    state.page_scroll = 0;
    state.build();
    let device = state.current_device().unwrap().0;
    state.information.next += 1;
    let request = DeviceInfoRequest {
        request: state.information.next,
        target: ControlTarget {
            device,
            generation: state.polling.generation,
            connection_stamp: None,
        },
    };
    state.information.pending = Some((request.clone(), Arc::new(AtomicBool::new(false))));
    state.device_information_outcome(Box::new(DeviceInfoOutcome::from_result(
        &request,
        SystemClock::default().unix(),
        Ok(FirmwareVersion {
            major: 1,
            minor: 21,
        }),
    )));
}

#[cfg(test)]
pub(super) fn exercise_native_information(state: &mut State) {
    unsafe {
        let snapshot = state.snapshot.clone();
        let configuration = state.configuration_devices.clone();
        let selected = state.selected_device.clone();
        let settings = state.settings.clone();
        let section = state.device_section;
        let mut reading = Reading::new(
            "razer:00bf:INFO-TEST",
            "Razer DeathAdder V4 Pro",
            "razer",
            0,
        );
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        reading.level = Some(50);
        reading.charging = Some(false);
        state.page = 1;
        state.selected_device = Some(reading.key.clone());
        state.settings.polling_controls = false;
        state.configuration_devices.clear();
        state.snapshot.devices = vec![DeviceView {
            reading: reading.clone(),
            name: reading.name.clone(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Mouse: 50%".into(),
            hidden: false,
        }];
        state.device_section = 0;
        state.build();
        state.configuration_visibility();
        let polling_sequence = state.polling.sequence;
        state.command(24, 0);
        assert!(IsWindowVisible(state.controls[&480]).as_bool());
        assert!(state.text(481).contains("Wireless"));
        assert!(state.text(483).contains("Reading"));
        let (request, cancel) = state.information.pending.as_ref().unwrap().clone();
        assert!(
            !windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(state.controls[&484])
                .as_bool()
        );
        let sequence = state.information.next;
        state.read_device_information(true);
        state.layout_page();
        state.build();
        state.layout_page();
        assert_eq!(
            state.information.next, sequence,
            "Repaint/rebuild must not requery firmware"
        );
        assert_eq!(
            state.polling.sequence, polling_sequence,
            "Information must not query settings"
        );
        state.device_information_outcome(Box::new(DeviceInfoOutcome::from_result(
            &request,
            1234,
            Ok(FirmwareVersion {
                major: 1,
                minor: 21,
            }),
        )));
        assert!(state.text(483).contains("1.21"));
        assert!(state.text(483).contains("Not reported"));
        assert!(state.text(486).contains("Polling rate"));
        assert!(state.text(488).contains("Not yet verified"));
        assert!(
            windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(state.controls[&484])
                .as_bool()
        );
        state.command(484, 0);
        let current_cancel = state.information.pending.as_ref().unwrap().1.clone();
        state.device_information_outcome(Box::new(DeviceInfoOutcome::from_result(
            &request,
            1234,
            Ok(FirmwareVersion {
                major: 9,
                minor: 99,
            }),
        )));
        assert!(
            !state.text(483).contains("9.99"),
            "Late results must not replace a newer read"
        );
        state.command(17, 0);
        assert!(current_cancel.load(Ordering::Acquire));
        assert!(state.information.pending.is_none());
        assert!(state.information.outcome.is_none());
        assert!(
            !cancel.load(Ordering::Acquire),
            "Accepted reads need no cancellation"
        );

        state.snapshot.devices.clear();
        let mut keyboard = ConfigurationDevice::from_reading(&Reading::new(
            "corsair:1bb3:INFO-KEYBOARD",
            "Keyboard",
            "corsair",
            0,
        ));
        keyboard.kind = "keyboard".into();
        keyboard.via = "usb".into();
        keyboard.capability = PollingCapability::Unavailable("Maintained software session".into());
        state.selected_device = Some(keyboard.key.clone());
        state.configuration_devices = vec![keyboard];
        state.build();
        state.command(24, 0);
        assert!(state.information.pending.is_none());
        assert!(state.text(483).contains("not available"));
        assert!(!state.text(486).contains("Battery"));
        state.device_information_outcome(Box::new(DeviceInfoOutcome::from_result(
            &request,
            1234,
            Ok(FirmwareVersion {
                major: 9,
                minor: 99,
            }),
        )));
        assert!(!state.text(483).contains("9.99"));

        let mut second = state.configuration_devices[0].clone();
        second.key = "corsair:1bb3:OTHER-KEYBOARD".into();
        state.configuration_devices.push(second);
        state.build();
        let sequence = state.polling.sequence;
        state.overview_select(1);
        assert_eq!(
            state.device_section, 4,
            "Selecting a device should retain the information view"
        );
        assert_eq!(state.polling.sequence, sequence);
        assert!(state.text(483).contains("not available"));
        state.information.clear();
        state.snapshot = snapshot;
        state.configuration_devices = configuration;
        state.selected_device = selected;
        state.settings = settings;
        state.device_section = section;
        state.build();
    }
}
impl State {
    pub(super) fn build_device_information(&mut self) {
        self.heading(480, "Device information", 24, 112, 600);
        self.label(481, "", 24, 112, 720);
        self.control(
            483,
            w!("STATIC"),
            "",
            WINDOW_STYLE::default(),
            24,
            112,
            720,
            50,
        );
        self.button(484, "Refresh information", 24, 112, 180);
        self.heading(485, "Supported controls", 24, 112, 350);
        self.control(
            486,
            w!("STATIC"),
            "",
            WINDOW_STYLE::default(),
            24,
            112,
            350,
            108,
        );
        self.heading(487, "Verification", 24, 112, 350);
        self.control(
            488,
            w!("STATIC"),
            "",
            WINDOW_STYLE::default(),
            24,
            112,
            350,
            108,
        );
        self.label(
            489,
            "Open Device info or refresh to check the firmware.",
            24,
            112,
            500,
        );
    }
    pub(super) fn read_device_information(&mut self, force: bool) {
        if self.dashboard.is_none() || self.page != 1 || self.device_section != 4 {
            return;
        }
        let Some((device, _)) = self.current_device() else {
            self.information.clear();
            return;
        };
        if !force && self.information.matches(&device, self.polling.generation) {
            self.render_device_information();
            return;
        }
        // Repeated clicks while a request is pending do not enqueue more work.
        if self.information.pending.is_some()
            && self.information.matches(&device, self.polling.generation)
        {
            return;
        }
        self.information.clear();
        self.information.next = self.information.next.wrapping_add(1);
        let request = DeviceInfoRequest {
            request: self.information.next,
            target: ControlTarget {
                device,
                generation: self.polling.generation,
                connection_stamp: None,
            },
        };
        if !hb_providers::device_info::supported(&request.target.device)
            || !request.target.device.online()
        {
            let message = if request.target.device.online() {
                "Firmware reading is not available for this device yet."
            } else {
                "Connect or wake the device, then select Refresh information."
            };
            self.information.outcome = Some(Box::new(DeviceInfoOutcome::failed(&request, message)));
        } else {
            let cancel = Arc::new(AtomicBool::new(false));
            self.information.pending = Some((request.clone(), cancel.clone()));
            if let Err(error) = self.runtime.request_device_info(request.clone(), cancel) {
                self.information.clear();
                self.information.outcome =
                    Some(Box::new(DeviceInfoOutcome::failed(&request, error.message)));
            }
        }
        self.render_device_information();
    }
    pub(super) fn device_information_outcome(&mut self, outcome: Box<DeviceInfoOutcome>) {
        if self.dashboard.is_none() || self.page != 1 || self.device_section != 4 {
            return;
        }
        if let Some((device, _)) = self.current_device()
            && self
                .information
                .accept(outcome, &device, self.polling.generation)
        {
            self.render_device_information();
        }
    }
    pub(super) fn render_device_information(&self) {
        if self.page != 1 || self.device_section != 4 || !self.controls.contains_key(&480) {
            return;
        }
        let Some((device, battery)) = self.current_device() else {
            return;
        };
        self.set_control_text(481, &connection_text(&device));
        let current = self.information.matches(&device, self.polling.generation);
        let outcome = current
            .then_some(self.information.outcome.as_ref())
            .flatten();
        let firmware = if let Some(version) = outcome.and_then(|o| o.firmware) {
            format!(
                "Reported firmware: {version}\r\nSeparate mouse / receiver versions: Not reported by this connection."
            )
        } else if current && self.information.pending.is_some() {
            "Reading the firmware version…".into()
        } else {
            outcome
                .and_then(|o| o.failure.as_deref())
                .map(firmware_message)
                .unwrap_or("Select Refresh information to read the firmware version.")
                .to_owned()
        };
        self.set_control_text(483, &firmware);
        let polling = matches!(device.capability, PollingCapability::ReadWrite)
            && self.has_polling_controls(&device);
        let mut controls = Vec::new();
        if battery.is_some() {
            controls.push("Battery monitoring and alerts");
        }
        if polling {
            controls.push(
                if device.kind == "mouse"
                    && hb_providers::controls::polling_menu_rates(&device)
                        .iter()
                        .any(|r| *r > 1000)
                {
                    "Polling rate and fullscreen boost"
                } else {
                    "Polling rate"
                },
            );
        }
        if hb_providers::dpi::candidate(&device) {
            controls.push("Sensitivity (DPI)");
        }
        let sleep = hb_providers::sleep_controls::model(&device);
        if sleep.is_some() {
            controls.push("Sleep timer");
        }
        if battery.is_some()
            && device.kind == "mouse"
            && (polling || sleep.is_some_and(|m| m.readable()))
        {
            controls.push(if device.wired_mouse() {
                "Battery saver (wireless)"
            } else {
                "Battery saver"
            });
        }
        self.set_control_text(
            486,
            &if controls.is_empty() {
                "No device settings controls are available for this model yet.".into()
            } else {
                controls.join("\r\n")
            },
        );
        self.set_control_text(488, &verification_text(&device, battery.is_some()));
        self.set_control_text(
            489,
            &outcome
                .filter(|o| o.checked_at > 0)
                .map(|o| {
                    format!(
                        "Checked {} · Refresh after a firmware update.",
                        polling_timestamp(o.checked_at)
                    )
                })
                .unwrap_or_else(|| {
                    "Firmware is checked only when you open this view or select Refresh.".into()
                }),
        );
        self.enable_control(
            484,
            device.online()
                && hb_providers::device_info::supported(&device)
                && !(current && self.information.pending.is_some()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(id: u64) -> DeviceInfoRequest {
        let mut reading = Reading::new("razer:00bf:UNITTEST", "Mouse", "razer", 0);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        DeviceInfoRequest {
            request: id,
            target: ControlTarget {
                device: ConfigurationDevice::from_reading(&reading),
                generation: 4,
                connection_stamp: None,
            },
        }
    }
    #[test]
    fn info_results_require_the_live_request_identity_and_epoch() {
        for change in 0..4 {
            let request = request(10);
            let mut ui = DeviceInfoUi {
                next: 10,
                pending: Some((request.clone(), Arc::new(AtomicBool::new(false)))),
                outcome: None,
            };
            let mut result = DeviceInfoOutcome::from_result(
                &request,
                123,
                Ok(FirmwareVersion {
                    major: 1,
                    minor: 21,
                }),
            );
            match change {
                0 => result.request = 9,
                1 => result.target.generation = 3,
                2 => result.target.device.serial = Some("OTHER".into()),
                _ => {}
            }
            assert_eq!(
                ui.accept(Box::new(result), &request.target.device, 4),
                change == 3
            );
        }
    }
    #[test]
    fn leaving_information_cancels_the_read_and_drops_the_cached_version() {
        let request = request(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ui = DeviceInfoUi {
            next: 1,
            pending: Some((request.clone(), cancel.clone())),
            outcome: None,
        };
        ui.clear();
        assert!(cancel.load(Ordering::Acquire));
        assert!(ui.pending.is_none());
        assert!(ui.outcome.is_none());
        let result = DeviceInfoOutcome::from_result(
            &request,
            123,
            Ok(FirmwareVersion {
                major: 1,
                minor: 21,
            }),
        );
        assert!(!ui.accept(Box::new(result), &request.target.device, 4));
        assert_eq!(ui.next, 1);
    }
    #[test]
    fn connection_and_verification_are_specific_and_do_not_claim_full_certification() {
        let mut device = request(1).target.device;
        assert_eq!(
            connection_text(&device),
            "Wireless · USB receiver · Connected"
        );
        let verification = verification_text(&device, true);
        assert!(verification.contains("User-tested"));
        assert!(verification.contains("Not yet verified"));
        device.key = "razer:00be:UNITTEST".into();
        assert!(connection_text(&device).starts_with("USB cable"));
        device.source = "simulation".into();
        assert!(verification_text(&device, true).contains("No hardware verification"));
        assert!(!firmware_message("raw report 0x81 HID failure").contains("0x81"));
    }
}
