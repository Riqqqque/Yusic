#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod auth;
mod icon;
mod icon_raster;
mod images;
mod install;
mod login;
mod model;
mod player;
mod resolver;
mod settings;
mod single;
mod tools;
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
    uninstall: bool,
    wait_for: Option<u32>,
    /// Visual QA: comma-separated UI states to show (banner, toast, account, suggest).
    ui_states: Vec<String>,
    size: Option<(u32, u32)>,
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
                "--uninstall" => a.uninstall = true,
                "--wait-for" => a.wait_for = it.next().and_then(|p| p.parse().ok()),
                "--ui" => a.ui_states = it.next().map(|s| s.split(',').map(str::to_owned).collect()).unwrap_or_default(),
                "--size" => {
                    a.size = it.next().and_then(|s| {
                        let (w, h) = s.split_once('x')?;
                        Some((w.parse().ok()?, h.parse().ok()?))
                    })
                }
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
    if let Some(pid) = args.wait_for {
        // Restarting after an update: let the old instance finish exiting.
        install::wait_for_process(pid);
    }
    remove_old_copies();
    win::lower_priority();
    if args.uninstall {
        install::uninstall(&data_dir());
        return Ok(());
    }
    let testing = args.snapshot.is_some() || args.exit_after.is_some();
    // Test runs clean up the data folder on start; never do that under a
    // running copy's feet.
    if testing && std::env::var_os("YUSIC_DATA_DIR").is_none() && single::is_running() {
        eprintln!("Yusic is running; set YUSIC_DATA_DIR for test runs.");
        std::process::exit(3);
    }
    if !testing && install::maybe_install(args.minimized)? {
        return Ok(());
    }
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

    let res = Arc::new(Resolver::new(tools::find(&dir), &dir, http.clone())?);
    let yt = Arc::new(Yt::new(&dir, http.clone())?);
    // A sign-in window from a previous run may have left its browser profile.
    login::clear_profile(&dir);

    let ui = AppWindow::new()?;
    images::init(http.clone(), rt.handle().clone());
    ui.set_app_icon(icon::slint_image(64));
    if let Some((w, h)) = args.size {
        ui.window().set_size(slint::LogicalSize::new(w as f32, h as f32));
    } else if settings.width >= 400 && settings.height >= 300 {
        ui.window().set_size(PhysicalSize::new(settings.width, settings.height));
    }

    let app = App::install(&ui, rt.handle().clone(), yt, res.clone(), dir.clone(), http.clone(), settings)?;
    app.persist.set(save_settings);
    if let Some(v) = args.volume {
        app.set_volume(v);
    }

    win::repaint_on_window_events(&ui);
    ui.window().on_close_requested(|| {
        if let Some(a) = app::app() {
            if !a.settings.borrow().close_to_tray {
                let _ = slint::quit_event_loop();
                return CloseRequestResponse::HideWindow;
            }
            // Most sessions end hidden in the tray; save now, not only on Quit.
            a.remember_window();
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
    start_background_jobs(&rt, &res, &dir, &http, testing);
    let update_watch = watch_for_new_version(&ui);

    // The native window (and with it the real display scale) only exists once
    // the event loop runs; load the first page after that so images are
    // decoded at the right resolution.
    let startup = std::rc::Rc::new(slint::Timer::default());
    let started = std::time::Instant::now();
    let startup_weak = std::rc::Rc::downgrade(&startup);
    let (route, play, sign_in, minimized, lyrics) =
        (args.route.clone(), args.play.clone(), args.sign_in, args.minimized, args.lyrics);
    let ui_states = args.ui_states.clone();
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
        win::install_paint_hook(&ui);
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
        apply_ui_states(&ui, &ui_states);
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

    app.remember_window();
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
    drop(update_watch);
    rt.shutdown_timeout(Duration::from_millis(300));
    Ok(())
}

/// Puts the window into states that are otherwise hard to reach, for
/// screenshots (`--ui banner,toast,account,suggest,queue`).
fn apply_ui_states(ui: &AppWindow, states: &[String]) {
    use slint::{ModelRc, SharedString, VecModel};
    for s in states {
        match s.as_str() {
            "banner" => ui.set_update_ready(true),
            "toast" => ui.set_toast(
                "Couldn't play \"A Fairly Long Song Title (feat. Someone Else)\", skipping: the file could not be decoded".into(),
            ),
            "account" => {
                ui.set_signed_in(true);
                let demo: Vec<CardData> = [
                    ("Liked music", "Auto playlist"),
                    ("Road trip", "Playlist • 48 songs"),
                    ("A playlist with a really quite long name that should elide", "Playlist • 112 songs"),
                    ("Focus", "Playlist • 31 songs"),
                ]
                .iter()
                .map(|(t, s)| CardData { title: (*t).into(), subtitle: (*s).into(), id: "x".into(), ..Default::default() })
                .collect();
                ui.set_side_playlists(ModelRc::new(VecModel::from(demo)));
                ui.set_account_open(true);
            }
            "suggest" => {
                ui.set_search_text("daft".into());
                let s: Vec<SharedString> =
                    ["daft punk", "daft punk get lucky", "daft punk one more time", "daft punk instant crush"]
                        .iter()
                        .map(|s| (*s).into())
                        .collect();
                ui.set_suggestions(ModelRc::new(VecModel::from(s)));
                ui.set_search_open(true);
            }
            "queue" => {
                ui.set_np_tab(0);
                ui.set_np_open(true);
            }
            "menu" => {
                let icons = ui.global::<Icons>();
                let e = |id: &str, label: &str, icon: SharedString, danger: bool| MenuEntry { id: id.into(), label: label.into(), icon, danger };
                let entries = vec![
                    e("radio", "Start radio", icons.get_radio(), false),
                    e("play-next", "Play next", icons.get_play_next(), false),
                    e("add-queue", "Add to queue", icons.get_queue_add(), false),
                    e("add-playlist", "Save to playlist", icons.get_playlist_add(), false),
                    e("remove-playlist", "Remove from playlist", icons.get_delete(), true),
                    e("go-artist", "Go to artist", icons.get_person(), false),
                    e("go-album", "Go to album", icons.get_album(), false),
                    e("copy-link", "Copy link", icons.get_link(), false),
                ];
                ui.set_menu_items(ModelRc::new(VecModel::from(entries)));
                ui.set_menu_x(900.0);
                ui.set_menu_y(420.0);
                ui.set_menu_open(true);
            }
            "new-playlist" => {
                ui.set_dialog_heading("New playlist".into());
                ui.set_dialog_ok("Create".into());
                ui.set_dialog_name("Late night drive".into());
                ui.set_dialog("playlist".into());
            }
            "pick" => {
                let demo: Vec<CardData> = ["Road trip", "Focus", "Gym", "Chill evenings"]
                    .iter()
                    .map(|t| CardData { title: (*t).into(), id: "x".into(), ..Default::default() })
                    .collect();
                ui.set_pick_playlists(ModelRc::new(VecModel::from(demo)));
                ui.set_dialog_heading("Save to playlist".into());
                ui.set_dialog("pick-playlist".into());
            }
            "confirm" => {
                ui.set_dialog_heading("Delete playlist".into());
                ui.set_dialog_message("Delete \"Road trip\"? This can't be undone.".into());
                ui.set_dialog_ok("Delete".into());
                ui.set_dialog("confirm".into());
            }
            "owned" | "subscribe" | "rated" => {
                // Applied once the page has loaded.
                let weak = ui.as_weak();
                let which = s.clone();
                slint::Timer::single_shot(Duration::from_secs(6), move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let mut h = ui.get_header();
                    match which.as_str() {
                        "owned" => {
                            h.owned = true;
                            h.saveable = true;
                        }
                        "subscribe" => h.subscribable = true,
                        _ => {
                            let mut now = ui.get_now();
                            now.rating = 1;
                            ui.set_now(now);
                        }
                    }
                    ui.set_header(h);
                });
            }
            _ => {}
        }
    }
}

fn notify(msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(a) = app::app() {
            a.notify(msg);
        }
    });
}

/// Downloads missing playback tools, keeps yt-dlp current (daily) and, for
/// installed copies, checks GitHub for app updates (every 6 hours).
fn start_background_jobs(rt: &tokio::runtime::Runtime, res: &Arc<Resolver>, dir: &std::path::Path, http: &reqwest::Client, testing: bool) {
    if !res.has_tools() {
        let (res, dir, http) = (res.clone(), dir.to_owned(), http.clone());
        rt.spawn(async move {
            notify("Setting up playback…".into());
            match tools::install(&dir, &http, notify).await {
                Ok(t) => {
                    res.set_tools(t);
                    notify("Ready to play".into());
                }
                Err(e) => notify(format!("Couldn't download playback components ({e:#}). Yusic will retry on the next start.")),
            }
        });
    }
    if testing {
        return;
    }
    let (res2, dir2) = (res.clone(), dir.to_owned());
    rt.spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        let stamp = tools::tools_dir(&dir2).join("yt-dlp.checked");
        let due = std::fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(24 * 3600))
            .unwrap_or(true);
        if due {
            res2.update_ytdlp().await;
            let _ = std::fs::create_dir_all(tools::tools_dir(&dir2));
            let _ = std::fs::write(&stamp, b"");
        }
    });
    if install::is_installed_copy() {
        let http = http.clone();
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                let auto = install::AUTO_UPDATE.load(std::sync::atomic::Ordering::Relaxed);
                if !auto {
                    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                    continue;
                }
                if let Ok(Some(v)) = install::update_from_github(&http).await {
                    notify(format!("Yusic {v} is ready. Restart to update."));
                    break;
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });
    }
}

/// Shows "Restart to update" once the exe on disk has been replaced (by the
/// GitHub updater or scripts\deploy.ps1).
fn watch_for_new_version(ui: &AppWindow) -> slint::Timer {
    let start = install::exe_stamp();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(30), move || {
        if start.is_some() && install::exe_stamp() != start {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_ready(true);
            }
        }
    });
    timer
}

fn save_snapshot(ui: &AppWindow, path: &std::path::Path) -> Result<()> {
    let buf = ui.window().take_snapshot()?;
    let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec())
        .context("bad snapshot buffer")?;
    img.save(path)?;
    Ok(())
}
