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

    let video_id = r
        .pointer("/playlistItemData/videoId")
        .or_else(|| col(0).and_then(|t| t.pointer("/runs/0/navigationEndpoint/watchEndpoint/videoId")))
        .or_else(|| r.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId"))
        .and_then(Value::as_str);
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
