//! Single instance: a second launch signals the running one to show its window.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::w;

pub struct Instance {
    _mutex: HANDLE,
    event: HANDLE,
}

/// Returns `None` (after waking the running instance) if Yusic is already running.
pub fn acquire() -> Option<Instance> {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\Yusic.Instance")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\Yusic.Show")) {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
            }
            let _ = CloseHandle(mutex);
            return None;
        }
        let event = CreateEventW(None, false, false, w!("Local\\Yusic.Show")).ok()?;
        Some(Instance { _mutex: mutex, event })
    }
}

impl Instance {
    /// Calls `f` (on a background thread) whenever another launch asks us to show.
    pub fn on_activate(&self, f: impl Fn() + Send + 'static) {
        let ev = self.event.0 as usize;
        std::thread::Builder::new()
            .name("yusic-instance".into())
            .spawn(move || loop {
                unsafe { WaitForSingleObject(HANDLE(ev as _), INFINITE) };
                f();
            })
            .ok();
    }
}
