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
const CHUNK: u64 = 10 << 20;
const FORMAT: &str = "140/bestaudio[ext=m4a]";
const YTDLP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Deserialize)]
struct Resolved {
    url: String,
    filesize: Option<u64>,
    #[serde(default)]
    http_headers: HashMap<String, String>,
}

pub struct Resolver {
    ytdlp: PathBuf,
    dir: PathBuf,
    data_dir: PathBuf,
    ytdlp_cache: PathBuf,
    http: reqwest::Client,
    procs: Semaphore,
    inflight: Mutex<HashMap<String, Arc<OnceCell<PathBuf>>>>,
    keep: Mutex<HashSet<String>>,
}

impl Resolver {
    pub fn new(ytdlp: PathBuf, data_dir: &Path, http: reqwest::Client) -> Result<Self> {
        let dir = data_dir.join("cache");
        let ytdlp_cache = data_dir.join("yt-dlp");
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&ytdlp_cache)?;
        let r = Self {
            ytdlp,
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

    /// Looks for yt-dlp next to the exe, in `tools/`, then on PATH.
    pub fn find_ytdlp() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("yt-dlp.exe"));
                candidates.push(dir.join("tools").join("yt-dlp.exe"));
            }
        }
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools").join("yt-dlp.exe"));
        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
            return Some(found);
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("yt-dlp.exe"))
                .find(|p| p.is_file())
        })
    }

    fn file_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.m4a"))
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
            if keep.contains(stem) || busy.contains(stem) {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Returns a playable local file, resolving and downloading if needed.
    /// Concurrent calls for the same id share one download; dropping the
    /// future cancels it (and kills yt-dlp) unless another caller still waits.
    pub async fn ensure(&self, id: &str, cookie: Option<String>) -> Result<PathBuf> {
        let cell = self
            .inflight
            .lock()
            .unwrap()
            .entry(id.to_owned())
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
        let done = Done { map: &self.inflight, id, cell };
        let res = done.cell.get_or_try_init(|| self.fetch(id, cookie)).await.cloned();
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

    async fn fetch(&self, id: &str, cookie: Option<String>) -> Result<PathBuf> {
        let path = self.file_for(id);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            let _ = disable_fragmented_edit_list(&path);
            return Ok(path);
        }
        let resolved = match self.resolve(id, None).await {
            Ok(r) => r,
            // Private, uploaded or age-restricted tracks need the account.
            Err(e) => match &cookie {
                Some(c) => self.resolve(id, Some(c)).await.map_err(|_| e)?,
                None => return Err(e),
            },
        };
        if let Err(e) = self.download(&resolved, &path).await {
            self.ytdlp_download(id, &path, cookie.as_deref())
                .await
                .with_context(|| format!("direct download failed ({e:#})"))?;
        }
        disable_fragmented_edit_list(&path)?;
        Ok(path)
    }

    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.ytdlp);
        c.args([
            "--ignore-config",
            "--js-runtimes",
            "node",
            "--no-warnings",
            "--no-playlist",
            "--no-progress",
            "-f",
            FORMAT,
            "--cache-dir",
        ])
        .arg(&self.ytdlp_cache)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
        c
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

    async fn resolve(&self, id: &str, cookie: Option<&str>) -> Result<Resolved> {
        let _permit = self.procs.acquire().await?;
        let mut cmd = self.command();
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
            cmd.args(["--print", "%(.{url,filesize,http_headers})j"]).arg(watch_url(id)).output(),
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

    async fn ytdlp_download(&self, id: &str, path: &Path, cookie: Option<&str>) -> Result<()> {
        let _permit = self.procs.acquire().await?;
        let mut cmd = self.command();
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
            cmd.args(["--force-overwrites", "-o"]).arg(path).arg(watch_url(id)).output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("yt-dlp timed out"))?
        .context("could not start yt-dlp")?;
        if !out.status.success() || !path.is_file() {
            bail!(ytdlp_error(&out.stderr));
        }
        Ok(())
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
