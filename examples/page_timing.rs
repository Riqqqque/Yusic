//! Times the YouTube Music requests behind each page (cold, then warm).
use std::time::Instant;
use rustypipe::client::RustyPipe;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rp = RustyPipe::builder().storage_dir(std::env::temp_dir()).no_botguard().build()?;
    for round in ["cold", "warm"] {
        let q = rp.query();
        let t = Instant::now(); q.music_search_main("daft punk").await?; let a = t.elapsed();
        let t = Instant::now(); q.music_album("MPREb_7ltM34kr0mH").await?; let b = t.elapsed();
        let t = Instant::now(); q.music_playlist("RDCLAK5uy_lJ8xZWiZj2GCw7MArjakb6b0zfvqwldps").await?; let c = t.elapsed();
        let t = Instant::now(); q.music_artist("UCRr1xG_2WIDs18a6cIiCxeA", false).await?; let d = t.elapsed();
        let t = Instant::now(); q.music_radio_track("fa5IWHDbftI").await?; let e = t.elapsed();
        println!("{round}: search {a:?} | album {b:?} | playlist {c:?} | artist {d:?} | radio {e:?}");
    }
    // Image fetches: HTTP/1.1 vs connection reuse
    let http = reqwest::Client::new();
    let urls: Vec<String> = (0..24).map(|i| format!("https://lh3.googleusercontent.com/qLhu6Py_4_xoBsoubKQsXlhOQGqU9YU1ZRAbFusF0LlrPkXbbpu7bEh-k_ZtE4JwLgubvucAQqcK1hRk=w{}-h{}-p-l90-rj", 200 + i, 200 + i)).collect();
    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for u in urls { let h = http.clone(); set.spawn(async move { h.get(u).send().await.ok().map(|r| r.version()) }); }
    let mut ver = None; while let Some(r) = set.join_next().await { ver = r.ok().flatten().or(ver); }
    println!("24 images in parallel: {:?} ({ver:?})", t.elapsed());
    Ok(())
}
