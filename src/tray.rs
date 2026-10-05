//! Notification-area icon. Events arrive on tray-icon's thread and are
//! forwarded to the Slint event loop.

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::app::app;

pub fn create() -> Result<TrayIcon> {
    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id("show", "Show Yusic", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("play", "Play / Pause", true, None),
        &MenuItem::with_id("next", "Next", true, None),
        &MenuItem::with_id("prev", "Previous", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("quit", "Quit", true, None),
    ])?;
    let icon = Icon::from_rgba(crate::icon::rgba(32), 32, 32)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Yusic")
        .with_icon(icon)
        .build()?;

    MenuEvent::set_event_handler(Some(|e: MenuEvent| {
        let id = e.id.0.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(a) = app() else { return };
            match id.as_str() {
                "show" => a.show_window(),
                "play" => a.tray_toggle_play(),
                "next" => a.tray_next(),
                "prev" => a.tray_prev(),
                "quit" => {
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        });
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = slint::invoke_from_event_loop(|| {
                if let Some(a) = app() {
                    a.toggle_window();
                }
            });
        }
    }));
    Ok(tray)
}
