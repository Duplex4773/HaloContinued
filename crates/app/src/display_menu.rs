//! Per-display tray controls. Inventory and native menu objects live only while
//! the menu is open; writes use the application's existing bounded I/O pool.
use super::*;
use hb_windows::displays::{Display, HdrOutcome, HdrRequest, HdrState};

const FIRST_DISPLAY: u16 = 24_000;
pub(super) struct DisplayMenuChoice {
    pub(super) id: u16,
    text: String,
    pub(super) enabled: bool,
    checked: bool,
    pub(super) request: Option<HdrRequest>,
}
impl DisplayMenuChoice {
    fn information(text: impl Into<String>) -> Self {
        Self {
            id: 0,
            text: text.into(),
            enabled: false,
            checked: false,
            request: None,
        }
    }
}
fn choices(inventory: Result<Vec<Display>, ProviderError>) -> Vec<DisplayMenuChoice> {
    match inventory {
        Err(_) => vec![DisplayMenuChoice::information(
            "Windows couldn't read the displays. Reopen this menu to try again.",
        )],
        Ok(displays) if displays.is_empty() => {
            vec![DisplayMenuChoice::information("No connected displays")]
        }
        Ok(displays) => displays
            .into_iter()
            .take(hb_windows::displays::MAX_DISPLAYS)
            .enumerate()
            .map(|(index, display)| {
                let (state, checked) = match display.hdr {
                    HdrState::Available { enabled, .. } => {
                        (if enabled { "HDR on" } else { "HDR off" }, enabled)
                    }
                    HdrState::Unsupported => ("HDR not supported", false),
                    HdrState::Unavailable(reason) => (reason, false),
                };
                let request = display.hdr_request();
                DisplayMenuChoice {
                    id: FIRST_DISPLAY + index as u16,
                    text: format!("{} · {state}", display.label()),
                    enabled: request.is_some(),
                    checked,
                    request,
                }
            })
            .collect(),
    }
}
fn append(menu: HMENU, choices: &[DisplayMenuChoice]) -> windows::core::Result<()> {
    unsafe {
        let child = Popup(CreatePopupMenu()?);
        for choice in choices {
            let label = wide_menu_data(&choice.text);
            let mut flags = MF_STRING;
            if !choice.enabled {
                flags |= MF_GRAYED;
            }
            if choice.checked {
                flags |= MF_CHECKED;
            }
            AppendMenuW(child.0, flags, choice.id as usize, PCWSTR(label.as_ptr()))?;
        }
        InsertMenuW(
            menu,
            509,
            MF_BYCOMMAND | MF_POPUP,
            child.0.0 as usize,
            w!("&Displays"),
        )?;
        std::mem::forget(child);
    }
    Ok(())
}
impl State {
    pub(super) fn append_display_menu(&self, menu: HMENU) -> Vec<DisplayMenuChoice> {
        let choices = if self.hdr_pending {
            vec![DisplayMenuChoice::information("Changing HDR…")]
        } else {
            // Query Windows on opening only: no polling or retained inventory.
            choices(hb_windows::displays::enumerate())
        };
        if append(menu, &choices).is_err() {
            return Vec::new();
        }
        choices
    }
    pub(super) fn change_hdr(&mut self, request: HdrRequest) {
        if self.hdr_pending {
            return;
        }
        match self.runtime.submit_hdr(request) {
            Ok(()) => self.hdr_pending = true,
            Err(error) => self.hdr_outcome(Err(error)),
        }
    }
    pub(super) fn hdr_outcome(&mut self, result: Result<HdrOutcome, ProviderError>) {
        self.hdr_pending = false;
        if result.is_ok() && self.error.starts_with("HDR: ") {
            self.error.clear();
            self.set_control_text(95, "");
        }
        let notice = match result {
            Ok(HdrOutcome::Verified) => None,
            Ok(HdrOutcome::Accepted) => Some((
                "HDR change requested",
                "Windows is updating the display. Reopen Displays to check HDR.".to_owned(),
            )),
            Err(error) => {
                self.error = format!("HDR: {}", error.message);
                self.set_control_text(95, user_message(&self.error));
                Some(("Couldn't change HDR", error.message))
            }
        };
        if let Some((title, text)) = notice
            && let Some(tray) = self.trays.values_mut().next()
        {
            let _ = tray.show_message(title, &text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources};
    fn fixture() -> Vec<DisplayMenuChoice> {
        vec![
            DisplayMenuChoice {
                id: FIRST_DISPLAY,
                text: "Display 1 · Panel & A · HDR on".into(),
                enabled: true,
                checked: true,
                request: None,
            },
            DisplayMenuChoice {
                id: FIRST_DISPLAY + 1,
                text: "Display 2 · Panel A · HDR off".into(),
                enabled: true,
                checked: false,
                request: None,
            },
            DisplayMenuChoice::information("Display 3 · HDR not supported"),
        ]
    }
    unsafe fn child(menu: HMENU) -> HMENU {
        unsafe {
            for index in 0..GetMenuItemCount(Some(menu)) {
                let mut caption = [0u16; 64];
                let length = GetMenuStringW(menu, index as u32, Some(&mut caption), MF_BYPOSITION);
                if String::from_utf16_lossy(&caption[..length.max(0) as usize]) == "&Displays" {
                    return GetSubMenu(menu, index);
                }
            }
            panic!("Displays submenu missing");
        }
    }
    #[test]
    fn display_rows_have_independent_checks_disabled_states_and_literal_names() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let menu = popup_menu(&Snapshot::default(), &Settings::default(), None).unwrap();
            append(menu.0, &fixture()).unwrap();
            let child = child(menu.0);
            assert_eq!(GetMenuItemCount(Some(child)), 3);
            assert_ne!(GetMenuState(child, 0, MF_BYPOSITION) & MF_CHECKED.0, 0);
            assert_eq!(GetMenuState(child, 1, MF_BYPOSITION) & MF_CHECKED.0, 0);
            assert_ne!(GetMenuState(child, 2, MF_BYPOSITION) & MF_GRAYED.0, 0);
            let mut caption = [0u16; 128];
            let length = GetMenuStringW(child, 0, Some(&mut caption), MF_BYPOSITION);
            assert!(String::from_utf16_lossy(&caption[..length as usize]).contains("Panel && A"));
        }
    }
    #[test]
    fn hdr_submenus_follow_both_palettes_without_retaining_gdi_resources() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let before = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            for index in 0..40 {
                let menu = popup_menu(&Snapshot::default(), &Settings::default(), None).unwrap();
                append(menu.0, &fixture()).unwrap();
                let appearance = PopupAppearance::new(
                    menu.0,
                    HWND::default(),
                    Palette::new(index % 2 == 0, false),
                )
                .unwrap();
                let nested = child(menu.0);
                assert!(appearance.menus.iter().any(|(handle, _)| *handle == nested));
                assert!(
                    appearance
                        .items
                        .iter()
                        .any(|item| item.command == u32::from(FIRST_DISPLAY) && item.enabled)
                );
                drop(menu);
                drop(appearance);
            }
            let after = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            assert!(
                after <= before + 2,
                "HDR menu GDI resources grew: {before} -> {after}"
            );
        }
    }
    #[test]
    fn empty_and_failed_inventory_remain_clear_and_non_actionable() {
        for rows in [
            choices(Ok(Vec::new())),
            choices(Err(ProviderError::new("Native diagnostic"))),
        ] {
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].enabled && rows[0].request.is_none());
            assert!(!rows[0].text.contains("Native diagnostic"));
        }
    }
}
