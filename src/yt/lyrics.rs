//! Lyrics: time-synced lyrics from LRCLIB (lrclib.net, a free open lyrics
//! database) when available, otherwise YouTube Music's own plain lyrics.
//!
//! Matching has to cope with video uploads ("Artist - Song (Official Video)
//! ft. X | Cover"). A match that isn't clearly the same recording (another
//! artist, or a different length, as with covers) is shown as plain text so
//! the line highlighting can't drift out of sync.

use anyhow::Result;
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq)]
pub enum LyricsBody {
    /// (start time in ms, line), sorted by time.
    Synced(Vec<(u32, String)>),
    Plain(Vec<String>),
    Instrumental,
}

#[derive(Clone, Debug)]
pub struct Lyrics {
    pub body: LyricsBody,
    pub source: String,
}

/// What we know about the track, for matching.
pub struct TrackInfo<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: Option<&'a str>,
    pub duration: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct LrcRecord {
    track_name: String,
    artist_name: String,
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    duration: Option<f64>,
}

impl LrcRecord {
    fn has_lyrics(&self) -> bool {
        self.instrumental
            || self.synced_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.plain_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty())
    }
}

const LRCLIB: &str = "https://lrclib.net/api";

/// A (title, artist) pair to search for.
struct Query {
    title: String,
    artist: String,
}

/// Looks the track up on LRCLIB, from the most to the least specific query.
pub async fn lrclib(http: &reqwest::Client, t: &TrackInfo<'_>) -> Result<Option<Lyrics>> {
    // 1. Exact match on the metadata we have.
    let mut url = reqwest::Url::parse(&format!("{LRCLIB}/get"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("track_name", t.title).append_pair("artist_name", t.artist);
        if let Some(a) = t.album {
            q.append_pair("album_name", a);
        }
        if let Some(d) = t.duration {
            q.append_pair("duration", &d.to_string());
        }
    }
    let resp = http.get(url).send().await?;
    if resp.status().is_success() {
        let rec: LrcRecord = resp.json().await.unwrap_or_default();
        if rec.has_lyrics() {
            return Ok(to_lyrics(rec, true));
        }
    } else if resp.status() != reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("LRCLIB returned HTTP {}", resp.status());
    }

    // 2. Structured searches with cleaned-up titles.
    let mut best: Option<(LrcRecord, bool)> = None;
    for q in queries(t.title, t.artist) {
        let mut url = reqwest::Url::parse(&format!("{LRCLIB}/search"))?;
        url.query_pairs_mut().append_pair("track_name", &q.title).append_pair("artist_name", &q.artist);
        let recs = search(http, url).await;
        if let Some(pick) = pick(recs, &q.title, t) {
            if pick.1 {
                return Ok(to_lyrics(pick.0, true));
            }
            best = best.or(Some(pick));
        }
    }

    // 3. Free-text search as a last resort.
    if best.is_none() {
        let text = strip_noise(t.title);
        let text = if norm(&text).contains(&norm(t.artist)) { text } else { format!("{} {text}", t.artist) };
        let mut url = reqwest::Url::parse(&format!("{LRCLIB}/search"))?;
        url.query_pairs_mut().append_pair("q", &text);
        let recs = search(http, url).await;
        best = pick(recs, &clean_title(t.title, t.artist), t);
    }
    Ok(best.and_then(|(rec, confident)| to_lyrics(rec, confident)))
}

async fn search(http: &reqwest::Client, url: reqwest::Url) -> Vec<LrcRecord> {
    match http.get(url).send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Best record whose title matches; `true` when it is confidently the same
/// recording (same artist and length), so its timestamps can be trusted.
fn pick(recs: Vec<LrcRecord>, title: &str, t: &TrackInfo<'_>) -> Option<(LrcRecord, bool)> {
    let want = norm(title);
    let mut scored: Vec<(i64, bool, LrcRecord)> = recs
        .into_iter()
        .filter(|r| r.has_lyrics())
        .filter(|r| {
            let got = norm(&strip_noise(&r.track_name));
            !got.is_empty() && (got.contains(&want) || want.contains(&got))
        })
        .map(|r| {
            let same_artist = artist_matches(&r.artist_name, t.artist);
            let diff = match (r.duration, t.duration) {
                (Some(a), Some(b)) => (a - b as f64).abs(),
                _ => 999.0,
            };
            let confident = same_artist && diff <= 3.0;
            // Prefer confident matches, then synced lyrics, then close length.
            let score = (!confident as i64) * 1_000_000
                + (r.synced_lyrics.is_none() as i64) * 10_000
                + diff.min(9_999.0) as i64;
            (score, confident, r)
        })
        .collect();
    scored.sort_by_key(|s| s.0);
    scored.into_iter().next().map(|(_, c, r)| (r, c))
}

fn to_lyrics(r: LrcRecord, trust_timing: bool) -> Option<Lyrics> {
    let source = "LRCLIB".to_string();
    if r.instrumental {
        return Some(Lyrics { body: LyricsBody::Instrumental, source });
    }
    let synced = r.synced_lyrics.as_deref().map(parse_lrc).filter(|l| !l.is_empty());
    if let Some(lines) = synced {
        if trust_timing {
            return Some(Lyrics { body: LyricsBody::Synced(lines), source });
        }
        // Different recording: keep the words, drop the timing.
        if r.plain_lyrics.as_deref().is_none_or(|p| p.trim().is_empty()) {
            return Some(Lyrics { body: LyricsBody::Plain(lines.into_iter().map(|l| l.1).collect()), source });
        }
    }
    let plain = r.plain_lyrics.filter(|p| !p.trim().is_empty())?;
    Some(Lyrics { body: LyricsBody::Plain(plain_lines(&plain)), source })
}

pub fn plain_lines(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim_end().to_string()).collect()
}

/// Parses `[mm:ss.xx] text` lines (several timestamps per line allowed).
pub fn parse_lrc(text: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            match parse_time(&stripped[..end]) {
                Some(ms) => times.push(ms),
                None => break, // metadata tag like [ar:...]
            }
            rest = stripped[end + 1..].trim_start();
        }
        for ms in times {
            out.push((ms, rest.to_string()));
        }
    }
    out.sort_by_key(|l| l.0);
    out
}

fn parse_time(s: &str) -> Option<u32> {
    let (m, sec) = s.split_once(':')?;
    let m: u32 = m.trim().parse().ok()?;
    let sec: f64 = sec.trim().parse().ok()?;
    if !(0.0..60.0).contains(&sec) {
        return None;
    }
    Some(m * 60_000 + (sec * 1000.0).round() as u32)
}

/// Lowercase letters/digits only, for loose comparisons.
fn norm(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\u{2019}' | '\'' => None,
            c if c.is_alphanumeric() => Some(c.to_lowercase().next().unwrap_or(c)),
            _ => Some(' '),
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn artist_matches(a: &str, b: &str) -> bool {
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// Removes video decorations: "(Official Video)", "[Lyrics]", "ft. X",
/// everything after " | ", trailing "Cover"/"Lyrics" and similar.
pub fn strip_noise(title: &str) -> String {
    const NOISE: [&str; 13] = [
        "official", "video", "lyric", "visualizer", "audio", "feat", "ft.", "remaster", "live", "version", "mv",
        "cover", "hd",
    ];
    let title = title.split(" | ").next().unwrap_or(title);
    let mut out = String::new();
    let mut depth = 0;
    let mut group = String::new();
    for c in title.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                if depth == 1 {
                    group.clear();
                    continue;
                }
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let g = group.to_lowercase();
                    if !NOISE.iter().any(|n| g.contains(n)) {
                        out.push('(');
                        out.push_str(&group);
                        out.push(')');
                    }
                    continue;
                }
            }
            _ => {}
        }
        if depth > 0 {
            group.push(c);
        } else {
            out.push(c);
        }
    }
    // "Song ft. Someone" / "Song feat. Someone" (outside brackets): cut the credit,
    // but keep a following " - " part.
    let lower = out.to_lowercase();
    for marker in [" ft. ", " feat. ", " ft ", " featuring "] {
        if let Some(i) = lower.find(marker) {
            let tail = lower[i..].find(" - ").map(|j| out[i + j..].to_string()).unwrap_or_default();
            out = format!("{}{}", &out[..i], tail);
            break;
        }
    }
    let mut out = out.trim().to_string();
    for suffix in [" cover", " lyrics", " official video", " official audio", " audio"] {
        if out.to_lowercase().ends_with(suffix) {
            out.truncate(out.len() - suffix.len());
            out = out.trim_end().to_string();
        }
    }
    out
}

/// "Song (Official Video) [feat. X]" -> "Song"; "Artist - Song" -> "Song".
pub fn clean_title(title: &str, artist: &str) -> String {
    let stripped = strip_noise(title);
    let out = match split_artist_title(&stripped) {
        // "Song - Remastered 2011": the part before the dash is the title.
        Some((a, b)) if is_noise_suffix(&b) => a,
        // "Artist - Song" (the usual video upload style).
        Some((a, b)) => {
            if artist_matches(&b, artist) && !artist_matches(&a, artist) { a } else { b }
        }
        None => stripped.clone(),
    };
    if out.is_empty() { title.to_string() } else { out }
}

fn split_artist_title(s: &str) -> Option<(String, String)> {
    let (a, b) = s.split_once(" - ")?;
    let (a, b) = (a.trim(), b.trim());
    (!a.is_empty() && !b.is_empty()).then(|| (a.to_string(), b.to_string()))
}

/// "Song - Remastered 2011" style suffixes after a dash.
fn is_noise_suffix(s: &str) -> bool {
    let l = s.to_lowercase();
    ["remaster", "live", "version", "edit", "mix", "mono", "stereo", "demo", "acoustic"]
        .iter()
        .any(|n| l.contains(n))
}

/// Search candidates: the cleaned title with the channel/artist we have, and
/// for "Artist - Song" uploads the artist named in the title.
fn queries(title: &str, artist: &str) -> Vec<Query> {
    let stripped = strip_noise(title);
    let mut out = Vec::new();
    match split_artist_title(&stripped) {
        Some((a, b)) if !is_noise_suffix(&b) => {
            out.push(Query { title: b.clone(), artist: a.clone() });
            if !artist_matches(&a, artist) {
                out.push(Query { title: b, artist: artist.to_string() });
            }
        }
        Some((a, _)) => out.push(Query { title: a, artist: artist.to_string() }),
        None => out.push(Query { title: stripped, artist: artist.to_string() }),
    }
    out.retain(|q| !q.title.is_empty() && !q.artist.is_empty());
    out
}

/// Index of the line being sung at `ms` (None before the first line).
pub fn active_line(lines: &[u32], ms: u32) -> Option<usize> {
    match lines.partition_point(|&t| t <= ms) {
        0 => None,
        n => Some(n - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc() {
        let l = parse_lrc("[ar:Someone]\n[00:12.50] Hello\n[01:02.00][00:05.00]Twice\n[00:20.00]");
        assert_eq!(
            l,
            vec![(5000, "Twice".into()), (12500, "Hello".into()), (20000, "".into()), (62000, "Twice".into())]
        );
    }

    #[test]
    fn active_line_lookup() {
        let t = [1000, 5000, 9000];
        assert_eq!(active_line(&t, 0), None);
        assert_eq!(active_line(&t, 1000), Some(0));
        assert_eq!(active_line(&t, 8999), Some(1));
        assert_eq!(active_line(&t, 60000), Some(2));
    }

    #[test]
    fn cleans_titles() {
        assert_eq!(
            clean_title("We Don't Fight Anymore (Official Music Video) (feat. Chris Stapleton)", "Carly Pearce"),
            "We Don't Fight Anymore"
        );
        assert_eq!(clean_title("Instant Crush (feat. Julian Casablancas)", "Daft Punk"), "Instant Crush");
        assert_eq!(clean_title("Song (Acoustic)", "X"), "Song (Acoustic)");
        assert_eq!(clean_title("Taylor Swift - Cleveland! (Official Lyric Video)", "Taylor Swift"), "Cleveland!");
        assert_eq!(
            clean_title("Meghan Trainor - Like I'm Gonna Lose You ft. John Legend | Cover", "Samantha Harvey"),
            "Like I'm Gonna Lose You"
        );
        assert_eq!(clean_title("Hey Jude - Remastered 2015", "The Beatles"), "Hey Jude");
    }

    #[test]
    fn cover_queries_use_the_original_artist() {
        let q = queries("Meghan Trainor - Like I'm Gonna Lose You ft. John Legend | Cover", "Samantha Harvey");
        assert_eq!(q[0].title, "Like I'm Gonna Lose You");
        assert_eq!(q[0].artist, "Meghan Trainor");
        assert_eq!(q[1].artist, "Samantha Harvey");
    }

    #[test]
    fn untrusted_timing_becomes_plain() {
        let rec = LrcRecord {
            synced_lyrics: Some("[00:01.00]a\n[00:02.00]b".into()),
            ..Default::default()
        };
        assert_eq!(to_lyrics(rec, false).unwrap().body, LyricsBody::Plain(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn normalizes() {
        assert_eq!(norm("Like I’m Gonna Lose You!"), "like im gonna lose you");
        assert!(artist_matches("Meghan Trainor, John Legend", "Meghan Trainor"));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Hits lrclib.net: `cargo test -- --ignored lrclib_`.
    #[tokio::test]
    #[ignore]
    async fn lrclib_finds_lyrics_for_a_cover_upload() {
        let http = reqwest::Client::builder().user_agent("Yusic tests").build().unwrap();
        let t = TrackInfo {
            title: "Meghan Trainor - Like I'm Gonna Lose You ft. John Legend | Cover",
            artist: "Samantha Harvey",
            album: None,
            duration: Some(204),
        };
        let l = lrclib(&http, &t).await.unwrap().expect("lyrics");
        // Different singer: words, but no timing.
        assert!(matches!(l.body, LyricsBody::Plain(ref lines) if lines.len() > 10), "{:?}", l.body);
    }
}
