//! UI-thread-owned HICONs. Animation frames are rendered once per visual state.
use hb_core::{DeviceView, Settings};
use windows::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
pub struct Icon(pub HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}
pub fn frames(
    device: &DeviceView,
    settings: &Settings,
    dark: bool,
    size: usize,
) -> windows::core::Result<Vec<Icon>> {
    let animated =
        settings.animation && device.reading.charging == Some(true) && device.reading.online();
    (0..if animated { 30 } else { 1 })
        .map(|frame| render(device, settings, dark, size, frame, animated))
        .collect()
}
fn render(
    d: &DeviceView,
    s: &Settings,
    dark: bool,
    n: usize,
    frame: usize,
    animated: bool,
) -> windows::core::Result<Icon> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: n as i32,
            biHeight: -(n as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut data = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut data, None, 0) }?;
    let pixels = unsafe { std::slice::from_raw_parts_mut(data as *mut u32, n * n) };
    pixels.fill(0);
    let foreground = match s.icon_theme.as_str() {
        "white" => [240., 240., 240.],
        "black" => [25., 25., 25.],
        _ => {
            if dark {
                [240., 240., 240.]
            } else {
                [30., 30., 30.]
            }
        }
    };
    let percent = s.percent_in_icon
        && d.reading.level.is_some()
        && d.reading.precision == hb_core::Precision::Exact;
    let level = d.reading.level.unwrap_or(0) as f64 / 100.;
    let color = if d.reading.charging == Some(true) {
        [75., 199., 133.]
    } else if d.reading.level.is_some_and(|l| l <= d.low_alert_at) {
        [232., 89., 84.]
    } else if s.warning_level > 0 && d.reading.level.is_some_and(|l| l <= s.warning_level) {
        [245., 166., 35.]
    } else {
        foreground
    };
    for y in 0..n {
        for x in 0..n {
            let px = (x as f64 + 0.5) * 32. / n as f64 - 16.;
            let py = (y as f64 + 0.5) * 32. / n as f64 - 16.;
            let radius = px.hypot(py);
            let angle = (py.atan2(px) + std::f64::consts::FRAC_PI_2)
                .rem_euclid(std::f64::consts::TAU)
                / std::f64::consts::TAU;
            let ring = radius > 12. && radius < 15.;
            let wave = animated && (angle - frame as f64 / 30.).rem_euclid(1.) < 0.18;
            let active = angle < level || wave;
            let symbol = if percent {
                false
            } else {
                match d.icon.as_str() {
                    "keyboard" => {
                        (px.abs() < 8. && py.abs() < 5.)
                            && (py.abs() > 3.
                                || px.abs() > 6.
                                || (px as i32 % 3 == 0 && py as i32 % 3 == 0))
                    }
                    "headset" => {
                        let r = px.hypot(py + 1.);
                        (r > 6. && r < 8. && py < 0.)
                            || (px.abs() > 6. && px.abs() < 9. && py > -1. && py < 7.)
                    }
                    "gamepad" | "dualshock" | "dualsense" => {
                        px.abs() < 8.
                            && py.abs() < 5.
                            && ((px + 4.).abs() < 1.
                                || (py).abs() < 1.
                                || (px - 4.).hypot(py) < 1.5)
                    }
                    "bluetooth" => {
                        let segment = |x1: f64, y1: f64, x2: f64, y2: f64| {
                            let dx = x2 - x1;
                            let dy = y2 - y1;
                            let t = ((px - x1) * dx + (py - y1) * dy) / (dx * dx + dy * dy);
                            let t = t.clamp(0., 1.);
                            (px - x1 - dx * t).hypot(py - y1 - dy * t) < 0.9
                        };
                        segment(0., -8., 0., 8.)
                            || segment(-4., -4., 4., 4.)
                            || segment(-4., 4., 4., -4.)
                            || segment(4., -4., 0., -8.)
                            || segment(4., 4., 0., 8.)
                    }
                    _ => {
                        ((px / 5.).powi(2) + (py / 8.).powi(2) > 0.6
                            && (px / 5.).powi(2) + (py / 8.).powi(2) < 1.)
                            || (px.abs() < 0.8 && py > -6. && py < -1.)
                    }
                }
            };
            if ring || symbol {
                let alpha = if !d.reading.online() {
                    100.
                } else if ring && !active {
                    65.
                } else {
                    255.
                };
                let c = if ring && active { color } else { foreground };
                pixels[y * n + x] = ((alpha as u32) << 24)
                    | (((c[0] * alpha / 255.) as u32) << 16)
                    | (((c[1] * alpha / 255.) as u32) << 8)
                    | (c[2] * alpha / 255.) as u32;
            }
        }
    }
    if percent {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, bitmap.into());
            let font = CreateFontW(
                -(n as i32 / 3),
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                DEFAULT_PITCH.0 as u32,
                windows::core::w!("Segoe UI"),
            );
            let old_font = SelectObject(dc, font.into());
            let _ = SetBkMode(dc, TRANSPARENT);
            let _ = SetTextColor(
                dc,
                COLORREF(
                    (foreground[0] as u32)
                        | ((foreground[1] as u32) << 8)
                        | ((foreground[2] as u32) << 16),
                ),
            );
            let text = d.reading.level.map_or("?".into(), |l| l.to_string());
            let mut text: Vec<u16> = text.encode_utf16().collect();
            let mut rect = RECT {
                left: 2,
                top: 2,
                right: n as i32 - 2,
                bottom: n as i32 - 2,
            };
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(dc, old_font);
            let _ = DeleteObject(font.into());
            SelectObject(dc, old);
            let _ = DeleteDC(dc);
        }
        for pixel in pixels.iter_mut() {
            if *pixel & 0xffffff != 0 && *pixel >> 24 == 0 {
                *pixel |= 0xff000000
            }
        }
    }
    if s.badges && d.reading.charging == Some(true) {
        for y in n * 3 / 4..n {
            for x in n * 3 / 4..n {
                if x >= n * 7 / 8 || y >= n * 7 / 8 {
                    pixels[y * n + x] = 0xff4bc785
                }
            }
        }
    }
    let mask = unsafe { CreateBitmap(n as i32, n as i32, 1, 1, None) };
    let result = unsafe {
        CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmColor: bitmap,
            hbmMask: mask,
            ..Default::default()
        })
    };
    unsafe {
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteObject(mask.into());
    }
    result.map(Icon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hb_core::{Connection, Reading};
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
    };
    #[test]
    fn cached_animation_frames_release_all_native_handles() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut reading = Reading::new("test:mouse", "Test mouse", "test", 0);
        reading.level = Some(73);
        reading.charging = Some(true);
        let mut device = DeviceView {
            reading,
            name: "Test mouse".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "73% charging".into(),
            hidden: false,
        };
        let mut settings = Settings::default();
        let count = || unsafe {
            (
                GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS),
                GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
            )
        };
        // Warm up GDI's process-wide cached brushes/font before measuring ownership.
        drop(frames(&device, &settings, true, 32).unwrap());
        settings.percent_in_icon = true;
        drop(frames(&device, &settings, true, 32).unwrap());
        settings.percent_in_icon = false;
        let before = count();
        for _ in 0..10 {
            let icons = frames(&device, &settings, true, 32).unwrap();
            assert_eq!(icons.len(), 30);
            assert!(icons.iter().all(|icon| !icon.0.0.is_null()));
        }
        assert_eq!(
            count(),
            before,
            "rendered HICON and DIB resources must be released"
        );
        settings.animation = false;
        assert_eq!(frames(&device, &settings, false, 32).unwrap().len(), 1);
        settings.animation = true;
        device.reading.connection = Connection::Sleeping;
        assert_eq!(frames(&device, &settings, true, 32).unwrap().len(), 1);
        device.reading.connection = Connection::Online;
        device.reading.charging = Some(false);
        assert_eq!(frames(&device, &settings, true, 32).unwrap().len(), 1);
        settings.percent_in_icon = true;
        let icons = frames(&device, &settings, true, 32).unwrap();
        assert_eq!(icons.len(), 1);
        drop(icons);
        assert_eq!(count(), before);
    }
}

#[cfg(test)]
mod pictogram_tests {
    use super::*;
    #[test]
    fn selected_pictograms_have_distinct_native_pixels() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut reading = hb_core::Reading::new("test:icon", "Icon", "test", 0);
        reading.level = Some(50);
        let mut device = DeviceView {
            reading,
            name: "Icon".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "50%".into(),
            hidden: false,
        };
        let mut images = std::collections::BTreeSet::new();
        for kind in ["mouse", "keyboard", "headset", "gamepad", "bluetooth"] {
            device.icon = kind.into();
            let icons = frames(&device, &Settings::default(), true, 32).unwrap();
            unsafe {
                let mut info = ICONINFO::default();
                GetIconInfo(icons[0].0, &mut info).unwrap();
                let mut pixels = vec![0u8; 32 * 32 * 4];
                let bytes = GetBitmapBits(
                    info.hbmColor,
                    pixels.len() as i32,
                    pixels.as_mut_ptr().cast(),
                );
                let _ = DeleteObject(info.hbmColor.into());
                let _ = DeleteObject(info.hbmMask.into());
                assert_eq!(bytes, pixels.len() as i32);
                assert!(
                    images.insert(pixels),
                    "Each selected kind must change the actual pixels"
                );
            }
        }
    }
}

#[cfg(test)]
fn native_pixels(icon: &Icon) -> Vec<u8> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon.0, &mut info).unwrap();
        let mut pixels = vec![0u8; 32 * 32 * 4];
        let bytes = GetBitmapBits(
            info.hbmColor,
            pixels.len() as i32,
            pixels.as_mut_ptr().cast(),
        );
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        assert_eq!(bytes, pixels.len() as i32);
        pixels
    }
}
#[cfg(test)]
mod warning_tests {
    use super::*;
    use hb_core::{Connection, Precision, Reading};

    fn device(level: Option<u8>) -> DeviceView {
        let mut reading = Reading::new("test:warning", "Warning", "test", 0);
        reading.level = level;
        reading.charging = Some(false);
        DeviceView {
            reading,
            name: "Warning".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "Warning".into(),
            hidden: false,
        }
    }
    fn image(d: &DeviceView, s: &Settings, dark: bool) -> Vec<u8> {
        native_pixels(&frames(d, s, dark, 32).unwrap()[0])
    }
    fn contains_color(pixels: &[u8], rgb: [u8; 3], alpha: u8) -> bool {
        let premultiplied = rgb.map(|c| (c as u32 * alpha as u32 / 255) as u8);
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| *p == [premultiplied[2], premultiplied[1], premultiplied[0], alpha])
    }

    #[test]
    fn warning_and_low_boundaries_have_distinct_native_colors_in_both_themes() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut d = device(Some(31));
        let mut s = Settings::default();
        for dark in [false, true] {
            let foreground = if dark { [240, 240, 240] } else { [30, 30, 30] };
            for (level, color) in [
                (31, foreground),
                (30, [245, 166, 35]),
                (21, [245, 166, 35]),
                (20, [232, 89, 84]),
            ] {
                d.reading.level = Some(level);
                let pixels = image(&d, &s, dark);
                assert!(
                    contains_color(&pixels, color, 255),
                    "level {level}, dark {dark}"
                );
                if level == 31 {
                    assert!(!contains_color(&pixels, [245, 166, 35], 255));
                }
            }
            d.low_alert_at = 10;
            d.reading.level = Some(20);
            assert!(contains_color(&image(&d, &s, dark), [245, 166, 35], 255));
            d.reading.level = Some(10);
            assert!(contains_color(&image(&d, &s, dark), [232, 89, 84], 255));
            s.warning_level = 0;
            d.reading.level = Some(20);
            assert!(!contains_color(&image(&d, &s, dark), [245, 166, 35], 255));
            d.reading.level = Some(10);
            assert!(contains_color(&image(&d, &s, dark), [232, 89, 84], 255));
            s.warning_level = 30;
            d.low_alert_at = 20;
        }
    }

    #[test]
    fn warning_preserves_charging_priority_coarse_glyph_and_cached_sleeping_level() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut d = device(Some(25));
        let mut s = Settings {
            percent_in_icon: true,
            ..Default::default()
        };
        d.reading.precision = Precision::Coarse;
        let coarse = image(&d, &s, true);
        assert!(contains_color(&coarse, [245, 166, 35], 255));
        s.percent_in_icon = false;
        assert_eq!(coarse, image(&d, &s, true));
        d.reading.precision = Precision::Exact;
        s.percent_in_icon = true;
        for connection in [Connection::Sleeping, Connection::Stale] {
            d.reading.connection = connection;
            let cached = image(&d, &s, true);
            assert!(contains_color(&cached, [245, 166, 35], 100));
            s.percent_in_icon = false;
            assert_ne!(
                cached,
                image(&d, &s, true),
                "Cached exact percent remains visible"
            );
            s.percent_in_icon = true;
        }
        d.reading.connection = Connection::Online;
        d.reading.level = Some(10);
        d.reading.charging = Some(true);
        let icons = frames(&d, &s, true, 32).unwrap();
        assert_eq!(icons.len(), 30);
        for icon in &icons {
            let pixels = native_pixels(icon);
            assert!(contains_color(&pixels, [75, 199, 133], 255));
            assert!(!contains_color(&pixels, [245, 166, 35], 255));
            assert!(!contains_color(&pixels, [232, 89, 84], 255));
        }
        drop(icons);
        d.reading.charging = Some(false);
        d.reading.level = None;
        let unknown = image(&d, &s, true);
        assert!(!contains_color(&unknown, [245, 166, 35], 255));
        s.percent_in_icon = false;
        assert_eq!(unknown, image(&d, &s, true));
        s.warning_level = 10;
        d.reading.level = Some(15);
        assert!(contains_color(&image(&d, &s, true), [232, 89, 84], 255));
    }
}
#[cfg(test)]
mod number_tests {
    use super::*;
    #[test]
    fn exact_percent_replaces_symbol_fits_ring_and_preserves_coarse_unknown_glyphs() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut reading = hb_core::Reading::new("test:percent", "Percent", "test", 0);
        reading.level = Some(100);
        let mut d = DeviceView {
            reading,
            name: "Percent".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "100%".into(),
            hidden: false,
        };
        let mut s = Settings::default();
        let glyph = native_pixels(&frames(&d, &s, true, 32).unwrap()[0]);
        s.percent_in_icon = true;
        let number = native_pixels(&frames(&d, &s, true, 32).unwrap()[0]);
        assert_ne!(number, glyph);
        for y in 0..32 {
            for x in 0..32 {
                if (x as f32 + 0.5 - 16.).hypot(y as f32 + 0.5 - 16.) > 12. {
                    let i = (y * 32 + x) * 4;
                    assert_eq!(
                        number[i..i + 4],
                        glyph[i..i + 4],
                        "100% text must fit inside ring"
                    );
                }
            }
        }
        d.reading.precision = hb_core::Precision::Coarse;
        let coarse = native_pixels(&frames(&d, &s, true, 32).unwrap()[0]);
        s.percent_in_icon = false;
        assert_eq!(coarse, native_pixels(&frames(&d, &s, true, 32).unwrap()[0]));
        d.reading.level = None;
        s.percent_in_icon = true;
        let unknown = native_pixels(&frames(&d, &s, true, 32).unwrap()[0]);
        s.percent_in_icon = false;
        assert_eq!(
            unknown,
            native_pixels(&frames(&d, &s, true, 32).unwrap()[0])
        );
    }
    #[test]
    fn charging_frames_preserve_percent_and_device_low_controls_ring_color() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let mut reading = hb_core::Reading::new("test:percent", "Percent", "test", 0);
        reading.level = Some(50);
        reading.charging = Some(true);
        let mut d = DeviceView {
            reading,
            name: "Percent".into(),
            icon: "mouse".into(),
            low_alert_at: 20,
            seconds_left: None,
            text: "50%".into(),
            hidden: false,
        };
        let s = Settings {
            percent_in_icon: true,
            ..Default::default()
        };
        let icons = frames(&d, &s, true, 32).unwrap();
        assert_eq!(icons.len(), 30);
        let first = native_pixels(&icons[0]);
        for icon in &icons[1..] {
            let pixels = native_pixels(icon);
            for y in 8..24 {
                for x in 8..24 {
                    let i = (y * 32 + x) * 4;
                    assert_eq!(first[i..i + 4], pixels[i..i + 4]);
                }
            }
        }
        drop(icons);
        d.reading.charging = Some(false);
        let normal = native_pixels(&frames(&d, &s, true, 32).unwrap()[0]);
        d.low_alert_at = 80;
        assert_ne!(normal, native_pixels(&frames(&d, &s, true, 32).unwrap()[0]));
    }
}
