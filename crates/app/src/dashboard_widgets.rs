//! Small, static native dashboard visuals. Captions remain readable to Windows
//! accessibility tools; painting uses stock GDI objects and dashboard-owned fonts.
use crate::dashboard_theme::{Palette, color_brush, draw_utf16, rounded_surface};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{
        Controls::{DRAWITEMSTRUCT, ODS_FOCUS},
        WindowsAndMessaging::*,
    },
};

pub fn progress(text: &str) -> Option<f64> {
    let (_, values) = text.rsplit_once('·')?;
    let (value, rest) = values.trim().split_once('/')?;
    let maximum: f64 = rest.split_whitespace().next()?.parse().ok()?;
    let value: f64 = value.trim().parse().ok()?;
    (value.is_finite() && maximum.is_finite() && maximum > 0.0)
        .then(|| (value / maximum).clamp(0.0, 1.0))
}

fn percent(text: &str) -> Option<f64> {
    let value: f64 = text.trim().strip_suffix('%')?.trim().parse().ok()?;
    value.is_finite().then(|| (value / 100.0).clamp(0.0, 1.0))
}

// Share wrapping metrics with the painter. Reflow only when visible content changes.
fn row_height(dc: HDC, text: &str, width: i32, dpi: i32) -> i32 {
    let mut bounds = RECT {
        right: width,
        ..Default::default()
    };
    let mut text: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        draw_utf16(
            dc,
            &mut text,
            &mut bounds,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
    }
    bounds.bottom.max(20 * dpi / 96)
}
pub fn panel_height(hwnd: HWND, text: &str, width: i32, mix: bool) -> i32 {
    unsafe {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
        let dc = GetDC(Some(hwnd));
        if dc.is_invalid() {
            return 260;
        }
        let saved = SaveDC(dc);
        let font = HFONT(SendMessageW(hwnd, WM_GETFONT, None, None).0 as _);
        SelectObject(dc, font.into());
        let gap = (if mix { 7 } else { 10 }) * dpi / 96;
        let height = 50 * dpi / 96
            + text
                .lines()
                .skip(1)
                .filter(|row| !row.is_empty())
                .map(|row| row_height(dc, row, (width - 32).max(1) * dpi / 96, dpi) + gap)
                .sum::<i32>();
        let _ = RestoreDC(dc, saved);
        ReleaseDC(Some(hwnd), dc);
        ((height * 96 + dpi - 1) / dpi).max(92)
    }
}
// Measure only during layout, using the same font and wrapping as native labels.
pub(crate) fn label_height(hwnd: HWND, text: &str, width: i32, font: HFONT) -> i32 {
    unsafe {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
        let dc = GetDC(Some(hwnd));
        if dc.is_invalid() {
            return 108;
        }
        let old = SelectObject(dc, font.into());
        let height = row_height(dc, text, width.max(1) * dpi / 96, dpi);
        SelectObject(dc, old);
        ReleaseDC(Some(hwnd), dc);
        (height * 96 + dpi - 1) / dpi + 2
    }
}
fn roster_lines(dc: HDC, text: &str, width: i32, dpi: i32) -> i32 {
    let mut metrics = TEXTMETRICW::default();
    unsafe {
        let _ = GetTextMetricsW(dc, &mut metrics);
    }
    let line = metrics.tmHeight.max(1);
    let height = row_height(dc, text, width, dpi);
    if height > (20 * dpi / 96).max(line) {
        2
    } else {
        1
    }
}

pub(crate) fn roster_height(hwnd: HWND, caption: &str, font: HFONT) -> i32 {
    unsafe {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
        let dc = GetDC(Some(hwnd));
        if dc.is_invalid() {
            return 60;
        }
        let old = SelectObject(dc, font.into());
        let mut lines = caption.lines();
        let name = lines.nth(1).unwrap_or_default();
        let detail = lines.nth(1).unwrap_or_default();
        let width = 82 * dpi / 96;
        let height = 22
            + 19 * roster_lines(dc, name, width, dpi)
            + 17 * roster_lines(dc, detail, width, dpi);
        SelectObject(dc, old);
        ReleaseDC(Some(hwnd), dc);
        height.max(60)
    }
}

pub(crate) fn meter(dc: HDC, rect: RECT, fraction: f64, palette: Palette, asleep: bool) {
    let height = rect.bottom - rect.top;
    let track = palette.track();
    rounded_surface(dc, &rect, track, track, height);
    let filled = RECT {
        right: rect.left
            + (f64::from(rect.right - rect.left) * fraction.clamp(0., 1.)).round() as i32,
        ..rect
    };
    if filled.right > filled.left {
        let color = if asleep && !palette.high_contrast {
            crate::dashboard_theme::mix(track, palette.accent, 58)
        } else {
            palette.accent
        };
        rounded_surface(
            dc,
            &filled,
            color,
            color,
            height.min(filled.right - filled.left),
        );
    }
}

pub(crate) fn battery_fraction(text: &str) -> Option<f64> {
    percent(text.trim_start_matches('~'))
}

pub fn draw(item: &DRAWITEMSTRUCT, mut palette: Palette, large_font: HFONT) {
    unsafe {
        let dc = item.hDC;
        let saved = SaveDC(dc);
        if GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) == crate::dashboard_theme::PANEL_MARKER {
            palette.background = palette.surface;
        }
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(item.hwndItem).max(96) as i32;
        let px = |n: i32| n * dpi / 96;
        let normal = HFONT(SendMessageW(item.hwndItem, WM_GETFONT, None, None).0 as _);
        SelectObject(dc, normal.into());
        SetBkMode(dc, TRANSPARENT);
        let len = GetWindowTextLengthW(item.hwndItem).max(0) as usize;
        let mut caption = vec![0u16; len + 1];
        let count = GetWindowTextW(item.hwndItem, &mut caption);
        let caption = String::from_utf16_lossy(&caption[..count as usize]);
        let mut lines = caption.lines();
        let rect = item.rcItem;
        let text = |value: &str,
                    left: i32,
                    top: i32,
                    right: i32,
                    height: i32,
                    color: COLORREF,
                    font: HFONT| {
            SelectObject(dc, font.into());
            SetTextColor(dc, color);
            let mut bounds = RECT {
                left,
                top,
                right,
                bottom: top + height,
            };
            let mut value: Vec<u16> = value.encode_utf16().collect();
            draw_utf16(
                dc,
                &mut value,
                &mut bounds,
                if item.CtlID == 88 {
                    DT_SINGLELINE | DT_VCENTER
                } else {
                    DT_WORDBREAK
                } | DT_NOPREFIX
                    | DT_END_ELLIPSIS,
            );
        };
        let metric = matches!(item.CtlID, 400..=409 | 420 | 421);
        let panel = matches!(item.CtlID, 74 | 76 | 410 | 411);
        FillRect(dc, &rect, color_brush(dc, palette.background));
        if item.CtlID >= 1000 {
            let selected = GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) != 0;
            if selected && palette.high_contrast {
                // Highlight and accent can be the same system colour. Use the
                // matching highlight text colour for every selected-card mark.
                palette.text = palette.selection_text;
                palette.disabled = palette.selection_text;
                palette.accent = palette.selection_text;
                palette.border = palette.selection;
            }
            let surface = RECT {
                right: rect.right - 1,
                bottom: rect.bottom - 1,
                ..rect
            };
            rounded_surface(
                dc,
                &surface,
                if selected {
                    palette.selection
                } else {
                    palette.background
                },
                if selected && !palette.high_contrast {
                    palette.selected_border()
                } else {
                    palette.border
                },
                px(7),
            );
            let kind = lines.next().unwrap_or("device");
            let name = lines.next().unwrap_or_default();
            let value = lines.next().unwrap_or("—");
            let detail = lines.next().unwrap_or_default();
            let center = (rect.top + rect.bottom) / 2;
            draw_symbol(
                dc,
                RECT {
                    left: rect.left + px(9),
                    right: rect.left + px(25),
                    top: center - px(8),
                    bottom: center + px(8),
                },
                kind,
                palette.accent,
            );
            let left = rect.left + px(33);
            let right = rect.right - px(41);
            let name_height = px(19 * roster_lines(dc, name, right - left, dpi));
            let detail_height = px(17 * roster_lines(dc, detail, right - left, dpi));
            let top = center - (name_height + detail_height + px(3)) / 2;
            text(name, left, top, right, name_height, palette.text, normal);
            text(
                detail,
                left,
                top + name_height + px(3),
                right,
                detail_height,
                palette.disabled,
                normal,
            );
            let mut bounds = RECT {
                left: rect.right - px(39),
                right: rect.right - px(8),
                ..rect
            };
            SetTextColor(dc, palette.text);
            draw_utf16(
                dc,
                &mut value.encode_utf16().collect::<Vec<_>>(),
                &mut bounds,
                DT_VCENTER | DT_SINGLELINE | DT_RIGHT | DT_NOPREFIX,
            );
            if item.itemState.0 & ODS_FOCUS.0 != 0 {
                let focus = RECT {
                    left: rect.left + px(4),
                    top: rect.top + px(4),
                    right: rect.right - px(4),
                    bottom: rect.bottom - px(4),
                };
                let _ = DrawFocusRect(dc, &focus);
            }
        } else if matches!(item.CtlID, 8 | 490..=495) {
            FillRect(dc, &rect, color_brush(dc, palette.border));
        } else if item.CtlID == 476 {
            let value = lines.next().unwrap_or("—");
            let detail = lines.next().unwrap_or_default();
            text(
                value,
                rect.left,
                rect.top,
                rect.right,
                px(34),
                palette.text,
                large_font,
            );
            text(
                detail,
                rect.left,
                rect.top + px(34),
                rect.right,
                px(21),
                palette.disabled,
                normal,
            );
            if let Some(fraction) = battery_fraction(value) {
                meter(
                    dc,
                    RECT {
                        top: rect.bottom - px(6),
                        bottom: rect.bottom - px(2),
                        ..rect
                    },
                    fraction,
                    palette,
                    detail == "Last known battery",
                );
            }
        } else if item.CtlID == 95 {
            FillRect(dc, &rect, color_brush(dc, palette.caption()));
            FillRect(
                dc,
                &RECT {
                    bottom: rect.top + 1,
                    ..rect
                },
                color_brush(dc, palette.border),
            );
            let value = if caption.is_empty() {
                "Monitoring active"
            } else {
                &caption
            };
            let mut bounds = RECT {
                left: rect.left + px(20),
                right: rect.right - px(20),
                ..rect
            };
            SetTextColor(dc, palette.disabled);
            draw_utf16(
                dc,
                &mut value.encode_utf16().collect::<Vec<_>>(),
                &mut bounds,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        } else if matches!(item.CtlID, 460..=463) {
            let surface = RECT {
                right: rect.right - 1,
                bottom: rect.bottom - 1,
                ..rect
            };
            rounded_surface(dc, &surface, palette.surface, palette.border, px(20));
        } else if metric || panel {
            let mut surface = rect;
            surface.right -= 1;
            surface.bottom -= 1;
            rounded_surface(dc, &surface, palette.surface, palette.border, px(12));
            let left = rect.left + px(16);
            let right = rect.right - px(16);
            let title = lines.next().unwrap_or_default();
            text(
                title,
                left,
                rect.top + px(12),
                right,
                px(24),
                palette.disabled,
                normal,
            );
            if metric {
                let value = lines.next().unwrap_or("—");
                text(
                    value,
                    left,
                    rect.top + px(35),
                    right,
                    px(30),
                    palette.text,
                    large_font,
                );
                let subtitle = lines.collect::<Vec<_>>().join(" ");
                text(
                    &subtitle,
                    left,
                    rect.top + px(71),
                    right,
                    px(40),
                    palette.disabled,
                    normal,
                );
                if item.CtlID == 402
                    && let Some(value) = percent(value)
                {
                    bar(dc, left, rect.bottom - px(12), right, px(4), value, palette);
                }
            } else {
                let mut y = rect.top + px(42);
                for row in lines.filter(|line| !line.is_empty()) {
                    if y + px(20) > rect.bottom {
                        break;
                    }
                    let fraction = if item.CtlID == 410 {
                        row.rsplit_once('·').and_then(|(_, value)| percent(value))
                    } else {
                        progress(row)
                    };
                    SelectObject(dc, normal.into());
                    let height = row_height(dc, row, right - left, dpi);
                    text(
                        row,
                        left,
                        y,
                        right,
                        height.min(rect.bottom - px(8) - y),
                        palette.text,
                        normal,
                    );
                    if let Some(value) = fraction {
                        bar(dc, left, y + height + px(1), right, px(3), value, palette);
                    }
                    y += height + px(if item.CtlID == 410 { 7 } else { 10 });
                }
            }
        } else {
            text(
                &caption,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom - rect.top,
                palette.text,
                normal,
            );
        }
        let _ = RestoreDC(dc, saved);
    }
}
/// A handful of monochrome line icons, drawn from stock GDI objects. No font,
/// bitmap, SVG runtime or image cache is needed by the dashboard or popup.
pub(crate) fn draw_symbol(dc: HDC, rect: RECT, kind: &str, color: COLORREF) {
    unsafe {
        let saved = SaveDC(dc);
        SelectObject(dc, GetStockObject(DC_PEN));
        SelectObject(dc, GetStockObject(NULL_BRUSH));
        SetDCPenColor(dc, color);
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        let x = |n| rect.left + n * w / 24;
        let y = |n| rect.top + n * h / 24;
        let line = |a, b, c, d| {
            let _ = MoveToEx(dc, x(a), y(b), None);
            let _ = LineTo(dc, x(c), y(d));
        };
        match kind {
            "mouse" => {
                let _ = RoundRect(dc, x(5), y(1), x(19), y(23), w / 2, h / 2);
                line(12, 5, 12, 10);
            }
            "keyboard" => {
                let _ = RoundRect(dc, x(1), y(5), x(23), y(20), w / 4, h / 4);
                for row in [9, 13] {
                    for col in [5, 9, 13, 17] {
                        line(col, row, col + 1, row);
                    }
                }
                line(7, 17, 17, 17);
            }
            "history" => {
                line(3, 3, 3, 21);
                line(3, 21, 23, 21);
                line(6, 16, 11, 10);
                line(11, 10, 16, 13);
                line(16, 13, 21, 5);
            }
            "power" => {
                SetArcDirection(dc, AD_CLOCKWISE);
                let _ = Arc(dc, x(3), y(3), x(21), y(23), x(18), y(5), x(6), y(5));
                line(12, 0, 12, 11);
            }
            "battery" => {
                let _ = RoundRect(dc, x(1), y(6), x(21), y(19), w / 5, h / 5);
                line(23, 10, 23, 15);
                line(5, 9, 5, 16);
                line(9, 9, 9, 16);
            }
            "boost" | "gamepad" | "controller" | "dualsense" | "dualshock" => {
                let _ = RoundRect(dc, x(1), y(5), x(23), y(20), w / 3, h / 3);
                line(5, 12, 11, 12);
                line(8, 9, 8, 15);
                line(17, 11, 18, 11);
                line(19, 15, 20, 15);
            }
            "polling" => {
                let _ = Arc(dc, x(2), y(2), x(22), y(22), x(22), y(18), x(2), y(18));
                line(12, 14, 18, 6);
            }
            "dashboard" => {
                let _ = RoundRect(dc, x(1), y(3), x(23), y(21), w / 4, h / 4);
                line(1, 8, 23, 8);
                line(8, 8, 8, 21);
            }
            "display" => {
                let _ = RoundRect(dc, x(1), y(3), x(23), y(18), w / 5, h / 5);
                line(7, 23, 17, 23);
                line(12, 18, 12, 23);
            }
            "devices" => {
                line(3, 7, 21, 7);
                line(3, 7, 8, 2);
                line(3, 7, 8, 12);
                line(3, 17, 21, 17);
                line(21, 17, 16, 12);
                line(21, 17, 16, 22);
            }
            "refresh" => {
                SetArcDirection(dc, AD_CLOCKWISE);
                let _ = Arc(dc, x(3), y(3), x(21), y(21), x(4), y(12), x(19), y(6));
                let _ = Arc(dc, x(3), y(3), x(21), y(21), x(20), y(12), x(5), y(18));
                line(21, 3, 21, 8);
                line(21, 8, 16, 8);
                line(3, 21, 3, 16);
                line(3, 16, 8, 16);
            }
            "more" => {
                for col in [5, 12, 19] {
                    line(col, 12, col + 2, 12);
                }
            }
            "settings" => {
                let points = [
                    (9, 1),
                    (15, 1),
                    (16, 5),
                    (20, 6),
                    (23, 11),
                    (20, 14),
                    (20, 18),
                    (15, 22),
                    (11, 20),
                    (7, 21),
                    (2, 17),
                    (3, 12),
                    (1, 8),
                    (5, 3),
                    (9, 4),
                    (9, 1),
                ]
                .map(|(a, b)| POINT { x: x(a), y: y(b) });
                let _ = Polyline(dc, &points);
                let _ = Ellipse(dc, x(8), y(8), x(16), y(16));
            }
            _ => {
                for (row, col) in [(5, 9), (12, 16), (19, 7)] {
                    line(2, row, 22, row);
                    let _ = Rectangle(dc, x(col - 2), y(row - 2), x(col + 2), y(row + 2));
                }
            }
        }
        let _ = RestoreDC(dc, saved);
    }
}
fn bar(dc: HDC, left: i32, top: i32, right: i32, height: i32, value: f64, palette: Palette) {
    unsafe {
        let rect = RECT {
            left,
            top,
            right,
            bottom: top + height,
        };
        FillRect(dc, &rect, color_brush(dc, palette.border));
        let filled = RECT {
            right: left + ((right - left) as f64 * value).round() as i32,
            ..rect
        };
        FillRect(dc, &filled, color_brush(dc, palette.accent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_roster_row_and_summary_meter_keep_high_contrast_marks_visible() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("BUTTON"),
                windows::core::w!("mouse\nTest mouse\n50%\nWireless"),
                WS_POPUP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                x,
                y,
                375,
                162,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1);
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 375, 162);
            let previous = SelectObject(dc, bitmap.into());
            let selection = COLORREF(0x00800000);
            let foreground = COLORREF(0x00ffffff);
            let palette = Palette {
                high_contrast: true,
                background: COLORREF(0),
                surface: COLORREF(0),
                text: foreground,
                disabled: COLORREF(0x00808080),
                border: foreground,
                selection,
                accent: selection,
                selection_text: foreground,
                dark: false,
            };
            let row = DRAWITEMSTRUCT {
                CtlID: 1000,
                hwndItem: hwnd,
                hDC: dc,
                rcItem: RECT {
                    left: 0,
                    top: 0,
                    right: 375,
                    bottom: 162,
                },
                ..Default::default()
            };
            let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
            draw(&row, palette, font);
            let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
            let px = |n| n * dpi / 96;
            assert!(
                (px(9)..px(25))
                    .any(|x| (81 - px(8)..81 + px(8)).any(|y| GetPixel(dc, x, y) == foreground)),
                "Selected symbol blends into selection background"
            );
            meter(
                dc,
                RECT {
                    left: px(9),
                    top: 100,
                    right: px(109),
                    bottom: 104,
                },
                0.5,
                palette,
                true,
            );
            assert_eq!(
                GetPixel(dc, px(25), 102),
                selection,
                "Filled bar must contrast with its track"
            );
            assert_eq!(
                GetPixel(dc, px(80), 102),
                foreground,
                "Unfilled bar remains distinct"
            );
            // A new roster row can be painted before its caption is populated.
            SetWindowTextW(hwnd, windows::core::w!("mouse\n\n—")).unwrap();
            assert!(roster_height(hwnd, "mouse\n\n—", font) >= 60);
            draw(&row, palette, font);

            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            DestroyWindow(hwnd).unwrap();
        }
    }
    #[test]
    fn bars_clamp_progress_and_reject_invalid_values() {
        assert_eq!(progress("Observed time · 15 / 30 min"), Some(0.5));
        assert_eq!(progress("Battery drop · 10 / 3 points"), Some(1.0));
        for text in ["Unknown", "Value · NaN / 3", "Value · 2 / 0"] {
            assert_eq!(progress(text), None);
        }
        assert_eq!(percent("25%"), Some(0.25));
        assert_eq!(percent("—"), None);
    }
}
