//! Matching a music video to the song it's for.
//!
//! YouTube Music's radio is mostly music videos, and many are live, acoustic
//! or TV performances that YouTube names with the plain song title (the AMA
//! performance of "Monster" is just "Monster"). Like YouTube Music's "don't
//! play music videos" setting, Yusic plays the official song instead. A song
//! counts as the same only when its title, artist and kind of version agree.

use crate::model::Item;

/// Words that mark a recording as a different version of a song.
const MARKS: &[(&str, &str)] = &[
    ("live", "live"),
    ("acoustic", "acoustic"),
    ("unplugged", "acoustic"),
    ("stripped", "acoustic"),
    ("session", "session"),
    ("sessions", "session"),
    ("lounge", "session"),
    ("remix", "remix"),
    ("rmx", "remix"),
    ("mix", "remix"),
    ("cover", "cover"),
    ("instrumental", "instrumental"),
    ("karaoke", "instrumental"),
    ("acapella", "acapella"),
    ("cappella", "acapella"),
    ("sped", "speed"),
    ("slowed", "speed"),
    ("nightcore", "speed"),
    ("reverb", "speed"),
    ("demo", "demo"),
    ("piano", "piano"),
    ("orchestral", "orchestral"),
    ("symphonic", "orchestral"),
    ("rehearsal", "live"),
    ("performance", "live"),
    ("concert", "live"),
    ("tour", "live"),
];

/// The kinds of version a title names, sorted (e.g. ["acoustic", "live"]).
pub fn marks(title: &str) -> Vec<&'static str> {
    let lower = title.to_lowercase();
    let mut out: Vec<&'static str> = words(&lower)
        .filter_map(|w| MARKS.iter().find(|(m, _)| *m == w).map(|(_, k)| *k))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty())
}

/// A title without its brackets, featured artists or "Artist - " prefix,
/// for comparing two names of the same song.
pub fn base_title(title: &str, artists: &[String]) -> String {
    let mut s = String::new();
    let mut depth = 0usize;
    for c in title.to_lowercase().chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => s.push(c),
            _ => {}
        }
    }
    // "Artist - Song" uploads.
    if let Some((before, after)) = s.split_once(" - ") {
        let before = norm(before);
        if artists.iter().any(|a| before.contains(&norm(a))) {
            s = after.to_owned();
        }
    }
    for cut in [" feat. ", " feat ", " ft. ", " ft ", " featuring "] {
        if let Some(i) = s.find(cut) {
            s.truncate(i);
        }
    }
    norm(&s)
}

fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace('&', " and ");
    words(&s).collect::<Vec<_>>().join(" ")
}

fn artist_names(i: &Item) -> Vec<String> {
    i.artists.iter().map(|a| a.name.clone()).collect()
}

fn same_artist(a: &Item, b: &Item) -> bool {
    a.artists.iter().any(|x| {
        b.artists.iter().any(|y| {
            (x.id.is_some() && x.id == y.id) || (!x.name.is_empty() && norm(&x.name) == norm(&y.name))
        })
    })
}

/// The song among `songs` (search results, best first) that is the same
/// recording kind as the video: same title, a shared artist, and the same
/// version marks. None when no song qualifies.
pub fn pick_song(video: &Item, songs: &[Item]) -> Option<Item> {
    let names = artist_names(video);
    let want = base_title(&video.title, &names);
    if want.is_empty() || video.artists.is_empty() {
        return None;
    }
    let want_marks = marks(&video.title);
    songs
        .iter()
        .filter(|s| s.kind == crate::model::Kind::Song && !s.id.is_empty() && s.id != video.id)
        .find(|s| {
            base_title(&s.title, &artist_names(s)) == want && same_artist(video, s) && marks(&s.title) == want_marks
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Link};

    fn item(kind: Kind, id: &str, title: &str, artists: &[(&str, &str)]) -> Item {
        Item {
            kind,
            id: id.into(),
            title: title.into(),
            artists: artists.iter().map(|(n, i)| Link { name: (*n).into(), id: Some((*i).into()) }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn names_the_kind_of_version() {
        assert_eq!(marks("Night Changes (Live Acoustic Session)"), ["acoustic", "live", "session"]);
        assert_eq!(marks("God Is A Woman in the Live Lounge"), ["live", "session"]);
        assert_eq!(marks("Peaches (feat. Giveon & Daniel Caesar)"), Vec::<&str>::new());
        assert_eq!(marks("Mixed Signals"), Vec::<&str>::new());
        assert_eq!(marks("Circles (Acoustic – One Night in Rome, Italy 2022)"), ["acoustic"]);
    }

    #[test]
    fn compares_titles_without_extras() {
        let a = ["Justin Bieber".to_string()];
        assert_eq!(base_title("Peaches (feat. GIVĒON & Daniel Caesar)", &a), "peaches");
        assert_eq!(base_title("Peaches (feat. Daniel Caesar & Giveon)", &a), "peaches");
        assert_eq!(base_title("Justin Bieber - Peaches", &a), "peaches");
        assert_eq!(base_title("What Do You Mean?", &a), "what do you mean");
        assert_eq!(base_title("Love Me Like You Do - From Fifty Shades", &a), "love me like you do from fifty shades");
    }

    #[test]
    fn a_live_video_named_like_the_song_plays_the_song() {
        let artists = [("Shawn Mendes", "UC1"), ("Justin Bieber", "UC2")];
        let video = item(Kind::Video, "ZXgvZZxoitc", "Monster", &artists);
        let songs = [
            item(Kind::Song, "live1", "Monster (Live)", &artists),
            item(Kind::Song, "other", "Monster", &[("Someone Else", "UC9")]),
            item(Kind::Song, "studio", "Monster", &[("Shawn Mendes", "UC1")]),
        ];
        assert_eq!(pick_song(&video, &songs).map(|s| s.id).as_deref(), Some("studio"));
    }

    #[test]
    fn an_honestly_named_version_stays_that_version() {
        let artists = [("Justin Bieber", "UC2")];
        let video = item(Kind::Video, "v", "What Do You Mean? (Acoustic)", &artists);
        let studio = item(Kind::Song, "s", "What Do You Mean?", &artists);
        assert!(pick_song(&video, std::slice::from_ref(&studio)).is_none());
        let acoustic = item(Kind::Song, "a", "What Do You Mean? (Acoustic)", &artists);
        assert_eq!(pick_song(&video, &[studio, acoustic]).map(|s| s.id).as_deref(), Some("a"));
    }

    #[test]
    fn different_songs_are_not_swapped() {
        let artists = [("Justin Bieber", "UC2")];
        let video = item(Kind::Video, "v", "Lonely", &artists);
        let songs = [item(Kind::Song, "s", "Lonely Girl", &artists), item(Kind::Video, "w", "Lonely", &artists)];
        assert!(pick_song(&video, &songs).is_none());
    }
}
