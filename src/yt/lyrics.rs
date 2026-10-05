//! Lyrics: time-synced lyrics from LRCLIB (lrclib.net, a free open lyrics
//! database) when available, otherwise YouTube Music's own plain lyrics.

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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrcRecord {
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    duration: Option<f64>,
}

const LRCLIB: &str = "https://lrclib.net/api";

/// Looks the track up on LRCLIB: exact match first, then a fuzzy search
/// with a cleaned-up title, keeping only results of about the same length.
pub async fn lrclib(http: &reqwest::Client, t: &TrackInfo<'_>) -> Result<Option<Lyrics>> {
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
        let rec: LrcRecord = resp.json().await?;
        if let Some(l) = to_lyrics(rec) {
            return Ok(Some(l));
        }
    } else if resp.status() != reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("LRCLIB returned HTTP {}", resp.status());
    }

    let mut url = reqwest::Url::parse(&format!("{LRCLIB}/search"))?;
    url.query_pairs_mut()
        .append_pair("track_name", &clean_title(t.title, t.artist))
        .append_pair("artist_name", t.artist);
    let resp = http.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let mut recs: Vec<LrcRecord> = resp.json().await.unwrap_or_default();
    // Same song length (±3 s) when we know it, synced lyrics preferred.
    if let Some(d) = t.duration {
        recs.retain(|r| r.duration.is_none_or(|rd| (rd - d as f64).abs() <= 3.0));
    }
    recs.sort_by_key(|r| (r.synced_lyrics.is_none(), r.plain_lyrics.is_none()));
    Ok(recs.into_iter().find_map(to_lyrics))
}

fn to_lyrics(r: LrcRecord) -> Option<Lyrics> {
    let source = "LRCLIB".to_string();
    if r.instrumental {
        return Some(Lyrics { body: LyricsBody::Instrumental, source });
    }
    if let Some(lines) = r.synced_lyrics.as_deref().map(parse_lrc).filter(|l| !l.is_empty()) {
        return Some(Lyrics { body: LyricsBody::Synced(lines), source });
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

/// "Song (Official Video) [feat. X]" -> "Song"; "Artist - Song" -> "Song".
pub fn clean_title(title: &str, artist: &str) -> String {
    const NOISE: [&str; 12] = [
        "official", "video", "lyric", "visualizer", "audio", "feat", "ft.", "remaster", "live", "edit", "version", "mv",
    ];
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
    let out = match out.split_once(" - ") {
        Some((a, b)) if !artist.is_empty() && a.to_lowercase().contains(&artist.to_lowercase()) => b,
        Some((a, _)) => a,
        None => &out,
    }
    .trim()
    .to_string();
    if out.is_empty() { title.to_string() } else { out }
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
        assert_eq!(clean_title("We Don't Fight Anymore (Official Music Video) (feat. Chris Stapleton)", "Carly Pearce"), "We Don't Fight Anymore");
        assert_eq!(clean_title("Instant Crush (feat. Julian Casablancas)", "Daft Punk"), "Instant Crush");
        assert_eq!(clean_title("Song (Acoustic)", "X"), "Song (Acoustic)");
        assert_eq!(clean_title("Taylor Swift - Cleveland! (Official Lyric Video)", "Taylor Swift"), "Cleveland!");
    }
}
