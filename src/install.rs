//! Per-user install (no admin), uninstall, and self-update from GitHub
//! releases.
//!
//! A downloaded Yusic.exe offers to install itself into
//! `%LOCALAPPDATA%\Programs\Yusic`, adds Start menu and desktop shortcuts and
//! an entry in Settings > Apps. Installed copies check GitHub for new
//! releases, verify the download's SHA-256 and swap it in; the next start (or
//! "Restart" in the app) runs the new version.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONQUESTION, MB_YESNO, MessageBoxW};
use windows::core::{GUID, HSTRING, Interface, PCWSTR};

pub const REPO: &str = "Riqqqque/Yusic";
const APP_EXE: &str = "Yusic.exe";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Yusic";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Programs")
        .join("Yusic")
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

pub fn is_installed_copy() -> bool {
    current_exe().and_then(|e| e.parent().map(|d| same_dir(d, &install_dir()))).unwrap_or(false)
}

/// Builds run from the source tree (cargo output or the local `dist` copy)
/// never install or self-update; scripts/deploy.ps1 updates them.
pub fn is_dev_copy() -> bool {
    let Some(exe) = current_exe() else { return true };
    let lower = exe.to_string_lossy().to_lowercase();
    let project = Path::new(env!("CARGO_MANIFEST_DIR"));
    lower.contains(r"\target\") || exe.starts_with(project) || exe.with_file_name("portable").exists()
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as _));
        path
    }
}

fn shortcut_paths() -> Vec<PathBuf> {
    [known_folder(&FOLDERID_Programs), known_folder(&FOLDERID_Desktop)]
        .into_iter()
        .flatten()
        .map(|d| d.join("Yusic.lnk"))
        .collect()
}

fn create_shortcut(lnk: &Path, target: &Path) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
        link.SetDescription(&HSTRING::from("Yusic"))?;
        let file: IPersistFile = link.cast()?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true)?;
    }
    Ok(())
}

fn reg_set_str(key: HKEY, name: &str, value: &str) {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe {
        let _ = RegSetValueExW(key, &HSTRING::from(name), Some(0), REG_SZ, Some(bytes));
    }
}

fn reg_set_dword(key: HKEY, name: &str, value: u32) {
    unsafe {
        let _ = RegSetValueExW(key, &HSTRING::from(name), Some(0), REG_DWORD, Some(&value.to_le_bytes()));
    }
}

fn register_uninstall(exe: &Path) {
    let mut key = HKEY::default();
    unsafe {
        let created = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(UNINSTALL_KEY),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if created.is_err() {
            return;
        }
    }
    let exe_s = exe.display().to_string();
    reg_set_str(key, "DisplayName", "Yusic");
    reg_set_str(key, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    reg_set_str(key, "Publisher", "Rique");
    reg_set_str(key, "DisplayIcon", &exe_s);
    reg_set_str(key, "InstallLocation", &install_dir().display().to_string());
    reg_set_str(key, "UninstallString", &format!("\"{exe_s}\" --uninstall"));
    reg_set_str(key, "URLInfoAbout", &format!("https://github.com/{REPO}"));
    reg_set_dword(key, "NoModify", 1);
    reg_set_dword(key, "NoRepair", 1);
    if let Ok(m) = std::fs::metadata(exe) {
        reg_set_dword(key, "EstimatedSize", (m.len() / 1024) as u32);
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
}

fn ask(text: &str) -> bool {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Yusic"), MB_YESNO | MB_ICONQUESTION) == IDYES
    }
}

/// Puts `src` at `dest`, moving a (possibly running) old copy aside first.
fn replace_exe(src: &Path, dest: &Path, copy: bool) -> Result<()> {
    if dest.exists() {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        std::fs::rename(dest, dest.with_file_name(format!("Yusic.old-{stamp}.exe")))
            .context("could not move the old version aside")?;
    }
    if copy {
        std::fs::copy(src, dest)?;
    } else {
        std::fs::rename(src, dest)?;
    }
    Ok(())
}

/// First run of a downloaded copy: offer to install it. Returns true when the
/// installed copy was started and this process should exit.
pub fn maybe_install() -> Result<bool> {
    if is_installed_copy() || is_dev_copy() {
        return Ok(false);
    }
    let Some(exe) = current_exe() else { return Ok(false) };
    if !ask(
        "Install Yusic?\n\nIt installs for your Windows account (no admin needed), adds Start menu and desktop \
         shortcuts, and keeps itself up to date.\n\nChoose No to just run it from here.",
    ) {
        return Ok(false);
    }
    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(APP_EXE);
    replace_exe(&exe, &target, true)?;
    for lnk in shortcut_paths() {
        let _ = create_shortcut(&lnk, &target);
    }
    register_uninstall(&target);
    std::process::Command::new(&target).creation_flags(DETACHED_PROCESS).spawn()?;
    Ok(true)
}

/// `Yusic.exe --uninstall` (from Settings > Apps).
pub fn uninstall(data_dir: &Path) {
    if !ask("Uninstall Yusic?") {
        return;
    }
    let wipe_data = ask("Also delete your Yusic settings, sign-in and downloaded tools?");
    for lnk in shortcut_paths() {
        let _ = std::fs::remove_file(lnk);
    }
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
    }
    if wipe_data {
        let _ = std::fs::remove_dir_all(data_dir);
    }
    // The running exe can't delete itself; let cmd do it once we've exited.
    let dir = install_dir();
    let _ = std::process::Command::new("cmd")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display()))
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();
}

/// `--wait-for <pid>`: a restart waits for the old instance to exit first.
pub fn wait_for_process(pid: u32) {
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let r = WaitForSingleObject(h, 20_000);
            let _ = r == WAIT_OBJECT_0;
            let _ = CloseHandle(h);
        }
    }
}

/// Starts the exe on disk (the new version after an update) once this
/// process has exited.
pub fn spawn_restart() -> Result<()> {
    let exe = current_exe().context("no exe path")?;
    std::process::Command::new(exe)
        .arg("--wait-for")
        .arg(std::process::id().to_string())
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

/// Size and modification time of the exe file, to notice it being replaced.
pub fn exe_stamp() -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(current_exe()?).ok()?;
    Some((m.len(), m.modified().ok()?))
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Checks GitHub for a newer release and, if there is one, downloads and
/// verifies it and swaps it into place. Returns the new version.
pub async fn update_from_github(http: &reqwest::Client) -> Result<Option<String>> {
    if !is_installed_copy() {
        return Ok(None);
    }
    let rel: Release = http
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if rel.draft || rel.prerelease || !is_newer(&rel.tag_name, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(APP_EXE))
        .context("release has no Yusic.exe")?;
    // Never install something we can't verify.
    let want = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .context("release asset has no SHA-256 digest")?
        .to_lowercase();
    let dir = install_dir();
    let part = dir.join("Yusic.exe.download");
    let mut resp = http
        .get(&asset.browser_download_url)
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
    if format!("{:x}", hasher.finalize()) != want {
        let _ = std::fs::remove_file(&part);
        bail!("downloaded update failed verification");
    }
    replace_exe(&part, &dir.join(APP_EXE), false)?;
    register_uninstall(&dir.join(APP_EXE));
    Ok(Some(rel.tag_name.trim_start_matches('v').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v0.2.1", "0.2.0"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }

    #[test]
    fn creates_shortcuts() {
        let dir = std::env::temp_dir().join(format!("yusic-lnk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lnk = dir.join("Yusic.lnk");
        create_shortcut(&lnk, &std::env::current_exe().unwrap()).unwrap();
        assert!(std::fs::metadata(&lnk).unwrap().len() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_builds_never_install() {
        // Test binaries live under target\.
        assert!(is_dev_copy());
        assert!(!is_installed_copy());
    }
}
