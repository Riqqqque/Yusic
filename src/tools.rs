//! External playback tools: yt-dlp and a JavaScript runtime for it (Node.js
//! or Deno). Uses what's installed, downloads what's missing into
//! `<data>\tools` (checksums verified), and keeps yt-dlp up to date.
//!
//! yt-dlp is the unpacked "folder" build: the single-file exe unpacks its
//! Python runtime on every start, which made each song take ~2 s longer to
//! begin. The folder build can't update itself, so Yusic does it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY: u32 = 0x0000_4000;
const YTDLP_ZIP: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_win.zip";
const YTDLP_LATEST: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const YTDLP_SUMS: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS";
const DENO_ZIP: &str = "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip";
const DENO_SUM: &str = "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip.sha256sum";

#[derive(Clone, Debug)]
pub struct Tools {
    pub ytdlp: PathBuf,
    /// yt-dlp `--js-runtimes` value.
    pub js_runtime: String,
}

pub fn tools_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tools")
}

fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|p| p.join(exe)).find(|p| p.is_file())
    })
}

/// Where Yusic keeps its own yt-dlp folder build.
pub fn ytdlp_dir(data_dir: &Path) -> PathBuf {
    tools_dir(data_dir).join("yt-dlp")
}

/// True for the unpacked build (an `_internal` folder next to the exe).
pub fn is_folder_build(ytdlp: &Path) -> bool {
    ytdlp.parent().is_some_and(|d| d.join("_internal").is_dir())
}

pub fn find_ytdlp(data_dir: &Path) -> Option<PathBuf> {
    // Folder builds first: they start much faster.
    let mut candidates = vec![ytdlp_dir(data_dir).join("yt-dlp.exe")];
    if let Some(root) = crate::install::project_root() {
        candidates.push(root.join("tools").join("yt-dlp").join("yt-dlp.exe"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("yt-dlp.exe"));
            candidates.push(dir.join("tools").join("yt-dlp.exe"));
        }
    }
    candidates.push(tools_dir(data_dir).join("yt-dlp.exe"));
    // Development checkout.
    if let Some(root) = crate::install::project_root() {
        candidates.push(root.join("tools").join("yt-dlp.exe"));
    }
    // Never a yt-dlp from PATH: those (pip installs) go stale and break, while
    // Yusic keeps its own copy current.
    candidates.into_iter().find(|p| p.is_file())
}

pub fn find_js_runtime(data_dir: &Path) -> Option<String> {
    if on_path("node.exe").is_some() {
        return Some("node".into());
    }
    if on_path("deno.exe").is_some() {
        return Some("deno".into());
    }
    let deno = tools_dir(data_dir).join("deno.exe");
    deno.is_file().then(|| format!("deno:{}", deno.display()))
}

/// What's installed right now (no network).
pub fn find(data_dir: &Path) -> Option<Tools> {
    Some(Tools { ytdlp: find_ytdlp(data_dir)?, js_runtime: find_js_runtime(data_dir)? })
}

/// Downloads whatever is missing. `progress` gets short status messages.
pub async fn install(data_dir: &Path, http: &reqwest::Client, progress: impl Fn(String)) -> Result<Tools> {
    let dir = tools_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let ytdlp = match find_ytdlp(data_dir) {
        Some(p) => p,
        None => {
            progress("Downloading yt-dlp…".into());
            let staged = download_ytdlp(data_dir, http).await?;
            let target = ytdlp_dir(data_dir);
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&staged, &target).context("could not install yt-dlp")?;
            target.join("yt-dlp.exe")
        }
    };
    let js_runtime = match find_js_runtime(data_dir) {
        Some(r) => r,
        None => {
            progress("Downloading the Deno JavaScript runtime (about 45 MB)…".into());
            let sum = http.get(DENO_SUM).send().await?.error_for_status()?.text().await?;
            let want = sum
                .split(|c: char| !c.is_ascii_hexdigit())
                .find(|w| w.len() == 64)
                .context("Deno checksum not found")?
                .to_lowercase();
            let zip = dir.join("deno.zip");
            download_verified(http, DENO_ZIP, &want, &zip).await?;
            progress("Unpacking Deno…".into());
            // Unpacked aside and moved in whole, so an interrupted unpack
            // never leaves a broken deno.exe that later runs would pick up.
            let staging = dir.join("deno-unpack");
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            let status = unzip(&zip, &staging).await?;
            let _ = std::fs::remove_file(&zip);
            let deno = dir.join("deno.exe");
            let unpacked = staging.join("deno.exe");
            let moved = status.success() && unpacked.is_file() && std::fs::rename(&unpacked, &deno).is_ok();
            let _ = std::fs::remove_dir_all(&staging);
            if !moved || !deno.is_file() {
                bail!("could not unpack Deno");
            }
            format!("deno:{}", deno.display())
        }
    };
    Ok(Tools { ytdlp, js_runtime })
}

async fn download_verified(http: &reqwest::Client, url: &str, sha256: &str, dest: &Path) -> Result<()> {
    let part = dest.with_extension("download");
    let mut resp = http
        .get(url)
        .timeout(Duration::from_secs(600))
        .send()
        .await?
        .error_for_status()?;
    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::create(&part).await?;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    let got = format!("{:x}", hasher.finalize());
    if got != sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("checksum mismatch for {url}");
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

async fn unzip(zip: &Path, dest: &Path) -> Result<std::process::ExitStatus> {
    let tar = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32")
        .join("tar.exe");
    Ok(tokio::process::Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dest)
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY)
        .status()
        .await?)
}

/// Downloads and verifies the latest yt-dlp folder build and unpacks it into
/// a staging folder next to its final place. Returns the staging folder.
async fn download_ytdlp(data_dir: &Path, http: &reqwest::Client) -> Result<PathBuf> {
    let dir = tools_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let sums = http.get(YTDLP_SUMS).send().await?.error_for_status()?.text().await?;
    let want = sums
        .lines()
        .find(|l| l.trim_end().ends_with(" yt-dlp_win.zip"))
        .and_then(|l| l.split_whitespace().next())
        .context("yt-dlp checksum not found")?
        .to_lowercase();
    let zip = dir.join("yt-dlp_win.zip");
    download_verified(http, YTDLP_ZIP, &want, &zip).await?;
    let staging = dir.join("yt-dlp.staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let status = unzip(&zip, &staging).await;
    let _ = std::fs::remove_file(&zip);
    if !status.is_ok_and(|s| s.success()) || !staging.join("yt-dlp.exe").is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("could not unpack yt-dlp");
    }
    Ok(staging)
}

async fn ytdlp_version(ytdlp: &Path) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(ytdlp)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Stages a newer yt-dlp folder build when `current` is outdated or isn't a
/// folder build yet. Returns the staging folder to swap in, or None.
pub async fn stage_ytdlp_update(current: &Path, data_dir: &Path, http: &reqwest::Client) -> Result<Option<PathBuf>> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    if is_folder_build(current) {
        let latest: Release = http
            .get(YTDLP_LATEST)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if ytdlp_version(current).await.as_deref() == Some(latest.tag_name.trim()) {
            return Ok(None);
        }
    }
    download_ytdlp(data_dir, http).await.map(Some)
}
