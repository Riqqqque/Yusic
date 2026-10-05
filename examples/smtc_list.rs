//! Lists Windows media sessions (what media keys and the volume flyout see).
//!
//! cargo run --release --example smtc_list

use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;

fn main() -> windows::core::Result<()> {
    let mgr = SessionManager::RequestAsync()?.join()?;
    for s in &mgr.GetSessions()? {
        let props = s.TryGetMediaPropertiesAsync()?.join()?;
        let info = s.GetPlaybackInfo()?;
        let ctl = info.Controls()?;
        println!(
            "{}: {:?} - {:?} [{:?}] next={} prev={} thumbnail={}",
            s.SourceAppUserModelId()?,
            props.Title()?,
            props.Artist()?,
            info.PlaybackStatus()?,
            ctl.IsNextEnabled()?,
            ctl.IsPreviousEnabled()?,
            props.Thumbnail().is_ok(),
        );
    }
    Ok(())
}
