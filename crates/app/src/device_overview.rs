//! Presentation-only device overview. Existing controls retain their commands,
//! drafts and validation. Card resources exist only while the dashboard is open.
use super::*;
use std::borrow::Cow;

const FIRST_CARD: u16 = 1000;
const LAST_CARD: u16 = 32767;
pub(super) const BATTERY_SECTION: u16 = 1;
pub(super) const INFO_SECTION: u16 = 4;
pub(super) const CUSTOM_DPI: &str = "Custom DPI…";
pub(super) const WORKSPACE_FOOTER: i32 = 34;

fn acceleration_status(enabled: Option<bool>) -> &'static str {
    match enabled {
        Some(true) => "On · Enhance pointer precision is enabled",
        Some(false) => "Off · Enhance pointer precision is disabled",
        None => "Unable to check · Select Refresh to try again",
    }
}

pub(super) fn number(value: u32) -> String {
    if value >= 1000 {
        format!("{},{:03}", value / 1000, value % 1000)
    } else {
        value.to_string()
    }
}

fn clean_line(value: &str) -> Cow<'_, str> {
    if value.chars().any(char::is_control) {
        Cow::Owned(
            value
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        )
    } else {
        Cow::Borrowed(value)
    }
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
        let mut rows = self
            .borrowed_device_rows()
            .filter(|(device, battery)| battery.is_none() && Some(device.key) != key)
            .take(usize::from(u16::MAX - 33000))
            .peekable();
        if rows.peek().is_none() {
            return Vec::new();
        }
        let mut choices = Vec::new();
        unsafe {
            let existing = named_submenu(menu, "&Switch device");
            let child = if let Some(existing) = existing {
                existing
            } else {
                let Ok(child) = CreatePopupMenu() else {
                    return Vec::new();
                };
                let owned = Popup(child);
                let before = if GetMenuState(menu, 503, MF_BYCOMMAND) == u32::MAX {
                    500
                } else {
                    503
                };
                if InsertMenuW(
                    menu,
                    before,
                    MF_BYCOMMAND | MF_POPUP,
                    child.0 as usize,
                    w!("&Switch device"),
                )
                .is_err()
                {
                    return Vec::new();
                }
                std::mem::forget(owned);
                child
            };
            for (index, (device, _)) in rows.enumerate() {
                let id = 33000 + index as u16;
                let text = wide_menu_data(&clean_line(self.workspace_name(device, None)));
                if AppendMenuW(child, MF_STRING, usize::from(id), PCWSTR(text.as_ptr())).is_ok() {
                    choices.push((id, device.key.to_owned()));
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
        let connection =
            match Self::workspace_connection(DeviceDescriptorRef::from(&descriptor), Some(view)) {
                "Wireless receiver" => "Wireless",
                other => other,
            };
        let detail = if let Some(rate) = rate {
            format!("{connection} · {} Hz", number(rate.hz()))
        } else if !view.reading.online() {
            format!("{connection} · Last known battery")
        } else {
            connection.to_owned()
        };
        let caption = wide_menu_data(&format!(
            "{}\t{}\n{detail}",
            clean_line(self.workspace_name(DeviceDescriptorRef::from(&descriptor), Some(view))),
            card_battery_value(&view.reading)
        ));
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
                        let label = wide(&format!("&Polling rate\t{} Hz", number(rate.hz())));
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
            let Some(options) = named_submenu(menu, "&More options") else {
                return Vec::new();
            };
            if InsertMenuW(
                options,
                0,
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
        self.overview_visibility = None;
        self.overview_keys.clear();
        self.heading(90, "Devices", 24, 64, 152);
        let mut devices = self.device_rows();
        devices.sort_by_key(|(device, _)| match device.kind.as_str() {
            "mouse" => 0,
            "keyboard" => 1,
            _ => 2,
        });
        for (index, (device, battery)) in devices
            .into_iter()
            .take(usize::from(LAST_CARD - FIRST_CARD) + 1)
            .enumerate()
        {
            let id = FIRST_CARD + index as u16;
            let caption =
                self.overview_caption(DeviceDescriptorRef::from(&device), battery.as_ref());
            let hwnd = self.control(
                id,
                w!("BUTTON"),
                &caption,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                20,
                96,
                156,
                60,
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
        self.button(496, "Save support report", 190, 148, 156);
        if self.overview_keys.is_empty() {
            self.label(
                91,
                "No devices found. Connect or wake a device, then select Refresh.",
                190,
                96,
                500,
            );
            return;
        }
        self.heading(88, "", 190, 68, 420);
        if self
            .current_device()
            .is_some_and(|(d, _)| d.kind == "mouse")
        {
            self.heading(497, "Windows mouse acceleration", 190, 112, 400);
            self.control(
                498,
                w!("STATIC"),
                acceleration_status(hb_windows::system::mouse_acceleration().ok()),
                WINDOW_STYLE::default(),
                190,
                140,
                400,
                42,
            );
            self.button(499, "Open mouse settings", 600, 112, 156);
            self.control(
                478,
                w!("STATIC"),
                "Windows setting for all mice. Games may handle mouse movement differently.",
                WINDOW_STYLE::default(),
                190,
                184,
                560,
                36,
            );
        }
        self.label(464, "", 190, 102, 420);
        self.widget(476, "", 660, 64, 94, 56);
        self.button(24, "Device information", 190, 443, 400);
        for id in [460, 490, 491, 492, 493, 494, 495] {
            self.widget(id, "", 190, 136, 568, 1);
        }
        self.label(466, "", 190, 112, 568);
        if let Some((device, Some(_))) = self.current_device() {
            self.button(18, "Battery and alerts", 190, 404, 400);
            self.label(477, "", 190, 112, 568);
            self.heading(467, "Low battery alert", 190, 112, 420);
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
            self.combo(469, &choices, selected, 190, 112, 150);
            self.check(
                470,
                "Play a sound (all devices)",
                self.settings.low_sound,
                350,
                112,
                260,
            );
            self.label(
                472,
                "Battery alerts are turned off in Settings.",
                190,
                112,
                500,
            );
        }
        self.button(473, "Edit settings", 190, 112, 120);
        self.build_device_information();
        self.refresh_overview_cards();
        self.refresh_workspace_freshness();
        self.render_device_information();
    }

    pub(super) fn refresh_mouse_acceleration(&self) {
        if self.dashboard_visible() && self.page == 1 && self.controls.contains_key(&498) {
            self.set_control_text(
                498,
                acceleration_status(hb_windows::system::mouse_acceleration().ok()),
            );
        }
    }

    fn workspace_name<'a>(
        &'a self,
        device: DeviceDescriptorRef<'a>,
        battery: Option<&'a DeviceView>,
    ) -> &'a str {
        if let Some(name) = self
            .settings
            .device(device.key)
            .and_then(|p| p.name.as_deref())
        {
            return name;
        }
        let name = battery.map_or(device.name, |view| view.name.as_str());
        let brand = provider_label(device.source);
        name.strip_prefix(brand)
            .and_then(|name| name.strip_prefix(' '))
            .filter(|name| !name.is_empty())
            .unwrap_or(name)
    }

    fn workspace_connection(
        device: DeviceDescriptorRef<'_>,
        battery: Option<&DeviceView>,
    ) -> &'static str {
        match *device.connection {
            Connection::Sleeping => "Sleeping",
            Connection::Stale => "Disconnected",
            Connection::Online if battery.is_some_and(|b| b.reading.charging == Some(true)) => {
                "Charging"
            }
            Connection::Online if battery.is_none() && device.kind == "keyboard" => "USB powered",
            Connection::Online => match hb_providers::device_metadata::connection_label_parts(
                device.key,
                device.kind,
                device.source,
                device.via,
            ) {
                "Wireless · USB receiver" => "Wireless receiver",
                "Device connection" => "Connected",
                label => label,
            },
        }
    }

    fn overview_caption(
        &self,
        device: DeviceDescriptorRef<'_>,
        battery: Option<&DeviceView>,
    ) -> String {
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
        format!(
            "{}\n{}\n{value}\n{}",
            clean_line(device.kind),
            clean_line(self.workspace_name(device, battery)),
            Self::workspace_connection(device, battery)
        )
    }

    pub(super) fn refresh_overview_cards(&self) -> bool {
        if self.dashboard.is_none() || self.page != 1 {
            return false;
        }
        let mut reflow = false;
        for (device, battery) in self.borrowed_device_rows() {
            if let Some(i) = self.overview_keys.iter().position(|key| key == device.key) {
                let id = FIRST_CARD + i as u16;
                let caption = self.overview_caption(device, battery);
                let hwnd = self.controls[&id];
                if !control_text_matches(hwnd, &caption) {
                    let previous = self.text(id);
                    // Battery values do not change row height. Only remeasure
                    // when the name or connection label changes its wrapping.
                    if previous.lines().nth(1) != caption.lines().nth(1)
                        || previous.lines().nth(3) != caption.lines().nth(3)
                    {
                        let height =
                            crate::dashboard_widgets::roster_height(hwnd, &caption, self.font);
                        let mut bounds = RECT::default();
                        unsafe {
                            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                            if GetClientRect(hwnd, &mut bounds).is_ok() {
                                reflow |= bounds.bottom != height * dpi / 96;
                            }
                        }
                    }
                    let text = wide(&caption);
                    unsafe {
                        let _ = SetWindowTextW(hwnd, PCWSTR(text.as_ptr()));
                    }
                }
            }
            if Some(device.key) != self.selected_device.as_deref() {
                continue;
            }
            self.set_control_text(88, &clean_line(self.workspace_name(device, battery)));
            self.set_control_text(
                464,
                &format!(
                    "{} · {}",
                    provider_label(device.source),
                    Self::workspace_connection(device, battery)
                ),
            );
            if let Some(battery) = battery {
                let estimate = if !battery.reading.online() {
                    "Last known battery".into()
                } else if battery.reading.charging == Some(true) {
                    "Charging".into()
                } else if let Some(seconds) = battery.seconds_left {
                    if seconds >= 3600 {
                        format!("About {} h left", (seconds / 3600).max(1))
                    } else {
                        format!("About {} min left", (seconds / 60).max(1))
                    }
                } else {
                    "Battery level".into()
                };
                self.set_control_text(
                    476,
                    &format!("{}\n{estimate}", card_battery_value(&battery.reading)),
                );
            } else {
                let level = if device.kind == "keyboard" {
                    "USB\nNo battery"
                } else {
                    "—\nNo battery reading"
                };
                self.set_control_text(476, level);
            }
        }
        reflow
    }

    pub(super) fn refresh_workspace_freshness(&self) {
        if self.page == 1
            && self.workspace_sections & BATTERY_SECTION != 0
            && let (_, Some(battery)) = selected_device_sources(
                &self.snapshot.devices,
                &self.configuration_devices,
                self.selected_device.as_deref(),
            )
        {
            self.set_control_text(477, &device_detail(battery));
        }
    }

    fn overview_place(&self, id: u16, x: i32, y: i32, width: i32, height: i32, surface: bool) {
        let Some(&hwnd) = self.controls.get(&id) else {
            return;
        };
        self.position_control(id, x, y - PAGE_TOP - self.page_scroll, width.max(1), height);
        unsafe {
            if id == 460 {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1);
                let _ = ShowWindow(hwnd, SW_HIDE);
                return;
            }
            if id < FIRST_CARD {
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_USERDATA,
                    if surface {
                        crate::dashboard_theme::PANEL_MARKER
                    } else {
                        0
                    },
                );
            }
            if GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE.0 == 0 {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let font = match id {
                88 => self.heading_font,
                18 | 24 | 48 | 89 | 90 | 98 | 430 | 439 | 450 | 467 | 485 | 487 | 497 => {
                    self.section_font
                }
                44..=47
                | 52
                | 96
                | 434
                | 435
                | 447
                | 457
                | 458
                | 464..=466
                | 472
                | 475
                | 476
                | 477
                | 478
                | 498
                | 481
                | 486
                | 488
                | 489 => self.small_font,
                _ => self.font,
            };
            if send(hwnd, WM_GETFONT, WPARAM(0), LPARAM(0)).0 != font.0 as isize {
                send(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(0));
            }
        }
    }

    pub(super) fn arrange_overview(&mut self) {
        if self.page != 1 || !self.controls.contains_key(&90) {
            return;
        }
        let Some(host) = self.page_host else { return };
        let dpi = unsafe { GetDpiForWindow(host).max(96) } as i32;
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.dashboard.unwrap(), &mut rect);
        }
        let left = 190;
        let right = (rect.right * 96 / dpi - 20).max(left + 450);
        let width = right - left;
        let inside = left + 12;
        let edge = right - 12;
        let inner_width = edge - inside;
        let dpi_custom = self.controls.contains_key(&474) && self.text(474) == CUSTOM_DPI;
        let visibility = self.workspace_sections
            | (u16::from(self.sleep.saver_details) << 4)
            | (u16::from(self.settings.notify) << 5)
            | (u16::from(dpi_custom) << 6);
        if self.overview_visibility != Some(visibility) {
            for (id, h) in &self.controls {
                // Disclosure headers and the DPI selector stay focused while
                // their content expands or collapses.
                if !is_fixed_control(*id) && *id < FIRST_CARD && !matches!(*id, 18 | 24 | 474) {
                    unsafe {
                        if *id == 460 {
                            SetWindowLongPtrW(*h, GWLP_USERDATA, 0);
                        }
                        let _ = SetWindowPos(
                            *h,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOZORDER
                                | SWP_NOACTIVATE
                                | SWP_HIDEWINDOW,
                        );
                    }
                }
            }
            self.overview_visibility = Some(visibility);
        }
        self.overview_place(90, 24, 65, 152, 23, false);
        if self.overview_keys.is_empty() {
            self.overview_place(91, left, 74, width, 46, false);
            self.overview_place(496, left, 126, 156, 33, false);
            self.page_height = 180 - PAGE_TOP;
            return;
        }
        let mut roster_y = 95;
        for index in 0..self.overview_keys.len() {
            let id = FIRST_CARD + index as u16;
            let height = crate::dashboard_widgets::roster_height(
                self.controls[&id],
                &self.text(id),
                self.font,
            );
            self.overview_place(id, 20, roster_y, 156, height, false);
            roster_y += height + 5;
        }
        self.overview_place(88, left, 67, width - 152, 31, false);
        // Keep the actual device name and connection on one line each.
        self.overview_place(464, left, 100, width - 152, 24, false);
        self.overview_place(476, right - 94, 64, 94, 60, false);
        let panel_y = 136;
        let mut y = panel_y;
        let mut rule = 490;
        let divider = |state: &Self, rule: &mut u16, y| {
            state.overview_place(*rule, inside, y, inner_width, 1, true);
            *rule += 1;
        };
        if self.controls.contains_key(&98) {
            let editable = self.controls.contains_key(&43)
                && self
                    .current_device()
                    .is_some_and(|(d, _)| matches!(d.capability, PollingCapability::ReadWrite));
            self.overview_place(98, inside, y + 14, inner_width - 165, 22, true);
            if editable {
                self.overview_place(44, inside, y + 38, inner_width - 165, 22, true);
                self.overview_place(43, edge - 152, y + 17, 90, 280, true);
                self.overview_place(40, edge - 56, y + 17, 56, 33, true);
                self.set_control_text(40, "Apply");
                y += 67;
            } else {
                self.overview_place(44, inside, y + 38, inner_width - 117, 50, true);
                self.overview_place(53, edge - 107, y + 17, 107, 33, true);
                y += 91;
            }
            divider(self, &mut rule, y);
        }
        if self.controls.contains_key(&450) {
            self.overview_place(450, inside, y + 14, inner_width - 165, 22, true);
            self.overview_place(457, inside, y + 38, inner_width - 165, 23, true);
            self.overview_place(474, edge - 156, y + 17, 94, 280, true);
            let dpi_action = if self.controls.contains_key(&453) {
                453
            } else {
                452
            };
            self.set_control_text(dpi_action, if dpi_action == 453 { "Apply" } else { "Read" });
            self.overview_place(dpi_action, edge - 56, y + 17, 56, 33, true);
            y += 67;
            if dpi_custom {
                self.overview_place(458, inside, y + 2, inner_width - 113, 42, true);
                self.overview_place(451, edge - 94, y + 2, 94, 27, true);
                y += 50;
            }
            divider(self, &mut rule, y);
        }
        if self.controls.contains_key(&497) {
            self.overview_place(497, inside, y + 14, inner_width - 172, 22, true);
            self.overview_place(498, inside, y + 38, inner_width - 172, 40, true);
            self.overview_place(499, edge - 160, y + 17, 160, 33, true);
            self.overview_place(478, inside, y + 79, inner_width, 36, true);
            y += 122;
            divider(self, &mut rule, y);
        }
        if self.controls.contains_key(&430) {
            let editable = self.controls.contains_key(&431);
            self.overview_place(430, inside, y + 14, inner_width - 172, 22, true);
            self.overview_place(
                if editable { 475 } else { 434 },
                inside,
                y + 38,
                if editable {
                    inner_width - 172
                } else {
                    inner_width
                },
                if editable { 22 } else { 45 },
                true,
            );
            if editable {
                self.overview_place(431, edge - 165, y + 17, 103, 280, true);
                self.overview_place(433, edge - 56, y + 17, 56, 33, true);
                self.set_control_text(433, "Apply");
            }
            y += if editable { 67 } else { 84 };
            divider(self, &mut rule, y);
        }
        if self.controls.contains_key(&49) {
            self.set_control_text(48, "Fullscreen boost");
            self.set_control_text(49, if self.checked(49) { "On" } else { "Off" });
            self.set_control_text(
                465,
                if self.checked(49) {
                    "Restore your desktop rate after the game"
                } else {
                    "Use a higher rate in fullscreen games"
                },
            );
            self.overview_place(48, inside, y + 14, inner_width - 145, 22, true);
            self.overview_place(465, inside, y + 38, inner_width - 145, 22, true);
            self.overview_place(50, edge - 136, y + 17, 90, 280, true);
            self.overview_place(49, edge - 44, y + 17, 44, 33, true);
            y += 66;
            divider(self, &mut rule, y);
        }
        if y == panel_y {
            self.set_control_text(91, "No hardware controls are available for this device.");
            self.overview_place(91, inside, y + 17, inner_width, 35, true);
            y += 66;
            divider(self, &mut rule, y);
        }
        if self.controls.contains_key(&469) {
            let open = self.workspace_sections & BATTERY_SECTION != 0;
            self.set_control_text(
                18,
                if open {
                    "▾ Battery and alerts"
                } else {
                    "▸ Battery and alerts"
                },
            );
            self.overview_place(18, inside, y + 1, inner_width, 37, true);
            y += 39;
            if open {
                y = self.arrange_workspace_battery(inside, edge, y);
            }
            divider(self, &mut rule, y);
        }
        let info = self.workspace_sections & INFO_SECTION != 0;
        self.set_control_text(
            24,
            if info {
                "▾ Device information"
            } else {
                "▸ Device information"
            },
        );
        self.overview_place(24, inside, y + 1, inner_width, 37, true);
        y += 39;
        if info {
            y = self.arrange_workspace_information(inside, edge, y);
        }
        self.overview_place(460, left, panel_y, width, y - panel_y, false);
        if self.controls.contains_key(&42) {
            self.overview_place(42, left + 10, y + 1, 165, 32, false);
            y += 33;
        }
        self.page_height = (y + 20).max(roster_y + 15) - PAGE_TOP;
    }

    fn arrange_workspace_battery(&self, x: i32, right: i32, mut y: i32) -> i32 {
        let width = right - x;
        self.overview_place(477, x, y + 2, width, 30, true);
        y += 34;
        if self.controls.contains_key(&439) {
            self.set_control_text(439, "Battery saver");
            self.overview_place(439, x, y + 6, width - 186, 22, true);
            if self.controls.contains_key(&440) {
                self.set_control_text(440, if self.checked(440) { "On" } else { "Off" });
                self.overview_place(440, right - 53, y + 8, 54, 30, true);
                self.set_control_text(
                    473,
                    if self.sleep.saver_details {
                        "Hide settings"
                    } else {
                        "Edit settings"
                    },
                );
                self.overview_place(473, right - 177, y + 7, 115, 32, true);
                let saved = self
                    .current_device()
                    .and_then(|(d, _)| self.settings.device(&d.key)?.low_battery_mode);
                let summary = if !self.settings.polling_controls {
                    "Allow device settings changes in Settings.".to_owned()
                } else if let Some(saved) = saved.filter(|_| self.checked(440)) {
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
                self.overview_place(466, x, y + 31, width - 186, 33, true);
                self.enable_control(446, self.settings.polling_controls && self.checked(440));
                if self.sleep.saver_details {
                    for (id, dx, dy, w, h) in [
                        (441, 0, 76, 200, 24),
                        (442, 200, 72, 80, 28),
                        (443, 0, 119, 200, 24),
                        (444, 200, 113, 145, 280),
                        (445, 353, 113, width - 353, 280),
                        (446, 0, 159, 188, 33),
                        (447, 0, 205, width, 56),
                    ] {
                        self.overview_place(id, x + dx, y + dy, w, h, true);
                    }
                    y += 203;
                }
            } else {
                self.overview_place(447, x, y + 31, width, 35, true);
            }
            y += 69;
        }
        self.overview_place(467, x, y + 8, width - 330, 22, true);
        self.overview_place(469, right - 325, y + 4, 126, 280, true);
        self.overview_place(470, right - 188, y + 4, 189, 33, true);
        if self.settings.notify {
            y + 50
        } else {
            self.overview_place(472, x, y + 46, width, 22, true);
            y + 75
        }
    }

    fn arrange_workspace_information(&self, x: i32, right: i32, mut y: i32) -> i32 {
        let w = right - x;
        let half = (w - 16) / 2;
        let height = |id, width, minimum| {
            self.controls.get(&id).map_or(minimum, |h| {
                crate::dashboard_widgets::label_height(*h, &self.text(id), width, self.small_font)
                    .max(minimum)
            })
        };
        let firmware_height = height(483, w, 44);
        let details_y = 126 + firmware_height - 44;
        let details_height = height(486, half, 94).max(height(488, half, 94));
        let checked_y = details_y + 28 + details_height + 3;
        let checked_height = height(489, w, 40);
        // Connection and firmware are read on demand, followed by the controls
        // that are useful when checking or reverting a device setting.
        for (id, dx, dy, width, height) in [
            (481, 0, 3, w, 24),
            (483, 0, 28, w, firmware_height),
            (484, 0, 32 + firmware_height, 165, 33),
            (485, 0, details_y, half, 24),
            (486, 0, details_y + 28, half, details_height),
            (487, half + 16, details_y, half, 24),
            (488, half + 16, details_y + 28, half, details_height),
            (489, 0, checked_y, w, checked_height),
        ] {
            self.overview_place(id, x + dx, y + dy, width, height, true);
        }
        y += checked_y + checked_height + 6;
        if self.controls.contains_key(&41) {
            self.set_control_text(41, "Check polling rate");
            self.overview_place(41, x, y, 144, 33, true);
            self.overview_place(45, x, y + 43, w, 35, true);
            self.overview_place(46, x, y + 79, w, 28, true);
            self.overview_place(47, x, y + 109, w, 32, true);
            y += 151;
        }
        if self.controls.contains_key(&452) {
            if self.controls.contains_key(&453) {
                self.set_control_text(452, "Check DPI");
                self.overview_place(452, x, y, 100, 33, true);
                self.overview_place(454, x + 109, y, 170, 33, true);
            }
            if self.text(474) != CUSTOM_DPI {
                self.overview_place(458, x, y + 42, w, 36, true);
            }
            self.overview_place(455, x, y + 80, w, 35, true);
            self.overview_place(456, x, y + 119, w, 38, true);
            y += 164;
        }
        if self.controls.contains_key(&432) {
            self.set_control_text(432, "Check current");
            self.overview_place(432, x, y, 126, 33, true);
            self.overview_place(434, x + 137, y + 1, w - 137, 47, true);
            self.overview_place(435, x, y + 55, w, 44, true);
            y += 107;
        }
        if self.controls.contains_key(&52) {
            self.overview_place(52, x, y, w, 61, true);
            y += 68;
        }
        for (id, dx, dy, width, height) in [
            (89, 0, 0, w, 25),
            (92, 0, 40, 108, 24),
            (11, 112, 35, w - 253, 29),
            (12, w - 133, 35, 133, 29),
            (94, 0, 82, 108, 24),
            (14, 112, 76, 160, 280),
            (93, 282, 82, w - 346, 24),
            (13, w - 60, 76, 60, 29),
            (96, 0, 120, w, 36),
            (15, 0, 165, 120, 33),
            (16, 129, 165, 145, 33),
            (496, 283, 165, 159, 33),
        ] {
            self.overview_place(id, x + dx, y + dy, width, height, true);
        }
        y + 211
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
        let information = self.workspace_sections & INFO_SECTION != 0;
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
        self.workspace_sections &= INFO_SECTION;
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
    fn mouse_acceleration_status_distinguishes_on_off_and_failed_reads() {
        assert!(acceleration_status(Some(true)).starts_with("On ·"));
        assert!(acceleration_status(Some(false)).starts_with("Off ·"));
        assert!(acceleration_status(None).starts_with("Unable to check"));
    }
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
