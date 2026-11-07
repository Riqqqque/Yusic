//! Lyrics, best first: YouTube Music's own timed lyrics (timed to the exact
//! track being played), time-synced lyrics from LRCLIB (lrclib.net, a free
//! open lyrics database), then YouTube Music's plain lyrics.
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

/// Reads YouTube Music's timed lyrics (from the mobile client's lyrics page).
/// None unless every line has a start time; some sources come without.
pub fn parse_timed(v: &serde_json::Value) -> Option<Lyrics> {
    use serde_json::Value;
    let rows = super::browse::find_key(v, "timedLyricsData")?.as_array()?;
    let mut lines = Vec::with_capacity(rows.len());
    for r in rows {
        let start = r.pointer("/cueRange/startTimeMilliseconds")?.as_str()?.parse::<u32>().ok()?;
        lines.push((start, r.get("lyricLine").and_then(Value::as_str).unwrap_or_default().trim().to_owned()));
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort_by_key(|l| l.0);
    let source = super::browse::find_key(v, "sourceMessage").and_then(Value::as_str).unwrap_or_default();
    Some(Lyrics { body: LyricsBody::Synced(lines), source: source.trim_start_matches("Source:").trim().to_owned() })
}

const LRCLIB: &str = "https://lrclib.net/api";
pub const LRCLIB_SOURCE: &str = "LRCLIB";

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
            // Timed for this length, or nothing to check it against.
            let fits = match (rec.duration, t.duration) {
                (Some(a), Some(b)) => (a - b as f64).abs() <= 3.0,
                _ => true,
            };
            return Ok(to_lyrics(rec, fits));
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

/// How well an LRCLIB title matches ours: exact, whole-word containment of
/// a reasonably long title, or no match.
fn title_match(want: &str, got: &str) -> Option<bool> {
    if want.is_empty() || got.is_empty() {
        return None;
    }
    if want == got {
        return Some(true);
    }
    let (short, long) = if want.len() <= got.len() { (want, got) } else { (got, want) };
    // Short titles ("me", "go") would match far too much.
    if short.len() < 5 || short.split(' ').count() * 2 < long.split(' ').count() {
        return None;
    }
    let padded = format!(" {long} ");
    padded.contains(&format!(" {short} ")).then_some(false)
}

/// Best record whose title matches; `true` when it is confidently the same
/// recording (same title, artist and length), so its timestamps can be trusted.
fn pick(recs: Vec<LrcRecord>, title: &str, t: &TrackInfo<'_>) -> Option<(LrcRecord, bool)> {
    let want = norm(title);
    let mut scored: Vec<(i64, bool, LrcRecord)> = recs
        .into_iter()
        .filter(|r| r.has_lyrics())
        .filter_map(|r| {
            let exact = title_match(&want, &norm(&strip_noise(&r.track_name)))?;
            let same_artist = artist_matches(&r.artist_name, t.artist);
            let diff = match (r.duration, t.duration) {
                (Some(a), Some(b)) => (a - b as f64).abs(),
                _ => 999.0,
            };
            let confident = exact && same_artist && diff <= 3.0;
            // Prefer confident matches, then exact titles, then the right
            // artist, then synced lyrics, then the closest length.
            let score = (!confident as i64) * 100_000_000
                + (!exact as i64) * 1_000_000
                + (!same_artist as i64) * 100_000
                + (r.synced_lyrics.is_none() as i64) * 10_000
                + diff.min(9_999.0) as i64;
            Some((score, confident, r))
        })
        .collect();
    scored.sort_by_key(|s| s.0);
    scored.into_iter().next().map(|(_, c, r)| (r, c))
}

fn to_lyrics(r: LrcRecord, trust_timing: bool) -> Option<Lyrics> {
    let source = LRCLIB_SOURCE.to_string();
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
/// Reads a WebVTT caption file into (start in ms, text), without timing tags
/// or notes like [Music]. YouTube's speech captions repeat each line as they
/// scroll, so only a cue's last line counts, once.
pub fn parse_vtt(text: &str) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut rows = block.lines();
        let Some(head) = rows.find(|l| l.contains(" --> ")) else { continue };
        let Some(ms) = head.split_whitespace().next().and_then(vtt_time) else { continue };
        let Some(line) = rows.map(clean_caption).filter(|l| !l.is_empty()).last() else { continue };
        if out.last().is_none_or(|l| l.1 != line) {
            out.push((ms, line));
        }
    }
    out
}

fn vtt_time(s: &str) -> Option<u32> {
    let (rest, frac) = s.split_once('.')?;
    let mut secs = 0u32;
    for part in rest.split(':') {
        secs = secs * 60 + part.parse::<u32>().ok()?;
    }
    Some(secs * 1000 + frac.parse::<u32>().ok()?)
}

fn clean_caption(line: &str) -> String {
    let mut out = String::new();
    let mut close = None;
    for c in line.chars() {
        match (close, c) {
            (None, '<') => close = Some('>'),
            (None, '[') => close = Some(']'),
            (None, _) => out.push(c),
            (Some(end), _) if c == end => close = None,
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words for comparing lyrics with speech captions, which rarely agree on
/// accents.
fn plain_words(s: &str) -> Vec<String> {
    norm(s)
        .chars()
        .map(|c| match c {
            '\u{e1}' | '\u{e0}' | '\u{e4}' | '\u{e2}' => 'a',
            '\u{e9}' | '\u{e8}' | '\u{eb}' | '\u{ea}' => 'e',
            '\u{ed}' | '\u{ec}' | '\u{ef}' | '\u{ee}' => 'i',
            '\u{f3}' | '\u{f2}' | '\u{f6}' | '\u{f4}' => 'o',
            '\u{fa}' | '\u{f9}' | '\u{fc}' | '\u{fb}' => 'u',
            '\u{f1}' => 'n',
            c => c,
        })
        .collect::<String>()
        .split(' ')
        .map(str::to_owned)
        .collect()
}

/// Checks timed lyrics against speech captions of the recording. Community
/// lyrics are sometimes only partly right; stretches of at least three lines
/// that aren't being sung around their time are replaced with the captions.
/// None when they agree, or when the captions don't look like this recording
/// (too little in common), so unrelated captions can't touch good lyrics.
pub fn repair_with_captions(lines: &[(u32, String)], caps: &[(u32, String)]) -> Option<Vec<(u32, String)>> {
    const BEFORE_MS: u32 = 8_000;
    const AFTER_MS: u32 = 12_000;
    let heard: Vec<(u32, Vec<String>)> = caps.iter().map(|(t, l)| (*t, plain_words(l))).collect();
    // Some(true): being sung then. None: too short to judge.
    let verdicts: Vec<Option<bool>> = lines
        .iter()
        .map(|(t, line)| {
            let words = plain_words(line);
            if words.len() < 3 {
                return None;
            }
            let near: Vec<&String> = heard
                .iter()
                .filter(|(c, _)| *c + BEFORE_MS >= *t && *c <= t + AFTER_MS)
                .flat_map(|(_, w)| w)
                .collect();
            let found = words.iter().filter(|w| near.contains(w)).count();
            Some(found * 2 >= words.len())
        })
        .collect();
    let judged = verdicts.iter().flatten().count();
    let good = verdicts.iter().flatten().filter(|v| **v).count();
    if judged < 6 || good * 10 < judged * 3 {
        return None;
    }
    // The stretches of wrong lines, as (first line, line after).
    let mut wrong = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if verdicts[i] != Some(false) {
            i += 1;
            continue;
        }
        let start = i;
        let mut bad = 0;
        while i < lines.len() && verdicts[i] != Some(true) {
            bad += (verdicts[i] == Some(false)) as usize;
            i += 1;
        }
        if bad >= 3 {
            wrong.push((start, i));
        }
    }
    if wrong.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut next = 0;
    for (start, end) in wrong {
        out.extend_from_slice(&lines[next..start]);
        let from = lines[start].0.saturating_sub(2_000);
        let to = lines.get(end).map_or(u32::MAX, |l| l.0);
        out.extend(caption_lines(caps.iter().filter(|(t, _)| *t >= from && *t < to)));
        next = end;
    }
    out.extend_from_slice(&lines[next..]);
    Some(out)
}

/// Note shown with lyrics whose timing is for another version of the song.
pub const UNTIMED_NOTE: &str = "not timed for this version";

/// Timed lyrics that run past the end of the track belong to a longer
/// version (a live or edited cut): keep the words, drop the timing.
pub fn fit_to_track(l: Lyrics, track_secs: Option<u32>) -> Lyrics {
    const SLACK_MS: u32 = 2_000;
    let LyricsBody::Synced(lines) = &l.body else { return l };
    let Some(secs) = track_secs else { return l };
    let last = lines.iter().rev().find(|x| !x.1.trim().is_empty()).map_or(0, |x| x.0);
    if last <= secs * 1000 + SLACK_MS {
        return l;
    }
    let words = lines.iter().map(|x| x.1.clone()).collect();
    Lyrics { body: LyricsBody::Plain(words), source: format!("{} ({UNTIMED_NOTE})", l.source) }
}

/// For diagnosis: (lines judged, lines heard) of timed lyrics against captions.
pub fn caption_agreement(lines: &[(u32, String)], caps: &[(u32, String)]) -> (usize, usize) {
    let heard: Vec<(u32, Vec<String>)> = caps.iter().map(|(t, l)| (*t, plain_words(l))).collect();
    let mut judged = 0;
    let mut good = 0;
    for (t, line) in lines {
        let words = plain_words(line);
        if words.len() < 3 {
            continue;
        }
        judged += 1;
        let near: Vec<&String> = heard.iter().filter(|(c, _)| *c + 8_000 >= *t && *c <= t + 12_000).flat_map(|(_, w)| w).collect();
        good += (words.iter().filter(|w| near.contains(w)).count() * 2 >= words.len()) as usize;
    }
    (judged, good)
}

/// Captions tidied into lyric lines. Captions wrap wherever they run out of
/// room, so punctuated ones are re-split at their punctuation (word times are
/// spread over each caption); others keep their breaks, with single stray
/// words joining the line before.
fn caption_lines<'a>(caps: impl Iterator<Item = &'a (u32, String)>) -> Vec<(u32, String)> {
    const MAX_WORDS: usize = 9;
    const MAX_SPREAD_MS: u32 = 4_000;
    let caps: Vec<&(u32, String)> = caps.collect();
    let ends_phrase = |w: &str| w.ends_with([',', '.', ';', '?', '!']);
    let mut rows: Vec<(u32, String)> = Vec::new();
    if caps.iter().any(|c| c.1.split(' ').any(ends_phrase)) {
        let (mut start, mut words) = (0, Vec::new());
        for (i, (t, text)) in caps.iter().map(|c| (c.0, &c.1)).enumerate() {
            let span = caps.get(i + 1).map_or(MAX_SPREAD_MS, |n| n.0.saturating_sub(t)).min(MAX_SPREAD_MS);
            let count = text.split(' ').count() as u32;
            for (k, w) in text.split(' ').enumerate() {
                if words.is_empty() {
                    start = t + span * k as u32 / count;
                }
                words.push(w);
                if ends_phrase(w) || words.len() >= MAX_WORDS {
                    rows.push((start, words.join(" ")));
                    words.clear();
                }
            }
        }
        if !words.is_empty() {
            rows.push((start, words.join(" ")));
        }
    } else {
        for (t, text) in caps.iter().map(|c| (c.0, &c.1)) {
            match rows.last_mut() {
                Some(last) if !text.contains(' ') && t.saturating_sub(last.0) < 5_000 => {
                    last.1.push(' ');
                    last.1.push_str(text);
                }
                _ => rows.push((t, text.clone())),
            }
        }
    }
    rows.into_iter()
        .map(|(t, line)| {
            let line = line.trim_end_matches([',', '.', ';']);
            let mut c = line.chars();
            let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
            (t, first + c.as_str())
        })
        .filter(|l| !l.1.is_empty())
        .collect()
}

/// A search result that can stand in for the track when it has no captions:
/// the same song by the same artist, at about the same length.
pub fn matches_track(found: &str, title: &str, artist: &str, secs: Option<u32>, length: Option<u32>) -> bool {
    const SLACK_SECS: u32 = 8;
    let found = format!(" {} ", norm(found));
    let close = matches!((secs, length), (Some(a), Some(b)) if a.abs_diff(b) <= SLACK_SECS);
    close && found.contains(&format!(" {} ", norm(&clean_title(title, artist)))) && found.contains(&format!(" {} ", norm(artist)))
}

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
    // ASCII lowercasing keeps byte offsets identical to `out` (full Unicode
    // lowercasing can change lengths, e.g. 'İ', and break the slicing below).
    let lower = out.to_ascii_lowercase();
    for marker in [" ft. ", " feat. ", " ft ", " featuring "] {
        if let Some(i) = lower.find(marker) {
            let tail = lower[i..].find(" - ").map(|j| out[i + j..].to_string()).unwrap_or_default();
            out = format!("{}{}", &out[..i], tail);
            break;
        }
    }
    let mut out = out.trim().to_string();
    for suffix in [" cover", " lyrics", " official video", " official audio", " audio"] {
        if out.to_ascii_lowercase().ends_with(suffix) {
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

    fn timed(rows: &[(u32, &str)]) -> Vec<(u32, String)> {
        rows.iter().map(|(t, l)| (*t * 1000, l.to_string())).collect()
    }

    #[test]
    fn reads_rolling_captions() {
        let vtt = "WEBVTT\n\n00:00:01.500 --> 00:00:03.000 align:start\n \none<00:00:02.000><c> two</c> [Music]\n\n00:00:03.000 --> 00:00:03.010\none two\n\n00:01:05.250 --> 00:01:07.000\none two\nthree four\n";
        assert_eq!(parse_vtt(vtt), vec![(1500, "one two".into()), (65_250, "three four".into())]);
    }

    #[test]
    fn keeps_lyrics_the_captions_agree_with() {
        let lyrics = timed(&[(10, "red green blue"), (20, "cat dog bird"), (30, "sun moon star"), (40, "fish frog crab"), (50, "oak elm ash"), (60, "tin zinc iron")]);
        assert_eq!(repair_with_captions(&lyrics, &lyrics), None);
    }

    #[test]
    fn replaces_only_the_wrong_stretch() {
        let lyrics = timed(&[(10, "red green blue"), (30, "cat dog bird"), (50, "sun moon star"), (70, "wrong words here"), (90, "not sung at all"), (110, "more made up text"), (130, "the very last line")]);
        let caps = timed(&[(10, "red green blue"), (30, "cat dog bird"), (50, "sun moon star"), (70, "the real verse"), (90, "goes like this")]);
        let fixed = repair_with_captions(&lyrics, &caps).unwrap();
        let text: Vec<&str> = fixed.iter().map(|l| l.1.as_str()).collect();
        assert_eq!(text, ["red green blue", "cat dog bird", "sun moon star", "The real verse", "Goes like this"]);
    }

    #[test]
    fn splits_captions_at_their_punctuation() {
        let caps = timed(&[(10, "the first phrase, and the"), (14, "second one. Then a third")]);
        let got = caption_lines(caps.iter());
        let text: Vec<&str> = got.iter().map(|l| l.1.as_str()).collect();
        assert_eq!(text, ["The first phrase", "And the second one", "Then a third"]);
        assert_eq!(got[0].0, 10_000);
        assert!(got[1].0 > 10_000 && got[1].0 < 14_000);
    }

    #[test]
    fn ignores_captions_of_something_else() {
        let lyrics = timed(&[(10, "red green blue"), (20, "cat dog bird"), (30, "sun moon star"), (40, "fish frog crab"), (50, "oak elm ash"), (60, "tin zinc iron")]);
        let caps = timed(&[(10, "nothing in common"), (30, "with those lines"), (50, "at any point")]);
        assert_eq!(repair_with_captions(&lyrics, &caps), None);
    }

    #[test]
    fn finds_a_stand_in_video() {
        assert!(matches_track("Some Artist - Song (Lyrics)", "Song", "Some Artist", Some(203), Some(200)));
        assert!(!matches_track("Some Artist - Song (Lyrics)", "Song", "Some Artist", Some(240), Some(200)));
        assert!(!matches_track("Other Band - Song (Lyrics)", "Song", "Some Artist", Some(200), Some(200)));
    }

    #[test]
    fn lyrics_longer_than_the_track_lose_their_timing() {
        let l = Lyrics { body: LyricsBody::Synced(timed(&[(3, "a b c"), (178, "d e f")])), source: "LRCLIB".into() };
        let fitted = fit_to_track(l.clone(), Some(168));
        assert!(matches!(fitted.body, LyricsBody::Plain(ref v) if v.len() == 2));
        assert!(fitted.source.contains(UNTIMED_NOTE));
        assert!(matches!(fit_to_track(l.clone(), Some(185)).body, LyricsBody::Synced(_)));
        assert!(matches!(fit_to_track(l, None).body, LyricsBody::Synced(_)));
    }

    #[test]
    fn timed_lyrics_need_every_time() {
        let timed = serde_json::json!({ "a": { "timedLyricsData": [
            { "lyricLine": "two", "cueRange": { "startTimeMilliseconds": "900" } },
            { "lyricLine": "one", "cueRange": { "startTimeMilliseconds": "100" } },
        ], "sourceMessage": "Source: LyricFind" } });
        let l = parse_timed(&timed).unwrap();
        assert_eq!(l.source, "LyricFind");
        assert_eq!(l.body, LyricsBody::Synced(vec![(100, "one".into()), (900, "two".into())]));
        let untimed = serde_json::json!({ "timedLyricsData": [{ "lyricLine": "one" }] });
        assert!(parse_timed(&untimed).is_none());
    }

    #[test]
    fn unicode_titles_dont_panic() {
        let t = "SEVİYORUM SENİ DELİLER GİBİ ft. Şebnem - İstanbul";
        let _ = strip_noise(t);
        let _ = clean_title(t, "İzel");
    }

    #[test]
    fn short_titles_dont_match_everything() {
        assert_eq!(title_match("me", "look what you made me do"), None);
        assert_eq!(title_match("like im gonna lose you", "like im gonna lose you"), Some(true));
        assert_eq!(title_match("like im gonna lose you", "like im gonna lose you remix"), Some(false));
        assert_eq!(title_match("love", "love story"), None);
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
