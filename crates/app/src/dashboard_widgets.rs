//! Small, static native dashboard visuals. Captions remain readable to Windows
//! accessibility tools; painting uses stock GDI objects and dashboard-owned fonts.
use crate::dashboard_theme::{Palette, color_brush, rounded_surface};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{Controls::DRAWITEMSTRUCT, WindowsAndMessaging::*},
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

pub fn draw(item: &DRAWITEMSTRUCT, palette: Palette, large_font: HFONT) {
    unsafe {
        let dc = item.hDC;
        let saved = SaveDC(dc);
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
            DrawTextW(
                dc,
                &mut value,
                &mut bounds,
                DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        };
        let metric = matches!(item.CtlID, 400..=409 | 420 | 421);
        let panel = matches!(item.CtlID, 74 | 76 | 410 | 411);
        FillRect(dc, &rect, color_brush(dc, palette.background));
        if metric || panel {
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
                    let mut measure = RECT {
                        left,
                        top: y,
                        right,
                        bottom: y,
                    };
                    let mut caption: Vec<u16> = row.encode_utf16().collect();
                    DrawTextW(
                        dc,
                        &mut caption,
                        &mut measure,
                        DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
                    );
                    let height = (measure.bottom - y).max(px(20));
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
            // Consistent section hierarchy without extra child windows.
            let accent = RECT {
                left: rect.left,
                top: rect.top + px(3),
                right: rect.left + px(3),
                bottom: rect.bottom - px(3),
            };
            FillRect(dc, &accent, color_brush(dc, palette.accent));
            if item.CtlID == 88
                && let Some((level, _)) = caption.split_once('%')
                && let Ok(level) = level.parse::<f64>()
            {
                bar(
                    dc,
                    rect.right - px(186),
                    rect.top + px(10),
                    rect.right - px(8),
                    px(7),
                    (level / 100.0).clamp(0.0, 1.0),
                    palette,
                );
            }
            text(
                &caption,
                rect.left + px(12),
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
