//! Compares lyrics coverage: YouTube Music (via rustypipe) vs LRCLIB.
use rustypipe::client::RustyPipe;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rp = RustyPipe::builder().storage_dir(std::env::temp_dir()).no_botguard().build()?;
    let http = reqwest::Client::builder().user_agent("Yusic (https://github.com)").build()?;
    for id in ["khnokW3Mw24", "fa5IWHDbftI", "0n5_EWUHhS4", "cWKnNCS3q7o", "nnIHW-aDhZk", "dQw4w9WgXcQ", "kJQP7kiw5Fk", "JGwWNGJdvx8"] {
        let d = rp.query().music_details(id).await?;
        let t = &d.track;
        let artist = t.artists.first().map(|a| a.name.clone()).unwrap_or_default();
        let ytm = match &d.lyrics_id {
            Some(l) => rp.query().music_lyrics(l).await.map(|l| format!("{} lines", l.body.lines().count())).unwrap_or_else(|e| format!("err {e}")),
            None => "none".into(),
        };
        let mut url = reqwest::Url::parse("https://lrclib.net/api/get")?;
        url.query_pairs_mut().append_pair("track_name", &t.name).append_pair("artist_name", &artist);
        if let Some(a) = &t.album { url.query_pairs_mut().append_pair("album_name", &a.name); }
        if let Some(dur) = t.duration { url.query_pairs_mut().append_pair("duration", &dur.to_string()); }
        let r = http.get(url).send().await?;
        let status = r.status();
        let v: serde_json::Value = r.json().await.unwrap_or_default();
        let lrc = if status.is_success() {
            format!("synced={} plain={}", v["syncedLyrics"].as_str().is_some(), v["plainLyrics"].as_str().is_some())
        } else { format!("HTTP {status}") };
        println!("{id} {:?} / {artist} ({:?}s) | ytm: {ytm} | lrclib: {lrc}", t.name, t.duration);
    }
    Ok(())
}
