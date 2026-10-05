//! UI-thread application state: navigation, page models, queue, playback and
//! the signed-in session.

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rustypipe::model::TrackItem;
use rustypipe::model::paginator::Paginator;
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use tokio::task::AbortHandle;

use crate::images;
use crate::login;
use crate::model::{Continuation, Header, Item, Kind, Page, Shelf, ShelfKind, fmt_duration};
use crate::player::{Meta, Player, PlayerEvent};
use crate::resolver::{Quality, Resolver};
use crate::settings::Settings;
use crate::yt::Yt;
use crate::yt::lyrics::{self, LyricsBody};
use crate::{AppWindow, Bridge, CardData, HeaderData, NowData, ShelfData, TrackData};

/// Consecutive unplayable tracks before playback stops instead of skipping on.
const MAX_FAILS: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Explore,
    Library,
    Search(String),
    Album(String),
    Playlist(String),
    Artist(String),
}

impl Route {
    pub fn parse(s: &str) -> Route {
        let (kind, arg) = s.split_once(':').unwrap_or((s, ""));
        match kind {
            "explore" => Route::Explore,
            "library" => Route::Library,
            "search" if !arg.is_empty() => Route::Search(arg.into()),
            "album" if !arg.is_empty() => Route::Album(arg.into()),
            "playlist" if !arg.is_empty() => Route::Playlist(arg.into()),
            "artist" if !arg.is_empty() => Route::Artist(arg.into()),
            _ => Route::Home,
        }
    }

    fn section(&self) -> &'static str {
        match self {
            Route::Home => "home",
            Route::Explore => "explore",
            Route::Library => "library",
            _ => "",
        }
    }
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

pub fn app() -> Option<Rc<App>> {
    APP.with(|a| a.borrow().clone())
}

struct ShelfModels {
    tracks: Rc<VecModel<TrackData>>,
}

#[derive(Default)]
struct State {
    route: Option<Route>,
    history: Vec<Route>,
    page_gen: u64,
    page: Page,
    shelf_models: Vec<ShelfModels>,
    shelves_model: Option<Rc<VecModel<ShelfData>>>,
    loading_more: bool,

    queue: Vec<Item>,
    qi: usize,
    play_gen: u64,
    shuffle: bool,
    repeat: i32,
    playing: bool,
    /// The player holds the current track (false while it downloads).
    has_source: bool,
    loaded_at: Option<Instant>,
    fail_streak: u32,
    /// Running downloads (current and next track) by video id.
    downloads: Vec<(String, AbortHandle)>,
    /// Bumped whenever the user starts something new; late async results
    /// for an older request are dropped.
    play_req: u64,
    /// More radio tracks to append as the queue runs low.
    radio_more: Option<Paginator<TrackItem>>,
    radio_busy: bool,
    /// The queue ran out while more songs were loading; play on when they arrive.
    pending_advance: bool,
    /// Track we already fetched end-of-queue autoplay for.
    autoplay_for: Option<String>,
    last_volume: f32,

    signed_in: bool,
    /// Recently shown pages, for instant back/revisits.
    page_cache: Vec<(Route, Instant, Page)>,
    /// Tracks whose best-quality file Windows couldn't decode; these use the
    /// compatible (AAC) format instead.
    compat: HashSet<String>,
    /// Video id whose lyrics are shown or loading.
    lyrics_for: Option<String>,
    /// Line start times (ms) when the lyrics are synced.
    lyrics_times: Vec<u32>,
    tick_ms: u64,
    suggest_gen: u64,
    toast_gen: u64,
    visible: bool,
    rng: u64,
}

const PAGE_CACHE_SIZE: usize = 12;
const PAGE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

impl State {
    fn cache_page(&mut self, route: Route, page: Page) {
        if page.shelves.is_empty() && matches!(page.header, Header::None) {
            return;
        }
        self.page_cache.retain(|c| c.0 != route);
        self.page_cache.push((route, Instant::now(), page));
        if self.page_cache.len() > PAGE_CACHE_SIZE {
            self.page_cache.remove(0);
        }
    }

    fn cached_page(&self, route: &Route) -> Option<Page> {
        self.page_cache
            .iter()
            .find(|c| &c.0 == route && c.1.elapsed() < PAGE_CACHE_TTL)
            .map(|c| c.2.clone())
    }

    /// Cancels downloads for tracks that are no longer current or next.
    fn prune_downloads(&mut self, keep: &[&str]) {
        self.downloads.retain(|(id, h)| {
            if h.is_finished() {
                false
            } else if keep.contains(&id.as_str()) {
                true
            } else {
                h.abort();
                false
            }
        });
    }

    fn current(&self) -> Option<&Item> {
        self.queue.get(self.qi)
    }

    fn next_index(&self) -> Option<usize> {
        if self.qi + 1 < self.queue.len() {
            Some(self.qi + 1)
        } else if self.repeat == 1 && !self.queue.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    fn next_id(&self) -> Option<String> {
        self.next_index().map(|i| self.queue[i].id.clone())
    }
}

pub struct App {
    ui: Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    yt: Arc<Yt>,
    res: Arc<Resolver>,
    player: Player,
    data_dir: PathBuf,
    tick: Timer,
    queue_model: Rc<VecModel<TrackData>>,
    sidebar_model: Rc<VecModel<CardData>>,
    profile_wipe: RefCell<Option<AbortHandle>>,
    st: RefCell<State>,
    pub settings: RefCell<Settings>,
}

impl App {
    pub fn install(
        ui: &AppWindow,
        rt: tokio::runtime::Handle,
        yt: Arc<Yt>,
        res: Arc<Resolver>,
        data_dir: PathBuf,
        settings: Settings,
    ) -> Result<Rc<App>> {
        let player = Player::new(|ev| {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app() {
                    app.on_player_event(ev);
                }
            });
        })?;
        player.set_volume(settings.volume);
        ui.set_volume(settings.volume);
        ui.set_sidebar_wide(settings.sidebar_wide);

        let queue_model = Rc::new(VecModel::default());
        ui.set_queue(ModelRc::from(queue_model.clone()));
        let sidebar_model = Rc::new(VecModel::default());
        ui.set_side_playlists(ModelRc::from(sidebar_model.clone()));

        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let last_volume = if settings.volume > 0.0 { settings.volume } else { 0.5 };
        let app = Rc::new(App {
            ui: ui.as_weak(),
            rt,
            yt,
            res,
            player,
            data_dir,
            tick: Timer::default(),
            queue_model,
            sidebar_model,
            profile_wipe: RefCell::new(None),
            st: RefCell::new(State { visible: true, rng: seed | 1, last_volume, ..Default::default() }),
            settings: RefCell::new(settings),
        });
        APP.with(|a| *a.borrow_mut() = Some(app.clone()));
        Self::bind(ui);
        Ok(app)
    }

    fn bind(ui: &AppWindow) {
        let b = ui.global::<Bridge>();
        fn with(f: impl Fn(&Rc<App>) + 'static) -> impl Fn() + 'static {
            move || {
                if let Some(a) = app() {
                    f(&a)
                }
            }
        }
        b.on_navigate(|kind, id| {
            if let Some(a) = app() {
                a.navigate(Route::parse(&format!("{kind}:{id}")), true);
            }
        });
        b.on_card_clicked(|c| {
            if let Some(a) = app() {
                a.open_card(&c);
            }
        });
        b.on_card_play(|c| {
            if let Some(a) = app() {
                a.play_card(&c);
            }
        });
        b.on_track_clicked(|si, row| {
            if let Some(a) = app() {
                if si >= 0 && row >= 0 {
                    a.track_clicked(si as usize, row as usize);
                }
            }
        });
        b.on_queue_clicked(|i| {
            if let Some(a) = app() {
                if i >= 0 {
                    a.jump_to(i as usize);
                }
            }
        });
        b.on_artist_clicked(|id| {
            if let Some(a) = app() {
                if !id.is_empty() {
                    a.navigate(Route::Artist(id.into()), true);
                }
            }
        });
        b.on_album_clicked(|id| {
            if let Some(a) = app() {
                if !id.is_empty() {
                    a.navigate(Route::Album(id.into()), true);
                }
            }
        });
        b.on_search(|q| {
            if let Some(a) = app() {
                let q = q.trim();
                if !q.is_empty() {
                    a.navigate(Route::Search(q.into()), true);
                }
            }
        });
        b.on_search_edited(|q| {
            if let Some(a) = app() {
                a.suggest(q.to_string());
            }
        });
        b.on_back(with(|a| a.back()));
        b.on_retry(with(|a| {
            let route = a.st.borrow().route.clone();
            if let Some(r) = route {
                a.open(r, false, false);
            }
        }));
        b.on_load_more(with(|a| a.load_more()));
        b.on_play_header(with(|a| a.play_header(false)));
        b.on_shuffle_header(with(|a| a.play_header(true)));
        b.on_radio_header(with(|a| a.radio_header()));
        b.on_toggle_play(with(|a| a.toggle_play()));
        b.on_next(with(|a| a.skip()));
        b.on_prev(with(|a| a.prev()));
        b.on_seek(|f| {
            if let Some(a) = app() {
                a.seek(f);
            }
        });
        b.on_set_volume(|v| {
            if let Some(a) = app() {
                a.set_volume(v);
            }
        });
        b.on_toggle_mute(with(|a| a.toggle_mute()));
        b.on_toggle_shuffle(with(|a| a.toggle_shuffle()));
        b.on_cycle_repeat(with(|a| {
            let r = {
                let mut st = a.st.borrow_mut();
                st.repeat = (st.repeat + 1) % 3;
                st.repeat
            };
            a.ui().set_repeat_mode(r);
            a.prefetch_next();
        }));
        b.on_account(with(|a| a.account_clicked()));
        b.on_sign_in(with(|a| a.sign_in()));
        b.on_sign_out(with(|a| a.sign_out()));
        b.on_np_changed(with(|a| a.np_changed()));
        b.on_restart_update(with(|a| {
            if let Err(e) = crate::install::spawn_restart() {
                a.toast(format!("Couldn't restart: {e:#}"));
                return;
            }
            let _ = slint::quit_event_loop();
        }));
        b.on_lyric_clicked(|i| {
            if let Some(a) = app() {
                a.lyric_clicked(i);
            }
        });
    }

    fn ui(&self) -> AppWindow {
        self.ui.unwrap()
    }

    /// Runs `fut` on tokio and hands its output to `done` on the UI thread.
    fn spawn<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<App>, T) + Send + 'static,
    ) -> AbortHandle {
        self.rt
            .spawn(async move {
                let v = fut.await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app() {
                        done(&app, v);
                    }
                });
            })
            .abort_handle()
    }

    pub fn notify(&self, msg: String) {
        self.toast(msg);
    }

    fn toast(&self, msg: impl Into<SharedString>) {
        let tok = {
            let mut st = self.st.borrow_mut();
            st.toast_gen += 1;
            st.toast_gen
        };
        self.ui().set_toast(msg.into());
        Timer::single_shot(Duration::from_secs(4), move || {
            if let Some(a) = app() {
                if a.st.borrow().toast_gen == tok {
                    a.ui().set_toast("".into());
                }
            }
        });
    }

    // ---------------------------------------------------------------- pages

    pub fn navigate(self: &Rc<Self>, route: Route, push: bool) {
        self.open(route, push, true);
    }

    /// Loads `route`, from the page cache when allowed and fresh.
    fn open(self: &Rc<Self>, route: Route, push: bool, use_cache: bool) {
        let (tok, can_back, cached) = {
            let mut st = self.st.borrow_mut();
            // Remember the page we're leaving, including anything loaded since.
            if let Some(leaving) = st.route.clone() {
                let page = std::mem::take(&mut st.page);
                st.cache_page(leaving, page);
            }
            if push {
                if let Some(cur) = st.route.take() {
                    if cur != route {
                        st.history.push(cur);
                        if st.history.len() > 50 {
                            st.history.remove(0);
                        }
                    }
                }
            }
            st.route = Some(route.clone());
            st.page_gen += 1;
            st.page = Page::default();
            st.shelf_models.clear();
            st.shelves_model = None;
            st.loading_more = false;
            let cached = if use_cache { st.cached_page(&route) } else { None };
            (st.page_gen, !st.history.is_empty(), cached)
        };
        let ui = self.ui();
        ui.set_section(route.section().into());
        ui.set_header(HeaderData::default());
        ui.set_shelves(ModelRc::default());
        ui.set_error("".into());
        ui.set_notice("".into());
        ui.set_can_back(can_back);
        ui.set_scroll_y(0.0);
        ui.set_np_open(false);
        ui.set_search_open(false);
        ui.set_account_open(false);
        if let Route::Search(q) = &route {
            ui.set_search_text(q.into());
        }
        if let Some(page) = cached {
            ui.set_loading(true);
            self.show_page(tok, Ok(page));
            return;
        }
        ui.set_loading(true);
        let yt = self.yt.clone();
        self.spawn(
            async move {
                match route {
                    Route::Home => yt.home().await,
                    Route::Library => yt.library().await,
                    Route::Explore => yt.explore().await,
                    Route::Search(q) => yt.search(&q).await,
                    Route::Album(id) => yt.album(&id).await,
                    Route::Playlist(id) => yt.playlist(&id).await,
                    Route::Artist(id) => yt.artist(&id).await,
                }
                .map_err(|e| format!("{e:#}"))
            },
            move |app, res| app.show_page(tok, res),
        );
    }

    fn reload_if(self: &Rc<Self>, routes: &[Route]) {
        let route = self.st.borrow().route.clone();
        if let Some(r) = route.filter(|r| routes.contains(r)) {
            self.open(r, false, false);
        }
    }

    fn back(self: &Rc<Self>) {
        let prev = self.st.borrow_mut().history.pop();
        if let Some(r) = prev {
            self.navigate(r, false);
        }
    }

    fn show_page(self: &Rc<Self>, tok: u64, res: Result<Page, String>) {
        if self.st.borrow().page_gen != tok {
            return;
        }
        let ui = self.ui();
        ui.set_loading(false);
        let page = match res {
            Ok(p) => p,
            Err(e) => {
                ui.set_error(format!("Couldn't load this page.\n{e}").into());
                return;
            }
        };
        let (header, image) = header_data(&page.header);
        ui.set_header(header);
        if let Some((thumb, w, h)) = image {
            // After set_header: a cached image calls back immediately.
            let weak = self.ui.clone();
            images::request(&thumb, w, h, move |img| {
                let Some(app) = app() else { return };
                if app.st.borrow().page_gen != tok {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    let mut h = ui.get_header();
                    h.image = img;
                    ui.set_header(h);
                }
            });
        }
        ui.set_notice(page.notice.as_str().into());
        let shelves = Rc::new(VecModel::default());
        let current = self.current_id();
        let mut models = Vec::new();
        for s in &page.shelves {
            let (data, m) = shelf_data(s, current.as_deref());
            shelves.push(data);
            models.push(m);
        }
        ui.set_shelves(ModelRc::from(shelves.clone()));
        let mut st = self.st.borrow_mut();
        st.page = page;
        st.shelf_models = models;
        st.shelves_model = Some(shelves);
    }

    fn load_more(self: &Rc<Self>) {
        let (cont, tok) = {
            let mut st = self.st.borrow_mut();
            if st.loading_more {
                return;
            }
            let Some(c) = st.page.continuation.clone() else { return };
            st.loading_more = true;
            (c, st.page_gen)
        };
        let yt = self.yt.clone();
        match cont {
            Continuation::Browse(token) => {
                self.spawn(
                    async move { yt.browse_more(&token).await.map_err(|e| e.to_string()) },
                    move |app, res| {
                        let current = app.current_id();
                        let mut st = app.st.borrow_mut();
                        if st.page_gen != tok {
                            return;
                        }
                        st.loading_more = false;
                        let Ok((shelves, next)) = res else {
                            st.page.continuation = None;
                            return;
                        };
                        st.page.continuation = next.map(Continuation::Browse);
                        for s in shelves {
                            let (data, m) = shelf_data(&s, current.as_deref());
                            if let Some(sm) = &st.shelves_model {
                                sm.push(data);
                            }
                            st.shelf_models.push(m);
                            st.page.shelves.push(s);
                        }
                    },
                );
            }
            Continuation::Tracks(pag) => {
                self.spawn(
                    async move { yt.more_tracks(&pag).await.map_err(|e| e.to_string()) },
                    move |app, res| {
                        let current = app.current_id();
                        let mut st = app.st.borrow_mut();
                        if st.page_gen != tok {
                            return;
                        }
                        st.loading_more = false;
                        let Ok((items, next)) = res else {
                            st.page.continuation = None;
                            return;
                        };
                        st.page.continuation = next.map(Continuation::Tracks);
                        let Some(shelf) = st.page.shelves.first() else { return };
                        let base = shelf.items.len();
                        if let Some(m) = st.shelf_models.first() {
                            for (k, it) in items.iter().enumerate() {
                                push_track(&m.tracks, base + k, it, current.as_deref());
                            }
                        }
                        st.page.shelves[0].items.extend(items.iter().cloned());
                        st.page.play.extend(items);
                    },
                );
            }
        }
    }

    fn suggest(self: &Rc<Self>, q: String) {
        let tok = {
            let mut st = self.st.borrow_mut();
            st.suggest_gen += 1;
            st.suggest_gen
        };
        if q.trim().is_empty() {
            self.ui().set_suggestions(ModelRc::default());
            return;
        }
        let yt = self.yt.clone();
        self.spawn(
            async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                yt.suggestions(&q).await.unwrap_or_default()
            },
            move |app, terms| {
                if app.st.borrow().suggest_gen != tok {
                    return;
                }
                let model: Vec<SharedString> = terms.into_iter().take(8).map(Into::into).collect();
                app.ui().set_suggestions(ModelRc::new(VecModel::from(model)));
            },
        );
    }

    // ---------------------------------------------------------------- clicks

    fn open_card(self: &Rc<Self>, c: &CardData) {
        let kind = Kind::from_str(&c.kind);
        let id = c.id.to_string();
        if id.is_empty() {
            return;
        }
        match kind {
            Kind::Album => self.navigate(Route::Album(id), true),
            Kind::Playlist | Kind::Podcast => self.navigate(Route::Playlist(id), true),
            Kind::Artist => self.navigate(Route::Artist(id), true),
            Kind::Song | Kind::Video | Kind::Episode => self.play_card(c),
        }
    }

    fn play_card(self: &Rc<Self>, c: &CardData) {
        let kind = Kind::from_str(&c.kind);
        if c.id.is_empty() {
            return;
        }
        if kind.playable() {
            let item = self.find_item(kind, &c.id).unwrap_or_else(|| Item {
                kind,
                id: c.id.to_string(),
                title: c.title.to_string(),
                subtitle: c.subtitle.to_string(),
                ..Default::default()
            });
            self.play_items(vec![item], 0, true);
            return;
        }
        let yt = self.yt.clone();
        let id = c.id.to_string();
        let req = self.st.borrow().play_req;
        self.spawn(
            async move { yt.playable(kind, &id).await.map_err(|e| format!("{e:#}")) },
            move |app, items| match items {
                _ if app.st.borrow().play_req != req => {}
                Ok(items) if !items.is_empty() => app.play_items(items, 0, false),
                Ok(_) => app.toast("Nothing to play here"),
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    fn find_item(&self, kind: Kind, id: &str) -> Option<Item> {
        let st = self.st.borrow();
        st.page
            .shelves
            .iter()
            .flat_map(|s| s.items.iter())
            .find(|i| i.kind == kind && i.id == id)
            .cloned()
    }

    fn track_clicked(self: &Rc<Self>, si: usize, row: usize) {
        let (shelf_kind, items) = {
            let st = self.st.borrow();
            let Some(s) = st.page.shelves.get(si) else { return };
            (s.kind, s.items.clone())
        };
        let Some(item) = items.get(row).cloned() else { return };
        if !item.kind.playable() {
            let card = card_data(&item);
            self.open_card(&card);
            return;
        }
        if shelf_kind.is_collection() {
            let playable: Vec<Item> = items.iter().filter(|i| i.kind.playable()).cloned().collect();
            let start = playable.iter().position(|i| i.id == item.id).unwrap_or(0);
            self.play_items(playable, start, false);
        } else {
            self.play_items(vec![item], 0, true);
        }
    }

    fn play_header(self: &Rc<Self>, shuffle: bool) {
        let items = self.st.borrow().page.play.clone();
        if items.is_empty() {
            return;
        }
        if shuffle {
            self.st.borrow_mut().shuffle = true;
            self.ui().set_shuffle(true);
            let start = (self.rand() as usize) % items.len();
            self.play_items(items, start, false);
        } else {
            self.play_items(items, 0, false);
        }
    }

    fn radio_header(self: &Rc<Self>) {
        let radio = self.st.borrow().page.radio.clone();
        let Some(radio) = radio else { return };
        let yt = self.yt.clone();
        let req = self.st.borrow().play_req;
        self.spawn(
            async move { yt.radio_playlist(&radio).await.map_err(|e| format!("{e:#}")) },
            move |app, r| match r {
                _ if app.st.borrow().play_req != req => {}
                Ok((items, more)) if !items.is_empty() => {
                    app.play_items(items, 0, false);
                    app.st.borrow_mut().radio_more = more;
                }
                Ok(_) => app.toast("This radio is empty"),
                Err(e) => app.toast(format!("Couldn't start radio: {e}")),
            },
        );
    }

    // ---------------------------------------------------------------- queue

    fn current_id(&self) -> Option<String> {
        self.st.borrow().current().map(|i| i.id.clone())
    }

    fn rand(&self) -> u64 {
        let mut st = self.st.borrow_mut();
        let mut x = st.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        st.rng = x;
        x
    }

    fn shuffle_after(&self, from: usize) {
        let len = self.st.borrow().queue.len();
        for i in (from + 1..len).rev() {
            let span = (i - from) as u64;
            let j = from + 1 + (self.rand() % span) as usize;
            self.st.borrow_mut().queue.swap(i, j);
        }
    }

    pub fn play_items(self: &Rc<Self>, items: Vec<Item>, start: usize, radio: bool) {
        let items: Vec<Item> = items.into_iter().filter(|i| i.kind.playable() && !i.id.is_empty()).collect();
        if items.is_empty() {
            return;
        }
        let start = start.min(items.len() - 1);
        let (shuffle, req) = {
            let mut st = self.st.borrow_mut();
            st.play_req += 1;
            st.queue = items;
            st.qi = start;
            st.radio_more = None;
            st.radio_busy = radio;
            st.pending_advance = false;
            st.autoplay_for = None;
            st.fail_streak = 0;
            (st.shuffle, st.play_req)
        };
        if shuffle {
            // Keep the chosen track first, shuffle the rest.
            {
                let mut st = self.st.borrow_mut();
                st.queue.swap(0, start);
                st.qi = 0;
            }
            self.shuffle_after(0);
        }
        self.refresh_queue();
        self.start_current();
        if radio {
            let id = self.current_id().unwrap_or_default();
            let yt = self.yt.clone();
            let rid = id.clone();
            self.spawn(
                async move { yt.radio(&rid).await.ok() },
                move |app, r| {
                    {
                        let mut st = app.st.borrow_mut();
                        if st.play_req != req {
                            return;
                        }
                        st.radio_busy = false;
                        if st.queue.len() != 1 || st.queue[0].id != id {
                            return;
                        }
                        if let Some((items, more)) = r {
                            st.queue.extend(items.into_iter().filter(|i| i.id != id && i.kind.playable()));
                            st.radio_more = more;
                        }
                    }
                    app.refresh_queue();
                    app.resume_if_waiting();
                    app.prefetch_next();
                },
            );
        }
    }

    /// Appends more radio tracks (or end-of-queue autoplay) when the queue runs low.
    fn extend_queue(self: &Rc<Self>) {
        let (pag, autoplay) = {
            let mut st = self.st.borrow_mut();
            if st.radio_busy || st.repeat != 0 || st.queue.is_empty() || st.qi + 3 < st.queue.len() {
                return;
            }
            match st.radio_more.take() {
                Some(p) => (Some(p), None),
                None => {
                    let last = st.queue.last().map(|i| i.id.clone()).unwrap_or_default();
                    if st.autoplay_for.as_deref() == Some(last.as_str()) {
                        return;
                    }
                    st.autoplay_for = Some(last.clone());
                    (None, Some(last))
                }
            }
        };
        let req = {
            let mut st = self.st.borrow_mut();
            st.radio_busy = true;
            st.play_req
        };
        let yt = self.yt.clone();
        self.spawn(
            async move {
                match (pag, autoplay) {
                    (Some(p), _) => yt.more_tracks(&p).await.ok(),
                    (None, Some(id)) => yt.radio(&id).await.ok(),
                    _ => None,
                }
            },
            move |app, r| {
                {
                    let mut st = app.st.borrow_mut();
                    if st.play_req != req {
                        return;
                    }
                    st.radio_busy = false;
                    let Some((items, more)) = r else { return };
                    let have: HashSet<String> = st.queue.iter().map(|i| i.id.clone()).collect();
                    st.queue.extend(items.into_iter().filter(|i| i.kind.playable() && !have.contains(&i.id)));
                    st.radio_more = more;
                }
                app.refresh_queue();
                app.resume_if_waiting();
                app.prefetch_next();
            },
        );
    }

    fn resume_if_waiting(self: &Rc<Self>) {
        let waiting = std::mem::take(&mut self.st.borrow_mut().pending_advance);
        if waiting {
            self.advance(false);
        }
    }

    fn refresh_queue(&self) {
        let (queue, qi) = {
            let st = self.st.borrow();
            (st.queue.clone(), st.qi)
        };
        let rows: Vec<TrackData> = queue
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let mut d = track_data(it, i, i == qi);
                d.subtitle = if it.artists.is_empty() { it.subtitle.clone() } else { it.artist_names() }.into();
                d
            })
            .collect();
        self.queue_model.set_vec(rows);
        for (i, it) in queue.iter().enumerate() {
            load_track_image(&self.queue_model, i, &it.id, &it.thumb);
        }
    }

    fn jump_to(self: &Rc<Self>, i: usize) {
        {
            let mut st = self.st.borrow_mut();
            if i >= st.queue.len() {
                return;
            }
            st.qi = i;
            st.fail_streak = 0;
            st.play_req += 1;
        }
        self.start_current();
    }

    fn start_current(self: &Rc<Self>) {
        let (item, tok, next) = {
            let mut st = self.st.borrow_mut();
            st.play_gen += 1;
            st.has_source = false;
            st.loaded_at = None;
            let Some(item) = st.current().cloned() else { return };
            let next = st.next_id();
            st.pending_advance = false;
            let mut keep = vec![item.id.as_str()];
            if let Some(n) = &next {
                keep.push(n);
            }
            st.prune_downloads(&keep);
            (item, st.play_gen, next)
        };
        let mut keep = vec![item.id.as_str()];
        if let Some(n) = &next {
            keep.push(n);
        }
        self.res.retain(&keep);

        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_now(NowData {
            id: item.id.as_str().into(),
            title: item.title.as_str().into(),
            artists: if item.artists.is_empty() { item.subtitle.clone() } else { item.artist_names() }.into(),
            album: item.album.as_ref().map(|a| a.name.as_str()).unwrap_or_default().into(),
            artist_id: item.first_artist_id().unwrap_or_default().into(),
            album_id: item.album.as_ref().and_then(|a| a.id.as_deref()).unwrap_or_default().into(),
            ..Default::default()
        });
        ui.set_buffering(true);
        self.reset_lyrics();
        ui.set_playing(false);
        ui.set_progress(0.0);
        ui.set_time_text("".into());
        self.reload_now_images();
        // Unload the previous track so media keys can't resume it while the
        // next one downloads (this also releases its file for deletion).
        self.player.clear();
        self.mark_active(&item.id);

        let res = self.res.clone();
        let id = item.id.clone();
        let cookie = self.yt.cookie();
        let quality = self.quality_for(&id);
        let handle = self.spawn(
            async move { res.ensure(&id, cookie, quality).await.map_err(|e| format!("{e:#}")) },
            move |app, r| app.file_ready(tok, item, r),
        );
        let id = self.current_id().unwrap_or_default();
        self.st.borrow_mut().downloads.push((id, handle));
    }

    fn file_ready(self: &Rc<Self>, tok: u64, item: Item, r: Result<PathBuf, String>) {
        if self.st.borrow().play_gen != tok {
            return;
        }
        let ui = self.ui();
        ui.set_buffering(false);
        let path = match r {
            Ok(p) => p,
            Err(e) => return self.track_failed(tok, &item.title, &e),
        };
        let artists = if item.artists.is_empty() { item.subtitle.clone() } else { item.artist_names() };
        let album = item.album.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let thumb = images::sized_url(&item.thumb, 300, 300);
        let meta = Meta { title: &item.title, artist: &artists, album: &album, thumb: &thumb };
        if let Err(e) = self.player.load(&path, &meta) {
            return self.decode_failed(tok, &item.title, &e.to_string());
        }
        {
            let mut st = self.st.borrow_mut();
            st.has_source = true;
            st.loaded_at = Some(Instant::now());
        }
        self.prefetch_next();
    }

    fn quality_for(&self, id: &str) -> Quality {
        if self.st.borrow().compat.contains(id) { Quality::Compatible } else { Quality::Best }
    }

    /// Windows couldn't play the downloaded file: fetch the track once more in
    /// the compatible format before giving up on it.
    fn decode_failed(self: &Rc<Self>, tok: u64, title: &str, err: &str) {
        let retry = {
            let mut st = self.st.borrow_mut();
            match st.current().map(|i| i.id.clone()) {
                Some(id) if st.play_gen == tok && !st.compat.contains(&id) => {
                    st.compat.insert(id);
                    true
                }
                _ => false,
            }
        };
        if retry {
            self.start_current();
        } else {
            self.track_failed(tok, title, err);
        }
    }

    /// Shows why a track can't play and moves on, unless several in a row failed.
    fn track_failed(self: &Rc<Self>, tok: u64, title: &str, err: &str) {
        let streak = {
            let mut st = self.st.borrow_mut();
            st.has_source = false;
            st.fail_streak += 1;
            st.fail_streak
        };
        let ui = self.ui();
        ui.set_buffering(false);
        ui.set_playing(false);
        if streak >= MAX_FAILS {
            self.st.borrow_mut().fail_streak = 0;
            ui.set_time_text("Playback stopped".into());
            self.toast(format!("Stopped: {MAX_FAILS} tracks in a row couldn't play ({err})"));
            return;
        }
        self.toast(format!("Couldn't play \"{title}\", skipping: {err}"));
        Timer::single_shot(Duration::from_millis(1500), move || {
            if let Some(a) = app() {
                if a.st.borrow().play_gen == tok {
                    a.advance(false);
                }
            }
        });
    }

    /// Downloads the next track while the current one plays, so skipping is
    /// instant. Also tops up radio queues and keeps only the two needed files.
    fn prefetch_next(self: &Rc<Self>) {
        self.extend_queue();
        let (cur, next) = {
            let st = self.st.borrow();
            (st.current().map(|i| i.id.clone()), st.next_id())
        };
        let mut keep: Vec<&str> = Vec::new();
        if let Some(c) = &cur {
            keep.push(c);
        }
        if let Some(n) = &next {
            keep.push(n);
        }
        self.res.retain(&keep);

        let mut st = self.st.borrow_mut();
        st.prune_downloads(&keep);
        // The current track gets the bandwidth first.
        if !st.has_source {
            return;
        }
        let Some(next) = next.filter(|n| Some(n) != cur.as_ref()) else { return };
        if st.downloads.iter().any(|(id, _)| *id == next) {
            return;
        }
        let res = self.res.clone();
        let cookie = self.yt.cookie();
        let quality = if st.compat.contains(&next) { Quality::Compatible } else { Quality::Best };
        let id = next.clone();
        let h = self.rt.spawn(async move {
            let _ = res.ensure(&id, cookie, quality).await;
        });
        st.downloads.push((next, h.abort_handle()));
    }

    fn mark_active(&self, id: &str) {
        let st = self.st.borrow();
        for m in &st.shelf_models {
            set_active(&m.tracks, id);
        }
        set_active(&self.queue_model, id);
    }

    /// Next button / media key: always moves to the next track.
    fn skip(self: &Rc<Self>) {
        self.st.borrow_mut().fail_streak = 0;
        self.advance(false);
    }

    /// `auto`: the track ended by itself (repeat-one replays it).
    fn advance(self: &Rc<Self>, auto: bool) {
        {
            let mut st = self.st.borrow_mut();
            if auto && st.repeat == 2 && st.has_source {
                drop(st);
                self.player.seek_secs(0.0);
                self.player.play();
                return;
            }
            match st.next_index() {
                Some(i) => st.qi = i,
                None => {
                    // End of the queue: autoplay may still be loading.
                    if st.radio_busy {
                        st.pending_advance = true;
                        drop(st);
                        self.toast("Loading more songs…");
                    }
                    return;
                }
            }
        }
        self.start_current();
    }

    fn prev(self: &Rc<Self>) {
        let has_source = self.st.borrow().has_source;
        let (pos, _) = if has_source { self.player.position() } else { (0.0, 0.0) };
        let restart = {
            let mut st = self.st.borrow_mut();
            if st.queue.is_empty() {
                return;
            }
            if (has_source && pos > 3.0) || st.qi == 0 {
                true
            } else {
                st.qi -= 1;
                st.fail_streak = 0;
                false
            }
        };
        if restart {
            if has_source {
                self.player.seek_secs(0.0);
                self.update_position();
            }
        } else {
            self.start_current();
        }
    }

    fn toggle_play(&self) {
        if self.st.borrow().has_source {
            self.player.toggle();
        }
    }

    fn seek(&self, f: f32) {
        if self.st.borrow().has_source {
            // Ending right after a seek to the end is not a decode failure.
            self.st.borrow_mut().loaded_at = None;
            self.player.seek_fraction(f);
            self.update_position();
        }
    }

    fn toggle_shuffle(self: &Rc<Self>) {
        let on = {
            let mut st = self.st.borrow_mut();
            st.shuffle = !st.shuffle;
            st.shuffle
        };
        self.ui().set_shuffle(on);
        if on {
            let qi = self.st.borrow().qi;
            self.shuffle_after(qi);
            self.refresh_queue();
            self.prefetch_next();
        }
    }

    pub fn set_volume(&self, v: f32) {
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.5 };
        self.player.set_volume(v);
        self.ui().set_volume(v);
        if v > 0.0 {
            self.st.borrow_mut().last_volume = v;
        }
        self.settings.borrow_mut().volume = v;
    }

    fn toggle_mute(&self) {
        let current = self.ui().get_volume();
        if current > 0.0 {
            self.st.borrow_mut().last_volume = current;
            self.set_volume(0.0);
        } else {
            let last = self.st.borrow().last_volume;
            self.set_volume(if last > 0.0 { last } else { 0.5 });
        }
    }

    fn on_player_event(self: &Rc<Self>, ev: PlayerEvent) {
        #[cfg(debug_assertions)]
        eprintln!("player event: {ev:?} pos={:?}", self.player.position());
        match ev {
            PlayerEvent::Playing(p) => {
                self.st.borrow_mut().playing = p;
                self.ui().set_playing(p);
                self.update_timer();
                self.update_position();
            }
            PlayerEvent::Ended => {
                let (has_source, instant, tok, title) = {
                    let st = self.st.borrow();
                    (
                        st.has_source,
                        st.loaded_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)),
                        st.play_gen,
                        st.current().map(|i| i.title.clone()).unwrap_or_default(),
                    )
                };
                if !has_source {
                    // A stale event from the previous track while the next one loads.
                    return;
                }
                if instant {
                    // "Ended" right after loading means the file wasn't playable.
                    self.decode_failed(tok, &title, "the file could not be decoded");
                } else {
                    self.st.borrow_mut().fail_streak = 0;
                    self.advance(true);
                }
            }
            PlayerEvent::Next => self.skip(),
            PlayerEvent::Previous => self.prev(),
            PlayerEvent::Failed(msg) => {
                let (has_source, tok, title) = {
                    let st = self.st.borrow();
                    (st.has_source, st.play_gen, st.current().map(|i| i.title.clone()).unwrap_or_default())
                };
                if has_source {
                    self.decode_failed(tok, &title, &msg);
                }
            }
        }
    }

    fn update_timer(&self) {
        let (playing, visible, synced) = {
            let st = self.st.borrow();
            (st.playing, st.visible, !st.lyrics_times.is_empty())
        };
        if !(playing && visible) {
            self.tick.stop();
            return;
        }
        // Synced lyrics need a finer clock than the progress bar.
        let ms = if synced && self.lyrics_visible() { 150 } else { 500 };
        if self.tick.running() && self.st.borrow().tick_ms == ms {
            return;
        }
        self.st.borrow_mut().tick_ms = ms;
        self.tick.start(TimerMode::Repeated, Duration::from_millis(ms), || {
            if let Some(app) = app() {
                app.update_position();
            }
        });
    }

    fn update_position(&self) {
        if !self.st.borrow().has_source {
            return;
        }
        let (pos, dur) = self.player.position();
        let ui = self.ui();
        if dur > 0.0 {
            ui.set_progress((pos / dur).clamp(0.0, 1.0) as f32);
            ui.set_time_text(format!("{} / {}", fmt_duration(pos as u32), fmt_duration(dur as u32)).into());
        }
        let active = {
            let st = self.st.borrow();
            if st.lyrics_times.is_empty() {
                return;
            }
            lyrics::active_line(&st.lyrics_times, (pos * 1000.0) as u32).map_or(-1, |i| i as i32)
        };
        if ui.get_lyrics_active() != active {
            ui.set_lyrics_active(active);
        }
    }

    // ---------------------------------------------------------------- lyrics

    fn lyrics_visible(&self) -> bool {
        let ui = self.ui();
        ui.get_np_open() && ui.get_np_tab() == 1 && self.st.borrow().visible
    }

    /// The Now Playing panel opened/closed or switched tabs.
    fn np_changed(self: &Rc<Self>) {
        if self.lyrics_visible() {
            self.ensure_lyrics();
        }
        self.update_timer();
        self.update_position();
    }

    fn reset_lyrics(self: &Rc<Self>) {
        {
            let mut st = self.st.borrow_mut();
            st.lyrics_for = None;
            st.lyrics_times.clear();
        }
        let ui = self.ui();
        ui.set_lyrics_state("".into());
        ui.set_lyrics_lines(ModelRc::default());
        ui.set_lyrics_active(-1);
        if self.lyrics_visible() {
            self.ensure_lyrics();
        }
    }

    fn ensure_lyrics(self: &Rc<Self>) {
        let Some(id) = self.current_id() else { return };
        {
            let mut st = self.st.borrow_mut();
            if st.lyrics_for.as_deref() == Some(id.as_str()) {
                return;
            }
            st.lyrics_for = Some(id.clone());
            st.lyrics_times.clear();
        }
        let ui = self.ui();
        ui.set_lyrics_state("loading".into());
        ui.set_lyrics_lines(ModelRc::default());
        ui.set_lyrics_active(-1);
        ui.set_lyrics_synced(false);
        ui.set_lyrics_source("".into());
        let yt = self.yt.clone();
        let vid = id.clone();
        self.spawn(
            async move { yt.lyrics(&vid).await.map_err(|e| format!("{e:#}")) },
            move |app, r| {
                if app.st.borrow().lyrics_for.as_deref() != Some(id.as_str()) {
                    return;
                }
                let ui = app.ui();
                match r {
                    Ok(Some(l)) => {
                        let (lines, times, synced): (Vec<String>, Vec<u32>, bool) = match l.body {
                            LyricsBody::Synced(v) => {
                                let times = v.iter().map(|x| x.0).collect();
                                (v.into_iter().map(|x| x.1).collect(), times, true)
                            }
                            LyricsBody::Plain(v) => (v, Vec::new(), false),
                            LyricsBody::Instrumental => (vec!["Instrumental".into()], Vec::new(), false),
                        };
                        app.st.borrow_mut().lyrics_times = times;
                        let model: Vec<SharedString> = lines.into_iter().map(Into::into).collect();
                        ui.set_lyrics_lines(ModelRc::new(VecModel::from(model)));
                        ui.set_lyrics_synced(synced);
                        ui.set_lyrics_source(if l.source.is_empty() { String::new() } else { format!("Source: {}", l.source) }.into());
                        ui.set_lyrics_state("ready".into());
                        app.update_timer();
                        app.update_position();
                    }
                    Ok(None) => ui.set_lyrics_state("none".into()),
                    Err(_) => {
                        // Allow a retry the next time the tab is opened.
                        app.st.borrow_mut().lyrics_for = None;
                        ui.set_lyrics_state("error".into());
                    }
                }
            },
        );
    }

    /// Clicking a synced line jumps to it.
    fn lyric_clicked(&self, i: i32) {
        let t = {
            let st = self.st.borrow();
            if !st.has_source || i < 0 {
                return;
            }
            st.lyrics_times.get(i as usize).copied()
        };
        if let Some(ms) = t {
            self.st.borrow_mut().loaded_at = None;
            self.player.seek_secs(ms as f64 / 1000.0);
            self.update_position();
        }
    }

    // ---------------------------------------------------------------- account

    /// Restores a saved session at startup and checks it is still valid.
    pub fn restore_session(self: &Rc<Self>) {
        if !self.yt.signed_in() {
            return;
        }
        self.set_signed_in(true);
        let yt = self.yt.clone();
        self.spawn(
            async move { yt.restore_session().await },
            |app, valid| {
                if !valid {
                    app.set_signed_in(false);
                    app.toast("Your YouTube session expired. Sign in again.");
                    app.reload_if(&[Route::Home, Route::Library]);
                }
            },
        );
    }

    fn set_signed_in(self: &Rc<Self>, on: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.signed_in = on;
            // Pages differ when signed in (personal Home, Library, private playlists).
            st.page_cache.clear();
        }
        self.ui().set_signed_in(on);
        if !on {
            self.sidebar_model.set_vec(Vec::new());
            return;
        }
        let yt = self.yt.clone();
        self.spawn(
            async move { yt.sidebar_playlists().await.unwrap_or_default() },
            |app, items| {
                if app.st.borrow().signed_in {
                    app.sidebar_model.set_vec(items.iter().map(card_data).collect::<Vec<_>>());
                }
            },
        );
    }

    fn account_clicked(self: &Rc<Self>) {
        let ui = self.ui();
        if self.st.borrow().signed_in {
            ui.set_account_open(!ui.get_account_open());
        } else {
            self.sign_in();
        }
    }

    pub fn sign_in(self: &Rc<Self>) {
        self.ui().set_account_open(false);
        // Don't let a previous attempt's cleanup delete the new window's profile.
        if let Some(h) = self.profile_wipe.borrow_mut().take() {
            h.abort();
        }
        let opened = login::open(
            &self.data_dir,
            crate::icon::slint_image(64),
            |cookie| {
                if let Some(app) = app() {
                    app.finish_sign_in(cookie);
                }
            },
            |err| {
                if let Some(app) = app() {
                    #[cfg(debug_assertions)]
                    eprintln!("sign-in window failed: {err}");
                    app.toast(format!("Couldn't open the sign-in window: {err}"));
                }
            },
        );
        if let Err(e) = opened {
            #[cfg(debug_assertions)]
            eprintln!("sign-in window failed: {e:#}");
            self.toast(format!("Couldn't open the sign-in window: {e:#}"));
        }
    }

    fn finish_sign_in(self: &Rc<Self>, cookie: String) {
        self.toast("Signing in…");
        let yt = self.yt.clone();
        self.spawn(
            async move { yt.sign_in(cookie).await.map_err(|e| format!("{e:#}")) },
            |app, r| match r {
                Ok(()) => {
                    app.set_signed_in(true);
                    app.toast("Signed in");
                    app.reload_if(&[Route::Home, Route::Library]);
                }
                Err(e) => app.toast(format!("Sign-in failed: {e}")),
            },
        );
        // The temporary browser profile is no longer needed once its
        // processes have exited.
        let dir = self.data_dir.clone();
        let wipe = self.rt.spawn(async move {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !login::profile_dir(&dir).exists() {
                    break;
                }
                login::clear_profile(&dir);
            }
        });
        *self.profile_wipe.borrow_mut() = Some(wipe.abort_handle());
    }

    fn sign_out(self: &Rc<Self>) {
        self.ui().set_account_open(false);
        let yt = self.yt.clone();
        self.spawn(async move { yt.sign_out().await }, |app, ()| {
            app.set_signed_in(false);
            app.toast("Signed out");
            app.reload_if(&[Route::Home, Route::Library]);
        });
    }

    // ---------------------------------------------------------------- window

    pub fn set_visible(&self, visible: bool) {
        self.st.borrow_mut().visible = visible;
        if !visible {
            images::trim();
        } else {
            self.update_position();
        }
        self.update_timer();
    }

    pub fn show_window(self: &Rc<Self>) {
        let ui = self.ui();
        let _ = ui.show();
        crate::win::apply_app_icon(ui.window());
        crate::win::bring_to_front(ui.window());
        self.set_visible(true);
        if self.rescale() {
            // Images were decoded for another display scale.
            let route = self.st.borrow().route.clone();
            if let Some(r) = route {
                self.navigate(r, false);
            }
            self.refresh_queue();
            self.reload_now_images();
        }
    }

    /// Adopts the window's current display scale. True if it changed.
    pub fn rescale(&self) -> bool {
        let s = self.ui().window().scale_factor().clamp(1.0, 4.0);
        if (s - images::scale()).abs() < 0.01 {
            return false;
        }
        images::set_scale(s);
        images::trim();
        true
    }

    fn reload_now_images(&self) {
        let Some(item) = self.st.borrow().current().cloned() else { return };
        for (px, art) in [(40u32, false), (544u32, true)] {
            let id = item.id.clone();
            let weak = self.ui.clone();
            images::request(&item.thumb, px, px, move |img| {
                let Some(ui) = weak.upgrade() else { return };
                let mut now = ui.get_now();
                if now.id.as_str() != id {
                    return;
                }
                if art { now.art = img } else { now.image = img }
                ui.set_now(now);
            });
        }
    }

    pub fn hide_window(&self) {
        let _ = self.ui().hide();
        self.set_visible(false);
    }

    pub fn toggle_window(self: &Rc<Self>) {
        if self.st.borrow().visible {
            self.hide_window();
        } else {
            self.show_window();
        }
    }

    pub fn tray_toggle_play(&self) {
        self.toggle_play();
    }

    pub fn tray_next(self: &Rc<Self>) {
        self.skip();
    }

    pub fn tray_prev(self: &Rc<Self>) {
        self.prev();
    }

    /// Releases the audio file and deletes every cached track.
    pub fn shutdown(&self) {
        login::close();
        {
            let mut st = self.st.borrow_mut();
            for (_, h) in st.downloads.drain(..) {
                h.abort();
            }
        }
        self.player.close();
        self.res.retain(&[]);
    }
}

// -------------------------------------------------------------------- models

/// Header model plus the image to load for it (url, width, height).
fn header_data(h: &Header) -> (HeaderData, Option<(String, u32, u32)>) {
    match h {
        Header::None => (HeaderData::default(), None),
        Header::Title(t) => (HeaderData { kind: "title".into(), title: t.into(), ..Default::default() }, None),
        Header::Collection { title, line1, line2, description, thumb, round } => (
            HeaderData {
                kind: "collection".into(),
                title: title.into(),
                line1: line1.into(),
                line2: line2.into(),
                description: description.into(),
                round: *round,
                ..Default::default()
            },
            Some((thumb.clone(), 264, 264)),
        ),
        Header::Artist { title, description, thumb } => (
            HeaderData {
                kind: "artist".into(),
                title: title.into(),
                description: description.into(),
                ..Default::default()
            },
            Some((thumb.clone(), 1280, 420)),
        ),
    }
}

fn card_data(it: &Item) -> CardData {
    CardData {
        kind: it.kind.as_str().into(),
        id: it.id.as_str().into(),
        playlist_id: it.playlist_id.as_str().into(),
        title: it.title.as_str().into(),
        subtitle: if it.subtitle.is_empty() { it.artist_names() } else { it.subtitle.clone() }.into(),
        image: Image::default(),
        round: it.kind == Kind::Artist,
        wide: it.wide,
    }
}

fn track_data(it: &Item, idx: usize, active: bool) -> TrackData {
    TrackData {
        id: it.id.as_str().into(),
        title: it.title.as_str().into(),
        subtitle: it.row_subtitle().into(),
        artists: if it.artists.is_empty() { it.subtitle.clone() } else { it.artist_names() }.into(),
        album: it.album.as_ref().map(|a| a.name.as_str()).unwrap_or_default().into(),
        duration: it.duration.map(fmt_duration).unwrap_or_default().into(),
        image: Image::default(),
        index: (idx + 1).to_string().into(),
        active,
        artist_id: it.first_artist_id().unwrap_or_default().into(),
        album_id: it.album.as_ref().and_then(|a| a.id.as_deref()).unwrap_or_default().into(),
    }
}

fn shelf_data(s: &Shelf, current: Option<&str>) -> (ShelfData, ShelfModels) {
    let cards = Rc::new(VecModel::default());
    let tracks = Rc::new(VecModel::default());
    match s.kind {
        ShelfKind::Cards => {
            for (i, it) in s.items.iter().enumerate() {
                cards.push(card_data(it));
                let (w, h) = if it.wide { (320, 180) } else { (180, 180) };
                let weak = Rc::downgrade(&cards);
                let id = it.id.clone();
                images::request_shaped(&it.thumb, w, h, it.kind == Kind::Artist, move |img| {
                    let Some(m) = weak.upgrade() else { return };
                    if let Some(mut d) = m.row_data(i) {
                        if d.id.as_str() == id {
                            d.image = img;
                            m.set_row_data(i, d);
                        }
                    }
                });
            }
        }
        _ => {
            for (i, it) in s.items.iter().enumerate() {
                push_track(&tracks, i, it, current);
            }
        }
    }
    let data = ShelfData {
        title: s.title.as_str().into(),
        kind: s.kind.as_str().into(),
        cards: ModelRc::from(cards),
        tracks: ModelRc::from(tracks.clone()),
    };
    (data, ShelfModels { tracks })
}

fn push_track(model: &Rc<VecModel<TrackData>>, i: usize, it: &Item, current: Option<&str>) {
    model.push(track_data(it, i, current == Some(it.id.as_str())));
    load_track_image(model, model.row_count() - 1, &it.id, &it.thumb);
}

fn load_track_image(model: &Rc<VecModel<TrackData>>, row: usize, id: &str, thumb: &str) {
    let weak = Rc::downgrade(model);
    let id = id.to_owned();
    images::request(thumb, 40, 40, move |img| {
        let Some(m) = weak.upgrade() else { return };
        if let Some(mut d) = m.row_data(row) {
            if d.id.as_str() == id {
                d.image = img;
                m.set_row_data(row, d);
            }
        }
    });
}

fn set_active(model: &Rc<VecModel<TrackData>>, id: &str) {
    for i in 0..model.row_count() {
        if let Some(mut d) = model.row_data(i) {
            let active = d.id.as_str() == id;
            if d.active != active {
                d.active = active;
                model.set_row_data(i, d);
            }
        }
    }
}
