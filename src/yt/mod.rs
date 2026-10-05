//! All YouTube Music data access. rustypipe handles the structured pages;
//! Home/Explore use a direct InnerTube browse call (see `browse.rs`).

pub mod browse;
pub mod lyrics;

use std::path::Path;
use std::sync::RwLock;

use anyhow::{Context, Result};
use rustypipe::client::{RustyPipe, RustyPipeQuery};
use rustypipe::model::paginator::Paginator;
use rustypipe::model::richtext::ToPlaintext;
use rustypipe::model::{
    AlbumItem, AlbumType, ArtistId, ArtistItem, MusicItem, MusicPlaylistItem, Thumbnail,
    TrackItem, TrackType,
};
use rustypipe::cache::CacheStorage;
use serde_json::{Value, json};

use crate::auth::{self, CookieStore, ProtectedStorage};

use crate::model::{
    Continuation, Header, Item, Kind, Link, Page, Shelf, ShelfKind, fmt_count,
};

const FALLBACK_CLIENT_VERSION: &str = "1.20260928.13.00";
const ORIGIN: &str = "https://music.youtube.com";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

pub struct Yt {
    rp: RustyPipe,
    http: reqwest::Client,
    cache: ProtectedStorage,
    cookies: CookieStore,
    cookie: RwLock<Option<String>>,
}

impl Yt {
    pub fn new(data_dir: &Path, http: reqwest::Client) -> Result<Self> {
        let storage = data_dir.join("rustypipe");
        std::fs::create_dir_all(&storage)?;
        // Older builds kept rustypipe's cache in plain JSON.
        let _ = std::fs::remove_file(storage.join("rustypipe_cache.json"));
        let cache_path = storage.join("cache.bin");
        // Streams come from yt-dlp, so rustypipe never needs botguard.
        let rp = RustyPipe::builder()
            .storage_dir(&storage)
            .storage(Box::new(ProtectedStorage(cache_path.clone())))
            .no_botguard()
            .build()?;
        let cookies = CookieStore(data_dir.join("auth.bin"));
        let cookie = RwLock::new(cookies.load());
        Ok(Self { rp, http, cache: ProtectedStorage(cache_path), cookies, cookie })
    }

    fn q(&self) -> RustyPipeQuery {
        self.rp.query()
    }

    pub fn signed_in(&self) -> bool {
        self.cookie.read().unwrap().is_some()
    }

    pub fn cookie(&self) -> Option<String> {
        self.cookie.read().unwrap().clone()
    }

    /// Validates the cookie with YouTube and stores it (encrypted).
    pub async fn sign_in(&self, cookie: String) -> Result<()> {
        if auth::cookie_value(&cookie, "SAPISID").is_none()
            && auth::cookie_value(&cookie, "__Secure-3PAPISID").is_none()
        {
            anyhow::bail!("YouTube did not return a signed-in session");
        }
        self.rp
            .user_auth_set_cookie(cookie.clone())
            .await
            .context("YouTube rejected the sign-in")?;
        self.cookies.save(&cookie);
        *self.cookie.write().unwrap() = Some(cookie);
        Ok(())
    }

    pub async fn sign_out(&self) {
        let _ = self.rp.user_auth_remove_cookie().await;
        self.cookies.clear();
        *self.cookie.write().unwrap() = None;
    }

    /// Re-applies the saved cookie at startup. False only when YouTube says
    /// the session is no longer valid (network errors keep the user signed in).
    pub async fn restore_session(&self) -> bool {
        let Some(cookie) = self.cookie() else { return false };
        match self.rp.user_auth_set_cookie(cookie).await {
            Ok(()) => true,
            Err(rustypipe::error::Error::Auth(_)) => {
                self.sign_out().await;
                false
            }
            Err(_) => true,
        }
    }

    /// rustypipe keeps the current YTM web client version in its cache.
    fn client_version(&self) -> String {
        self.cache
            .read()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| {
                v.pointer("/clients/desktop_music/data/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_owned())
    }

    async fn innertube_browse(&self, browse_id: Option<&str>, ctoken: Option<&str>) -> Result<Value> {
        let mut url = String::from("https://music.youtube.com/youtubei/v1/browse?prettyPrint=false");
        if let Some(t) = ctoken {
            url.push_str(&format!("&ctoken={t}&continuation={t}&type=next"));
        }
        let mut body = json!({
            "context": { "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": self.client_version(),
                "hl": "en",
                "gl": "US",
            }}
        });
        if let Some(id) = browse_id {
            body["browseId"] = json!(id);
        }
        let mut req = self
            .http
            .post(url)
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::USER_AGENT, BROWSER_UA);
        if let Some(cookie) = self.cookie() {
            if let Some(auth) = auth::sapisid_hash(&cookie, ORIGIN) {
                req = req
                    .header(reqwest::header::COOKIE, cookie)
                    .header(reqwest::header::AUTHORIZATION, auth)
                    .header("X-Origin", ORIGIN)
                    .header("X-Goog-AuthUser", "0");
            }
        }
        let resp = req.json(&body).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }

    async fn browse_page(&self, browse_id: &str, prefetch: usize) -> Result<Page> {
        let v = self.innertube_browse(Some(browse_id), None).await?;
        let mut parsed = browse::parse_browse(&v);
        // The first response only carries a few shelves; pull more so the page fills the screen.
        for _ in 0..prefetch {
            let Some(t) = parsed.continuation.take() else { break };
            match self.innertube_browse(None, Some(&t)).await {
                Ok(v) => {
                    let more = browse::parse_browse(&v);
                    parsed.shelves.extend(more.shelves);
                    parsed.continuation = more.continuation;
                }
                Err(_) => break,
            }
        }
        if parsed.shelves.is_empty() {
            anyhow::bail!("YouTube Music returned an empty page");
        }
        Ok(Page {
            shelves: parsed.shelves,
            continuation: parsed.continuation.map(Continuation::Browse),
            ..Default::default()
        })
    }

    pub async fn home(&self) -> Result<Page> {
        self.browse_page("FEmusic_home", 0).await
    }

    pub async fn explore(&self) -> Result<Page> {
        let mut page = self.browse_page("FEmusic_explore", 0).await?;
        page.header = Header::Title("Explore".into());
        Ok(page)
    }

    pub async fn browse_more(&self, token: &str) -> Result<(Vec<Shelf>, Option<String>)> {
        let v = self.innertube_browse(None, Some(token)).await?;
        let p = browse::parse_browse(&v);
        Ok((p.shelves, p.continuation))
    }

    pub async fn album(&self, id: &str) -> Result<Page> {
        let a = self.q().music_album(id).await.context("could not load album")?;
        let album_link = Link { name: a.name.clone(), id: Some(a.id.clone()) };
        let tracks: Vec<Item> = a
            .tracks
            .iter()
            .map(|t| {
                let mut it = track_item(t);
                it.album.get_or_insert(album_link.clone());
                if it.thumb.is_empty() {
                    it.thumb = best_thumb(&a.cover);
                }
                it
            })
            .collect();
        let total: u32 = tracks.iter().filter_map(|t| t.duration).sum();
        let mut line1 = vec![album_type_name(a.album_type).to_owned(), join_artists(&a.artists)];
        if let Some(y) = a.year {
            line1.push(y.to_string());
        }
        let mut shelves = vec![Shelf { title: String::new(), kind: ShelfKind::Album, items: tracks.clone() }];
        if !a.variants.is_empty() {
            shelves.push(Shelf {
                title: "Other versions".into(),
                kind: ShelfKind::Cards,
                items: a.variants.iter().map(album_item).collect(),
            });
        }
        Ok(Page {
            header: Header::Collection {
                title: a.name.clone(),
                line1: line1.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • "),
                line2: format!("{} • {}", songs(a.track_count.max(tracks.len() as u16) as u64), fmt_total(total)),
                description: a.description.as_ref().map(|d| d.to_plaintext()).unwrap_or_default(),
                thumb: best_thumb(&a.cover),
                round: false,
            },
            radio: tracks.first().map(|t| format!("RDAMVM{}", t.id)),
            play: tracks,
            shelves,
            ..Default::default()
        })
    }

    pub async fn playlist(&self, id: &str) -> Result<Page> {
        let p = if self.signed_in() {
            match self.q().authenticated().music_playlist(id).await {
                Ok(p) => p,
                Err(_) => self.q().music_playlist(id).await.context("could not load playlist")?,
            }
        } else {
            self.q().music_playlist(id).await.context("could not load playlist")?
        };
        let tracks: Vec<Item> = p.tracks.items.iter().map(track_item).collect();
        let mut line1 = vec!["Playlist".to_owned()];
        if let Some(c) = &p.channel {
            line1.push(c.name.clone());
        }
        let count = p.track_count.unwrap_or(tracks.len() as u64);
        let mut shelves = vec![Shelf { title: String::new(), kind: ShelfKind::TrackList, items: tracks.clone() }];
        if !p.related_playlists.items.is_empty() {
            shelves.push(Shelf {
                title: "Related playlists".into(),
                kind: ShelfKind::Cards,
                items: p.related_playlists.items.iter().map(playlist_item).collect(),
            });
        }
        let continuation = p.tracks.ctoken.is_some().then(|| Continuation::Tracks(p.tracks.clone()));
        Ok(Page {
            header: Header::Collection {
                title: p.name.clone(),
                line1: line1.join(" • "),
                line2: songs(count),
                description: p.description.as_ref().map(|d| d.to_plaintext()).unwrap_or_default(),
                thumb: best_thumb(&p.thumbnail),
                round: false,
            },
            radio: tracks.first().map(|t| format!("RDAMVM{}", t.id)),
            play: tracks,
            shelves,
            continuation,
            ..Default::default()
        })
    }

    pub async fn more_tracks(&self, pag: &Paginator<TrackItem>) -> Result<(Vec<Item>, Option<Paginator<TrackItem>>)> {
        let Some(next) = pag.next(self.q()).await? else {
            return Ok((Vec::new(), None));
        };
        let items = next.items.iter().map(track_item).collect();
        let more = next.ctoken.is_some().then_some(next);
        Ok((items, more))
    }

    pub async fn artist(&self, id: &str) -> Result<Page> {
        let a = self.q().music_artist(id, false).await.context("could not load artist")?;
        let top: Vec<Item> = a.tracks.iter().map(track_item).collect();
        let mut shelves = Vec::new();
        if !top.is_empty() {
            shelves.push(Shelf { title: "Top songs".into(), kind: ShelfKind::Tracks, items: top.clone() });
        }
        let (albums, singles): (Vec<&AlbumItem>, Vec<&AlbumItem>) =
            a.albums.iter().partition(|al| matches!(al.album_type, AlbumType::Album | AlbumType::Audiobook));
        for (title, list) in [("Albums", albums), ("Singles & EPs", singles)] {
            if !list.is_empty() {
                shelves.push(Shelf {
                    title: title.into(),
                    kind: ShelfKind::Cards,
                    items: list.into_iter().map(album_item).collect(),
                });
            }
        }
        if !a.playlists.is_empty() {
            shelves.push(Shelf {
                title: "Featured on".into(),
                kind: ShelfKind::Cards,
                items: a.playlists.iter().map(playlist_item).collect(),
            });
        }
        if !a.similar_artists.is_empty() {
            shelves.push(Shelf {
                title: "Fans might also like".into(),
                kind: ShelfKind::Cards,
                items: a.similar_artists.iter().map(artist_item).collect(),
            });
        }
        let mut description = a.description.clone().unwrap_or_default();
        if let Some(subs) = a.subscriber_count {
            description = if description.is_empty() {
                format!("{} subscribers", fmt_count(subs))
            } else {
                format!("{} subscribers • {description}", fmt_count(subs))
            };
        }
        Ok(Page {
            header: Header::Artist {
                title: a.name.clone(),
                description,
                thumb: best_thumb(&a.header_image),
            },
            radio: a.radio_id.clone(),
            play: top,
            shelves,
            ..Default::default()
        })
    }

    pub async fn search(&self, query: &str) -> Result<Page> {
        // The combined search mis-parses song rows (artist "Song", no album), so
        // songs and videos come from their filtered searches, fetched in parallel.
        let q = self.q();
        let (main, tracks, videos) = tokio::join!(
            q.music_search_main(query),
            q.music_search_tracks(query),
            q.music_search_videos(query),
        );
        let r = main.context("search failed")?;
        let mut top: Option<Item> = None;
        let songs: Vec<Item> = tracks.map(|r| r.items.items.iter().take(6).map(track_item).collect()).unwrap_or_default();
        let videos: Vec<Item> = videos.map(|r| r.items.items.iter().take(4).map(track_item).collect()).unwrap_or_default();
        let (mut albums, mut artists, mut playlists) = (Vec::new(), Vec::new(), Vec::new());
        for it in &r.items.items {
            let item = match it {
                MusicItem::Track(t) => track_item(t),
                MusicItem::Album(a) => album_item(a),
                MusicItem::Artist(a) => artist_item(a),
                MusicItem::Playlist(p) => playlist_item(p),
                MusicItem::User(_) => continue,
            };
            if top.is_none() {
                top = Some(item.clone());
            }
            match item.kind {
                Kind::Song | Kind::Video | Kind::Episode => {}
                Kind::Album => albums.push(item),
                Kind::Artist => artists.push(item),
                Kind::Playlist | Kind::Podcast => playlists.push(item),
            }
        }
        let mut shelves = Vec::new();
        if let Some(t) = top.filter(|t| !t.kind.playable()) {
            shelves.push(Shelf { title: "Top result".into(), kind: ShelfKind::Cards, items: vec![t] });
        }
        for (title, kind, items) in [
            ("Songs", ShelfKind::Tracks, songs),
            ("Videos", ShelfKind::Tracks, videos),
            ("Albums", ShelfKind::Cards, albums),
            ("Artists", ShelfKind::Cards, artists),
            ("Playlists", ShelfKind::Cards, playlists),
        ] {
            if !items.is_empty() {
                shelves.push(Shelf { title: title.into(), kind, items });
            }
        }
        let notice = if shelves.is_empty() { format!("No results for \"{query}\"") } else { String::new() };
        Ok(Page { shelves, notice, ..Default::default() })
    }

    pub async fn library(&self) -> Result<Page> {
        if !self.signed_in() {
            return Ok(Page {
                header: Header::Title("Library".into()),
                notice: "Sign in to see your playlists, liked songs and saved albums.".into(),
                ..Default::default()
            });
        }
        let q = self.q();
        let (playlists, albums, artists, history) = tokio::join!(
            q.music_saved_playlists(),
            q.music_saved_albums(),
            q.music_saved_artists(),
            q.music_history(),
        );
        if let (Err(rustypipe::error::Error::Auth(_)), Err(_), Err(_)) = (&playlists, &albums, &artists) {
            anyhow::bail!("Your YouTube session expired. Sign in again.");
        }
        let mut shelves = Vec::new();
        let mut lists = vec![liked_music_item()];
        if let Ok(p) = playlists {
            lists.extend(p.items.iter().filter(|p| p.id != "LM").map(playlist_item));
        }
        shelves.push(Shelf { title: "Playlists".into(), kind: ShelfKind::Cards, items: lists });
        if let Ok(h) = history {
            let items: Vec<Item> = h.items.iter().take(20).map(|h| track_item(&h.item)).collect();
            if !items.is_empty() {
                shelves.push(Shelf { title: "Recently played".into(), kind: ShelfKind::List, items });
            }
        }
        if let Ok(a) = albums {
            if !a.items.is_empty() {
                shelves.push(Shelf {
                    title: "Albums".into(),
                    kind: ShelfKind::Cards,
                    items: a.items.iter().map(album_item).collect(),
                });
            }
        }
        if let Ok(a) = artists {
            if !a.items.is_empty() {
                shelves.push(Shelf {
                    title: "Artists".into(),
                    kind: ShelfKind::Cards,
                    items: a.items.iter().map(artist_item).collect(),
                });
            }
        }
        Ok(Page { header: Header::Title("Library".into()), shelves, ..Default::default() })
    }

    /// Liked music + saved playlists for the sidebar.
    pub async fn sidebar_playlists(&self) -> Result<Vec<Item>> {
        let p = self.q().music_saved_playlists().await?;
        let mut out = vec![liked_music_item()];
        out.extend(p.items.iter().filter(|p| p.id != "LM").map(playlist_item));
        Ok(out)
    }

    /// Lyrics for a track: synced lyrics from LRCLIB when available, else
    /// YouTube Music's own. Ok(None) means no source has lyrics.
    pub async fn lyrics(&self, video_id: &str) -> Result<Option<lyrics::Lyrics>> {
        let details = self.q().music_details(video_id).await.context("could not load track details")?;
        let t = &details.track;
        let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
        let info = lyrics::TrackInfo {
            title: &t.name,
            artist: &artist,
            album: t.album.as_ref().map(|a| a.name.as_str()),
            duration: t.duration,
        };
        let ytm = async {
            let id = details.lyrics_id.as_ref()?;
            let l = self.q().music_lyrics(id).await.ok()?;
            (!l.body.trim().is_empty()).then(|| lyrics::Lyrics {
                body: lyrics::LyricsBody::Plain(lyrics::plain_lines(&l.body)),
                source: l.footer.trim().trim_start_matches("Source:").trim().to_string(),
            })
        };
        let (lrc, ytm) = tokio::join!(lyrics::lrclib(&self.http, &info), ytm);
        let lrc = lrc.ok().flatten();
        Ok(match (lrc, ytm) {
            (Some(l), _) if matches!(l.body, lyrics::LyricsBody::Synced(_)) => Some(l),
            (_, Some(y)) => Some(y),
            (l, None) => l,
        })
    }

    pub async fn suggestions(&self, query: &str) -> Result<Vec<String>> {
        Ok(self.q().music_search_suggestion(query).await?.terms)
    }

    /// Up-next radio for a track (what YTM plays after a single song), plus
    /// a paginator for more.
    pub async fn radio(&self, video_id: &str) -> Result<(Vec<Item>, Option<Paginator<TrackItem>>)> {
        let r = self.q().music_radio_track(video_id).await?;
        Ok(radio_parts(r))
    }

    pub async fn radio_playlist(&self, radio_id: &str) -> Result<(Vec<Item>, Option<Paginator<TrackItem>>)> {
        let r = self.q().music_radio(radio_id).await?;
        Ok(radio_parts(r))
    }

    /// Tracks to play when a card's play button is pressed.
    pub async fn playable(&self, kind: Kind, id: &str) -> Result<Vec<Item>> {
        Ok(match kind {
            Kind::Album => self.album(id).await?.play,
            Kind::Playlist | Kind::Podcast => self.playlist(id).await?.play,
            Kind::Artist => {
                let page = self.artist(id).await?;
                match page.radio {
                    Some(r) => self.radio_playlist(&r).await.map(|r| r.0).unwrap_or(page.play),
                    None => page.play,
                }
            }
            _ => Vec::new(),
        })
    }
}

fn radio_parts(r: Paginator<TrackItem>) -> (Vec<Item>, Option<Paginator<TrackItem>>) {
    let items = r.items.iter().map(track_item).collect();
    let more = r.ctoken.is_some().then_some(r);
    (items, more)
}

fn liked_music_item() -> Item {
    Item {
        kind: Kind::Playlist,
        id: "LM".into(),
        title: "Liked music".into(),
        subtitle: "Auto playlist".into(),
        thumb: "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music-@576.png".into(),
        ..Default::default()
    }
}

fn join_artists(a: &[ArtistId]) -> String {
    a.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn links(a: &[ArtistId]) -> Vec<Link> {
    a.iter().map(|a| Link { name: a.name.clone(), id: a.id.clone() }).collect()
}

pub fn best_thumb(t: &[Thumbnail]) -> String {
    t.iter().max_by_key(|t| t.width).map(|t| t.url.clone()).unwrap_or_default()
}

fn album_type_name(t: AlbumType) -> &'static str {
    match t {
        AlbumType::Album => "Album",
        AlbumType::Ep => "EP",
        AlbumType::Single => "Single",
        AlbumType::Audiobook => "Audiobook",
        AlbumType::Show => "Podcast",
        _ => "Album",
    }
}

fn songs(n: u64) -> String {
    format!("{} song{}", fmt_count(n), if n == 1 { "" } else { "s" })
}

fn fmt_total(secs: u32) -> String {
    let (h, m) = (secs / 3600, secs / 60 % 60);
    let plural = |n: u32, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    match (h, m) {
        (0, m) => plural(m, "minute"),
        (h, 0) => plural(h, "hour"),
        (h, m) => format!("{}, {}", plural(h, "hour"), plural(m, "minute")),
    }
}

pub fn track_item(t: &TrackItem) -> Item {
    Item {
        kind: match t.track_type {
            TrackType::Video => Kind::Video,
            TrackType::Episode => Kind::Episode,
            _ => Kind::Song,
        },
        id: t.id.clone(),
        title: t.name.clone(),
        thumb: best_thumb(&t.cover),
        artists: links(&t.artists),
        album: t.album.as_ref().map(|a| Link { name: a.name.clone(), id: Some(a.id.clone()) }),
        duration: t.duration,
        ..Default::default()
    }
}

fn album_item(a: &AlbumItem) -> Item {
    let mut sub = vec![album_type_name(a.album_type).to_owned()];
    let artists = join_artists(&a.artists);
    if !artists.is_empty() {
        sub.push(artists);
    }
    if let Some(y) = a.year {
        sub.push(y.to_string());
    }
    Item {
        kind: Kind::Album,
        id: a.id.clone(),
        title: a.name.clone(),
        subtitle: sub.join(" • "),
        thumb: best_thumb(&a.cover),
        artists: links(&a.artists),
        ..Default::default()
    }
}

fn artist_item(a: &ArtistItem) -> Item {
    Item {
        kind: Kind::Artist,
        id: a.id.clone(),
        title: a.name.clone(),
        subtitle: a
            .subscriber_count
            .map(|n| format!("{} subscribers", fmt_count(n)))
            .unwrap_or_else(|| "Artist".into()),
        thumb: best_thumb(&a.avatar),
        ..Default::default()
    }
}

fn playlist_item(p: &MusicPlaylistItem) -> Item {
    let mut sub = vec![if p.is_podcast { "Podcast" } else { "Playlist" }.to_owned()];
    if let Some(c) = &p.channel {
        sub.push(c.name.clone());
    }
    if let Some(n) = p.track_count {
        sub.push(format!("{} songs", fmt_count(n)));
    }
    Item {
        kind: if p.is_podcast { Kind::Podcast } else { Kind::Playlist },
        id: p.id.clone(),
        title: p.name.clone(),
        subtitle: sub.join(" • "),
        thumb: best_thumb(&p.thumbnail),
        ..Default::default()
    }
}
