#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod auth;
mod icon;
mod icon_raster;
mod images;
mod login;
mod model;
mod player;
mod resolver;
mod settings;
mod single;
mod tray;
mod win;
mod yt;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{CloseRequestResponse, ComponentHandle, PhysicalSize};

use app::{App, Route};
use resolver::Resolver;
use settings::Settings;
use yt::Yt;

slint::include_modules!();

#[derive(Default)]
struct Args {
    minimized: bool,
    route: Option<Route>,
    snapshot: Option<PathBuf>,
    delay_ms: u64,
    play: Option<String>,
    exit_after: Option<u64>,
    volume: Option<f32>,
    sign_in: bool,
    lyrics: bool,
}

impl Args {
    fn parse() -> Args {
        let mut a = Args { delay_ms: 5000, ..Default::default() };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--minimized" => a.minimized = true,
                "--route" => a.route = it.next().map(|r| Route::parse(&r)),
                "--snapshot" => a.snapshot = it.next().map(PathBuf::from),
                "--delay" => a.delay_ms = it.next().and_then(|d| d.parse().ok()).unwrap_or(5000),
                "--play" => a.play = it.next(),
                "--exit-after" => a.exit_after = it.next().and_then(|d| d.parse().ok()),
                "--volume" => a.volume = it.next().and_then(|v| v.parse().ok()),
                "--sign-in" => a.sign_in = true,
                "--lyrics" => a.lyrics = true,
                _ => {}
            }
        }
        a
    }
}

fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("YUSIC_DATA_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Yusic")
}

fn main() {
    if let Err(e) = run() {
        win::error_box(&format!("{e:#}"));
    }
}

/// scripts\deploy.ps1 renames a running exe aside; remove those leftovers.
fn remove_old_copies() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy().to_lowercase();
        if name.starts_with("yusic.old") && name.ends_with(".exe") && e.path() != exe {
            // Fails harmlessly if that copy is still running.
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    remove_old_copies();
    let testing = args.snapshot.is_some() || args.exit_after.is_some();
    let save_settings = !testing && args.volume.is_none();
    let instance = if testing {
        None
    } else {
        match single::acquire() {
            Some(i) => Some(i),
            None => return Ok(()),
        }
    };

    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let settings = Settings::load(&dir);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("yusic-io")
        .enable_all()
        .build()?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("Yusic/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()?;

    let ytdlp = Resolver::find_ytdlp()
        .context("yt-dlp.exe was not found. Place it next to yusic.exe (or in a 'tools' folder beside it).")?;
    let res = Arc::new(Resolver::new(ytdlp, &dir, http.clone())?);
    let yt = Arc::new(Yt::new(&dir, http.clone())?);
    // A sign-in window from a previous run may have left its browser profile.
    login::clear_profile(&dir);

    let ui = AppWindow::new()?;
    images::init(http, rt.handle().clone());
    ui.set_app_icon(icon::slint_image(64));
    if settings.width >= 400 && settings.height >= 300 {
        ui.window().set_size(PhysicalSize::new(settings.width, settings.height));
    }

    let app = App::install(&ui, rt.handle().clone(), yt, res, dir.clone(), settings)?;
    if let Some(v) = args.volume {
        app.set_volume(v);
    }

    ui.window().on_close_requested(|| {
        if let Some(a) = app::app() {
            a.set_visible(false);
        }
        CloseRequestResponse::HideWindow
    });

    let tray = tray::create().ok();
    if let Some(inst) = &instance {
        inst.on_activate(|| {
            let _ = slint::invoke_from_event_loop(|| {
                if let Some(a) = app::app() {
                    a.show_window();
                }
            });
        });
    }

    if args.minimized {
        app.set_visible(false);
    } else {
        ui.show()?;
        if app.settings.borrow().maximized {
            ui.window().set_maximized(true);
        }
    }
    app.restore_session();

    // The native window (and with it the real display scale) only exists once
    // the event loop runs; load the first page after that so images are
    // decoded at the right resolution.
    let startup = std::rc::Rc::new(slint::Timer::default());
    let started = std::time::Instant::now();
    let startup_weak = std::rc::Rc::downgrade(&startup);
    let (route, play, sign_in, minimized, lyrics) =
        (args.route.clone(), args.play.clone(), args.sign_in, args.minimized, args.lyrics);
    let ui_weak = ui.as_weak();
    startup.start(slint::TimerMode::Repeated, Duration::from_millis(15), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let realized = raw_window_handle::HasWindowHandle::window_handle(&ui.window().window_handle()).is_ok();
        if !(realized || minimized || started.elapsed() > Duration::from_secs(3)) {
            return;
        }
        if let Some(t) = startup_weak.upgrade() {
            t.stop();
        }
        let Some(app) = app::app() else { return };
        win::apply_app_icon(ui.window());
        app.rescale();
        app.navigate(route.clone().unwrap_or(Route::Home), false);
        if sign_in {
            app.sign_in();
        }
        if let Some(id) = &play {
            app.play_items(
                vec![model::Item { id: id.clone(), title: id.clone(), ..Default::default() }],
                0,
                true,
            );
        }
        if lyrics {
            ui.set_np_tab(1);
            ui.set_np_open(true);
        }
    });

    if let Some(path) = args.snapshot.clone() {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(args.delay_ms), move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(e) = save_snapshot(&ui, &path) {
                    eprintln!("snapshot failed: {e:#}");
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    if let Some(secs) = args.exit_after {
        slint::Timer::single_shot(Duration::from_secs(secs), || {
            let _ = slint::quit_event_loop();
        });
    }

    slint::run_event_loop_until_quit()?;

    {
        let mut s = app.settings.borrow_mut();
        let size = ui.window().size();
        s.maximized = ui.window().is_maximized();
        if !s.maximized && size.width >= 400 {
            s.width = size.width;
            s.height = size.height;
        }
        s.sidebar_wide = ui.get_sidebar_wide();
        if save_settings {
            s.save(&dir);
        }
    }
    app.shutdown();
    // Give a closed sign-in window's browser processes a moment to exit.
    for _ in 0..10 {
        if !login::profile_dir(&dir).exists() {
            break;
        }
        login::clear_profile(&dir);
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(tray);
    rt.shutdown_timeout(Duration::from_millis(300));
    Ok(())
}

fn save_snapshot(ui: &AppWindow, path: &std::path::Path) -> Result<()> {
    let buf = ui.window().take_snapshot()?;
    let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec())
        .context("bad snapshot buffer")?;
    img.save(path)?;
    Ok(())
}
