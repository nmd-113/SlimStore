use crate::app::with_window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::Cell;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{Dwm::*, Gdi::*},
    UI::{
        Controls::MARGINS,
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Input::KeyboardAndMouse::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

// Keep in sync with the header's caption and drag button dimensions in main.slint.
const HEADER: f32 = 60.;
const CAPTION: f32 = 44.;
const DRAG: f32 = 36.;
const SUBCLASS: usize = 0x4150_5051;

thread_local! {
    static MAX_PRESS: Cell<bool> = const { Cell::new(false) };
    static CAPTION_CLICK: Cell<Option<(i32, POINT)>> = const { Cell::new(None) };
}

fn hwnd(window: &slint::Window) -> Option<HWND> {
    let provider = window.window_handle();
    let RawWindowHandle::Win32(handle) = provider.window_handle().ok()?.as_raw() else {
        return None;
    };
    Some(handle.hwnd.get() as HWND)
}

pub enum Action {
    Minimize,
    ToggleMaximize,
}

pub fn command(window: &slint::Window, action: Action) {
    if let Some(hwnd) = hwnd(window) {
        unsafe {
            let command = match action {
                Action::Minimize => SC_MINIMIZE,
                Action::ToggleMaximize if IsZoomed(hwnd) != 0 => SC_RESTORE,
                Action::ToggleMaximize => SC_MAXIMIZE,
            };
            PostMessageW(hwnd, WM_SYSCOMMAND, command as usize, 0);
        }
    }
}

fn attribute(hwnd: HWND, id: u32, value: i32) -> bool {
    unsafe { DwmSetWindowAttribute(hwnd, id, (&value as *const i32).cast(), 4) >= 0 }
}

pub fn update_theme(window: &slint::Window, dark: bool) {
    if let Some(hwnd) = hwnd(window) {
        if !attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, i32::from(dark)) {
            attribute(hwnd, 19, i32::from(dark));
        }
        let native_border = attribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            if dark { 0x0049_3b33 } else { 0x00dd_d5ce },
        );
        with_window(|ui| ui.set_client_border(!native_border));
    }
}

pub fn install(window: &slint::Window, dark: bool) {
    if let Some(hwnd) = hwnd(window) {
        update_theme(window, dark);
        attribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, DWMWCP_ROUND);
        unsafe {
            let margin = MARGINS {
                cyTopHeight: 1,
                ..Default::default()
            };
            DwmExtendFrameIntoClientArea(hwnd, &margin);
            if SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS, 0) != 0 {
                // Keep the normal window styles used by the shell. Winit still
                // supplies the frameless client area in WM_NCCALCSIZE.
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_CAPTION as isize);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
}

fn point(lparam: LPARAM) -> POINT {
    POINT {
        x: lparam as i16 as i32,
        y: (lparam >> 16) as i16 as i32,
    }
}

fn frame_size(dpi: u32) -> i32 {
    unsafe {
        GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

fn fit_snapped_client(hwnd: HWND, bounds: &mut RECT) {
    // A keyboard half-screen snap can include the invisible sizing frame on
    // all sides. Winit already handles maximize and drag-to-edge geometry.
    unsafe {
        if IsZoomed(hwnd) != 0 {
            return;
        }
        let monitor = MonitorFromRect(bounds, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut monitor_info) == 0 {
            return;
        }
        let work = monitor_info.rcWork;
        let frame = frame_size(GetDpiForWindow(hwnd).max(96));
        if frame <= 0 || bounds.top != work.top - frame || bounds.bottom != work.bottom + frame {
            return;
        }
        let half_width = (work.right - work.left) / 2;
        if (bounds.right - bounds.left - 2 * frame - half_width).abs() > 1 {
            return;
        }
        if bounds.left == work.left - frame || bounds.right == work.right + frame {
            bounds.left += frame;
            bounds.right -= frame;
            bounds.top = work.top;
            bounds.bottom = work.bottom;
        }
    }
}

fn hit_test(hwnd: HWND, p: POINT) -> u32 {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect) == 0
            || p.x < 0
            || p.y < 0
            || p.x >= rect.right
            || p.y >= rect.bottom
        {
            return HTNOWHERE;
        }
        let dpi = GetDpiForWindow(hwnd).max(96);
        let (scale, header) = with_window(|ui| {
            use slint::ComponentHandle;
            (ui.window().scale_factor(), ui.get_header_height())
        })
        .unwrap_or((dpi as f32 / 96., HEADER));
        let px = |logical: f32| (scale * logical).round() as i32;
        let right = rect.right - p.x;
        let bottom = rect.bottom - p.y;
        if IsZoomed(hwnd) == 0 {
            let edge = frame_size(dpi);
            let horizontal = if p.x < edge {
                -1
            } else if right <= edge {
                1
            } else {
                0
            };
            let vertical = if p.y < edge {
                -1
            } else if bottom <= edge {
                1
            } else {
                0
            };
            match (horizontal, vertical) {
                (-1, -1) => return HTTOPLEFT,
                (1, -1) => return HTTOPRIGHT,
                (-1, 1) => return HTBOTTOMLEFT,
                (1, 1) => return HTBOTTOMRIGHT,
                (-1, _) => return HTLEFT,
                (1, _) => return HTRIGHT,
                (_, -1) => return HTTOP,
                (_, 1) => return HTBOTTOM,
                _ => {}
            }
        }
        if p.y < px(header) {
            if right > px(CAPTION) && right <= px(2. * CAPTION) {
                return HTMAXBUTTON;
            }
            if right > px(3. * CAPTION) && right <= px(3. * CAPTION + DRAG) {
                return HTCAPTION;
            }
        }
        if with_window(|ui| ui.invoke_sidebar_caption(p.x as f32 / scale, p.y as f32 / scale))
            == Some(true)
        {
            return HTCAPTION;
        }
        HTCLIENT
    }
}

fn max_hover(value: bool) {
    with_window(|ui| ui.set_maximize_hover(value));
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        match message {
            WM_NCCALCSIZE if wparam != 0 => {
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
                fit_snapped_client(hwnd, &mut params.rgrc[0]);
                return result;
            }
            WM_STYLECHANGING if wparam as i32 == GWL_STYLE => {
                (*(lparam as *mut STYLESTRUCT)).styleNew |= WS_CAPTION;
            }
            WM_NCHITTEST => {
                let mut p = point(lparam);
                if ScreenToClient(hwnd, &mut p) != 0 {
                    let hit = hit_test(hwnd, p);
                    if hit != HTNOWHERE {
                        return hit as LRESULT;
                    }
                }
            }
            WM_NCMOUSEMOVE => {
                let hover = wparam == HTMAXBUTTON as usize;
                max_hover(hover);
                if hover {
                    let mut tracking = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_NONCLIENT | TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut tracking);
                }
            }
            WM_NCMOUSELEAVE => {
                max_hover(false);
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTCAPTION as usize => {
                // Handle either a native double-click or a pair of caption presses.
                let now = GetMessageTime();
                let position = point(lparam);
                let previous = CAPTION_CLICK.replace(Some((now, position)));
                if message == WM_NCLBUTTONDBLCLK
                    || previous.is_some_and(|(time, last)| {
                        now.wrapping_sub(time) as u32 <= GetDoubleClickTime()
                            && (last.x - position.x).abs() <= GetSystemMetrics(SM_CXDOUBLECLK)
                            && (last.y - position.y).abs() <= GetSystemMetrics(SM_CYDOUBLECLK)
                    })
                {
                    CAPTION_CLICK.set(None);
                    with_window(|ui| {
                        use slint::ComponentHandle;
                        command(ui.window(), Action::ToggleMaximize);
                    });
                    return 0;
                }
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTMAXBUTTON as usize => {
                // Win32 cannot track a button it did not draw. Capture the complete gesture
                // and let the window manager carry out the resulting system command.
                MAX_PRESS.set(true);
                SetCapture(hwnd);
                with_window(|ui| ui.set_maximize_pressed(true));
                return 0;
            }
            WM_MOUSEMOVE if MAX_PRESS.get() => {
                let inside = hit_test(hwnd, point(lparam)) == HTMAXBUTTON;
                max_hover(inside);
                with_window(|ui| ui.set_maximize_pressed(inside));
                return 0;
            }
            WM_LBUTTONUP if MAX_PRESS.replace(false) => {
                ReleaseCapture();
                with_window(|ui| {
                    use slint::ComponentHandle;
                    ui.set_maximize_pressed(false);
                    if hit_test(hwnd, point(lparam)) == HTMAXBUTTON {
                        command(ui.window(), Action::ToggleMaximize);
                    }
                });
                return 0;
            }
            WM_CAPTURECHANGED | WM_CANCELMODE if MAX_PRESS.replace(false) => {
                with_window(|ui| ui.set_maximize_pressed(false));
                max_hover(false);
            }
            WM_SIZE if wparam != SIZE_MINIMIZED as usize => {
                with_window(|ui| ui.set_native_maximized(wparam == SIZE_MAXIMIZED as usize));
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(window_proc), SUBCLASS);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}

pub fn maximized(window: &slint::Window) -> bool {
    hwnd(window).is_some_and(|hwnd| unsafe { IsZoomed(hwnd) != 0 })
}
