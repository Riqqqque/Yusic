//! External playback tools: yt-dlp and a JavaScript runtime for it (Node.js
//! or Deno). Uses what's installed, downloads what's missing into
//! `<data>\tools` (checksums verified), and keeps yt-dlp up to date.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
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

pub fn find_ytdlp(data_dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("yt-dlp.exe"));
            candidates.push(dir.join("tools").join("yt-dlp.exe"));
        }
    }
    candidates.push(tools_dir(data_dir).join("yt-dlp.exe"));
    // Development checkout.
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools").join("yt-dlp.exe"));
    candidates.into_iter().find(|p| p.is_file()).or_else(|| on_path("yt-dlp.exe"))
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
            let sums = http.get(YTDLP_SUMS).send().await?.error_for_status()?.text().await?;
            let want = sums
                .lines()
                .find(|l| l.trim_end().ends_with(" yt-dlp.exe"))
                .and_then(|l| l.split_whitespace().next())
                .context("yt-dlp checksum not found")?
                .to_lowercase();
            let path = dir.join("yt-dlp.exe");
            download_verified(http, YTDLP_URL, &want, &path).await?;
            path
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
            let tar = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
                .join("System32")
                .join("tar.exe");
            let status = tokio::process::Command::new(tar)
                .arg("-xf")
                .arg(&zip)
                .arg("-C")
                .arg(&dir)
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .await?;
            let _ = std::fs::remove_file(&zip);
            let deno = dir.join("deno.exe");
            if !status.success() || !deno.is_file() {
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

/// Runs `yt-dlp -U` for a copy Yusic manages (standalone exe in a folder we
/// own; never a pip install on PATH). Returns true if it updated.
pub async fn update_ytdlp(ytdlp: &Path, data_dir: &Path) -> bool {
    let ours = ytdlp.starts_with(tools_dir(data_dir))
        || std::env::current_exe().ok().and_then(|e| e.parent().map(|d| ytdlp.starts_with(d))).unwrap_or(false)
        || ytdlp.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")));
    if !ours {
        return false;
    }
    let out = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new(ytdlp)
            .args(["-U", "--no-warnings"])
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).contains("Updated yt-dlp"),
        _ => false,
    }
}
