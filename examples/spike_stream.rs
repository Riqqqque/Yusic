//! Spike 1: resolve + download audio bytes via rustypipe and via yt-dlp, and time both.
//!
//! cargo run --release --example spike_stream

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rustypipe::client::RustyPipe;
use rustypipe::model::AudioFormat;
use rustypipe::param::StreamFilter;

const CHUNK: u64 = 10 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let out = std::env::temp_dir().join("yusic-spike");
    std::fs::create_dir_all(&out)?;

    let rp = RustyPipe::builder().storage_dir(&out).build()?;
    let http = reqwest::Client::builder().build()?;

    let mut ids: Vec<(String, String)> = Vec::new();
    for q in ["daft punk", "taylor swift", "lofi hip hop"] {
        let t = Instant::now();
        let res = rp.query().music_search_tracks(q).await?;
        println!("search {q:?}: {} items in {:?}", res.items.items.len(), t.elapsed());
        ids.extend(res.items.items.into_iter().take(3).map(|t| (t.id, t.name)));
    }
    // Music videos (non-ATV uploads)
    ids.push(("dQw4w9WgXcQ".into(), "Never Gonna Give You Up (MV)".into()));

    let mut ok = [0u32; 2];
    for (id, name) in &ids {
        println!("\n== {id} {name}");

        let t = Instant::now();
        match rustypipe_download(&rp, &http, id, &out).await {
            Ok(n) => {
                ok[0] += 1;
                println!("  rustypipe: OK {} KB in {:?}", n / 1024, t.elapsed());
            }
            Err(e) => println!("  rustypipe: FAIL {e:#} ({:?})", t.elapsed()),
        }

        if std::env::var("SKIP_YTDLP").is_ok() { continue; }
        let t = Instant::now();
        match ytdlp_download(id, &out).await {
            Ok(n) => {
                ok[1] += 1;
                println!("  yt-dlp:    OK {} KB in {:?}", n / 1024, t.elapsed());
            }
            Err(e) => println!("  yt-dlp:    FAIL {e:#} ({:?})", t.elapsed()),
        }
    }
    println!("\nrustypipe {}/{}  yt-dlp {}/{}", ok[0], ids.len(), ok[1], ids.len());
    Ok(())
}

async fn rustypipe_download(
    rp: &RustyPipe,
    http: &reqwest::Client,
    id: &str,
    out: &Path,
) -> Result<u64> {
    let t = Instant::now();
    let clients: Vec<rustypipe::client::ClientType> = match std::env::var("CLIENTS").ok().as_deref() {
        Some("tv") => vec![rustypipe::client::ClientType::Tv],
        Some("ios") => vec![rustypipe::client::ClientType::Ios],
        Some("android") => vec![rustypipe::client::ClientType::Android],
        Some("music") => vec![rustypipe::client::ClientType::DesktopMusic],
        _ => rp.query().player_client_order().to_vec(),
    };
    let player = rp.query().player_from_clients(id, &clients).await?;
    let ua = rp.query().user_agent(player.client_type).into_owned();
    let filter = StreamFilter::new().audio_formats([AudioFormat::M4a]).no_video();
    let s = player
        .select_audio_stream(&filter)
        .context("no m4a audio stream")?;
    println!(
        "  rustypipe: player via {:?} in {:?}, itag {} {} B",
        player.client_type,
        t.elapsed(),
        s.itag,
        s.size
    );

    let mut buf = Vec::with_capacity(s.size as usize);
    let mut start = 0u64;
    while start < s.size {
        let end = (start + CHUNK).min(s.size) - 1;
        let resp = http
            .get(format!("{}&range={start}-{end}", s.url))
            .header(reqwest::header::USER_AGENT, &ua)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("HTTP {} at range {start}-{end}", resp.status());
        }
        buf.extend_from_slice(&resp.bytes().await?);
        start = end + 1;
    }
    if buf.len() as u64 != s.size {
        bail!("short download: {} of {}", buf.len(), s.size);
    }
    tokio::fs::write(out.join(format!("{id}.rp.m4a")), &buf).await?;
    Ok(buf.len() as u64)
}

async fn ytdlp_download(id: &str, out: &Path) -> Result<u64> {
    let path: PathBuf = out.join(format!("{id}.ytdlp.m4a"));
    let _ = std::fs::remove_file(&path);
    let status = tokio::process::Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/yt-dlp.exe"))
        .args(["--js-runtimes", "node", "-f", "140", "--no-playlist", "-q", "--no-warnings", "-o"])
        .arg(&path)
        .arg(format!("https://music.youtube.com/watch?v={id}"))
        .status()
        .await?;
    if !status.success() {
        bail!("yt-dlp exit {status}");
    }
    Ok(std::fs::metadata(&path)?.len())
}
