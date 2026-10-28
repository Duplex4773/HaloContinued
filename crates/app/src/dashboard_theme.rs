//! Dashboard-only colors and documented native-control painting. Control input,
//! focus, selection and accessibility remain with the Windows control classes.
//! The themed scrollbar handles pointer input too, because the native tracking
//! loop paints directly to its DC, bypassing WM_PAINT.
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{
            Controls::*,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                GetCapture, GetFocus, IsWindowEnabled, ReleaseCapture, SetCapture, TME_LEAVE,
                TRACKMOUSEEVENT, TrackMouseEvent, VK_DOWN, VK_END, VK_HOME, VK_NEXT, VK_PRIOR,
                VK_UP,
            },
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: COLORREF,
    pub surface: COLORREF,
    pub text: COLORREF,
    pub disabled: COLORREF,
    pub border: COLORREF,
    pub accent: COLORREF,
    pub selection: COLORREF,
    pub selection_text: COLORREF,
    pub dark: bool,
    pub high_contrast: bool,
}
pub(crate) const PANEL_MARKER: isize = 0x4842_504c;
const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(r | (g << 8) | (b << 16))
}
impl Palette {
    pub fn new(dark: bool, high_contrast: bool) -> Self {
        if high_contrast {
            unsafe {
                return Self {
                    background: COLORREF(GetSysColor(COLOR_WINDOW)),
                    surface: COLORREF(GetSysColor(COLOR_WINDOW)),
                    text: COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
                    disabled: COLORREF(GetSysColor(COLOR_GRAYTEXT)),
                    border: COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
                    accent: COLORREF(GetSysColor(COLOR_HIGHLIGHT)),
                    selection: COLORREF(GetSysColor(COLOR_HIGHLIGHT)),
                    selection_text: COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)),
                    dark: false,
                    high_contrast,
                };
            }
        }
        if dark {
            Self {
                background: rgb(22, 27, 26),
                surface: rgb(32, 38, 37),
                text: rgb(237, 242, 239),
                disabled: rgb(169, 185, 174),
                border: rgb(53, 65, 59),
                accent: rgb(126, 225, 178),
                selection: rgb(36, 62, 50),
                selection_text: rgb(126, 225, 178),
                dark,
                high_contrast,
            }
        } else {
            Self {
                background: rgb(247, 248, 247),
                surface: rgb(255, 255, 255),
                text: rgb(32, 38, 36),
                disabled: rgb(89, 103, 94),
                border: rgb(220, 227, 223),
                accent: rgb(17, 107, 72),
                selection: rgb(222, 241, 229),
                selection_text: rgb(17, 107, 72),
                dark,
                high_contrast,
            }
        }
    }
    pub fn d2d(color: COLORREF) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
            r: (color.0 & 255) as f32 / 255.,
            g: ((color.0 >> 8) & 255) as f32 / 255.,
            b: ((color.0 >> 16) & 255) as f32 / 255.,
            a: 1.,
        }
    }
}
/// Use the documented DC stock brush for a single fill/frame inside a saved DC.
/// The brush belongs to Windows; RestoreDC restores its selected color.
pub(crate) fn color_brush(hdc: HDC, color: COLORREF) -> HBRUSH {
    unsafe {
        SetDCBrushColor(hdc, color);
        HBRUSH(GetStockObject(DC_BRUSH).0)
    }
}
pub(crate) fn rounded_surface(
    hdc: HDC,
    rect: &RECT,
    fill: COLORREF,
    border: COLORREF,
    radius: i32,
) {
    unsafe {
        let saved = SaveDC(hdc);
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        SetDCBrushColor(hdc, fill);
        SetDCPenColor(hdc, border);
        let _ = RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius,
            radius,
        );
        let _ = RestoreDC(hdc, saved);
    }
}

struct Brush(HBRUSH);
impl Brush {
    fn new(color: COLORREF) -> Self {
        Self(unsafe { CreateSolidBrush(color) })
    }
}
impl Drop for Brush {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}
struct WidgetFont {
    dpi: u32,
    font: HFONT,
}
impl Drop for WidgetFont {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
        }
    }
}
pub struct DashboardTheme {
    pub palette: Palette,
    background: Brush,
    surface: Brush,
    widget_font: std::cell::RefCell<Option<WidgetFont>>,
}
impl DashboardTheme {
    pub fn new(dark: bool, high_contrast: bool) -> Self {
        let palette = Palette::new(dark, high_contrast);
        Self {
            palette,
            background: Brush::new(palette.background),
            surface: Brush::new(palette.surface),
            widget_font: std::cell::RefCell::new(None),
        }
    }
    pub fn background_brush(&self) -> HBRUSH {
        self.background.0
    }
    pub fn apply_window(&self, hwnd: HWND) {
        unsafe {
            let dark = i32::from(self.palette.dark);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as _,
                size_of::<i32>() as u32,
            );
            if !self.palette.high_contrast {
                for (attribute, color) in [
                    (DWMWA_CAPTION_COLOR, self.palette.background),
                    (DWMWA_TEXT_COLOR, self.palette.text),
                    (DWMWA_BORDER_COLOR, self.palette.border),
                ] {
                    let _ = DwmSetWindowAttribute(
                        hwnd,
                        attribute,
                        &color as *const _ as _,
                        size_of::<COLORREF>() as u32,
                    );
                }
            } else {
                let default_color = 0xffff_ffffu32;
                for attribute in [DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_BORDER_COLOR] {
                    let _ = DwmSetWindowAttribute(
                        hwnd,
                        attribute,
                        &default_color as *const _ as _,
                        size_of::<u32>() as u32,
                    );
                }
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
    pub fn apply_control(&self, hwnd: HWND) {
        unsafe {
            let class = class_name(hwnd);
            if matches!(
                class.as_str(),
                "Button" | "ComboBox" | "Edit" | "ListBox" | "ScrollBar"
            ) {
                let mut data = 0usize;
                if GetWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID, Some(&mut data))
                    .as_bool()
                {
                    if self.palette.high_contrast {
                        cancel_scroll(hwnd, data);
                    }
                    (*(data as *mut ControlAppearance)).palette = self.palette;
                } else {
                    let data = Box::into_raw(Box::new(ControlAppearance {
                        palette: self.palette,
                        hovered: false,
                        scroll: ScrollGesture::Idle,
                        kind: match class.as_str() {
                            "ComboBox" => ControlKind::Combo,
                            "Edit" | "ListBox" => ControlKind::Text,
                            "ScrollBar" => ControlKind::Scroll,
                            _ => ControlKind::Button,
                        },
                    }));
                    if !SetWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID, data as usize)
                        .as_bool()
                    {
                        drop(Box::from_raw(data));
                    }
                }
            }
            // Empty theme disables visual-style white surfaces for edit/list/combo.
            // Buttons retain their native theme in light/high-contrast mode.
            if class != "Button" {
                let theme = if self.palette.dark {
                    w!("")
                } else {
                    PCWSTR::null()
                };
                let _ = SetWindowTheme(hwnd, theme, theme);
            }
            if class == "ListBox" {
                SendMessageW(
                    hwnd,
                    LB_SETITEMHEIGHT,
                    Some(WPARAM(0)),
                    Some(LPARAM((28 * GetDpiForWindow(hwnd) / 96) as isize)),
                );
            }
            if class == "ComboBox" {
                let height = (28 * GetDpiForWindow(hwnd) / 96) as isize;
                SendMessageW(
                    hwnd,
                    CB_SETITEMHEIGHT,
                    Some(WPARAM(usize::MAX)),
                    Some(LPARAM(height)),
                );
                SendMessageW(
                    hwnd,
                    CB_SETITEMHEIGHT,
                    Some(WPARAM(0)),
                    Some(LPARAM(height)),
                );
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
    pub fn control_colors(&self, message: u32, hdc: HDC, child: HWND) -> Option<LRESULT> {
        let surface = unsafe { GetWindowLongPtrW(child, GWLP_USERDATA) == PANEL_MARKER }
            || matches!(message, WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX)
            || (message == WM_CTLCOLORSTATIC && unsafe { class_name(child) } == "Edit");
        if !surface && !matches!(message, WM_CTLCOLORSTATIC | WM_CTLCOLORBTN) {
            return None;
        }
        unsafe {
            SetTextColor(
                hdc,
                if IsWindowEnabled(child).as_bool() {
                    if !self.palette.high_contrast
                        && matches!(GetDlgCtrlID(child), 45..=47 | 77 | 78 | 95..=97 | 99 | 213 | 216 | 219 | 464..=466 | 472 | 481 | 489)
                    {
                        self.palette.disabled
                    } else {
                        self.palette.text
                    }
                } else {
                    self.palette.disabled
                },
            );
            SetBkColor(
                hdc,
                if surface {
                    self.palette.surface
                } else {
                    self.palette.background
                },
            );
            SetBkMode(hdc, TRANSPARENT);
        }
        Some(LRESULT(if surface {
            self.surface.0.0
        } else {
            self.background.0.0
        } as isize))
    }
    pub fn draw_item(&self, lparam: LPARAM) -> Option<LRESULT> {
        if lparam.0 == 0 {
            return None;
        }
        unsafe {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            if item.CtlType == ODT_STATIC || (item.CtlType == ODT_BUTTON && item.CtlID >= 1000) {
                let dpi = GetDpiForWindow(item.hwndItem).max(96);
                let mut cache = self.widget_font.borrow_mut();
                if cache.as_ref().is_none_or(|cached| cached.dpi != dpi) {
                    *cache = Some(WidgetFont {
                        dpi,
                        font: CreateFontW(
                            -24 * dpi as i32 / 96,
                            0,
                            0,
                            0,
                            600,
                            0,
                            0,
                            0,
                            DEFAULT_CHARSET,
                            OUT_DEFAULT_PRECIS,
                            CLIP_DEFAULT_PRECIS,
                            CLEARTYPE_QUALITY,
                            DEFAULT_PITCH.0 as u32,
                            w!("Segoe UI"),
                        ),
                    });
                }
                let font = cache.as_ref().unwrap().font;
                drop(cache);
                crate::dashboard_widgets::draw(item, self.palette, font);
                return Some(LRESULT(1));
            }
            if item.CtlType != ODT_COMBOBOX && item.CtlType != ODT_LISTBOX {
                return None;
            }
            let saved = SaveDC(item.hDC);
            let selected = item.itemState.0 & ODS_SELECTED.0 != 0
                && item.itemState.0 & ODS_COMBOBOXEDIT.0 == 0;
            let bg = if selected {
                self.palette.selection
            } else {
                self.palette.surface
            };
            let fg = if item.itemState.0 & ODS_DISABLED.0 != 0 {
                self.palette.disabled
            } else if selected {
                self.palette.selection_text
            } else {
                self.palette.text
            };
            FillRect(item.hDC, &item.rcItem, color_brush(item.hDC, bg));
            if item.itemID != u32::MAX {
                let len = SendMessageW(
                    item.hwndItem,
                    if item.CtlType == ODT_LISTBOX {
                        LB_GETTEXTLEN
                    } else {
                        CB_GETLBTEXTLEN
                    },
                    Some(WPARAM(item.itemID as usize)),
                    None,
                )
                .0;
                if len >= 0 {
                    let mut text = vec![0u16; len as usize + 1];
                    SendMessageW(
                        item.hwndItem,
                        if item.CtlType == ODT_LISTBOX {
                            LB_GETTEXT
                        } else {
                            CB_GETLBTEXT
                        },
                        Some(WPARAM(item.itemID as usize)),
                        Some(LPARAM(text.as_mut_ptr() as isize)),
                    );
                    text.truncate(len as usize);
                    let mut rect = item.rcItem;
                    rect.left += 6;
                    SetTextColor(item.hDC, fg);
                    SetBkMode(item.hDC, TRANSPARENT);
                    DrawTextW(
                        item.hDC,
                        &mut text,
                        &mut rect,
                        DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                }
            }
            if item.itemState.0 & ODS_FOCUS.0 != 0 && item.itemState.0 & ODS_NOFOCUSRECT.0 == 0 {
                let _ = DrawFocusRect(item.hDC, &item.rcItem);
            }
            let _ = RestoreDC(item.hDC, saved);
        }
        Some(LRESULT(1))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Button,
    Combo,
    Text,
    Scroll,
}
#[derive(Clone, Copy)]
struct ControlAppearance {
    palette: Palette,
    hovered: bool,
    kind: ControlKind,
    scroll: ScrollGesture,
}

#[derive(Clone, Copy)]
enum ScrollGesture {
    Idle,
    Drag { offset: i32 },
    Repeat { command: u32, y: i32 },
}

const SCROLL_REPEAT: usize = SUBCLASS_ID + 1;

unsafe fn cancel_scroll(hwnd: HWND, data: usize) {
    unsafe {
        if (*(data as *const ControlAppearance)).kind != ControlKind::Scroll {
            return;
        }
        (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Idle;
        let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
        if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
    }
}

unsafe fn scroll_geometry(hwnd: HWND) -> Option<(SCROLLBARINFO, SCROLLINFO)> {
    unsafe {
        let mut bar = SCROLLBARINFO {
            cbSize: size_of::<SCROLLBARINFO>() as u32,
            ..Default::default()
        };
        let mut info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            ..Default::default()
        };
        GetClientRect(hwnd, &mut bar.rcScrollBar).ok()?;
        GetScrollInfo(hwnd, SB_CTL, &mut info).ok()?;
        // Native thumb geometry is calculated during native painting. Since
        // that painting is suppressed, derive it from the authoritative range.
        let height = bar.rcScrollBar.bottom;
        let arrow = bar.rcScrollBar.right.min(height / 2).max(0);
        let track = (height - 2 * arrow).max(0);
        let range = (i64::from(info.nMax) - i64::from(info.nMin) + 1).max(1);
        let minimum_thumb = (18 * GetDpiForWindow(hwnd).max(96) / 96) as i32;
        let thumb = ((i64::from(track) * i64::from(info.nPage) / range) as i32)
            .max(minimum_thumb)
            .min(track);
        let max = (info.nMax - info.nPage.saturating_sub(1) as i32).max(info.nMin);
        let offset = if max > info.nMin {
            (i64::from(track - thumb) * i64::from(info.nPos - info.nMin)
                / i64::from(max - info.nMin)) as i32
        } else {
            0
        };
        bar.dxyLineButton = arrow;
        bar.xyThumbTop = arrow + offset;
        bar.xyThumbBottom = bar.xyThumbTop + thumb;
        Some((bar, info))
    }
}

unsafe fn notify_scroll(hwnd: HWND, command: u32, position: i32) {
    unsafe {
        if let Ok(parent) = GetParent(hwnd) {
            // Dashboard pages are bounded to far less than the message's 16-bit
            // position limit. Both native/high-contrast and themed input use it.
            SendMessageW(
                parent,
                WM_VSCROLL,
                Some(WPARAM(
                    command as usize | ((position as u16 as usize) << 16),
                )),
                Some(LPARAM(hwnd.0 as isize)),
            );
        }
    }
}

unsafe fn scroll_input(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, data: usize) -> bool {
    unsafe {
        let gesture = (*(data as *const ControlAppearance)).scroll;
        let x = lp.0 as i16 as i32;
        let y = (lp.0 >> 16) as i16 as i32;
        match msg {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                if !IsWindowEnabled(hwnd).as_bool() {
                    return true;
                }
                let Some((bar, _)) = scroll_geometry(hwnd) else {
                    return true;
                };
                let height = bar.rcScrollBar.bottom - bar.rcScrollBar.top;
                let command = if y < bar.dxyLineButton {
                    SB_LINEUP
                } else if y >= height - bar.dxyLineButton {
                    SB_LINEDOWN
                } else if y < bar.xyThumbTop {
                    SB_PAGEUP
                } else if y >= bar.xyThumbBottom {
                    SB_PAGEDOWN
                } else {
                    (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Drag {
                        offset: y - bar.xyThumbTop,
                    };
                    SetCapture(hwnd);
                    if GetCapture() != hwnd {
                        (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Idle;
                    }
                    return true;
                };
                (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Repeat {
                    command: command.0 as u32,
                    y,
                };
                SetCapture(hwnd);
                if GetCapture() == hwnd {
                    // No idle timer: repeat exists only while the mouse is held.
                    SetTimer(Some(hwnd), SCROLL_REPEAT, 400, None);
                } else {
                    (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Idle;
                }
                notify_scroll(hwnd, command.0 as u32, 0);
                true
            }
            WM_MOUSEMOVE => {
                if let ScrollGesture::Drag { offset } = gesture {
                    if let Some((bar, info)) = scroll_geometry(hwnd) {
                        let travel = bar.rcScrollBar.bottom
                            - bar.rcScrollBar.top
                            - 2 * bar.dxyLineButton
                            - (bar.xyThumbBottom - bar.xyThumbTop);
                        let max = (info.nMax - info.nPage.saturating_sub(1) as i32).max(info.nMin);
                        if travel > 0 {
                            let pixel = (y - offset - bar.dxyLineButton).clamp(0, travel);
                            let position = info.nMin
                                + ((i64::from(pixel) * i64::from(max - info.nMin)
                                    + i64::from(travel / 2))
                                    / i64::from(travel)) as i32;
                            if position != info.nPos {
                                notify_scroll(hwnd, SB_THUMBTRACK.0 as u32, position);
                            }
                        }
                    }
                } else if let ScrollGesture::Repeat { command, .. } = gesture {
                    let mut bounds = RECT::default();
                    let inside = GetClientRect(hwnd, &mut bounds).is_ok()
                        && x >= bounds.left
                        && x < bounds.right;
                    (*(data as *mut ControlAppearance)).scroll = ScrollGesture::Repeat {
                        command,
                        // Captured movement outside either side of the control
                        // pauses repeat until the pointer returns.
                        y: if inside { y } else { i32::MIN },
                    };
                }
                true
            }
            WM_TIMER if wp.0 == SCROLL_REPEAT => {
                if let ScrollGesture::Repeat { command, y } = gesture {
                    SetTimer(Some(hwnd), SCROLL_REPEAT, 60, None);
                    if let Some((bar, _)) = scroll_geometry(hwnd) {
                        let height = bar.rcScrollBar.bottom - bar.rcScrollBar.top;
                        let inside = match command as i32 {
                            c if c == SB_LINEUP.0 => y >= 0 && y < bar.dxyLineButton,
                            c if c == SB_LINEDOWN.0 => {
                                y >= height - bar.dxyLineButton && y < height
                            }
                            c if c == SB_PAGEUP.0 => y >= bar.dxyLineButton && y < bar.xyThumbTop,
                            _ => y >= bar.xyThumbBottom && y < height - bar.dxyLineButton,
                        };
                        if inside {
                            notify_scroll(hwnd, command, 0);
                        }
                    }
                }
                true
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED | WM_CANCELMODE | WM_KILLFOCUS => {
                cancel_scroll(hwnd, data);
                true
            }
            WM_SETFOCUS => true,
            WM_KEYDOWN => {
                let command = match wp.0 as u16 {
                    k if k == VK_UP.0 => SB_LINEUP,
                    k if k == VK_DOWN.0 => SB_LINEDOWN,
                    k if k == VK_PRIOR.0 => SB_PAGEUP,
                    k if k == VK_NEXT.0 => SB_PAGEDOWN,
                    k if k == VK_HOME.0 => SB_TOP,
                    k if k == VK_END.0 => SB_BOTTOM,
                    _ => return false,
                };
                notify_scroll(hwnd, command.0 as u32, 0);
                true
            }
            _ => false,
        }
    }
}

const SUBCLASS_ID: usize = 0x48425448;
unsafe fn class_name(hwnd: HWND) -> String {
    unsafe {
        let mut name = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut name);
        String::from_utf16_lossy(&name[..len.max(0) as usize])
    }
}
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    std::panic::catch_unwind(|| unsafe { control_message(hwnd, msg, wp, lp, data) })
        .unwrap_or(LRESULT(0))
}
unsafe fn control_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, data: usize) -> LRESULT {
    unsafe {
        if msg == WM_NCDESTROY {
            cancel_scroll(hwnd, data);
            let _ = RemoveWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID);
            drop(Box::from_raw(data as *mut ControlAppearance));
            return DefSubclassProc(hwnd, msg, wp, lp);
        }
        let appearance = *(data as *const ControlAppearance);
        let input_background = appearance.palette.background;
        let mut palette = appearance.palette;
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) == PANEL_MARKER {
            palette.background = palette.surface;
        }
        // Device cards use the same native button behaviour, with owner drawing
        // sharing the dashboard's single cached value font.
        if appearance.kind == ControlKind::Button
            && GetWindowLongW(hwnd, GWL_STYLE) as u32 & BS_TYPEMASK as u32 == BS_OWNERDRAW as u32
            && matches!(msg, WM_PAINT | WM_PRINTCLIENT | WM_PRINT)
        {
            return DefSubclassProc(hwnd, msg, wp, lp);
        }
        if !palette.high_contrast && appearance.kind != ControlKind::Text && msg == WM_ERASEBKGND {
            // Buttons, selectors and the scrollbar paint their complete client
            // area. A separate native erasure exposes a light frame in dark mode.
            return LRESULT(1);
        }
        if !palette.high_contrast && appearance.kind == ControlKind::Scroll {
            if matches!(msg, WM_SHOWWINDOW | WM_ENABLE) && wp.0 == 0 {
                cancel_scroll(hwnd, data);
            }
            if msg == SBM_GETSCROLLBARINFO && lp.0 != 0 {
                if let Some((mut bar, _)) = scroll_geometry(hwnd) {
                    let _ = GetWindowRect(hwnd, &mut bar.rcScrollBar);
                    *(lp.0 as *mut SCROLLBARINFO) = bar;
                    return LRESULT(1);
                }
                return LRESULT(0);
            }
            if scroll_input(hwnd, msg, wp, lp, data) {
                return LRESULT(0);
            }
            // Never let a range update draw the native scrollbar underneath
            // the custom surface, even if a caller requests immediate redraw.
            let update = match msg {
                SBM_SETSCROLLINFO => Some((msg, WPARAM(0), lp, wp.0 != 0)),
                SBM_SETPOS => Some((msg, wp, LPARAM(0), lp.0 != 0)),
                SBM_SETRANGEREDRAW => Some((SBM_SETRANGE, wp, lp, true)),
                _ => None,
            };
            if let Some((message, wp, lp, redraw)) = update {
                let result = DefSubclassProc(hwnd, message, wp, lp);
                if redraw {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return result;
            }
        }
        if !palette.high_contrast
            && matches!(appearance.kind, ControlKind::Button | ControlKind::Combo)
            && matches!(msg, WM_MOUSEMOVE | WM_MOUSELEAVE)
        {
            let hovered = msg == WM_MOUSEMOVE;
            if hovered != appearance.hovered {
                (*(data as *mut ControlAppearance)).hovered = hovered;
                if hovered {
                    let mut tracking = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        ..Default::default()
                    };
                    let _ = TrackMouseEvent(&mut tracking);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        if !palette.high_contrast && appearance.kind == ControlKind::Text {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            if msg == WM_NCPAINT {
                let dc = GetWindowDC(Some(hwnd));
                let saved = SaveDC(dc);
                let mut bounds = RECT::default();
                if GetWindowRect(hwnd, &mut bounds).is_ok() {
                    let rect = RECT {
                        left: 0,
                        top: 0,
                        right: bounds.right - bounds.left,
                        bottom: bounds.bottom - bounds.top,
                    };
                    FrameRect(dc, &rect, color_brush(dc, palette.border));
                }
                let _ = RestoreDC(dc, saved);
                ReleaseDC(Some(hwnd), dc);
            }
            return result;
        }
        if matches!(msg, WM_PAINT | WM_PRINTCLIENT | WM_PRINT) && !palette.high_contrast {
            let printing = matches!(msg, WM_PRINTCLIENT | WM_PRINT);
            let combo = appearance.kind == ControlKind::Combo;
            let mut ps = PAINTSTRUCT::default();
            let hdc = if printing {
                HDC(wp.0 as *mut _)
            } else {
                BeginPaint(hwnd, &mut ps)
            };
            let saved = SaveDC(hdc);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            if appearance.kind == ControlKind::Scroll {
                FillRect(hdc, &rect, color_brush(hdc, palette.background));
                if let Some((bar, _)) = scroll_geometry(hwnd) {
                    let inset = (rect.right / 3).max(2);
                    let thumb = RECT {
                        left: inset,
                        right: rect.right - inset,
                        top: bar.xyThumbTop,
                        bottom: bar.xyThumbBottom,
                    };
                    if thumb.bottom > thumb.top {
                        rounded_surface(hdc, &thumb, palette.disabled, palette.disabled, inset * 2);
                    }
                    // Arrow hit regions remain native; draw subtle matching affordances.
                    SetTextColor(hdc, palette.disabled);
                    SetBkMode(hdc, TRANSPARENT);
                    for (glyph, top) in [('˄', 0), ('˅', rect.bottom - bar.dxyLineButton)] {
                        let mut arrow_rect = RECT {
                            left: 0,
                            right: rect.right,
                            top,
                            bottom: top + bar.dxyLineButton,
                        };
                        DrawTextW(
                            hdc,
                            &mut [glyph as u16],
                            &mut arrow_rect,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );
                    }
                }
            } else if combo {
                let enabled = IsWindowEnabled(hwnd).as_bool();
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                FillRect(hdc, &rect, color_brush(hdc, palette.background));
                rounded_surface(
                    hdc,
                    &rect,
                    input_background,
                    if appearance.hovered && enabled {
                        palette.accent
                    } else {
                        palette.border
                    },
                    8 * dpi / 96,
                );
                let font = SendMessageW(hwnd, WM_GETFONT, None, None);
                if font.0 != 0 {
                    SelectObject(hdc, HGDIOBJ(font.0 as _));
                }
                let selected = SendMessageW(hwnd, CB_GETCURSEL, None, None).0;
                if selected >= 0 {
                    let len =
                        SendMessageW(hwnd, CB_GETLBTEXTLEN, Some(WPARAM(selected as usize)), None)
                            .0;
                    if len >= 0 {
                        let mut text = vec![0u16; len as usize + 1];
                        SendMessageW(
                            hwnd,
                            CB_GETLBTEXT,
                            Some(WPARAM(selected as usize)),
                            Some(LPARAM(text.as_mut_ptr() as isize)),
                        );
                        text.truncate(len as usize);
                        let mut label = rect;
                        label.left += 9 * dpi / 96;
                        label.right -= 28 * dpi / 96;
                        SetTextColor(
                            hdc,
                            if enabled {
                                palette.text
                            } else {
                                palette.disabled
                            },
                        );
                        SetBkMode(hdc, TRANSPARENT);
                        DrawTextW(
                            hdc,
                            &mut text,
                            &mut label,
                            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                        );
                    }
                }
                let mut arrow_rect = rect;
                arrow_rect.left = rect.right - 28 * dpi / 96;
                SetTextColor(
                    hdc,
                    if enabled {
                        palette.text
                    } else {
                        palette.disabled
                    },
                );
                SetBkMode(hdc, TRANSPARENT);
                DrawTextW(
                    hdc,
                    &mut ['▾' as u16],
                    &mut arrow_rect,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            } else {
                let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
                let check = style & BS_TYPEMASK as u32 == BS_AUTOCHECKBOX as u32;
                let state = SendMessageW(hwnd, BM_GETSTATE, None, None).0 as u32;
                let selected = style & BS_PUSHLIKE as u32 != 0
                    && SendMessageW(hwnd, BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize;
                let enabled = IsWindowEnabled(hwnd).as_bool();
                let id = GetDlgCtrlID(hwnd);
                let navigation = matches!(id, 1..=3 | 6 | 17..=19 | 22 | 24);
                let primary = matches!(id, 15 | 40 | 210 | 433 | 453) && !check;
                let active = enabled && (appearance.hovered || state & BST_PUSHED != 0);
                let background = if check {
                    palette.background
                } else if primary && enabled {
                    palette.accent
                } else if navigation {
                    if active {
                        palette.selection
                    } else {
                        palette.background
                    }
                } else if selected || active {
                    palette.selection
                } else {
                    palette.surface
                };
                let background = if active && !check && !navigation {
                    let amount: u32 = if state & BST_PUSHED != 0 { 20 } else { 10 };
                    let channel = |shift: u32| -> u32 {
                        let a = (background.0 >> shift) & 255;
                        let b = (palette.accent.0 >> shift) & 255;
                        (a * (100 - amount) + b * amount) / 100
                    };
                    rgb(channel(0), channel(8), channel(16))
                } else {
                    background
                };
                FillRect(hdc, &rect, color_brush(hdc, palette.background));
                let mut text_rect = rect;
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                if check {
                    let size = 18 * dpi / 96;
                    let mark = RECT {
                        left: 1,
                        top: (rect.bottom - size) / 2,
                        right: 1 + size,
                        bottom: (rect.bottom + size) / 2,
                    };
                    let checked =
                        SendMessageW(hwnd, BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize;
                    rounded_surface(
                        hdc,
                        &mark,
                        if checked && enabled {
                            palette.accent
                        } else {
                            palette.surface
                        },
                        if enabled && (checked || active) {
                            palette.accent
                        } else if enabled {
                            palette.disabled
                        } else {
                            palette.border
                        },
                        4 * dpi / 96,
                    );
                    if checked {
                        let mut mark_rect = mark;
                        let mut tick = ['✓' as u16];
                        SetTextColor(
                            hdc,
                            if enabled {
                                palette.background
                            } else {
                                palette.disabled
                            },
                        );
                        SetBkMode(hdc, TRANSPARENT);
                        DrawTextW(
                            hdc,
                            &mut tick,
                            &mut mark_rect,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );
                    }
                    text_rect.left = mark.right + 9 * dpi / 96;
                } else {
                    rounded_surface(
                        hdc,
                        &rect,
                        background,
                        if navigation {
                            background
                        } else if selected || (primary && enabled) || active {
                            palette.accent
                        } else {
                            palette.border
                        },
                        10 * dpi / 96,
                    );
                    // A persistent underline identifies the current page even
                    // without colour. It uses the same paint pass as the label.
                    if selected {
                        let rail = RECT {
                            left: rect.left + 14 * dpi / 96,
                            right: rect.right - 14 * dpi / 96,
                            top: rect.bottom - 3 * dpi / 96,
                            bottom: rect.bottom - dpi / 96,
                        };
                        FillRect(hdc, &rail, color_brush(hdc, palette.accent));
                    }
                }
                let font = SendMessageW(hwnd, WM_GETFONT, None, None);
                if font.0 != 0 {
                    SelectObject(hdc, HGDIOBJ(font.0 as _));
                }
                let mut text = vec![0u16; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
                let len = GetWindowTextW(hwnd, &mut text);
                text.truncate(len.max(0) as usize);
                SetTextColor(
                    hdc,
                    if IsWindowEnabled(hwnd).as_bool() {
                        if primary {
                            if palette.dark {
                                rgb(18, 43, 32)
                            } else {
                                rgb(255, 255, 255)
                            }
                        } else if selected || active {
                            palette.selection_text
                        } else {
                            palette.text
                        }
                    } else {
                        palette.disabled
                    },
                );
                SetBkMode(hdc, TRANSPARENT);
                let ui = SendMessageW(hwnd, WM_QUERYUISTATE, None, None).0 as u32;
                let mut flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
                if !check {
                    flags |= DT_CENTER;
                }
                if ui & UISF_HIDEACCEL != 0 {
                    flags |= DT_HIDEPREFIX;
                }
                DrawTextW(hdc, &mut text, &mut text_rect, flags);
                if GetFocus() == hwnd && ui & UISF_HIDEFOCUS == 0 {
                    text_rect.left += 2;
                    text_rect.top += 2;
                    text_rect.right -= 2;
                    text_rect.bottom -= 2;
                    let _ = DrawFocusRect(hdc, &text_rect);
                }
            }
            let _ = RestoreDC(hdc, saved);
            if printing {
                // The caller owns the print DC.
            } else {
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        let result = DefSubclassProc(hwnd, msg, wp, lp);
        if !palette.high_contrast
            && matches!(
                msg,
                BM_SETCHECK
                    | BM_SETSTATE
                    | WM_ENABLE
                    | WM_SETTEXT
                    | WM_UPDATEUISTATE
                    | WM_SETFOCUS
                    | WM_KILLFOCUS
                    | CB_SETCURSEL
                    | CB_RESETCONTENT
                    | CB_ADDSTRING
                    | CB_DELETESTRING
                    | SBM_SETSCROLLINFO
                    | SBM_SETPOS
                    | SBM_SETRANGE
            )
        {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_combo_text_and_static_colors_use_the_dashboard_palette() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            let combo = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("COMBOBOX"),
                w!(""),
                WS_POPUP
                    | WINDOW_STYLE((CBS_DROPDOWNLIST | CBS_OWNERDRAWFIXED | CBS_HASSTRINGS) as u32),
                x,
                y,
                240,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            SendMessageW(
                combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(w!("Test device").as_ptr() as isize)),
            );
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), None);
            let label = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Ready"),
                WS_POPUP,
                x,
                y,
                240,
                40,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 240, 40);
            let previous = SelectObject(dc, bitmap.into());
            for (dark, hc) in [(false, false), (true, false), (true, true)] {
                let theme = DashboardTheme::new(dark, hc);
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: 240,
                    bottom: 40,
                };
                for (state, expected) in [
                    (
                        ODS_FLAGS(ODS_SELECTED.0 | ODS_COMBOBOXEDIT.0),
                        theme.palette.surface,
                    ),
                    (ODS_SELECTED, theme.palette.selection),
                ] {
                    let item = DRAWITEMSTRUCT {
                        CtlType: ODT_COMBOBOX,
                        itemID: 0,
                        itemState: state,
                        hwndItem: combo,
                        hDC: dc,
                        rcItem: rect,
                        ..Default::default()
                    };
                    assert_eq!(
                        theme.draw_item(LPARAM(&item as *const _ as isize)),
                        Some(LRESULT(1))
                    );
                    assert_eq!(GetPixel(dc, 235, 35), expected);
                    assert!((0..40).any(|y| (6..150).any(|x| GetPixel(dc, x, y) != expected)));
                }
                let brush = theme.control_colors(WM_CTLCOLORSTATIC, dc, label).unwrap();
                assert_eq!(GetTextColor(dc), theme.palette.text);
                assert_eq!(GetBkColor(dc), theme.palette.background);
                FillRect(dc, &rect, HBRUSH(brush.0 as *mut _));
                assert_eq!(GetPixel(dc, 235, 35), theme.palette.background);
            }
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            DestroyWindow(label).unwrap();
            DestroyWindow(combo).unwrap();
        }
    }
    #[test]
    fn dark_combo_print_keeps_arrow_surface_dark() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("COMBOBOX"),
                w!(""),
                WS_POPUP | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                x,
                y,
                240,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let theme = DashboardTheme::new(true, false);
            theme.apply_control(hwnd);
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).unwrap();
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, rect.right, rect.bottom);
            let previous = SelectObject(dc, bitmap.into());
            for message in [WM_PRINT, WM_PRINTCLIENT] {
                SendMessageW(
                    hwnd,
                    message,
                    Some(WPARAM(dc.0 as usize)),
                    Some(LPARAM(
                        (PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize,
                    )),
                );
                assert_eq!(GetPixel(dc, rect.right - 4, 4), theme.palette.background);
            }
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            DestroyWindow(hwnd).unwrap();
        }
    }
    #[test]
    fn dark_buttons_print_dark_surfaces_and_readable_labels() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            for style in [0, BS_AUTOCHECKBOX] {
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    w!("Enable alerts"),
                    WS_POPUP | WINDOW_STYLE(style as u32),
                    x,
                    y,
                    240,
                    40,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                let theme = DashboardTheme::new(true, false);
                theme.apply_control(hwnd);
                let screen = GetDC(None);
                let dc = CreateCompatibleDC(Some(screen));
                let bitmap = CreateCompatibleBitmap(screen, 240, 40);
                let previous = SelectObject(dc, bitmap.into());
                SendMessageW(
                    hwnd,
                    WM_PRINTCLIENT,
                    Some(WPARAM(dc.0 as usize)),
                    Some(LPARAM(PRF_CLIENT as isize)),
                );
                let expected = if style == BS_AUTOCHECKBOX {
                    theme.palette.background
                } else {
                    theme.palette.surface
                };
                assert_eq!(GetPixel(dc, 235, 35), expected);
                assert!((0..40).any(|y| (24..220).any(|x| {
                    let c = GetPixel(dc, x, y);
                    c.0 & 255 > 180 && (c.0 >> 8) & 255 > 180 && (c.0 >> 16) & 255 > 180
                })));
                SelectObject(dc, previous);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                DestroyWindow(hwnd).unwrap();
            }
        }
    }
    #[test]
    fn native_controls_keep_state_across_theme_changes_and_release_subclasses() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            for _ in 0..10 {
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    w!("Enable &alerts"),
                    WS_OVERLAPPEDWINDOW | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    x,
                    y,
                    240,
                    60,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                SendMessageW(
                    hwnd,
                    BM_SETCHECK,
                    Some(WPARAM(BST_CHECKED.0 as usize)),
                    None,
                );
                for (dark, hc) in [(false, false), (true, false), (true, true), (false, false)] {
                    let theme = DashboardTheme::new(dark, hc);
                    theme.apply_control(hwnd);
                    let _ = UpdateWindow(hwnd);
                    assert_eq!(
                        SendMessageW(hwnd, BM_GETCHECK, None, None).0,
                        BST_CHECKED.0 as isize
                    );
                    let mut data = 0;
                    assert!(
                        GetWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID, Some(&mut data))
                            .as_bool()
                    );
                    assert_eq!((*(data as *const ControlAppearance)).palette, theme.palette);
                }
                DestroyWindow(hwnd).unwrap();
            }
        }
    }
    #[test]
    fn hover_repaints_only_at_boundaries_and_never_changes_checkbox_value() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("Alerts"),
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                x,
                y,
                240,
                40,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            DashboardTheme::new(true, false).apply_control(hwnd);
            let mut data = 0;
            assert!(
                GetWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID, Some(&mut data)).as_bool()
            );
            let _ = ValidateRect(Some(hwnd), None);
            SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
            assert!((*(data as *const ControlAppearance)).hovered);
            assert!(GetUpdateRect(hwnd, None, false).as_bool());
            let _ = ValidateRect(Some(hwnd), None);
            for _ in 0..100 {
                SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
            }
            assert!(!GetUpdateRect(hwnd, None, false).as_bool());
            SendMessageW(hwnd, WM_MOUSELEAVE, None, None);
            assert!(!(*(data as *const ControlAppearance)).hovered);
            assert!(GetUpdateRect(hwnd, None, false).as_bool());
            assert_eq!(SendMessageW(hwnd, BM_GETCHECK, None, None).0, 0);
            DestroyWindow(hwnd).unwrap();
        }
    }

    #[test]
    fn scrollbar_updates_defer_paint_and_cancel_tracking_without_idle_redraws() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("SCROLLBAR"),
                w!(""),
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(SBS_VERT as u32),
                x,
                y,
                18,
                300,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            for dark in [false, true] {
                let theme = DashboardTheme::new(dark, false);
                theme.apply_control(hwnd);
                let info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                    nMin: 0,
                    nMax: 999,
                    nPage: 100,
                    nPos: 250,
                    ..Default::default()
                };
                SetScrollInfo(hwnd, SB_CTL, &info, false);
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
                let dc = GetDC(Some(hwnd));
                // Track edge must stay themed even when Windows is asked for
                // synchronous range redraw or background erasure.
                assert_eq!(GetPixel(dc, 0, 150), theme.palette.background);
                let _ = ValidateRect(Some(hwnd), None);
                SetScrollPos(hwnd, SB_CTL, 300, true);
                assert_eq!(GetPixel(dc, 0, 150), theme.palette.background);
                assert!(GetUpdateRect(hwnd, None, false).as_bool());
                assert_eq!(
                    SendMessageW(hwnd, WM_ERASEBKGND, Some(WPARAM(dc.0 as usize)), None).0,
                    1
                );
                assert_eq!(GetPixel(dc, 0, 150), theme.palette.background);
                let _ = UpdateWindow(hwnd);
                let (_, current) = scroll_geometry(hwnd).unwrap();
                assert_eq!(current.nPos, 300);
                let _ = ValidateRect(Some(hwnd), None);
                for _ in 0..100 {
                    SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(LPARAM(100 << 16)));
                }
                assert!(!GetUpdateRect(hwnd, None, false).as_bool());
                let mut data = 0;
                assert!(
                    GetWindowSubclass(hwnd, Some(control_proc), SUBCLASS_ID, Some(&mut data))
                        .as_bool()
                );
                SendMessageW(hwnd, WM_LBUTTONDOWN, None, Some(LPARAM(2 << 16)));
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Repeat { .. }
                ));
                SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(LPARAM((2 << 16) | 30)));
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Repeat { y: i32::MIN, .. }
                ));
                SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(LPARAM((2 << 16) | 5)));
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Repeat { y: 2, .. }
                ));
                SendMessageW(hwnd, WM_CANCELMODE, None, None);
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Idle
                ));
                assert_ne!(GetCapture(), hwnd);
                let _ = ValidateRect(Some(hwnd), None);
                SendMessageW(hwnd, WM_TIMER, Some(WPARAM(SCROLL_REPEAT)), None);
                assert!(!GetUpdateRect(hwnd, None, false).as_bool());
                let (bar, _) = scroll_geometry(hwnd).unwrap();
                SendMessageW(
                    hwnd,
                    WM_LBUTTONDOWN,
                    None,
                    Some(LPARAM(((bar.xyThumbTop + 2) << 16) as isize)),
                );
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Drag { .. }
                ));
                DashboardTheme::new(false, true).apply_control(hwnd);
                assert!(matches!(
                    (*(data as *const ControlAppearance)).scroll,
                    ScrollGesture::Idle
                ));
                assert_ne!(GetCapture(), hwnd);
                ReleaseDC(Some(hwnd), dc);
            }
            DestroyWindow(hwnd).unwrap();
        }
    }

    #[test]
    fn palettes_and_resources_follow_preferences() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        assert!(Palette::new(true, false).dark);
        assert!(!Palette::new(true, true).dark);
        assert_eq!(Palette::new(false, true).background, unsafe {
            COLORREF(GetSysColor(COLOR_WINDOW))
        });
        assert_ne!(
            Palette::new(true, false).text,
            Palette::new(true, false).background
        );
        for _ in 0..100 {
            for (dark, hc) in [(false, false), (true, false), (true, true)] {
                let theme = DashboardTheme::new(dark, hc);
                assert!(!theme.background_brush().0.is_null());
            }
        }
    }

    #[test]
    fn themed_controls_do_not_erase_a_light_surface_before_custom_paint() {
        let _guard = crate::ui::NATIVE_TEST_LOCK.lock().unwrap();
        let (x, y) = crate::test_display::origin();
        unsafe {
            for class in [w!("BUTTON"), w!("COMBOBOX")] {
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    w!("Test"),
                    WS_POPUP,
                    x,
                    y,
                    100,
                    30,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                let theme = DashboardTheme::new(true, false);
                theme.apply_control(hwnd);
                let screen = GetDC(None);
                let dc = CreateCompatibleDC(Some(screen));
                let bitmap = CreateCompatibleBitmap(screen, 100, 30);
                let old = SelectObject(dc, bitmap.into());
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 30,
                };
                FillRect(dc, &rect, color_brush(dc, theme.palette.background));
                assert_eq!(
                    SendMessageW(hwnd, WM_ERASEBKGND, Some(WPARAM(dc.0 as usize)), None).0,
                    1
                );
                assert_eq!(GetPixel(dc, 50, 15), theme.palette.background);
                SelectObject(dc, old);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                DestroyWindow(hwnd).unwrap();
            }
        }
    }
}
