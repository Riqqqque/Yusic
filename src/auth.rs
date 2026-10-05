//! Account credentials at rest. Everything is encrypted with Windows DPAPI,
//! so only the current Windows user can read it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).ok()?;
        Some(take_blob(out))
    }
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).ok()?;
        Some(take_blob(out))
    }
}

unsafe fn take_blob(b: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = unsafe { std::slice::from_raw_parts(b.pbData, b.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(b.pbData as _)));
    }
    v
}

fn write_protected(path: &Path, data: &[u8]) {
    if let Some(enc) = protect(data) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, enc).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

fn read_protected(path: &Path) -> Option<Vec<u8>> {
    unprotect(&std::fs::read(path).ok()?)
}

/// rustypipe cache storage (client versions + login cookie), DPAPI-encrypted.
pub struct ProtectedStorage(pub PathBuf);

impl rustypipe::cache::CacheStorage for ProtectedStorage {
    fn write(&self, data: &str) {
        write_protected(&self.0, data.as_bytes());
    }

    fn read(&self) -> Option<String> {
        String::from_utf8(read_protected(&self.0)?).ok()
    }
}

/// The YouTube cookie header we use for our own InnerTube calls and yt-dlp.
pub struct CookieStore(pub PathBuf);

impl CookieStore {
    pub fn load(&self) -> Option<String> {
        String::from_utf8(read_protected(&self.0)?).ok().filter(|c| !c.is_empty())
    }

    pub fn save(&self, cookie: &str) {
        write_protected(&self.0, cookie.as_bytes());
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn cookie_value<'a>(cookie: &'a str, name: &str) -> Option<&'a str> {
    cookie.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

/// `Authorization` header value YouTube's web clients send with cookie auth.
pub fn sapisid_hash(cookie: &str, origin: &str) -> Option<String> {
    let sapisid = cookie_value(cookie, "SAPISID").or_else(|| cookie_value(cookie, "__Secure-3PAPISID"))?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let hash = sha1_smol::Sha1::from(format!("{ts} {sapisid} {origin}")).digest().to_string();
    Some(format!("SAPISIDHASH {ts}_{hash}"))
}

/// Netscape cookies.txt for yt-dlp (session cookies on .youtube.com).
pub fn netscape_cookies(cookie: &str) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for kv in cookie.split(';') {
        if let Some((k, v)) = kv.trim().split_once('=') {
            out.push_str(&format!(".youtube.com\tTRUE\t/\tTRUE\t0\t{k}\t{v}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip() {
        let enc = protect(b"secret").unwrap();
        assert_ne!(enc, b"secret");
        assert_eq!(unprotect(&enc).unwrap(), b"secret");
    }

    #[test]
    fn sapisid_hash_format() {
        let h = sapisid_hash("A=1; SAPISID=abc/def; B=2", "https://music.youtube.com").unwrap();
        let (prefix, rest) = h.split_once(' ').unwrap();
        assert_eq!(prefix, "SAPISIDHASH");
        let (_ts, hash) = rest.split_once('_').unwrap();
        assert_eq!(hash.len(), 40);
        assert!(sapisid_hash("A=1", "x").is_none());
    }

    #[test]
    fn netscape_format() {
        let n = netscape_cookies("SID=x; HSID=y");
        assert!(n.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tSID\tx\n"));
        assert!(n.contains("\tHSID\ty\n"));
    }
}
