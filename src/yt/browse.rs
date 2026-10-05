//! Minimal parser for YouTube Music InnerTube "browse" responses (Home, Explore).
//! Unknown renderers are skipped so layout changes degrade to missing shelves,
//! not errors.

use serde_json::Value;

use crate::model::{Item, Kind, Link, Shelf, ShelfKind};

pub struct Parsed {
    pub shelves: Vec<Shelf>,
    pub continuation: Option<String>,
}

pub fn parse_browse(v: &Value) -> Parsed {
    // Initial page: contents.singleColumnBrowseResultsRenderer.tabs[0].tabRenderer.content.sectionListRenderer
    // Continuation: continuationContents.sectionListContinuation
    let section = v
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer")
        .or_else(|| v.pointer("/continuationContents/sectionListContinuation"));
    let Some(section) = section else {
        return Parsed { shelves: Vec::new(), continuation: None };
    };
    let shelves = section
        .get("contents")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_section).collect())
        .unwrap_or_default();
    let continuation = section
        .pointer("/continuations/0/nextContinuationData/continuation")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Parsed { shelves, continuation }
}

fn parse_section(s: &Value) -> Option<Shelf> {
    if let Some(r) = s.get("musicCarouselShelfRenderer").or_else(|| s.get("musicImmersiveCarouselShelfRenderer")) {
        let title = runs_text(r.pointer("/header/musicCarouselShelfBasicHeaderRenderer/title")
            .or_else(|| r.pointer("/header/musicImmersiveCarouselShelfBasicHeaderRenderer/title")));
        let contents = r.get("contents")?.as_array()?;
        let list = contents.iter().any(|c| c.get("musicResponsiveListItemRenderer").is_some());
        let items: Vec<Item> = contents.iter().filter_map(parse_item).collect();
        if items.is_empty() {
            return None;
        }
        return Some(Shelf {
            title,
            kind: if list { ShelfKind::List } else { ShelfKind::Cards },
            items,
        });
    }
    if let Some(r) = s.get("musicShelfRenderer") {
        let title = runs_text(r.get("title"));
        let items: Vec<Item> = r.get("contents")?.as_array()?.iter().filter_map(parse_item).collect();
        if items.is_empty() {
            return None;
        }
        return Some(Shelf { title, kind: ShelfKind::Tracks, items });
    }
    if let Some(r) = s.get("gridRenderer") {
        let title = runs_text(r.pointer("/header/gridHeaderRenderer/title"));
        let items: Vec<Item> = r.get("items")?.as_array()?.iter().filter_map(parse_item).collect();
        if items.is_empty() {
            return None;
        }
        return Some(Shelf { title, kind: ShelfKind::Cards, items });
    }
    None
}

fn parse_item(c: &Value) -> Option<Item> {
    if let Some(r) = c.get("musicTwoRowItemRenderer") {
        return parse_two_row(r);
    }
    if let Some(r) = c.get("musicResponsiveListItemRenderer") {
        return parse_list_item(r);
    }
    None
}

fn parse_two_row(r: &Value) -> Option<Item> {
    let title = runs_text(r.get("title"));
    let subtitle_runs = r.pointer("/subtitle/runs").and_then(Value::as_array);
    let subtitle = runs_text(r.get("subtitle"));
    let thumb = last_thumb(r.pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails"));
    let wide = r
        .get("aspectRatio")
        .and_then(Value::as_str)
        .is_some_and(|a| a.contains("RECTANGLE"));
    let nav = r.get("navigationEndpoint")?;
    let mut item = Item {
        title,
        subtitle,
        thumb,
        wide,
        artists: subtitle_runs.map(|r| artist_links(r)).unwrap_or_default(),
        ..Default::default()
    };
    if let Some(w) = nav.get("watchEndpoint") {
        item.id = w.get("videoId")?.as_str()?.to_owned();
        item.kind = watch_kind(w);
        item.playlist_id = w.get("playlistId").and_then(Value::as_str).unwrap_or_default().to_owned();
        return Some(item);
    }
    let b = nav.get("browseEndpoint")?;
    let (kind, id) = browse_target(b)?;
    item.kind = kind;
    item.id = id;
    Some(item)
}

fn parse_list_item(r: &Value) -> Option<Item> {
    let col = |i: usize| r.pointer(&format!("/flexColumns/{i}/musicResponsiveListItemFlexColumnRenderer/text"));
    let title = runs_text(col(0));
    let mut item = Item {
        title,
        thumb: last_thumb(r.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")),
        ..Default::default()
    };
    // Second (and third) column: artists, album, plays
    let mut sub_runs: Vec<Value> = Vec::new();
    for i in 1..r.get("flexColumns").and_then(Value::as_array).map_or(0, Vec::len) {
        if let Some(runs) = col(i).and_then(|t| t.get("runs")).and_then(Value::as_array) {
            if !sub_runs.is_empty() {
                sub_runs.push(serde_json::json!({ "text": " • " }));
            }
            sub_runs.extend(runs.iter().cloned());
        }
    }
    item.artists = artist_links(&sub_runs);
    item.album = sub_runs.iter().find_map(|run| {
        let b = run.pointer("/navigationEndpoint/browseEndpoint")?;
        let (kind, id) = browse_target(b)?;
        (kind == Kind::Album).then(|| Link { name: run_str(run), id: Some(id) })
    });
    item.subtitle = sub_runs.iter().map(run_str).collect::<String>();
    item.duration = r
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text")
        .map(|t| runs_text(Some(t)))
        .and_then(|t| parse_duration(&t));

    let video_id = r
        .pointer("/playlistItemData/videoId")
        .or_else(|| col(0).and_then(|t| t.pointer("/runs/0/navigationEndpoint/watchEndpoint/videoId")))
        .or_else(|| r.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId"))
        .and_then(Value::as_str);
    item.set_video_id = r
        .pointer("/playlistItemData/playlistSetVideoId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(v) = video_id {
        item.id = v.to_owned();
        let w = col(0)
            .and_then(|t| t.pointer("/runs/0/navigationEndpoint/watchEndpoint"))
            .or_else(|| r.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint"));
        item.kind = w.map(watch_kind).unwrap_or(Kind::Song);
        return Some(item);
    }
    let b = r.pointer("/navigationEndpoint/browseEndpoint")?;
    let (kind, id) = browse_target(b)?;
    item.kind = kind;
    item.id = id;
    Some(item)
}

/// "3:45" / "1:02:03" -> seconds.
fn parse_duration(s: &str) -> Option<u32> {
    let mut total = 0u32;
    for part in s.trim().split(':') {
        total = total.checked_mul(60)?.checked_add(part.trim().parse().ok()?)?;
    }
    (total > 0).then_some(total)
}

pub struct ParsedPlaylist {
    /// The user's own (editable) playlist.
    pub owned: bool,
    /// "Save to library" state, when the page offers it.
    pub saved: Option<bool>,
    pub title: String,
    pub subtitle: String,
    pub second_subtitle: String,
    pub description: String,
    pub thumb: String,
    pub items: Vec<Item>,
    pub continuation: Option<String>,
}

/// A playlist page (`browseId=VL<id>`), including the user's own editable
/// playlists and Liked music, which use the album-style responsive header.
pub fn parse_playlist(v: &Value) -> Option<ParsedPlaylist> {
    let tc = v.pointer("/contents/twoColumnBrowseResultsRenderer")?;
    let head_section = tc.pointer("/tabs/0/tabRenderer/content/sectionListRenderer/contents/0");
    let header = head_section.and_then(|h| {
        h.pointer("/musicEditablePlaylistDetailHeaderRenderer/header/musicResponsiveHeaderRenderer")
            .or_else(|| h.get("musicResponsiveHeaderRenderer"))
    });
    let shelf = tc.pointer("/secondaryContents/sectionListRenderer/contents/0/musicPlaylistShelfRenderer")?;
    let (items, continuation) = list_with_continuation(shelf.get("contents"));
    let description = header
        .and_then(|h| h.pointer("/description/musicDescriptionShelfRenderer/description"))
        .map(|d| runs_text(Some(d)))
        .unwrap_or_default();
    let owned = head_section.is_some_and(|h| h.get("musicEditablePlaylistDetailHeaderRenderer").is_some());
    Some(ParsedPlaylist {
        owned,
        saved: if owned { None } else { library_toggle(v) },
        title: runs_text(header.and_then(|h| h.get("title"))),
        subtitle: runs_text(header.and_then(|h| h.get("subtitle"))),
        second_subtitle: runs_text(header.and_then(|h| h.get("secondSubtitle"))),
        description,
        thumb: last_thumb(header.and_then(|h| h.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails"))),
        items,
        continuation: continuation.or_else(|| {
            shelf
                .pointer("/continuations/0/nextContinuationData/continuation")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }),
    })
}

/// Depth-first search for the first object with key `key`.
pub fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                return Some(x);
            }
            m.values().find_map(|x| find_key(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

fn all_with_key<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                out.push(x);
            }
            for x in m.values() {
                all_with_key(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| all_with_key(x, key, out)),
        _ => {}
    }
}

/// The "Save to library" toggle on album/playlist pages: Some(saved).
pub fn library_toggle(v: &Value) -> Option<bool> {
    let mut toggles = Vec::new();
    all_with_key(v.get("contents").unwrap_or(v), "toggleButtonRenderer", &mut toggles);
    toggles.into_iter().find_map(|t| {
        let icon = t.pointer("/defaultIcon/iconType").and_then(Value::as_str).unwrap_or_default();
        icon.contains("LIBRARY").then(|| t.get("isToggled").and_then(Value::as_bool).unwrap_or(false))
    })
}

/// Artist page subscribe button: (channel id, subscribed).
pub fn subscription(v: &Value) -> Option<(String, bool)> {
    let b = find_key(v, "subscribeButtonRenderer")?;
    let id = b.get("channelId").and_then(Value::as_str)?.to_owned();
    Some((id, b.get("subscribed").and_then(Value::as_bool).unwrap_or(false)))
}

/// Like status from a `next` response: "LIKE", "DISLIKE" or "INDIFFERENT".
pub fn like_status(v: &Value) -> Option<String> {
    let b = find_key(v, "likeButtonRenderer")?;
    b.get("likeStatus").and_then(Value::as_str).map(str::to_owned)
}

/// More playlist rows, from either continuation response style.
pub fn parse_playlist_continuation(v: &Value) -> (Vec<Item>, Option<String>) {
    if let Some(items) = v.pointer("/onResponseReceivedActions/0/appendContinuationItemsAction/continuationItems") {
        return list_with_continuation(Some(items));
    }
    if let Some(shelf) = v.pointer("/continuationContents/musicPlaylistShelfContinuation") {
        let (items, cont) = list_with_continuation(shelf.get("contents"));
        let cont = cont.or_else(|| {
            shelf
                .pointer("/continuations/0/nextContinuationData/continuation")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        return (items, cont);
    }
    (Vec::new(), None)
}

/// Rows plus the token from a trailing `continuationItemRenderer`, if any.
fn list_with_continuation(contents: Option<&Value>) -> (Vec<Item>, Option<String>) {
    let Some(arr) = contents.and_then(Value::as_array) else { return (Vec::new(), None) };
    let mut cont = None;
    let items = arr
        .iter()
        .filter_map(|c| {
            if let Some(t) = c
                .pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token")
                .and_then(Value::as_str)
            {
                cont = Some(t.to_owned());
                return None;
            }
            parse_item(c)
        })
        .filter(|i| i.kind.playable())
        .collect();
    (items, cont)
}

fn watch_kind(w: &Value) -> Kind {
    match w
        .pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")
        .and_then(Value::as_str)
    {
        Some("MUSIC_VIDEO_TYPE_ATV") | None => Kind::Song,
        Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE") => Kind::Episode,
        Some(_) => Kind::Video,
    }
}

/// Maps a browse endpoint to (kind, id usable with rustypipe).
fn browse_target(b: &Value) -> Option<(Kind, String)> {
    let id = b.get("browseId")?.as_str()?;
    let page = b
        .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = match page {
        "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => Kind::Album,
        "MUSIC_PAGE_TYPE_PLAYLIST" => Kind::Playlist,
        "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" | "MUSIC_PAGE_TYPE_LIBRARY_ARTIST" => Kind::Artist,
        "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE" => Kind::Podcast,
        _ if id.starts_with("MPREb") => Kind::Album,
        _ if id.starts_with("VL") => Kind::Playlist,
        _ if id.starts_with("UC") => Kind::Artist,
        _ => return None,
    };
    let id = match kind {
        Kind::Playlist | Kind::Podcast => id.strip_prefix("VL").unwrap_or(id),
        _ => id,
    };
    Some((kind, id.to_owned()))
}

fn artist_links(runs: &[Value]) -> Vec<Link> {
    runs.iter()
        .filter_map(|run| {
            let b = run.pointer("/navigationEndpoint/browseEndpoint")?;
            let (kind, id) = browse_target(b)?;
            (kind == Kind::Artist).then(|| Link { name: run_str(run), id: Some(id) })
        })
        .collect()
}

fn run_str(run: &Value) -> String {
    run.get("text").and_then(Value::as_str).unwrap_or_default().to_owned()
}

pub fn runs_text(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.get("simpleText").and_then(Value::as_str) {
        return s.to_owned();
    }
    v.get("runs")
        .and_then(Value::as_array)
        .map(|runs| runs.iter().map(run_str).collect())
        .unwrap_or_default()
}

fn last_thumb(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .and_then(|a| a.last())
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("3:45"), Some(225));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("live"), None);
    }

    #[test]
    fn parses_editable_playlist() {
        let row = |id: &str| serde_json::json!({"musicResponsiveListItemRenderer": {
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Song"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Artist", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCx",
                    "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}}}]}}}
            ],
            "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "3:05"}]}}}],
            "playlistItemData": {"videoId": id}
        }});
        let v = serde_json::json!({"contents": {"twoColumnBrowseResultsRenderer": {
            "tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [{"musicEditablePlaylistDetailHeaderRenderer": {"header": {"musicResponsiveHeaderRenderer": {
                "title": {"runs": [{"text": "Mine"}]},
                "subtitle": {"runs": [{"text": "Playlist"}, {"text": " • "}, {"text": "Private"}]},
                "secondSubtitle": {"runs": [{"text": "2 songs"}]},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [{"url": "t"}]}}}
            }}}}]}}}}],
            "secondaryContents": {"sectionListRenderer": {"contents": [{"musicPlaylistShelfRenderer": {"contents": [
                row("aaaaaaaaaaa"), row("bbbbbbbbbbb"),
                {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "NEXT"}}}}
            ]}}]}}
        }}});
        let p = parse_playlist(&v).unwrap();
        assert_eq!(p.title, "Mine");
        assert_eq!(p.second_subtitle, "2 songs");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].duration, Some(185));
        assert_eq!(p.items[0].artists[0].name, "Artist");
        assert_eq!(p.continuation.as_deref(), Some("NEXT"));
    }

    #[test]
    fn parses_carousel_and_continuation() {
        let v = serde_json::json!({
            "contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {
                "contents": [{"musicCarouselShelfRenderer": {
                    "header": {"musicCarouselShelfBasicHeaderRenderer": {"title": {"runs": [{"text": "Hits"}]}}},
                    "contents": [{"musicTwoRowItemRenderer": {
                        "title": {"runs": [{"text": "Country Hotlist"}]},
                        "subtitle": {"runs": [{"text": "Ella", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCabc",
                            "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}}}]},
                        "thumbnailRenderer": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [{"url": "a"}, {"url": "b"}]}}},
                        "navigationEndpoint": {"browseEndpoint": {"browseId": "VLRDCLAK5uy",
                            "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": "MUSIC_PAGE_TYPE_PLAYLIST"}}}}
                    }}]
                }}, {"musicTastebuilderShelfRenderer": {}}],
                "continuations": [{"nextContinuationData": {"continuation": "TOKEN"}}]
            }}}}]}}
        });
        let p = parse_browse(&v);
        assert_eq!(p.continuation.as_deref(), Some("TOKEN"));
        assert_eq!(p.shelves.len(), 1);
        let it = &p.shelves[0].items[0];
        assert_eq!(it.kind, Kind::Playlist);
        assert_eq!(it.id, "RDCLAK5uy");
        assert_eq!(it.thumb, "b");
        assert_eq!(it.artists[0].id.as_deref(), Some("UCabc"));
    }
}

#[cfg(test)]
mod sample_tests {
    /// Runs the playlist parser on a saved response:
    /// `YUSIC_PLAYLIST_JSON=path cargo test -- --ignored sample_playlist`.
    #[test]
    #[ignore]
    fn sample_playlist() {
        let path = std::env::var("YUSIC_PLAYLIST_JSON").expect("YUSIC_PLAYLIST_JSON");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let p = super::parse_playlist(&v).expect("parsed");
        let with_dur = p.items.iter().filter(|i| i.duration.is_some()).count();
        let with_artist = p.items.iter().filter(|i| !i.artists.is_empty()).count();
        let with_thumb = p.items.iter().filter(|i| !i.thumb.is_empty()).count();
        println!(
            "title={} items={} durations={} artists={} thumbs={} header_thumb={} continuation={}",
            !p.title.is_empty(), p.items.len(), with_dur, with_artist, with_thumb, !p.thumb.is_empty(), p.continuation.is_some()
        );
        assert!(!p.items.is_empty());
    }
}
