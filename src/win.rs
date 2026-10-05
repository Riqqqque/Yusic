//! Small Win32 helpers for the main window.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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
    let Some(h) = hwnd(window) else { return };
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else { return };
        let inst = HINSTANCE(module.0);
        for (kind, mx, my) in [(ICON_SMALL, SM_CXSMICON, SM_CYSMICON), (ICON_BIG, SM_CXICON, SM_CYICON)] {
            let icon = LoadImageW(
                Some(inst),
                PCWSTR(1 as *const u16),
                IMAGE_ICON,
                GetSystemMetrics(mx),
                GetSystemMetrics(my),
                LR_DEFAULTCOLOR,
            );
            if let Ok(icon) = icon {
                SendMessageW(h, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// Un-minimizes (without un-maximizing) and focuses the window.
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
