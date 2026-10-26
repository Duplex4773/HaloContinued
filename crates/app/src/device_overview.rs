//! Presentation-only device overview. Existing controls retain their commands,
//! drafts and validation. Card resources exist only while the dashboard is open.
use super::*;

const FIRST_CARD: u16 = 1000;
const LAST_CARD: u16 = 32767;

fn clean_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn boost_menu_rates(
    device: &ConfigurationDevice,
    observation: Option<&PollingObservation>,
) -> Vec<PollingRate> {
    hb_providers::controls::polling_menu_rates(device)
        .iter()
        .copied()
        .filter(|hz| *hz > 1000)
        .filter_map(|hz| PollingRate::try_from(hz).ok())
        .filter(|rate| observation.is_none_or(|o| o.supported.contains(rate)))
        .collect()
}

fn card_battery_value(reading: &Reading) -> String {
    reading.level.map_or_else(
        || "—".into(),
        |level| {
            if reading.precision != Precision::Exact {
                format!("~{level}%")
            } else {
                format!("{level}%")
            }
        },
    )
}

impl State {
    pub(super) fn append_configuration_menu(
        &self,
        menu: HMENU,
        key: Option<&str>,
    ) -> Vec<(u16, String)> {
        if !self.settings.fluent_menu {
            return Vec::new();
        }
        let rows = self.device_rows();
        let choices = rows
            .iter()
            .filter(|(d, b)| b.is_none() && Some(d.key.as_str()) != key)
            .take(usize::from(u16::MAX - 33000))
            .enumerate()
            .map(|(i, (d, _))| (33000 + i as u16, d.key.clone()))
            .collect::<Vec<_>>();
        if choices.is_empty() {
            return choices;
        }
        unsafe {
            let existing = (0..GetMenuItemCount(Some(menu))).find_map(|i| {
                let mut caption = [0u16; 64];
                let n = GetMenuStringW(menu, i as u32, Some(&mut caption), MF_BYPOSITION);
                (String::from_utf16_lossy(&caption[..n.max(0) as usize]) == "Other &devices")
                    .then(|| GetSubMenu(menu, i))
            });
            let child = if let Some(existing) = existing {
                existing
            } else {
                let Ok(child) = CreatePopupMenu() else {
                    return Vec::new();
                };
                let owned = Popup(child);
                let before = (0..GetMenuItemCount(Some(menu)))
                    .find(|i| GetMenuItemID(menu, *i) == 504)
                    .map(|i| i.saturating_sub(1) as u32)
                    .unwrap_or(0);
                if InsertMenuW(
                    menu,
                    before,
                    MF_BYPOSITION | MF_POPUP,
                    child.0 as usize,
                    w!("Other &devices"),
                )
                .is_err()
                {
                    return Vec::new();
                }
                std::mem::forget(owned);
                child
            };
            for (id, key) in &choices {
                if let Some((d, _)) = rows.iter().find(|(d, _)| &d.key == key) {
                    let text = wide_menu_data(&self.configuration_name(d));
                    let _ = AppendMenuW(child, MF_STRING, *id as usize, PCWSTR(text.as_ptr()));
                }
            }
        }
        choices
    }
    pub(super) fn prepare_overview_menu(
        &self,
        menu: HMENU,
        key: Option<&str>,
    ) -> Vec<(u16, Option<PollingRate>)> {
        let Some(key) = key else {
            return Vec::new();
        };
        let Some(view) = self.snapshot.devices.iter().find(|d| d.reading.key == key) else {
            return Vec::new();
        };
        let descriptor = ConfigurationDevice::from_reading(&view.reading);
        let rate = self
            .polling
            .observations
            .get(key)
            .or_else(|| self.tray_polling.observations.get(key))
            .or_else(|| self.tooltip_rates.get(key))
            .filter(|o| {
                self.settings.polling_controls && o.target.device.matches_reading(&view.reading)
            })
            .and_then(|o| o.rate);
        let mut summary = battery_summary(view);
        if let Some(rate) = rate {
            summary.push_str(&format!(" · {} Hz last confirmed", rate.hz()));
        }
        let caption = wide_menu_data(&format!("{}\n{summary}", clean_line(&view.name)));
        unsafe {
            let info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STRING,
                dwTypeData: windows::core::PWSTR(caption.as_ptr() as _),
                ..Default::default()
            };
            let _ = SetMenuItemInfoW(menu, 0, true, &info);
            if let Some(rate) = rate {
                for index in 0..GetMenuItemCount(Some(menu)) {
                    let mut label = [0u16; 64];
                    let length =
                        GetMenuStringW(menu, index as u32, Some(&mut label), MF_BYPOSITION);
                    if String::from_utf16_lossy(&label[..length.max(0) as usize]) == "&Polling rate"
                    {
                        let label = wide(&format!("&Polling rate\t{} Hz", rate.hz()));
                        let info = MENUITEMINFOW {
                            dwTypeData: windows::core::PWSTR(label.as_ptr() as _),
                            ..info
                        };
                        let _ = SetMenuItemInfoW(menu, index as u32, true, &info);
                        break;
                    }
                }
            }
        }
        let saved = self
            .settings
            .device(key)
            .and_then(|p| p.fullscreen_boost_rate);
        let observation = self
            .polling
            .observations
            .get(key)
            .filter(|o| {
                o.target.generation == self.polling.generation
                    && o.target.device.matches_reading(&view.reading)
            })
            .or_else(|| {
                self.tray_polling.observations.get(key).filter(|o| {
                    o.target.generation == self.tray_polling.generation
                        && o.target.device.matches_reading(&view.reading)
                })
            });
        if !boost_controls_visible(&descriptor, observation, saved) {
            return Vec::new();
        }
        let rates = boost_menu_rates(&descriptor, observation);
        let mut choices = vec![(549, None)];
        choices.extend(
            rates
                .into_iter()
                .enumerate()
                .map(|(i, r)| (550 + i as u16, Some(r))),
        );
        unsafe {
            let Ok(child) = CreatePopupMenu() else {
                return Vec::new();
            };
            let child = Popup(child);
            for (id, rate) in &choices {
                let caption =
                    wide(&rate.map_or_else(|| "Off".into(), |r| format!("{} Hz", r.hz())));
                let flags = MF_STRING
                    | if saved == *rate {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    }
                    | if (rate.is_none() && saved.is_some())
                        || (self.settings.polling_controls && descriptor.online())
                    {
                        MF_ENABLED
                    } else {
                        MF_GRAYED
                    };
                if AppendMenuW(child.0, flags, *id as usize, PCWSTR(caption.as_ptr())).is_err() {
                    return Vec::new();
                }
            }
            AppendMenuW(child.0, MF_SEPARATOR, 0, None).ok();
            AppendMenuW(child.0, MF_STRING, 503, w!("Game boost settings…")).ok();
            let caption = wide(&saved.map_or_else(
                || "&Game boost\tOff".into(),
                |r| format!("&Game boost\t{} Hz", r.hz()),
            ));
            if InsertMenuW(
                menu,
                3,
                MF_BYPOSITION | MF_POPUP,
                child.0.0 as usize,
                PCWSTR(caption.as_ptr()),
            )
            .is_err()
            {
                return Vec::new();
            }
            std::mem::forget(child);
        }
        choices
    }
    pub(super) fn build_overview(&mut self) {
        if self.page != 1 {
            return;
        }
        self.overview_keys.clear();
        for (index, (device, battery)) in self
            .device_rows()
            .into_iter()
            .take(usize::from(LAST_CARD - FIRST_CARD) + 1)
            .enumerate()
        {
            let id = FIRST_CARD + index as u16;
            let caption = self.overview_caption(&device, battery.as_ref());
            let hwnd = self.control(
                id,
                w!("BUTTON"),
                &caption,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                24,
                146,
                365,
                154,
            );
            unsafe {
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_USERDATA,
                    isize::from(self.selected_device.as_ref() == Some(&device.key)),
                );
            }
            self.overview_keys.push(device.key);
        }
        for (id, text) in [
            (17, "Performance"),
            (18, "Battery"),
            (19, "Sensitivity"),
            (22, "Device options"),
            (24, "Device info"),
        ] {
            self.control(
                id,
                w!("BUTTON"),
                text,
                WS_TABSTOP | WINDOW_STYLE((BS_RADIOBUTTON | BS_PUSHLIKE) as u32),
                24,
                318,
                130,
                36,
            );
        }
        self.button(23, "More options", 24, 700, 140);
        self.button(473, "Edit settings", 24, 700, 120);
        // Native static panels are painted behind their controls, with no
        // bitmaps, animation, timers or retained graphics surfaces.
        for id in 460..=463 {
            let h = self.control(
                id,
                w!("STATIC"),
                "",
                WINDOW_STYLE(13) | WS_CLIPSIBLINGS,
                24,
                380,
                752,
                180,
            );
            unsafe {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        self.label(464, "", 24, 112, 740);
        self.label(465, "Restores your desktop rate afterwards", 24, 112, 350);
        self.label(466, "", 24, 112, 600);
        self.widget(468, "", 24, 360, 740, 1);
        if let Some((device, Some(_))) = self.current_device() {
            self.heading(467, "Low battery alert", 24, 112, 600);
            let saved = self.settings.device(&device.key).and_then(|p| p.low);
            let mut levels = vec![0, 5, 10, 15, 20, 25, 30, 50];
            if let Some(level) = saved {
                levels.push(level);
            }
            levels.sort_unstable();
            levels.dedup();
            let mut choices = vec![format!("Default ({}%)", self.settings.low)];
            choices.extend(levels.iter().map(|n| {
                if *n == 0 {
                    "Off".into()
                } else {
                    format!("{n}%")
                }
            }));
            let selected = saved
                .and_then(|n| levels.iter().position(|v| *v == n))
                .map_or(0, |i| i + 1);
            self.combo(469, &choices, selected, 24, 112, 150);
            self.check(
                470,
                "Play a sound (all devices)",
                self.settings.low_sound,
                200,
                112,
                260,
            );
            self.label(
                472,
                "Notifications are off. Enable battery alerts in Settings.",
                24,
                112,
                650,
            );
        }
        self.build_device_information();
        self.set_control_text(99, "How often your mouse sends updates.");
        self.set_control_text(48, "Game boost");
        self.set_control_text(49, if self.checked(49) { "On" } else { "Off" });
        self.set_control_text(40, "Apply");
        self.set_control_text(41, "Check rate");
        self.set_control_text(98, "Polling rate");
        self.set_control_text(5, "Save support report");
    }

    fn overview_caption(
        &self,
        device: &ConfigurationDevice,
        battery: Option<&DeviceView>,
    ) -> String {
        let kind = match device.kind.as_str() {
            "keyboard" => "keyboard",
            "mouse" => "mouse",
            _ => "device",
        };
        let name = battery
            .map(|b| b.name.clone())
            .unwrap_or_else(|| self.configuration_name(device));
        let value = battery.map_or_else(
            || {
                if device.kind == "keyboard" {
                    "USB".into()
                } else {
                    "—".into()
                }
            },
            |b| card_battery_value(&b.reading),
        );
        let status = match device.connection {
            Connection::Sleeping => "Sleeping · Last known battery",
            Connection::Stale => "Disconnected · Last known battery",
            Connection::Online if battery.is_none() && device.kind == "keyboard" => {
                "Wired keyboard · No battery"
            }
            Connection::Online if battery.is_some_and(|b| b.reading.charging == Some(true)) => {
                "Charging"
            }
            Connection::Online if device.kind == "mouse" => {
                hb_providers::device_metadata::connection_label(device)
            }
            Connection::Online => "Connected",
        };
        let rate = self
            .polling
            .observations
            .get(&device.key)
            .or_else(|| self.tooltip_rates.get(&device.key))
            .filter(|o| {
                DeviceDescriptorRef::from(&o.target.device)
                    .same_identity(DeviceDescriptorRef::from(device))
            })
            .and_then(|o| o.rate);
        let status = rate.map_or_else(
            || status.to_owned(),
            |r| format!("{} Hz · {status}", r.hz()),
        );
        format!("{kind}\n{}\n{value}\n{status}", clean_line(&name))
    }

    pub(super) fn refresh_overview_cards(&self) {
        if self.dashboard.is_none() || self.page != 1 {
            return;
        }
        let rows = self.device_rows();
        for (i, key) in self.overview_keys.iter().enumerate() {
            if let Some((device, battery)) = rows.iter().find(|(d, _)| &d.key == key) {
                self.set_control_text(
                    FIRST_CARD + i as u16,
                    &self.overview_caption(device, battery.as_ref()),
                );
            }
        }
        let connected = rows.iter().filter(|(d, _)| d.online()).count();
        self.set_control_text(
            464,
            &format!("{connected} connected · Select a device to adjust its settings"),
        );
    }

    fn overview_place(&self, id: u16, x: i32, y: i32, width: i32, height: i32, surface: bool) {
        if let Some(h) = self.controls.get(&id) {
            self.position_control(id, x, y - PAGE_TOP - self.page_scroll, width, height);
            unsafe {
                if matches!(id, 460..=463) {
                    SetWindowLongPtrW(*h, GWLP_USERDATA, 1);
                    let _ = ShowWindow(*h, SW_HIDE);
                    return;
                }
                // Use the native control's existing user-data slot, with no
                // extra allocation or property-table entry. Cards use this
                // slot for selection instead.
                if id < FIRST_CARD {
                    SetWindowLongPtrW(
                        *h,
                        GWLP_USERDATA,
                        if surface {
                            crate::dashboard_theme::PANEL_MARKER
                        } else {
                            0
                        },
                    );
                }
                let _ = SetWindowPos(
                    *h,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                let font = if matches!(id, 48 | 89 | 98 | 430 | 439 | 450 | 467 | 480 | 485 | 487) {
                    Some(self.section_font)
                } else if id >= FIRST_CARD
                    || matches!(id, 44..=47 | 52 | 96 | 99 | 434 | 435 | 447 | 464..=466 | 470 | 472 | 481 | 486 | 488 | 489)
                {
                    Some(self.small_font)
                } else {
                    None
                };
                if let Some(font) = font {
                    send(*h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(0));
                }
            }
        }
    }

    pub(super) fn arrange_overview(&mut self) {
        if self.device_section != 4 {
            self.information.clear();
        }
        if self.page != 1 || !self.controls.contains_key(&464) {
            return;
        }
        let Some(host) = self.page_host else {
            return;
        };
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.dashboard.unwrap(), &mut rect);
        }
        let dpi = unsafe { GetDpiForWindow(host).max(96) } as i32;
        let right = (rect.right * 96 / dpi - 40).max(776);
        let width = right - 24;
        // Retain native fields and their unsaved contents, but display only the
        // selected task. No hardware operation runs when a tab is selected.
        for (id, h) in &self.controls {
            if !is_fixed_control(*id) {
                unsafe {
                    if matches!(*id, 460..=463) {
                        SetWindowLongPtrW(*h, GWLP_USERDATA, 0);
                    }
                    let _ = SetWindowPos(
                        *h,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
                    );
                }
            }
        }
        self.overview_place(90, 24, 82, width, 30, false);
        self.overview_place(464, 24, 118, width, 24, false);
        let card_width = (width - 12) / 2;
        for index in 0..self.overview_keys.len() {
            self.overview_place(
                FIRST_CARD + index as u16,
                24 + (index % 2) as i32 * (card_width + 12),
                154 + (index / 2) as i32 * 174,
                card_width,
                162,
                false,
            );
        }
        let rows = self.overview_keys.len().div_ceil(2) as i32;
        let tabs = 162 + rows * 174;
        if self.overview_keys.is_empty() {
            self.overview_place(91, 24, 160, width, 48, false);
            self.page_height = 230 - PAGE_TOP;
            self.refresh_overview_cards();
            return;
        }
        let mouse = self
            .current_device()
            .is_some_and(|(d, _)| d.kind == "mouse");
        let battery = self.current_device().is_some_and(|(_, b)| b.is_some());
        let sleep = mouse
            && self
                .current_device()
                .is_some_and(|(d, _)| hb_providers::sleep_controls::model(&d).is_some());
        let battery_page = battery || sleep;
        self.set_control_text(18, if battery { "Battery" } else { "Sleep" });
        let sensitivity = mouse
            && self
                .current_device()
                .is_some_and(|(d, _)| hb_providers::dpi::candidate(&d));
        if (self.device_section == 1 && !battery_page) || (self.device_section == 2 && !sensitivity)
        {
            self.device_section = 0;
        }
        for (i, id) in [17, 18, 19].into_iter().enumerate() {
            self.overview_place(
                id,
                24 + [0, 106, 180][i],
                tabs,
                [102, 70, 100][i],
                36,
                false,
            );
            self.enable_control(
                id,
                match id {
                    18 => battery_page,
                    19 => sensitivity,
                    _ => true,
                },
            );
            unsafe {
                send(
                    self.controls[&id],
                    BM_SETCHECK,
                    WPARAM(usize::from(self.device_section == i as u16)),
                    LPARAM(0),
                );
            }
        }
        self.overview_place(22, right - 270, tabs, 142, 36, false);
        self.overview_place(24, right - 120, tabs, 120, 36, false);
        unsafe {
            send(
                self.controls[&22],
                BM_SETCHECK,
                WPARAM(usize::from(self.device_section == 3)),
                LPARAM(0),
            );
        }
        unsafe {
            send(
                self.controls[&24],
                BM_SETCHECK,
                WPARAM(usize::from(self.device_section == 4)),
                LPARAM(0),
            );
        }
        self.overview_place(468, 24, tabs + 48, width, 1, false);
        let y = tabs + 70;
        let x = 40;
        let w = width - 32;
        self.set_control_text(99, "How often your device sends updates.");
        self.set_control_text(48, "Game boost");
        self.set_control_text(49, if self.checked(49) { "On" } else { "Off" });
        self.set_control_text(40, "Apply");
        self.set_control_text(41, "Check rate");
        let mut end;
        match self.device_section {
            4 => {
                self.overview_place(460, 24, y, width, 162, false);
                for (id, dx, dy, ww, hh) in [
                    (480, 0, 16, w, 28),
                    (481, 0, 48, w, 24),
                    (483, 0, 74, w, 38),
                    (484, 0, 120, 185, 34),
                ] {
                    self.overview_place(id, x + dx, y + dy, ww, hh, true);
                }
                let lower = y + 174;
                let half = (width - 12) / 2;
                self.overview_place(461, 24, lower, half, 150, false);
                self.overview_place(462, 24 + half + 12, lower, half, 150, false);
                self.overview_place(485, x, lower + 16, half - 32, 28, true);
                self.overview_place(486, x, lower + 52, half - 32, 88, true);
                self.overview_place(487, x + half + 12, lower + 16, half - 32, 28, true);
                self.overview_place(488, x + half + 12, lower + 52, half - 32, 88, true);
                self.overview_place(489, 24, lower + 162, width, 24, false);
                end = lower + 194;
                self.render_device_information();
            }
            1 => {
                end = y;
                if self.controls.contains_key(&430) {
                    let has_sleep_controls = self.controls.contains_key(&431);
                    let height = if !has_sleep_controls {
                        108
                    } else if self.sleep.details {
                        210
                    } else {
                        152
                    };
                    self.overview_place(460, 24, end, width, height, false);
                    self.overview_place(
                        430,
                        x,
                        end + 16,
                        if has_sleep_controls { w - 180 } else { w },
                        26,
                        true,
                    );
                    if has_sleep_controls {
                        self.overview_place(431, x, end + 54, 125, 280, true);
                        self.overview_place(433, x + 133, end + 54, 64, 36, true);
                        self.set_control_text(433, "Apply");
                        self.overview_place(432, x + 205, end + 54, 140, 36, true);
                        self.set_control_text(432, "Check current");
                        self.overview_place(434, x, end + 102, w, 44, true);
                        self.set_control_text(
                            23,
                            if self.sleep.details {
                                "Fewer details"
                            } else {
                                "More details"
                            },
                        );
                        self.overview_place(23, right - 170, end + 12, 150, 34, true);
                        if self.sleep.details {
                            self.overview_place(435, x, end + 152, w, 48, true);
                        }
                    } else {
                        self.overview_place(434, x, end + 54, w, 46, true);
                    }
                    end += height + 12;
                }
                if self.controls.contains_key(&439) {
                    let editable = self.controls.contains_key(&440);
                    let height = if self.sleep.saver_details && editable {
                        294
                    } else {
                        76
                    };
                    self.overview_place(461, 24, end, width, height, false);
                    self.set_control_text(439, "Battery saver");
                    self.overview_place(439, x, end + 16, w - 220, 26, true);
                    if editable {
                        self.set_control_text(440, if self.checked(440) { "On" } else { "Off" });
                        self.overview_place(440, right - 80, end + 24, 62, 26, true);
                        self.set_control_text(
                            473,
                            if self.sleep.saver_details {
                                "Hide settings"
                            } else {
                                "Edit settings"
                            },
                        );
                        self.overview_place(473, right - 206, end + 20, 120, 34, true);
                        let configured = self.current_device().and_then(|(device, _)| {
                            self.settings
                                .device(&device.key)
                                .and_then(|p| p.low_battery_mode)
                        });
                        let summary = if !self.settings.polling_controls {
                            "Allow device changes in Settings to use Battery saver.".to_owned()
                        } else if let Some(saved) = configured.filter(|_| self.checked(440)) {
                            format!(
                                "Save power at {}% or below. Game boost takes priority.",
                                saved.threshold
                            )
                        } else if self.checked(440) {
                            "Choose settings, then select Save.".to_owned()
                        } else if self.sleep.saver_details {
                            "Turn on Battery saver to save these settings.".to_owned()
                        } else {
                            "Reduce power use when the battery is low.".to_owned()
                        };
                        self.set_control_text(466, &summary);
                        self.overview_place(466, x, end + 43, w - 220, 22, true);
                        self.enable_control(
                            446,
                            self.settings.polling_controls && self.checked(440),
                        );
                        if self.sleep.saver_details {
                            for (id, dx, dy, ww, hh) in [
                                (441, 0, 86, 220, 24),
                                (442, 240, 82, 90, 28),
                                (443, 0, 130, 230, 24),
                                (444, 240, 126, 210, 280),
                                (445, 466, 126, 210, 280),
                                (446, 0, 172, 250, 34),
                                (447, 0, 218, w, 70),
                            ] {
                                self.overview_place(id, x + dx, end + dy, ww, hh, true);
                            }
                        }
                    } else {
                        self.overview_place(447, x, end + 43, w, 30, true);
                    }
                    end += height + 12;
                }
                if self.controls.contains_key(&467) {
                    let height = if self.settings.notify { 108 } else { 140 };
                    self.overview_place(462, 24, end, width, height, false);
                    self.overview_place(467, x, end + 16, w, 26, true);
                    self.overview_place(469, x, end + 54, 160, 280, true);
                    self.overview_place(470, x + 172, end + 56, 270, 30, true);
                    if !self.settings.notify {
                        self.overview_place(472, x, end + 102, w, 24, true);
                    }
                    end += height + 12;
                }
            }
            2 => {
                self.overview_place(460, 24, y, width, 300, false);
                for (id, dx, dy, ww, hh) in [
                    (450, 0, 16, w, 28),
                    (457, 0, 54, w, 24),
                    (458, 0, 84, w, 48),
                    (451, 0, 144, 150, 28),
                    (453, 166, 144, 130, 34),
                    (452, 312, 144, 140, 34),
                    (454, 468, 144, 220, 34),
                    (455, 0, 190, w, 44),
                    (456, 0, 240, w, 48),
                ] {
                    self.overview_place(id, x + dx, y + dy, ww, hh, true);
                }
                end = y + 316;
            }
            3 => {
                self.overview_place(460, 24, y, width, 320, false);
                for (id, dx, dy, ww, hh) in [
                    (89, 0, 16, w, 28),
                    (92, 0, 65, 120, 24),
                    (11, 135, 60, 340, 28),
                    (12, 490, 60, 210, 28),
                    (94, 0, 112, 120, 24),
                    (14, 135, 108, 210, 280),
                    (93, 370, 112, 210, 24),
                    (13, 600, 108, 80, 28),
                    (96, 0, 156, w, 58),
                    (15, 0, 234, 170, 34),
                    (16, 186, 234, 180, 34),
                ] {
                    self.overview_place(id, x + dx, y + dy, ww, hh, true);
                }
                end = y + 336;
            }
            _ => {
                if !self.controls.contains_key(&98) {
                    self.set_control_text(91,"This device has no polling controls. Select Device options for its available settings.");
                    self.overview_place(91, 24, y, width, 54, false);
                    end = y + 80;
                } else {
                    let has_rate = self.controls.contains_key(&43)
                        && self.current_device().is_some_and(|(d, _)| {
                            matches!(d.capability, PollingCapability::ReadWrite)
                        });
                    let height = if self.control_details {
                        294
                    } else if has_rate {
                        142
                    } else {
                        184
                    };
                    self.overview_place(460, 24, y, width, height, false);
                    self.overview_place(98, x, y + 16, 300, 28, true);
                    self.overview_place(99, x, y + 44, w, 22, true);
                    if has_rate {
                        self.overview_place(44, right - 340, y + 20, 320, 24, true);
                        self.overview_place(43, x, y + 72, 125, 280, true);
                        self.overview_place(40, x + 133, y + 72, 64, 36, true);
                        self.overview_place(41, x + 205, y + 72, 100, 36, true);
                        self.set_control_text(
                            23,
                            if self.control_details {
                                "Fewer details"
                            } else {
                                "More options"
                            },
                        );
                        self.overview_place(23, right - 170, y + 72, 150, 34, true);
                        self.overview_place(47, x, y + 114, w, 24, true);
                        if self.control_details {
                            self.overview_place(45, x, y + 174, w, 40, true);
                            self.overview_place(46, x, y + 214, w, 24, true);
                            self.overview_place(42, x, y + 246, 225, 32, true);
                        }
                    } else {
                        if let Some((device, _)) = self.current_device()
                            && matches!(device.capability, PollingCapability::Unavailable(_))
                        {
                            self.set_control_text(44, polling_unavailable(&device));
                        }
                        self.overview_place(44, x, y + 82, w, 48, true);
                        self.overview_place(53, x, y + 138, 175, 32, true);
                    }
                    end = y + height + 16;
                    if self.controls.contains_key(&49) {
                        self.overview_place(
                            461,
                            24,
                            end,
                            width,
                            if self.control_details { 220 } else { 132 },
                            false,
                        );
                        self.overview_place(48, x, end + 16, 350, 28, true);
                        self.overview_place(49, right - 86, end + 16, 66, 26, true);
                        self.overview_place(50, x, end + 76, 125, 280, true);
                        self.overview_place(465, x + 137, end + 80, w - 137, 40, true);
                        self.overview_place(
                            52,
                            x,
                            if self.control_details {
                                end + 124
                            } else {
                                end + 44
                            },
                            w,
                            if self.control_details { 90 } else { 22 },
                            true,
                        );
                        self.set_control_text(52,if self.control_details {
                            "Uses Windows fullscreen gaming detection; some borderless games are missed.\r\nBoosts after 10 seconds; restores after 15 seconds on the desktop. Restores after restart when possible.\r\nAnti-cheat compatibility is not guaranteed."
                        } else { "Use a higher rate in fullscreen games." });
                        end += if self.control_details { 236 } else { 148 };
                    }
                }
            }
        }
        self.page_height = end - PAGE_TOP;
        self.refresh_overview_cards();
    }

    pub(super) fn save_overview_alert(&mut self) {
        let Some((device, Some(_))) = self.current_device() else {
            return;
        };
        let text = self.text(469);
        let low = if text.starts_with("Default (") {
            None
        } else if text == "Off" {
            Some(0)
        } else {
            let Some(low) = text
                .strip_suffix('%')
                .and_then(|s| s.parse::<u8>().ok())
                .filter(|v| *v <= 100)
            else {
                return;
            };
            Some(low)
        };
        self.settings.device_mut(&device.key).low = low;
        self.set_control_text(13, &low.map(|n| n.to_string()).unwrap_or_default());
        self.set_control_text(96, &device_alert_summary(&self.settings, &device.key));
        self.save();
    }

    pub(super) fn overview_select(&mut self, index: usize) {
        let Some(key) = self.overview_keys.get(index).cloned() else {
            return;
        };
        if self.selected_device.as_ref() == Some(&key) {
            return;
        }
        let information = self.device_section == 4;
        self.polling.abandon();
        self.insights.abandon();
        self.information.clear();
        self.selected_device = Some(key.clone());
        if let Some(i) = self
            .snapshot
            .devices
            .iter()
            .position(|d| d.reading.key == key)
        {
            self.selected = i;
        }
        self.device_section = if information { 4 } else { 0 };
        self.control_details = false;
        self.sleep.details = false;
        self.sleep.saver_details = false;
        self.page_scroll = 0;
        self.build();
        if information {
            self.read_device_information(false);
        } else {
            self.read_polling();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_names_cannot_inject_card_rows() {
        assert_eq!(clean_line("Mouse\r\n99%\t& test"), "Mouse  99% & test");
    }
    #[test]
    fn card_battery_values_preserve_measurement_precision() {
        let mut reading = Reading::new("mouse", "Mouse", "simulation", 0);
        assert_eq!(card_battery_value(&reading), "—");
        reading.level = Some(50);
        assert_eq!(card_battery_value(&reading), "50%");
        for precision in [Precision::Coarse, Precision::Estimated] {
            reading.precision = precision;
            assert_eq!(card_battery_value(&reading), "~50%");
        }
    }
    #[test]
    fn boost_menu_respects_confirmed_connection_rates_and_catalog_allowlist() {
        let mut reading = Reading::new("razer:00bf:UNITTEST", "Mouse", "razer", 0);
        reading.kind = "mouse".into();
        reading.via = "usb".into();
        let device = ConfigurationDevice::from_reading(&reading);
        assert_eq!(
            boost_menu_rates(&device, None)
                .iter()
                .map(|r| r.hz())
                .collect::<Vec<_>>(),
            [2000, 4000, 8000]
        );
        let mut observation = PollingObservation {
            target: ControlTarget {
                device: device.clone(),
                generation: 1,
                connection_stamp: None,
            },
            supported: [250, 1000, 2000]
                .into_iter()
                .map(|hz| PollingRate::try_from(hz).unwrap())
                .collect(),
            rate: PollingRate::try_from(1000).ok(),
            timestamp: 0,
            evidence: String::new(),
        };
        assert_eq!(
            boost_menu_rates(&device, Some(&observation))
                .iter()
                .map(|r| r.hz())
                .collect::<Vec<_>>(),
            [2000]
        );
        observation.supported.retain(|r| r.hz() <= 1000);
        assert!(boost_menu_rates(&device, Some(&observation)).is_empty());
        observation.supported.clear();
        assert!(boost_menu_rates(&device, Some(&observation)).is_empty());
    }
}
