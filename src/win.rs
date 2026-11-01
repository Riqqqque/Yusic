//! Small Win32 helpers for the main window.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, IsIconic, LR_DEFAULTCOLOR, LoadImageW,
    MB_ICONERROR, MB_OK, MessageBoxW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SW_RESTORE,
    SendMessageW, SetForegroundWindow, ShowWindow, WM_SETICON,
};
use windows::core::{HSTRING, PCWSTR};

fn hwnd(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?.as_raw();
    match raw {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as _)),
        _ => None,
    }
}

/// Sets the title-bar and Alt+Tab icon from the icon embedded in the exe
/// (resource id 1, written by build.rs).
pub fn apply_app_icon(window: &slint::Window) {
    // Loaded once; the window keeps using the same handles.
    static ICONS: std::sync::OnceLock<[isize; 2]> = std::sync::OnceLock::new();
    let Some(h) = hwnd(window) else { return };
    let icons = ICONS.get_or_init(|| unsafe {
        let Ok(module) = GetModuleHandleW(None) else { return [0; 2] };
        let inst = HINSTANCE(module.0);
        [(SM_CXSMICON, SM_CYSMICON), (SM_CXICON, SM_CYICON)].map(|(mx, my)| {
            LoadImageW(Some(inst), PCWSTR(1 as *const u16), IMAGE_ICON, GetSystemMetrics(mx), GetSystemMetrics(my), LR_DEFAULTCOLOR)
                .map(|i| i.0 as isize)
                .unwrap_or(0)
        })
    });
    for (kind, icon) in [ICON_SMALL, ICON_BIG].into_iter().zip(icons) {
        if *icon != 0 {
            unsafe {
                SendMessageW(h, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(*icon)));
            }
        }
    }
}

/// Runs Yusic (and the yt-dlp/JS processes it starts) below normal priority so
/// games and other foreground work always win the CPU. Audio playback is not
/// affected: Windows schedules the audio render thread through MMCSS.
pub fn lower_priority() {
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// True while a fullscreen app (a game, a video) is in front on the monitor
/// Yusic's window is on, so nothing Yusic draws can be seen. Yusic on another
/// monitor than the fullscreen app stays live (lyrics keep scrolling there).
pub fn covered_by_fullscreen(window: &slint::Window) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsWindowVisible,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };
    let Some(me) = hwnd(window) else { return false };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == me || !IsWindowVisible(fg).as_bool() {
            return false;
        }
        // Overlays (game bar, Discord, recorders) span the screen but are
        // see-through; they don't hide Yusic.
        let ex = GetWindowLongPtrW(fg, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(fg, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        if cloaked != 0 {
            return false;
        }
        // The desktop spans the monitor too, but it's behind everything.
        let mut class = [0u16; 32];
        let n = GetClassNameW(fg, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if class == "Progman" || class == "WorkerW" {
            return false;
        }
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONULL);
        if mon.is_invalid() || mon != MonitorFromWindow(me, MONITOR_DEFAULTTONEAREST) {
            return false;
        }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut r = RECT::default();
        if !GetMonitorInfoW(mon, &mut info).as_bool() || GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        let m = info.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

thread_local! {
    static REPAINT_UI: std::cell::RefCell<Option<slint::Weak<crate::AppWindow>>> = const { std::cell::RefCell::new(None) };
    static OLD_WNDPROC: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static HOOKED: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static FLIP_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The software renderer only redraws what changed and presents just that.
/// When Windows has thrown the window's pixels away (shown again from the
/// tray, restored, uncovered after a fullscreen game, display changes) parts
/// of the window stay blank. This repaints all of it once.
pub fn full_repaint(ui: &crate::AppWindow) {
    use slint::ComponentHandle;
    REPAINT_UI.with(|w| *w.borrow_mut() = Some(ui.as_weak()));
    schedule_flip();
}

fn schedule_flip() {
    if FLIP_PENDING.with(|p| p.replace(true)) {
        return;
    }
    let _ = slint::invoke_from_event_loop(|| {
        FLIP_PENDING.with(|p| p.set(false));
        let ui = REPAINT_UI.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade()));
        if let Some(ui) = ui {
            use slint::ComponentHandle;
            ui.set_repaint_flip(!ui.get_repaint_flip());
            ui.window().request_redraw();
        }
    });
}

/// Watches the window's WM_PAINT: a paint with a real update region means
/// Windows invalidated pixels (Yusic's own redraws have none), so repaint
/// everything. Safe to call again; re-hooks if the window was recreated.
pub fn install_paint_hook(ui: &crate::AppWindow) {
    use slint::ComponentHandle;
    use windows::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};
    REPAINT_UI.with(|w| *w.borrow_mut() = Some(ui.as_weak()));
    let Some(h) = hwnd(ui.window()) else { return };
    if HOOKED.with(|x| x.get()) == h.0 as isize {
        return;
    }
    unsafe {
        let old = SetWindowLongPtrW(h, GWLP_WNDPROC, paint_hook as *const () as isize);
        if old != 0 {
            OLD_WNDPROC.with(|o| o.set(old));
            HOOKED.with(|x| x.set(h.0 as isize));
        }
    }
}

unsafe extern "system" fn paint_hook(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::GetUpdateRect;
    use windows::Win32::UI::WindowsAndMessaging::{CallWindowProcW, DefWindowProcW, WM_ENDSESSION, WM_PAINT, WNDPROC};
    if msg == WM_ENDSESSION && w.0 != 0 {
        // Shutdown or sign-out: the process is about to be killed.
        if let Some(a) = crate::app::app() {
            a.remember_window();
        }
    }
    if msg == WM_PAINT {
        let mut r = RECT::default();
        if unsafe { GetUpdateRect(h, Some(&mut r), false) }.as_bool() {
            schedule_flip();
        }
    }
    let old = OLD_WNDPROC.with(|o| o.get());
    if old == 0 {
        return unsafe { DefWindowProcW(h, msg, w, l) };
    }
    let old: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(old) };
    unsafe { CallWindowProcW(old, h, msg, w, l) }
}

/// Slint applies a new display scale at once but keeps the old window size
/// until a resize arrives, and none comes in some cases (the renderer skips
/// frames meanwhile). Shortly after a scale change, tell Slint the window's
/// real size and redraw. Only Slint hears about it: asking Windows to resize
/// here would fight a maximize or restore in progress.
pub fn after_scale_change(ui: &crate::AppWindow) {
    use slint::ComponentHandle;
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(h) = hwnd(ui.window()) else { return };
        let mut r = windows::Win32::Foundation::RECT::default();
        let known = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(h, &mut r) }.is_ok();
        let (w, ht) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
        if known && w >= 1.0 && ht >= 1.0 && !unsafe { IsIconic(h) }.as_bool() {
            let scale = ui.window().scale_factor();
            let size = slint::LogicalSize::new(w / scale, ht / scale);
            ui.window().dispatch_event(slint::platform::WindowEvent::Resized { size });
        }
        full_repaint(&ui);
    });
}

/// Repaints after events where Windows may have dropped the contents but no
/// paint arrives (e.g. a DPI change).
pub fn repaint_on_window_events(ui: &crate::AppWindow) {
    use slint::ComponentHandle;
    use slint::winit_030::winit::event::WindowEvent;
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, WindowEvent::Occluded(false) | WindowEvent::ScaleFactorChanged { .. }) {
            if let Some(ui) = weak.upgrade() {
                full_repaint(&ui);
                if matches!(event, WindowEvent::ScaleFactorChanged { .. }) {
                    after_scale_change(&ui);
                }
            }
        }
        // Minimize and restore arrive as resizes; the progress tick follows
        // them. Focus means the window is in view again (e.g. after a game).
        if matches!(event, WindowEvent::Resized(_) | WindowEvent::Focused(true)) {
            slint::Timer::single_shot(std::time::Duration::ZERO, || {
                if let Some(a) = crate::app::app() {
                    a.window_state_changed();
                }
            });
        }
        EventResult::Propagate
    });
}

/// Puts text on the clipboard.
pub fn copy_text(text: &str) -> bool {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = (|| -> Option<()> {
            let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).is_err() {
                // Still ours when the clipboard didn't take it.
                let _ = windows::Win32::Foundation::GlobalFree(Some(mem));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

/// Opens a web link in the default browser.
pub fn open_url(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from(url),
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Un-minimizes (without un-maximizing) and focuses the window.
/// The largest window content that fits the primary monitor, in logical
/// pixels. A window can't be larger than the screen: asking for that leaves
/// Slint drawing a bigger window than the one it has a buffer for, and the
/// software renderer panics.
pub fn max_window_size() -> Option<(f32, f32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut info).as_bool() {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = dpi_x.max(96) as f32 / 96.0;
        let r = info.rcWork;
        // Room for the window frame and title bar.
        Some(((r.right - r.left) as f32 / scale - 16.0, (r.bottom - r.top) as f32 / scale - 48.0))
    }
}

pub fn is_minimized(window: &slint::Window) -> bool {
    hwnd(window).is_some_and(|h| unsafe { IsIconic(h).as_bool() })
}

pub fn bring_to_front(window: &slint::Window) {
    if let Some(h) = hwnd(window) {
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
    }
}

pub fn error_box(msg: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from("Yusic"), MB_OK | MB_ICONERROR);
    }
}
