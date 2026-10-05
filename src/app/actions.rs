//! Context menus, likes, playlist management, library saves and settings.

use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{App, Route, card_data};
use crate::model::{Item, Kind};
use crate::yt::{Privacy, Rating};
use crate::{CardData, Icons, MenuEntry, SettingsData, Theme};

/// What a context menu was opened on.
#[derive(Clone)]
pub(super) enum MenuTarget {
    Track { item: Item, queue_index: Option<usize> },
    Card { kind: Kind, id: String, title: String },
}

/// What the open dialog's OK button does.
#[derive(Clone, Default)]
pub(super) enum DialogMode {
    #[default]
    None,
    /// New playlist, optionally with songs to put in it.
    NewPlaylist(Vec<String>),
    /// Playlist id and its privacy when the dialog opened ("" if unknown).
    EditPlaylist(String, String),
    DeletePlaylist(String),
    /// Choosing a playlist for these songs.
    PickPlaylist(Vec<String>),
}

pub(super) const REGIONS: [(&str, &str); 24] = [
    ("US", "United States"),
    ("GB", "United Kingdom"),
    ("CA", "Canada"),
    ("AU", "Australia"),
    ("IE", "Ireland"),
    ("NZ", "New Zealand"),
    ("DE", "Germany"),
    ("FR", "France"),
    ("ES", "Spain"),
    ("IT", "Italy"),
    ("NL", "Netherlands"),
    ("SE", "Sweden"),
    ("PL", "Poland"),
    ("BR", "Brazil"),
    ("MX", "Mexico"),
    ("AR", "Argentina"),
    ("CO", "Colombia"),
    ("JP", "Japan"),
    ("KR", "South Korea"),
    ("IN", "India"),
    ("ID", "Indonesia"),
    ("PH", "Philippines"),
    ("NG", "Nigeria"),
    ("ZA", "South Africa"),
];

fn region_name(code: &str) -> &'static str {
    REGIONS.iter().find(|r| r.0.eq_ignore_ascii_case(code)).map_or("United States", |r| r.1)
}

fn region_code(name: &str) -> &'static str {
    REGIONS.iter().find(|r| r.1 == name).map_or("US", |r| r.0)
}

fn parse_color(hex: &str) -> Option<slint::Color> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn share_link(kind: Kind, id: &str) -> String {
    match kind {
        Kind::Song | Kind::Video | Kind::Episode => format!("https://music.youtube.com/watch?v={id}"),
        Kind::Playlist | Kind::Podcast => format!("https://music.youtube.com/playlist?list={id}"),
        Kind::Album => format!("https://music.youtube.com/browse/{id}"),
        Kind::Artist => format!("https://music.youtube.com/channel/{id}"),
    }
}

impl App {
    // ------------------------------------------------------------ settings

    pub(super) fn settings_data(&self) -> SettingsData {
        let s = self.settings.borrow();
        SettingsData {
            quality: s.quality.as_str().into(),
            autoplay: s.autoplay,
            prefetch: s.prefetch,
            lrclib: s.lrclib,
            lyrics_size: s.lyrics_size as i32,
            close_to_tray: s.close_to_tray,
            start_with_windows: s.start_with_windows,
            start_minimized: s.start_minimized,
            game_mode: s.game_mode,
            accent: s.accent.as_str().into(),
            region: region_name(&s.region).into(),
            auto_update: s.auto_update,
            version: env!("CARGO_PKG_VERSION").into(),
            installed: crate::install::is_installed_copy(),
        }
    }

    /// Pushes the current settings to the UI and the services that use them.
    pub(super) fn apply_settings(&self) {
        let ui = self.ui();
        let s = self.settings.borrow().clone();
        ui.set_settings(self.settings_data());
        let regions: Vec<SharedString> = REGIONS.iter().map(|r| r.1.into()).collect();
        ui.set_regions(ModelRc::new(VecModel::from(regions)));
        if let Some(c) = parse_color(&s.accent) {
            ui.global::<Theme>().set_accent(c);
        }
        self.yt.set_region(&s.region);
        self.yt.use_lrclib.store(s.lrclib, std::sync::atomic::Ordering::Relaxed);
        crate::install::AUTO_UPDATE.store(s.auto_update, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn change_setting(self: &Rc<Self>, key: &str, value: &str) {
        let on = value == "1";
        {
            let mut s = self.settings.borrow_mut();
            match key {
                "quality" => s.quality = value.into(),
                "autoplay" => s.autoplay = on,
                "prefetch" => s.prefetch = on,
                "lrclib" => s.lrclib = on,
                "lyrics-size" => s.lyrics_size = value.parse().unwrap_or(20),
                "close-to-tray" => s.close_to_tray = on,
                "start-with-windows" => s.start_with_windows = on,
                "start-minimized" => s.start_minimized = on,
                "game-mode" => s.game_mode = on,
                "accent" => s.accent = value.into(),
                "region" => s.region = region_code(value).into(),
                "auto-update" => s.auto_update = on,
                _ => return,
            }
        }
        self.apply_settings();
        self.save_settings();
        match key {
            "start-with-windows" | "start-minimized" => {
                let (enabled, minimized) = {
                    let s = self.settings.borrow();
                    (s.start_with_windows, s.start_minimized)
                };
                if let Err(e) = crate::install::set_autostart(enabled, minimized) {
                    self.toast(format!("Couldn't change startup: {e:#}"));
                }
            }
            "region" => {
                // Pages depend on the region.
                self.st.borrow_mut().page_cache.clear();
            }
            "lrclib" => {
                // Look the current song's lyrics up again with the new source.
                self.st.borrow_mut().lyrics_for = None;
            }
            "prefetch" | "quality" => {
                // Drop a prefetch made under the old setting, then prepare again.
                let cur = self.current_id();
                {
                    let keep: Vec<&str> = cur.iter().map(String::as_str).collect();
                    self.st.borrow_mut().prune_downloads(&keep);
                }
                self.prefetch_next();
            }
            _ => {}
        }
    }

    pub(super) fn save_settings(&self) {
        if self.persist.get() {
            self.settings.borrow().save(&self.data_dir);
        }
    }

    pub(super) fn check_updates(self: &Rc<Self>) {
        if !crate::install::is_installed_copy() {
            self.toast("This is a development copy; it's updated by deploys, not from GitHub.");
            return;
        }
        self.toast("Checking for updates…");
        let http = self.http.clone();
        self.spawn(async move { crate::install::update_from_github(&http).await.map_err(|e| format!("{e:#}")) }, |app, r| {
            match r {
                Ok(Some(v)) => {
                    app.ui().set_update_ready(true);
                    app.toast(format!("Yusic {v} is ready. Restart to update."));
                }
                Ok(None) => app.toast("Yusic is up to date."),
                Err(e) => app.toast(format!("Couldn't check for updates: {e}")),
            }
        });
    }

    pub(super) fn update_ytdlp_now(self: &Rc<Self>) {
        self.toast("Updating yt-dlp…");
        let res = self.res.clone();
        self.spawn(async move { res.update_ytdlp().await }, |app, updated| {
            app.toast(if updated { "yt-dlp was updated." } else { "yt-dlp is already up to date." });
        });
    }

    // ------------------------------------------------------------ likes & library

    /// Gets the current song's like state (signed in only).
    pub(super) fn load_rating(self: &Rc<Self>, video_id: &str) {
        if !self.yt.signed_in() {
            return;
        }
        let yt = self.yt.clone();
        let id = video_id.to_owned();
        let vid = id.clone();
        self.spawn(async move { yt.rating(&vid).await.ok() }, move |app, r| {
            let Some(r) = r else { return };
            let ui = app.ui();
            let mut now = ui.get_now();
            if now.id.as_str() == id {
                now.rating = rating_int(r);
                ui.set_now(now);
            }
        });
    }

    pub(super) fn rate_current(self: &Rc<Self>, value: i32) {
        let Some(id) = self.current_id() else { return };
        if !self.yt.signed_in() {
            self.toast("Sign in to like songs.");
            return;
        }
        let rating = match value {
            1 => Rating::Like,
            2 => Rating::Dislike,
            _ => Rating::None,
        };
        let ui = self.ui();
        let mut now = ui.get_now();
        let before = now.rating;
        now.rating = value;
        ui.set_now(now);
        let yt = self.yt.clone();
        let vid = id.clone();
        self.spawn(async move { yt.rate(&vid, rating).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
            // The song may have changed while the request ran.
            let still_current = app.current_id().as_deref() == Some(id.as_str());
            match r {
                Ok(()) => {
                    // Liked music changed.
                    app.st.borrow_mut().page_cache.retain(|c| c.0 != Route::Playlist("LM".into()));
                    if rating == Rating::Dislike && still_current {
                        app.toast("Disliked. Skipping.");
                        app.skip();
                    }
                }
                Err(e) => {
                    if still_current {
                        let ui = app.ui();
                        let mut now = ui.get_now();
                        now.rating = before;
                        ui.set_now(now);
                    }
                    app.toast(format!("Couldn't update the rating: {e}"));
                }
            }
        });
    }

    pub(super) fn toggle_saved(self: &Rc<Self>) {
        let (id, saved, tok, route) = {
            let st = self.st.borrow();
            (st.page.actions.library_id.clone(), st.page.actions.saved.unwrap_or(false), st.page_gen, st.route.clone())
        };
        let Some(id) = id else { return };
        self.set_header_flag(tok, |h| h.saved = !saved, |a| a.saved = Some(!saved));
        let yt = self.yt.clone();
        self.spawn(async move { yt.set_saved(&id, !saved).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
            match r {
                Ok(()) => {
                    app.toast(if saved { "Removed from your library." } else { "Saved to your library." });
                    app.st.borrow_mut().page_cache.retain(|c| c.0 != Route::Library);
                    app.refresh_sidebar();
                }
                Err(e) => {
                    app.set_header_flag(tok, |h| h.saved = saved, |a| a.saved = Some(saved));
                    app.forget_page(route.as_ref());
                    app.toast(format!("Couldn't update your library: {e}"));
                }
            }
        });
    }

    /// Applies an optimistic header change, but only to the page it was made on.
    fn set_header_flag(&self, tok: u64, header: impl FnOnce(&mut crate::HeaderData), actions: impl FnOnce(&mut crate::model::PageActions)) {
        if self.st.borrow().page_gen != tok {
            return;
        }
        let ui = self.ui();
        let mut h = ui.get_header();
        header(&mut h);
        ui.set_header(h);
        actions(&mut self.st.borrow_mut().page.actions);
    }

    /// Drops a cached page so its next visit reloads it.
    fn forget_page(&self, route: Option<&Route>) {
        if let Some(r) = route {
            self.st.borrow_mut().page_cache.retain(|c| &c.0 != r);
        }
    }

    pub(super) fn toggle_subscribed(self: &Rc<Self>) {
        let (id, on, tok, route) = {
            let st = self.st.borrow();
            (st.page.actions.channel_id.clone(), st.page.actions.subscribed.unwrap_or(false), st.page_gen, st.route.clone())
        };
        let Some(id) = id else { return };
        self.set_header_flag(tok, |h| h.subscribed = !on, |a| a.subscribed = Some(!on));
        let yt = self.yt.clone();
        self.spawn(async move { yt.set_subscribed(&id, !on).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
            if let Err(e) = r {
                app.set_header_flag(tok, |h| h.subscribed = on, |a| a.subscribed = Some(on));
                app.forget_page(route.as_ref());
                app.toast(format!("Couldn't change your subscription: {e}"));
            }
        });
    }

    /// Reloads the sidebar's playlist list.
    pub(super) fn refresh_sidebar(self: &Rc<Self>) {
        if !self.yt.signed_in() {
            return;
        }
        let yt = self.yt.clone();
        self.spawn(async move { yt.sidebar_playlists().await.unwrap_or_default() }, |app, items| {
            if app.st.borrow().signed_in {
                app.set_playlists(&items);
            }
        });
    }

    pub(super) fn set_playlists(&self, items: &[Item]) {
        self.sidebar_model.set_vec(items.iter().map(card_data).collect::<Vec<_>>());
    }

    // ------------------------------------------------------------ context menus

    fn entry(&self, id: &str, label: &str, icon: SharedString, danger: bool) -> MenuEntry {
        MenuEntry { id: id.into(), label: label.into(), icon, danger }
    }

    fn open_menu(&self, entries: Vec<MenuEntry>, x: f32, y: f32) {
        let ui = self.ui();
        ui.set_menu_items(ModelRc::new(VecModel::from(entries)));
        ui.set_menu_x(x);
        ui.set_menu_y(y);
        ui.set_menu_open(true);
    }

    pub(super) fn track_menu(self: &Rc<Self>, item: Item, queue_index: Option<usize>, x: f32, y: f32) {
        if !item.kind.playable() {
            let card = card_data(&item);
            return self.card_menu(card, x, y);
        }
        let ui = self.ui();
        let icons = ui.global::<Icons>();
        let mut e = vec![
            self.entry("radio", "Start radio", icons.get_radio(), false),
            self.entry("play-next", "Play next", icons.get_play_next(), false),
            self.entry("add-queue", "Add to queue", icons.get_queue_add(), false),
        ];
        if self.yt.signed_in() {
            e.push(self.entry("add-playlist", "Save to playlist", icons.get_playlist_add(), false));
        }
        let owned = self.st.borrow().page.actions.owned_playlist.clone();
        if queue_index.is_none() && owned.is_some() && item.set_video_id.is_some() {
            e.push(self.entry("remove-playlist", "Remove from playlist", icons.get_delete(), true));
        }
        if let Some(i) = queue_index {
            if i != self.st.borrow().qi {
                e.push(self.entry("remove-queue", "Remove from queue", icons.get_delete(), false));
            }
        }
        if item.first_artist_id().is_some() {
            e.push(self.entry("go-artist", "Go to artist", icons.get_person(), false));
        }
        if item.album.as_ref().and_then(|a| a.id.as_ref()).is_some() {
            e.push(self.entry("go-album", "Go to album", icons.get_album(), false));
        }
        e.push(self.entry("copy-link", "Copy link", icons.get_link(), false));
        self.st.borrow_mut().menu = Some(MenuTarget::Track { item, queue_index });
        self.open_menu(e, x, y);
    }

    pub(super) fn card_menu(self: &Rc<Self>, card: CardData, x: f32, y: f32) {
        let kind = Kind::from_str(&card.kind);
        if kind.playable() {
            let item = Item {
                kind,
                id: card.id.to_string(),
                title: card.title.to_string(),
                subtitle: card.subtitle.to_string(),
                ..Default::default()
            };
            return self.track_menu(item, None, x, y);
        }
        let ui = self.ui();
        let icons = ui.global::<Icons>();
        let mut e = vec![
            self.entry("play", "Play", icons.get_play(), false),
            self.entry("shuffle", "Shuffle play", icons.get_shuffle(), false),
            self.entry("play-next", "Play next", icons.get_play_next(), false),
            self.entry("add-queue", "Add to queue", icons.get_queue_add(), false),
        ];
        let open = match kind {
            Kind::Album => "Go to album",
            Kind::Artist => "Go to artist",
            _ => "Go to playlist",
        };
        e.push(self.entry("open", open, icons.get_chevron_right(), false));
        e.push(self.entry("copy-link", "Copy link", icons.get_link(), false));
        self.st.borrow_mut().menu = Some(MenuTarget::Card { kind, id: card.id.to_string(), title: card.title.to_string() });
        self.open_menu(e, x, y);
    }

    pub(super) fn menu_action(self: &Rc<Self>, action: &str) {
        let Some(target) = self.st.borrow_mut().menu.take() else { return };
        match target {
            MenuTarget::Track { item, queue_index } => match action {
                "radio" => self.play_items(vec![item], 0, true),
                "play-next" => self.enqueue(vec![item], true),
                "add-queue" => self.enqueue(vec![item], false),
                "add-playlist" => self.pick_playlist(vec![item.id]),
                "remove-playlist" => self.remove_from_playlist(item),
                "remove-queue" => {
                    if let Some(i) = queue_index {
                        self.remove_from_queue(i, &item.id);
                    }
                }
                "go-artist" => {
                    if let Some(id) = item.first_artist_id() {
                        self.navigate(Route::Artist(id.into()), true);
                    }
                }
                "go-album" => {
                    if let Some(id) = item.album.as_ref().and_then(|a| a.id.clone()) {
                        self.navigate(Route::Album(id), true);
                    }
                }
                "copy-link" => self.copy_link(item.kind, &item.id),
                _ => {}
            },
            MenuTarget::Card { kind, id, title } => match action {
                "open" => {
                    let card = CardData { kind: kind.as_str().into(), id: id.as_str().into(), ..Default::default() };
                    self.open_card(&card);
                }
                "copy-link" => self.copy_link(kind, &id),
                "play" | "shuffle" | "play-next" | "add-queue" => {
                    let yt = self.yt.clone();
                    let action = action.to_owned();
                    let req = self.st.borrow().play_req;
                    self.spawn(async move { yt.playable(kind, &id).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
                        match r {
                            Ok(items) if !items.is_empty() => match action.as_str() {
                                "play" | "shuffle" if app.st.borrow().play_req == req => {
                                    let start = if action == "shuffle" {
                                        app.st.borrow_mut().shuffle = true;
                                        app.ui().set_shuffle(true);
                                        (app.rand() as usize) % items.len()
                                    } else {
                                        0
                                    };
                                    app.play_items(items, start, false);
                                }
                                "play-next" => app.enqueue(items, true),
                                "add-queue" => app.enqueue(items, false),
                                _ => {}
                            },
                            Ok(_) => app.toast(format!("Nothing to play in \"{title}\".")),
                            Err(e) => app.toast(format!("Couldn't load \"{title}\": {e}")),
                        }
                    });
                }
                _ => {}
            },
        }
    }

    fn copy_link(&self, kind: Kind, id: &str) {
        if crate::win::copy_text(&share_link(kind, id)) {
            self.toast("Link copied.");
        } else {
            self.toast("Couldn't copy the link.");
        }
    }

    /// Adds songs right after the current one, or at the end of the queue.
    pub(super) fn enqueue(self: &Rc<Self>, items: Vec<Item>, next: bool) {
        let items: Vec<Item> = items.into_iter().filter(|i| i.kind.playable()).collect();
        if items.is_empty() {
            return;
        }
        let count = items.len();
        let empty = self.st.borrow().queue.is_empty();
        if empty {
            return self.play_items(items, 0, false);
        }
        {
            let mut st = self.st.borrow_mut();
            let at = if next { (st.qi + 1).min(st.queue.len()) } else { st.queue.len() };
            st.queue.splice(at..at, items);
        }
        self.refresh_queue();
        self.prefetch_next();
        let what = if count == 1 { "1 song".to_string() } else { format!("{count} songs") };
        self.toast(if next { format!("Playing {what} next.") } else { format!("Added {what} to the queue.") });
    }

    fn remove_from_queue(self: &Rc<Self>, i: usize, id: &str) {
        {
            let mut st = self.st.borrow_mut();
            // The queue may have changed since the menu opened.
            let i = if st.queue.get(i).is_some_and(|q| q.id == id) {
                i
            } else {
                match st.queue.iter().position(|q| q.id == id) {
                    Some(p) => p,
                    None => return,
                }
            };
            if i == st.qi {
                return;
            }
            st.queue.remove(i);
            if i < st.qi {
                st.qi -= 1;
            }
        }
        self.refresh_queue();
        self.prefetch_next();
    }

    // ------------------------------------------------------------ playlists

    fn require_sign_in(&self, what: &str) -> bool {
        if self.yt.signed_in() {
            return true;
        }
        self.toast(format!("Sign in to {what}."));
        false
    }

    fn open_dialog(&self, kind: &str, heading: &str, ok: &str, mode: DialogMode) {
        let ui = self.ui();
        ui.set_menu_open(false);
        ui.set_account_open(false);
        ui.set_dialog_heading(heading.into());
        ui.set_dialog_ok(ok.into());
        ui.set_dialog(kind.into());
        self.st.borrow_mut().dialog = mode;
    }

    pub(super) fn new_playlist(self: &Rc<Self>) {
        if !self.require_sign_in("create playlists") {
            return;
        }
        // Coming from "Save to playlist" (still open): the new playlist gets those songs.
        let from_picker = self.ui().get_dialog() == "pick-playlist";
        let songs = match std::mem::take(&mut self.st.borrow_mut().dialog) {
            DialogMode::PickPlaylist(ids) if from_picker => ids,
            _ => Vec::new(),
        };
        let ui = self.ui();
        ui.set_dialog_name("".into());
        ui.set_dialog_desc("".into());
        ui.set_dialog_privacy("private".into());
        self.open_dialog("playlist", "New playlist", "Create", DialogMode::NewPlaylist(songs));
    }

    pub(super) fn edit_playlist(self: &Rc<Self>) {
        let Some(id) = self.st.borrow().page.actions.owned_playlist.clone() else { return };
        let ui = self.ui();
        let h = ui.get_header();
        ui.set_dialog_name(h.title.clone());
        ui.set_dialog_desc(h.description.clone());
        // The header's subtitle usually says "Private", "Unlisted" or "Public".
        // If it doesn't, nothing is selected and privacy is left unchanged.
        let words = h.line1.to_lowercase();
        let privacy = ["private", "unlisted", "public"].into_iter().find(|p| words.contains(p)).unwrap_or("");
        ui.set_dialog_privacy(privacy.into());
        self.open_dialog("playlist", "Edit playlist", "Save", DialogMode::EditPlaylist(id, privacy.into()));
    }

    pub(super) fn delete_playlist(self: &Rc<Self>) {
        let Some(id) = self.st.borrow().page.actions.owned_playlist.clone() else { return };
        let title = self.ui().get_header().title;
        self.ui().set_dialog_message(format!("Delete \"{title}\"? This can't be undone.").into());
        self.open_dialog("confirm", "Delete playlist", "Delete", DialogMode::DeletePlaylist(id));
    }

    fn pick_playlist(self: &Rc<Self>, video_ids: Vec<String>) {
        if !self.require_sign_in("save songs to playlists") {
            return;
        }
        let ui = self.ui();
        ui.set_pick_playlists(ModelRc::default());
        ui.set_pick_loading(true);
        self.open_dialog("pick-playlist", "Save to playlist", "", DialogMode::PickPlaylist(video_ids.clone()));
        // Only playlists the user can add to, marked if the song is already there.
        let yt = self.yt.clone();
        let want = video_ids.clone();
        self.spawn(async move { yt.add_targets(&video_ids).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
            // Ignore answers for a picker that was closed or reopened for other songs.
            if !matches!(&app.st.borrow().dialog, DialogMode::PickPlaylist(ids) if *ids == want) {
                return;
            }
            let ui = app.ui();
            ui.set_pick_loading(false);
            let cards: Vec<CardData> = match r {
                Ok(list) if !list.is_empty() => list
                    .into_iter()
                    .map(|(id, title, contains)| CardData {
                        id: id.into(),
                        title: title.into(),
                        subtitle: if contains { "Already added".into() } else { "".into() },
                        ..Default::default()
                    })
                    .collect(),
                // Fall back to the sidebar's list (Liked Music can't be a target).
                _ => app.sidebar_model.iter().filter(|c| c.id.as_str() != "LM").collect(),
            };
            ui.set_pick_playlists(ModelRc::new(VecModel::from(cards)));
        });
    }

    pub(super) fn choose_playlist(self: &Rc<Self>, playlist_id: String) {
        let DialogMode::PickPlaylist(ids) = std::mem::take(&mut self.st.borrow_mut().dialog) else { return };
        let title = self
            .ui()
            .get_pick_playlists()
            .iter()
            .find(|c| c.id.as_str() == playlist_id)
            .map(|c| c.title.to_string())
            .unwrap_or_else(|| "the playlist".into());
        self.close_dialog();
        let yt = self.yt.clone();
        let pid = playlist_id.clone();
        self.spawn(async move { yt.add_to_playlist(&pid, &ids).await.map_err(|e| format!("{e:#}")) }, move |app, r| {
            match r {
                Ok(true) => {
                    app.toast(format!("Saved to {title}."));
                    app.playlist_changed(&playlist_id);
                }
                Ok(false) => app.toast(format!("Already in {title}.")),
                Err(e) => app.toast(format!("Couldn't save to {title}: {e}")),
            }
        });
    }

    /// A playlist's contents changed: drop its cached page and reload it if open.
    fn playlist_changed(self: &Rc<Self>, playlist_id: &str) {
        let route = Route::Playlist(playlist_id.to_owned());
        self.forget_page(Some(&route));
        self.reload_if(&[route]);
    }

    pub(super) fn dialog_submit(self: &Rc<Self>) {
        let ui = self.ui();
        let name = ui.get_dialog_name().trim().to_string();
        let desc = ui.get_dialog_desc().trim().to_string();
        let privacy = Privacy::from_str(&ui.get_dialog_privacy());
        let mode = self.st.borrow().dialog.clone();
        match mode {
            DialogMode::NewPlaylist(songs) => {
                if name.is_empty() {
                    return self.toast("Give the playlist a name.");
                }
                self.close_dialog();
                let yt = self.yt.clone();
                let title = name.clone();
                self.spawn(
                    async move { yt.create_playlist(&name, &desc, privacy, &songs).await.map_err(|e| format!("{e:#}")) },
                    move |app, r| match r {
                        Ok(id) => {
                            app.toast(format!("Created \"{title}\"."));
                            app.st.borrow_mut().page_cache.retain(|c| c.0 != Route::Library);
                            app.refresh_sidebar();
                            app.navigate(Route::Playlist(id), true);
                        }
                        Err(e) => app.toast(format!("Couldn't create the playlist: {e}")),
                    },
                );
            }
            DialogMode::EditPlaylist(id, original) => {
                if name.is_empty() {
                    return self.toast("Give the playlist a name.");
                }
                let chosen = ui.get_dialog_privacy().to_string();
                let privacy = (!chosen.is_empty() && chosen != original).then_some(privacy);
                self.close_dialog();
                let yt = self.yt.clone();
                let pid = id.clone();
                self.spawn(
                    async move { yt.edit_playlist(&pid, &name, &desc, privacy).await.map_err(|e| format!("{e:#}")) },
                    move |app, r| match r {
                        Ok(()) => {
                            app.toast("Playlist updated.");
                            app.refresh_sidebar();
                            app.playlist_changed(&id);
                        }
                        Err(e) => app.toast(format!("Couldn't update the playlist: {e}")),
                    },
                );
            }
            DialogMode::DeletePlaylist(id) => {
                self.close_dialog();
                let yt = self.yt.clone();
                self.spawn(async move { yt.delete_playlist(&id).await.map_err(|e| format!("{e:#}")) }, |app, r| {
                    match r {
                        Ok(()) => {
                            app.toast("Playlist deleted.");
                            app.st.borrow_mut().page_cache.clear();
                            app.refresh_sidebar();
                            app.open(Route::Library, true, false);
                        }
                        Err(e) => app.toast(format!("Couldn't delete the playlist: {e}")),
                    }
                });
            }
            DialogMode::PickPlaylist(_) | DialogMode::None => self.close_dialog(),
        }
    }

    pub(super) fn close_dialog(&self) {
        self.st.borrow_mut().dialog = DialogMode::None;
        self.ui().set_dialog("".into());
    }

    fn remove_from_playlist(self: &Rc<Self>, item: Item) {
        let Some(pid) = self.st.borrow().page.actions.owned_playlist.clone() else { return };
        let Some(set_id) = item.set_video_id.clone() else { return };
        let yt = self.yt.clone();
        let title = item.title.clone();
        let playlist = pid.clone();
        self.spawn(
            async move { yt.remove_from_playlist(&pid, &item.id, &set_id).await.map_err(|e| format!("{e:#}")) },
            move |app, r| match r {
                Ok(()) => {
                    app.toast(format!("Removed \"{title}\"."));
                    app.playlist_changed(&playlist);
                }
                Err(e) => app.toast(format!("Couldn't remove \"{title}\": {e}")),
            },
        );
    }
}

fn rating_int(r: Rating) -> i32 {
    match r {
        Rating::None => 0,
        Rating::Like => 1,
        Rating::Dislike => 2,
    }
}
