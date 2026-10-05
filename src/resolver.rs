//! Turns a video id into a local audio file.
//!
//! yt-dlp resolves the stream URL (it is the only extractor that keeps up with
//! YouTube's signature/PO-token changes), then we download it in ranged chunks.
//! If our download is rejected, yt-dlp downloads the file itself.
//!
//! Files only live while they are needed: the app tells the resolver which
//! tracks to keep (the current one and the next one) and everything else in
//! the cache folder is deleted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OnceCell, Semaphore};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY: u32 = 0x0000_4000;
const CHUNK: u64 = 10 << 20;
/// Best audio first: 256 kbps Opus/AAC (YouTube Music Premium, needs the
/// signed-in session), then ~160 kbps Opus, then 128 kbps AAC.
const FORMAT_BEST: &str = "774/141/251/140/bestaudio[ext=webm]/bestaudio[ext=m4a]";
/// 128 kbps AAC: decodes on every Windows install.
const FORMAT_COMPAT: &str = "140/bestaudio[ext=m4a]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Best,
    /// Retry format for a file Windows could not decode.
    Compatible,
}

impl Quality {
    fn format(self) -> &'static str {
        match self {
            Quality::Best => FORMAT_BEST,
            Quality::Compatible => FORMAT_COMPAT,
        }
    }

    /// Cache file stem: `<id>` or `<id>~c`.
    fn stem(self, id: &str) -> String {
        match self {
            Quality::Best => id.to_owned(),
            Quality::Compatible => format!("{id}~c"),
        }
    }
}
const YTDLP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Deserialize)]
struct Resolved {
    url: String,
    filesize: Option<u64>,
    #[serde(default)]
    ext: String,
    #[serde(default)]
    http_headers: HashMap<String, String>,
}

pub struct Resolver {
    /// yt-dlp and its JS runtime; None until setup has found or installed them.
    tools: std::sync::RwLock<Option<crate::tools::Tools>>,
    dir: PathBuf,
    data_dir: PathBuf,
    ytdlp_cache: PathBuf,
    http: reqwest::Client,
    procs: Semaphore,
    inflight: Mutex<HashMap<String, Arc<OnceCell<PathBuf>>>>,
    keep: Mutex<HashSet<String>>,
}

impl Resolver {
    pub fn new(tools: Option<crate::tools::Tools>, data_dir: &Path, http: reqwest::Client) -> Result<Self> {
        let dir = data_dir.join("cache");
        let ytdlp_cache = data_dir.join("yt-dlp");
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&ytdlp_cache)?;
        let r = Self {
            tools: std::sync::RwLock::new(tools),
            dir,
            data_dir: data_dir.to_owned(),
            ytdlp_cache,
            http,
            procs: Semaphore::new(2),
            inflight: Mutex::new(HashMap::new()),
            keep: Mutex::new(HashSet::new()),
        };
        // Nothing from a previous session is needed, including cookie files
        // left behind if the app was killed mid-request.
        r.sweep();
        if let Ok(rd) = std::fs::read_dir(data_dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("cookies-") && name.ends_with(".txt") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(r)
    }

    pub fn set_tools(&self, tools: crate::tools::Tools) {
        *self.tools.write().unwrap() = Some(tools);
    }

    pub fn has_tools(&self) -> bool {
        self.tools.read().unwrap().is_some()
    }

    /// Updates yt-dlp while no download is using it.
    pub async fn update_ytdlp(&self) -> bool {
        let Some(ytdlp) = self.tools.read().unwrap().as_ref().map(|t| t.ytdlp.clone()) else { return false };
        let Ok(_all) = self.procs.acquire_many(2).await else { return false };
        crate::tools::update_ytdlp(&ytdlp, &self.data_dir).await
    }

    /// An already downloaded file for this stem, whatever its format.
    fn existing(&self, stem: &str) -> Option<PathBuf> {
        ["webm", "m4a", "opus", "mp4"]
            .iter()
            .map(|ext| self.dir.join(format!("{stem}.{ext}")))
            .find(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
    }

    /// Sets the tracks whose files must stay on disk and deletes the rest.
    pub fn retain(&self, ids: &[&str]) {
        *self.keep.lock().unwrap() = ids.iter().map(|s| s.to_string()).collect();
        self.sweep();
    }

    /// Deletes cached audio that is neither kept nor being downloaded. Files the
    /// player still has open fail to delete and are picked up by a later sweep.
    pub fn sweep(&self) {
        let keep = self.keep.lock().unwrap().clone();
        let busy: HashSet<String> = self.inflight.lock().unwrap().keys().cloned().collect();
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let path = e.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            // "<id>~c" is the compatible-format copy of <id>.
            let id = stem.split('~').next().unwrap_or(stem);
            if keep.contains(id) || busy.contains(stem) {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Returns a playable local file, resolving and downloading if needed.
    /// Concurrent calls for the same id share one download; dropping the
    /// future cancels it (and kills yt-dlp) unless another caller still waits.
    pub async fn ensure(&self, id: &str, cookie: Option<String>, quality: Quality) -> Result<PathBuf> {
        let stem = quality.stem(id);
        let cell = self
            .inflight
            .lock()
            .unwrap()
            .entry(stem.clone())
            .or_default()
            .clone();
        // Removes the in-flight entry when the last waiter finishes or is cancelled.
        struct Done<'a> {
            map: &'a Mutex<HashMap<String, Arc<OnceCell<PathBuf>>>>,
            id: &'a str,
            cell: Arc<OnceCell<PathBuf>>,
        }
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                let mut map = self.map.lock().unwrap();
                let last = map.get(self.id).is_some_and(|c| Arc::ptr_eq(c, &self.cell))
                    && Arc::strong_count(&self.cell) == 2;
                if last {
                    map.remove(self.id);
                }
            }
        }
        let done = Done { map: &self.inflight, id: &stem, cell };
        let res = done.cell.get_or_try_init(|| self.fetch(id, &stem, cookie, quality)).await.cloned();
        drop(done);
        // Finished after the user moved on: don't leave it lying around.
        // The lock is held so a concurrent retain() can't re-add the id in between.
        {
            let keep = self.keep.lock().unwrap();
            if !keep.contains(id) {
                if let Ok(p) = &res {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        res
    }

    async fn fetch(&self, id: &str, stem: &str, cookie: Option<String>, quality: Quality) -> Result<PathBuf> {
        if let Some(path) = self.existing(stem) {
            let _ = disable_fragmented_edit_list(&path);
            return Ok(path);
        }
        // Signed in: ask with the account first, which also unlocks Premium's
        // 256 kbps formats and private/uploaded tracks; fall back to anonymous.
        let resolved = match &cookie {
            Some(c) => match self.resolve(id, Some(c), quality).await {
                Ok(r) => r,
                Err(_) => self.resolve(id, None, quality).await?,
            },
            None => self.resolve(id, None, quality).await?,
        };
        let ext = match resolved.ext.as_str() {
            "webm" | "m4a" | "opus" | "mp4" => resolved.ext.clone(),
            _ => "m4a".to_owned(),
        };
        let path = self.dir.join(format!("{stem}.{ext}"));
        let path = match self.download(&resolved, &path).await {
            Ok(()) => path,
            Err(e) => self
                .ytdlp_download(id, stem, cookie.as_deref(), quality)
                .await
                .with_context(|| format!("direct download failed ({e:#})"))?,
        };
        disable_fragmented_edit_list(&path)?;
        Ok(path)
    }

    fn command(&self, quality: Quality) -> Result<tokio::process::Command> {
        let tools = self
            .tools
            .read()
            .unwrap()
            .clone()
            .context("Yusic is still downloading its playback components. Try again in a moment.")?;
        let mut c = tokio::process::Command::new(&tools.ytdlp);
        c.args(["--ignore-config", "--js-runtimes"])
            .arg(&tools.js_runtime)
            .args(["--no-warnings", "--no-playlist", "--no-progress", "-f", quality.format(), "--cache-dir"])
            .arg(&self.ytdlp_cache)
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
        Ok(c)
    }

    /// Writes a short-lived cookies.txt for yt-dlp; deleted when dropped.
    fn cookie_file(&self, cookie: &str) -> Result<TempFile> {
        let n: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let path = self.data_dir.join(format!("cookies-{n:x}.txt"));
        std::fs::write(&path, crate::auth::netscape_cookies(cookie))?;
        Ok(TempFile(path))
    }

    async fn resolve(&self, id: &str, cookie: Option<&str>, quality: Quality) -> Result<Resolved> {
        let _permit = self.procs.acquire().await?;
        let mut cmd = self.command(quality)?;
        let _cookies = match cookie {
            Some(c) => {
                let f = self.cookie_file(c)?;
                cmd.arg("--cookies").arg(&f.0);
                Some(f)
            }
            None => None,
        };
        let out = tokio::time::timeout(
            YTDLP_TIMEOUT,
            cmd.args(["--print", "%(.{url,filesize,http_headers,ext})j"]).arg(watch_url(id)).output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("yt-dlp timed out"))?
        .context("could not start yt-dlp")?;
        if !out.status.success() {
            bail!(ytdlp_error(&out.stderr));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().find(|l| l.starts_with('{')).context("yt-dlp returned no stream")?;
        Ok(serde_json::from_str(line)?)
    }

    async fn download(&self, r: &Resolved, path: &Path) -> Result<()> {
        let part = path.with_extension("part");
        let mut file = tokio::fs::File::create(&part).await?;
        let headers: reqwest::header::HeaderMap = (&r.http_headers).try_into().unwrap_or_default();
        let result: Result<()> = async {
            let mut written = 0u64;
            match r.filesize {
                Some(size) => {
                    while written < size {
                        let end = (written + CHUNK).min(size) - 1;
                        let url = format!("{}&range={written}-{end}", r.url);
                        let n = self.fetch_into(&url, &headers, &mut file).await?;
                        if n == 0 {
                            bail!("server sent no data");
                        }
                        written += n;
                    }
                    if written != size {
                        bail!("short download ({written} of {size} bytes)");
                    }
                }
                None => written = self.fetch_into(&r.url, &headers, &mut file).await?,
            }
            if written == 0 {
                bail!("empty download");
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        drop(file);
        match result {
            Ok(()) => Ok(tokio::fs::rename(&part, path).await?),
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                Err(e)
            }
        }
    }

    async fn fetch_into(
        &self,
        url: &str,
        headers: &reqwest::header::HeaderMap,
        file: &mut tokio::fs::File,
    ) -> Result<u64> {
        let mut resp = self.http.get(url).headers(headers.clone()).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let mut n = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
            n += chunk.len() as u64;
        }
        Ok(n)
    }

    /// Lets yt-dlp download the file itself; returns where it put it.
    async fn ytdlp_download(&self, id: &str, stem: &str, cookie: Option<&str>, quality: Quality) -> Result<PathBuf> {
        let _permit = self.procs.acquire().await?;
        let mut cmd = self.command(quality)?;
        let _cookies = match cookie {
            Some(c) => {
                let f = self.cookie_file(c)?;
                cmd.arg("--cookies").arg(&f.0);
                Some(f)
            }
            None => None,
        };
        let out = tokio::time::timeout(
            YTDLP_TIMEOUT * 4,
            cmd.args(["--force-overwrites", "-o"])
                .arg(self.dir.join(format!("{stem}.%(ext)s")))
                .arg(watch_url(id))
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("yt-dlp timed out"))?
        .context("could not start yt-dlp")?;
        match self.existing(stem) {
            Some(p) if out.status.success() => Ok(p),
            _ => bail!(ytdlp_error(&out.stderr)),
        }
    }
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn watch_url(id: &str) -> String {
    format!("https://music.youtube.com/watch?v={id}")
}

fn ytdlp_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    if text.contains("JavaScript runtime") || text.contains("js-runtimes") {
        return "yt-dlp needs Node.js on PATH to play YouTube audio".into();
    }
    text.lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .map(|l| {
            l.trim_start_matches("ERROR:")
                .trim()
                .trim_start_matches("[youtube]")
                .trim()
                .to_owned()
        })
        .unwrap_or_else(|| "yt-dlp failed".into())
}

/// YouTube's DASH audio is fragmented MP4. When it carries an edit list (the
/// AAC priming offset), Media Foundation jumps straight to the end of the
/// track. Renaming the `edts` box to `free` keeps every offset intact and only
/// drops the ~36 ms priming trim. Non-fragmented files are left alone.
fn disable_fragmented_edit_list(path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::File::options().read(true).write(true).open(path)?;
    let mut head = vec![0u8; 64 * 1024];
    let n = f.read(&mut head)?;
    head.truncate(n);
    if let Some(at) = find_fragmented_edts(&head) {
        f.seek(SeekFrom::Start(at as u64 + 4))?;
        f.write_all(b"free")?;
    }
    Ok(())
}

/// Offset of `moov/trak/edts` if the file is fragmented (has `mvex` or a `dash` brand).
fn find_fragmented_edts(d: &[u8]) -> Option<usize> {
    fn boxes(d: &[u8], mut i: usize, end: usize) -> impl Iterator<Item = (usize, usize, [u8; 4])> + '_ {
        std::iter::from_fn(move || {
            if i + 8 > end.min(d.len()) {
                return None;
            }
            let size = u32::from_be_bytes(d[i..i + 4].try_into().ok()?) as usize;
            let kind: [u8; 4] = d[i + 4..i + 8].try_into().ok()?;
            if size < 8 {
                return None;
            }
            let at = i;
            i += size;
            Some((at, size, kind))
        })
    }
    let mut fragmented = false;
    let mut edts = None;
    for (at, size, kind) in boxes(d, 0, d.len()) {
        match &kind {
            b"ftyp" => fragmented |= d.get(at + 8..at + 12) == Some(b"dash"),
            b"moov" => {
                for (t, tsize, tkind) in boxes(d, at + 8, at + size) {
                    match &tkind {
                        b"mvex" => fragmented = true,
                        b"trak" => {
                            edts = edts.or_else(|| {
                                boxes(d, t + 8, t + tsize).find(|b| &b.2 == b"edts").map(|b| b.0)
                            });
                        }
                        _ => {}
                    }
                }
                break;
            }
            _ => {}
        }
    }
    edts.filter(|_| fragmented)
}

#[cfg(test)]
mod tests {
    use super::find_fragmented_edts;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn finds_edts_only_in_fragmented_files() {
        let trak = bx(b"trak", &[bx(b"tkhd", &[0; 4]), bx(b"edts", &bx(b"elst", &[0; 8]))].concat());
        let moov = bx(b"moov", &[bx(b"mvhd", &[0; 4]), trak.clone(), bx(b"mvex", &[])].concat());
        let file = [bx(b"ftyp", b"dash\0\0\0\0"), moov].concat();
        let at = find_fragmented_edts(&file).expect("edts");
        assert_eq!(&file[at + 4..at + 8], b"edts");

        let plain_moov = bx(b"moov", &[bx(b"mvhd", &[0; 4]), trak].concat());
        let plain = [bx(b"ftyp", b"isom\0\0\0\0"), plain_moov].concat();
        assert_eq!(find_fragmented_edts(&plain), None);
    }
}
