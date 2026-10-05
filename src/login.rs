//! Google sign-in in a temporary WebView2 window. Once YouTube Music loads
//! signed in, its cookies are handed back and the webview is destroyed, so
//! no browser processes stay around during normal use.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::Result;
use slint::ComponentHandle;
use wry::{NewWindowResponse, PageLoadEvent, WebContext, WebView, WebViewBuilder};

slint::slint! {
    export component LoginWindow inherits Window {
        in property <image> app-icon;
        title: "Sign in - Yusic";
        icon: app-icon;
        background: #ffffff;
        preferred-width: 520px;
        preferred-height: 720px;
        min-width: 400px;
        min-height: 500px;
    }
}

const LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?ltmpl=music&service=youtube&passive=true&continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop%26next%3Dhttps%253A%252F%252Fmusic.youtube.com%252F";

type OnCookie = Box<dyn FnOnce(String)>;

struct Login {
    // Field order matters: the webview must go before its window.
    webview: Option<WebView>,
    context: WebContext,
    window: LoginWindow,
    on_cookie: Option<OnCookie>,
    on_error: Option<OnError>,
}

type OnError = Box<dyn FnOnce(String)>;

thread_local! {
    static LOGIN: RefCell<Option<Login>> = const { RefCell::new(None) };
}

pub fn profile_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("WebView2")
}

/// Deletes the sign-in browser profile. It only holds a session we have
/// already copied, and keeping it would leave a second live login on disk.
pub fn clear_profile(data_dir: &Path) {
    let _ = std::fs::remove_dir_all(profile_dir(data_dir));
}

/// Opens the sign-in window (or focuses it). `on_cookie` receives the
/// YouTube cookie header once the user is signed in; `on_error` is called if
/// the embedded browser can't start.
pub fn open(
    data_dir: &Path,
    icon: slint::Image,
    on_cookie: impl FnOnce(String) + 'static,
    on_error: impl FnOnce(String) + 'static,
) -> Result<()> {
    if let Some(window) = LOGIN.with(|l| l.borrow().as_ref().map(|l| l.window.as_weak())) {
        if let Some(w) = window.upgrade() {
            w.show()?;
            crate::win::bring_to_front(w.window());
        }
        return Ok(());
    }
    let window = LoginWindow::new()?;
    window.set_app_icon(icon);
    window.show()?;
    window.window().on_close_requested(|| {
        // Defer: dropping the window inside its own callback is not allowed.
        let _ = slint::invoke_from_event_loop(close);
        slint::CloseRequestResponse::HideWindow
    });
    LOGIN.with(|l| {
        *l.borrow_mut() = Some(Login {
            webview: None,
            context: WebContext::new(Some(profile_dir(data_dir))),
            window,
            on_cookie: Some(Box::new(on_cookie)),
            on_error: Some(Box::new(on_error)),
        })
    });
    // The native window only exists once the event loop has processed show().
    attach_webview(0);
    Ok(())
}

fn attach_webview(attempt: u32) {
    let result = LOGIN.with(|l| {
        let mut l = l.borrow_mut();
        let Some(login) = l.as_mut() else { return Ok(true) };
        let handle = login.window.window().window_handle();
        if raw_window_handle::HasWindowHandle::window_handle(&handle).is_err() {
            return Ok(false);
        }
        let webview = WebViewBuilder::new_with_web_context(&mut login.context)
            .with_url(LOGIN_URL)
            .with_focused(true)
            // Keep everything in this one window.
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_on_page_load_handler(|event, url| {
                if matches!(event, PageLoadEvent::Finished) && url.starts_with("https://music.youtube.com") {
                    // Reading cookies pumps WebView2 messages; never do it inside its callback.
                    let _ = slint::invoke_from_event_loop(harvest);
                }
            })
            .build(&handle)
            .map_err(|e| e.to_string())?;
        login.webview = Some(webview);
        crate::win::apply_app_icon(login.window.window());
        Ok::<bool, String>(true)
    });
    match result {
        Ok(true) => {}
        Ok(false) if attempt < 40 => {
            slint::Timer::single_shot(std::time::Duration::from_millis(50), move || attach_webview(attempt + 1));
        }
        Ok(false) => fail("the sign-in window did not open".into()),
        Err(e) => fail(e),
    }
}

fn fail(msg: String) {
    let on_error = LOGIN.with(|l| l.borrow_mut().as_mut().and_then(|l| l.on_error.take()));
    close();
    if let Some(cb) = on_error {
        cb(msg);
    }
}

fn harvest() {
    let cookie = LOGIN.with(|l| {
        let l = l.borrow();
        let login = l.as_ref()?;
        let cookies = login.webview.as_ref()?.cookies_for_url("https://music.youtube.com").ok()?;
        let header = cookies
            .iter()
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; ");
        let signed_in = cookies.iter().any(|c| c.name() == "SAPISID" || c.name() == "__Secure-3PAPISID");
        signed_in.then_some(header)
    });
    let Some(cookie) = cookie else { return };
    let on_cookie = LOGIN.with(|l| l.borrow_mut().as_mut().and_then(|l| l.on_cookie.take()));
    close();
    if let Some(cb) = on_cookie {
        cb(cookie);
    }
}

/// Destroys the webview (its browser processes exit) and the window.
pub fn close() {
    let login = LOGIN.with(|l| l.borrow_mut().take());
    if let Some(login) = login {
        let Login { webview, context, window, .. } = login;
        drop(webview);
        drop(context);
        let _ = window.hide();
    }
}
