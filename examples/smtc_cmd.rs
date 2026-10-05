//! Sends a media command to the session whose app id contains a filter
//! (default "yusic-test"), like pressing a media key.
//!
//! cargo run --release --example smtc_cmd -- next [filter]

use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;

fn main() -> windows::core::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "next".into());
    let filter = std::env::args().nth(2).unwrap_or_else(|| "yusic-test".into());
    let mgr = SessionManager::RequestAsync()?.join()?;
    for s in &mgr.GetSessions()? {
        let id = s.SourceAppUserModelId()?.to_string();
        if !id.to_lowercase().contains(&filter) {
            continue;
        }
        let ok = match cmd.as_str() {
            "next" => s.TrySkipNextAsync()?.join()?,
            "prev" => s.TrySkipPreviousAsync()?.join()?,
            "pause" => s.TryPauseAsync()?.join()?,
            "play" => s.TryPlayAsync()?.join()?,
            _ => false,
        };
        println!("{id}: {cmd} -> {ok}");
    }
    Ok(())
}
