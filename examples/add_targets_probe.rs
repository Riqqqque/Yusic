//! Read-only check of `playlist/get_add_to_playlist` (the "Save to playlist"
//! list) with the signed-in cookie from the Yusic data folder.
//! Usage: cargo run --example add_targets_probe -- <videoId>
#[path = "../src/auth.rs"]
#[allow(dead_code)]
mod auth;

use serde_json::{Value, json};

const ORIGIN: &str = "https://music.youtube.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let video = std::env::args().nth(1).unwrap_or_else(|| "fa5IWHDbftI".into());
    let dir = std::env::var_os("YUSIC_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap()).join("Yusic"));
    let cookie = auth::CookieStore(dir.join("auth.bin")).load().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
    let hash = auth::sapisid_hash(&cookie, ORIGIN).ok_or_else(|| anyhow::anyhow!("no SAPISID"))?;
    let body = json!({
        "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": "1.20260928.13.00", "hl": "en", "gl": "US" } },
        "videoIds": [video],
    });
    let v: Value = reqwest::Client::new()
        .post("https://music.youtube.com/youtubei/v1/playlist/get_add_to_playlist?prettyPrint=false")
        .header("Origin", ORIGIN)
        .header("X-Origin", ORIGIN)
        .header("X-Goog-AuthUser", "0")
        .header("Cookie", cookie)
        .header("Authorization", hash)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let list = find(&v, "playlists").and_then(Value::as_array).cloned().unwrap_or_default();
    println!("top-level keys: {:?}", v.as_object().map(|o| o.keys().collect::<Vec<_>>()));
    println!("{} playlists", list.len());
    if let Some(r) = list.get(1).map(|p| &p["playlistAddToOptionRenderer"]) {
        let mut r = r.clone();
        r.as_object_mut().map(|o| o.remove("thumbnailRenderer"));
        println!("{}", serde_json::to_string_pretty(&r)?);
    }
    for p in list.iter().take(4) {
        let r = &p["playlistAddToOptionRenderer"];
        println!(
            "keys {:?}\n  id {:?} title {:?} contains {:?}",
            r.as_object().map(|o| o.keys().collect::<Vec<_>>()),
            r["playlistId"].as_str(),
            r["title"],
            r["containsSelectedVideos"].as_str()
        );
    }
    Ok(())
}

fn find<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find(x, key)),
        _ => None,
    }
}
