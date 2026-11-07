//! Settings use the same lightweight native cards as Devices. The page owns
//! only controls; it adds no timers, images or retained rendering surfaces.
use super::*;

pub(super) const SETTINGS_HEIGHT: i32 = 930;
pub(super) const ADVANCED_HEIGHT: i32 = 1490;

impl State {
    pub(super) fn build_settings(&mut self) {
        let advanced_y = |y| y + settings_provider_tail(providers().len());
        let value = serde_json::to_value(&self.settings).unwrap_or_default();
        self.heading(221, "Settings", 20, 76, PAGE_WIDTH);
        self.label(
            222,
            "Make Halo Continued work your way. Select Save settings to keep changes.",
            20,
            112,
            PAGE_WIDTH,
        );
        for (id, y, height) in [
            (460, 148, 224),
            (461, 388, 196),
            (462, 600, 100),
            (463, 716, 144),
        ] {
            let panel = self.control(
                id,
                w!("STATIC"),
                "",
                WINDOW_STYLE(13),
                20,
                y,
                PAGE_WIDTH,
                height,
            );
            unsafe {
                SetWindowLongPtrW(panel, GWLP_USERDATA, 1);
                let _ = ShowWindow(panel, SW_HIDE);
            }
        }
        for (id, caption, y) in [
            (90, "Battery alerts", 164),
            (91, "Tray icons and startup", 404),
            (92, "Battery monitoring", 616),
            (93, "Device controls", 732),
        ] {
            self.heading(id, caption, 36, y, PAGE_WIDTH - 32);
            unsafe {
                send(
                    self.controls[&id],
                    WM_SETFONT,
                    WPARAM(self.section_font.0 as usize),
                    LPARAM(0),
                );
            }
        }
        for (i, x, y, width) in [
            (0, 36, 198, 344),
            (1, 416, 198, 326),
            (6, 36, 232, 344),
            (8, 416, 232, 326),
            (12, 36, 266, PAGE_WIDTH - 32),
            (3, 36, 438, 344),
            (4, 416, 438, 326),
            (7, 36, 472, 344),
            (5, 416, 472, 326),
            (13, 36, 544, PAGE_WIDTH - 32),
            (2, 416, 652, 326),
            (9, 20, advanced_y(1196), PAGE_WIDTH),
            (10, 20, advanced_y(1230), PAGE_WIDTH),
            (11, 20, advanced_y(1306), PAGE_WIDTH),
        ] {
            let (key, label) = CHECKS[i];
            self.check(
                setting_check_id(i),
                label,
                if key == "bluetooth" {
                    self.settings.enabled("bluetooth")
                } else {
                    value[key].as_bool().unwrap_or(false)
                },
                x,
                y,
                width,
            );
        }
        self.enable_control(111, false);
        self.label(202, "Low battery alert (%)", 36, 308, 180);
        self.edit(203, &self.settings.low.to_string(), 226, 304, 80);
        self.label(214, "Orange warning (%)", 416, 308, 210);
        self.edit(215, &self.settings.warning_level.to_string(), 650, 304, 80);
        self.label(
            220,
            "Use 0 to turn a level off. A device's own alert level takes priority.",
            36,
            340,
            PAGE_WIDTH - 32,
        );
        self.label(204, "Icon colour", 36, 512, 120);
        let themes = THEME_CHOICES
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>();
        let chosen = ["auto", "white", "black", "windows", "topbar"]
            .iter()
            .position(|s| *s == self.settings.icon_theme)
            .unwrap_or(0);
        self.combo(205, &themes, chosen, 166, 508, 214);
        let start_at_sign_in = *self
            .startup_initial
            .get_or_insert_with(hb_windows::system::is_startup);
        self.check(206, "Start at sign-in", start_at_sign_in, 416, 508, 326);
        self.label(200, "Check every (seconds)", 36, 656, 210);
        self.edit(201, &self.settings.interval.to_string(), 270, 652, 100);
        self.check(
            112,
            "Allow device settings changes",
            self.settings.polling_controls,
            36,
            766,
            344,
        );
        self.check(
            113,
            "Apply saved rates at startup",
            self.settings.restore_polling_on_startup,
            416,
            766,
            326,
        );
        self.control(216, w!("STATIC"), "Change polling rate, sensitivity (DPI) and sleep time for supported devices on Devices.", WINDOW_STYLE::default(), 36, 806, PAGE_WIDTH - 32, 40);
        self.heading(207, "Device brands to monitor", 20, 932, PAGE_WIDTH);
        for (i, p) in providers().iter().enumerate() {
            self.check(
                300 + i as u16,
                provider_label(p),
                self.settings.enabled(p),
                20 + (i as i32 % 4) * 184,
                966 + (i as i32 / 4) * 28,
                180,
            );
        }
        self.button(
            212,
            if self.more_options {
                "Hide advanced options"
            } else {
                "Advanced options"
            },
            20,
            884,
            200,
        );
        self.control(
            213,
            w!("STATIC"),
            "Extra PlayStation checks are optional and may affect other controller apps.",
            WINDOW_STYLE::default(),
            20,
            advanced_y(1262),
            PAGE_WIDTH,
            40,
        );
        self.heading(
            208,
            "About Halo Continued",
            20,
            advanced_y(1340),
            PAGE_WIDTH,
        );
        self.label(
            218,
            concat!("Version ", env!("CARGO_PKG_VERSION"), " · Windows 11"),
            20,
            advanced_y(1380),
            440,
        );
        self.button(217, "Open data folder", 548, advanced_y(1374), 210);
        self.edit(
            209,
            self.settings
                .release_repository
                .clone()
                .as_deref()
                .unwrap_or(""),
            20,
            advanced_y(1416),
            PAGE_WIDTH,
        );
        self.label(
            219,
            "Closing this window keeps monitoring active. To quit, choose Exit in the tray menu.",
            20,
            advanced_y(1440),
            PAGE_WIDTH,
        );
        self.button(210, "&Save settings", 20, 682, 160);
        self.button(5, "Save support report", 194, 682, 205);
        let message = user_message(&self.error).to_owned();
        self.control(
            95,
            w!("STATIC"),
            &message,
            WINDOW_STYLE::default(),
            415,
            732,
            365,
            46,
        );
        // Mark controls inside cards so text/checkbox backgrounds match the
        // panel. The four hidden anchors are painted by the existing page host.
        for id in [
            90, 91, 92, 93, 100, 101, 102, 103, 104, 105, 106, 107, 108, 112, 113, 114, 115, 200,
            201, 202, 203, 204, 205, 206, 214, 215, 216, 220,
        ] {
            if let Some(control) = self.controls.get(&id) {
                unsafe {
                    SetWindowLongPtrW(
                        *control,
                        GWLP_USERDATA,
                        crate::dashboard_theme::PANEL_MARKER,
                    );
                }
            }
        }
        self.update_more_options();
    }
}
