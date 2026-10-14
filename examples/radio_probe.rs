//! Read-only check of radio/autoplay sources: track radio vs playlist mix,
//! signed out and (if a Yusic sign-in exists) signed in.
//! Usage: cargo run --example radio_probe -- <playlistId> <videoId>
#[path = "../src/auth.rs"]
#[allow(dead_code)]
mod auth;

use rustypipe::client::RustyPipe;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let playlist = args.next().unwrap_or_else(|| "RDCLAK5uy_lJ8xZWiZj2GCw7MArjakb6b0zfvqwldps".into());
    let video = args.next().unwrap_or_else(|| "fa5IWHDbftI".into());
    let rp = RustyPipe::builder().storage_dir(std::env::temp_dir()).no_botguard().no_reporter().build()?;
    let dir = std::path::PathBuf::from(std::env::var("LOCALAPPDATA")?).join("Yusic");
    let cookie = auth::CookieStore(dir.join("auth.bin")).load();
    if let Some(c) = &cookie {
        rp.user_auth_set_cookie(c.clone()).await?;
    }
    for auth in [false, true] {
        if auth && cookie.is_none() {
            println!("(no sign-in, skipping authenticated)");
            continue;
        }
        let q = if auth { rp.query().authenticated() } else { rp.query() };
        let tag = if auth { "signed in" } else { "signed out" };
        match q.music_radio(&format!("RDAMPL{playlist}")).await {
            Ok(r) => println!("{tag} playlist mix: {} tracks, more={} first={:?}", r.items.len(), r.ctoken.is_some(),
                r.items.iter().take(3).map(|t| t.name.as_str()).collect::<Vec<_>>()),
            Err(e) => println!("{tag} playlist mix: error {e}"),
        }
        match q.music_radio_track(&video).await {
            Ok(r) => println!("{tag} track radio: {} tracks, more={}", r.items.len(), r.ctoken.is_some()),
            Err(e) => println!("{tag} track radio: error {e}"),
        }
    }
    Ok(())
}
