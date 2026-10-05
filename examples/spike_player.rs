//! Spike 2: Windows.Media.Playback.MediaPlayer from an unpackaged exe.
//! Plays a local m4a, publishes metadata to SMTC, then drives it through the
//! global session manager (the same path media keys and the volume flyout use).
//!
//! cargo run --release --example spike_player -- <file.m4a>

use std::time::Duration;

use anyhow::{Context, Result};
use windows::Foundation::{TypedEventHandler, Uri};
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;
use windows::Media::Core::MediaSource;
use windows::Media::MediaPlaybackType;
use windows::Media::Playback::{
    MediaCommandEnablingRule, MediaPlaybackCommandManager,
    MediaPlaybackCommandManagerNextReceivedEventArgs, MediaPlaybackItem, MediaPlayer,
};
use windows::Storage::StorageFile;
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::core::HSTRING;

fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: spike_player <file.m4a>")?;
    let source = if path.starts_with("http") {
        MediaSource::CreateFromUri(&Uri::CreateUri(&HSTRING::from(&path))?)?
    } else {
        let path = std::fs::canonicalize(&path)?;
        let path = path.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(&path))?.join()?;
        MediaSource::CreateFromStorageFile(&file)?
    };
    let t0 = std::time::Instant::now();
    let item = MediaPlaybackItem::Create(&source)?;
    let props = item.GetDisplayProperties()?;
    props.SetType(MediaPlaybackType::Music)?;
    let music = props.MusicProperties()?;
    music.SetTitle(&HSTRING::from("Yusic spike track"))?;
    music.SetArtist(&HSTRING::from("Spike Artist"))?;
    props.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&Uri::CreateUri(
        &HSTRING::from("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"),
    )?)?)?;
    item.ApplyDisplayProperties(&props)?;

    let player = MediaPlayer::new()?;
    // Silent unless --audible: decoding is what we test.
    if !std::env::args().any(|a| a == "--audible") {
        player.SetIsMuted(true)?;
    }
    let cm = player.CommandManager()?;
    cm.NextBehavior()?.SetEnablingRule(MediaCommandEnablingRule::Always)?;
    cm.PreviousBehavior()?.SetEnablingRule(MediaCommandEnablingRule::Always)?;
    cm.NextReceived(&TypedEventHandler::<
        MediaPlaybackCommandManager,
        MediaPlaybackCommandManagerNextReceivedEventArgs,
    >::new(|_, args| {
        println!("  >> NextReceived");
        if let Some(a) = args.as_ref() {
            a.SetHandled(true)?;
        }
        Ok(())
    }))?;
    player.SetSource(&item)?;
    player.Play()?;

    while player.PlaybackSession()?.Position()?.Duration == 0 {
        std::thread::sleep(Duration::from_millis(20));
        if t0.elapsed() > Duration::from_secs(15) { anyhow::bail!("no audio after 15s"); }
    }
    println!("time to audio: {:?}", t0.elapsed());
    std::thread::sleep(Duration::from_secs(3));
    let state = |p: &MediaPlayer| -> Result<String> {
        let s = p.PlaybackSession()?;
        Ok(format!(
            "{:?} @ {:.1}s",
            s.PlaybackState()?,
            s.Position()?.Duration as f64 / 1e7
        ))
    };
    println!("player: {}", state(&player)?);

    let mgr = SessionManager::RequestAsync()?.join()?;
    let sessions = mgr.GetSessions()?;
    let mut ours = None;
    for s in &sessions {
        let id = s.SourceAppUserModelId()?;
        let mp = s.TryGetMediaPropertiesAsync()?.join()?;
        let status = s.GetPlaybackInfo()?.PlaybackStatus()?;
        println!("session {id}: {:?} - {:?} [{status:?}]", mp.Title()?, mp.Artist()?);
        if mp.Title()? == "Yusic spike track" {
            ours = Some(s);
        }
    }
    let s = ours.context("our session is NOT visible to SMTC")?;
    let ctl = s.GetPlaybackInfo()?.Controls()?;
    println!(
        "controls: play={} pause={} next={} prev={}",
        ctl.IsPlayEnabled()?,
        ctl.IsPauseEnabled()?,
        ctl.IsNextEnabled()?,
        ctl.IsPreviousEnabled()?
    );

    println!("-> TryPauseAsync (same as media key)");
    s.TryPauseAsync()?.join()?;
    std::thread::sleep(Duration::from_millis(800));
    println!("player: {}", state(&player)?);

    println!("-> TryPlayAsync");
    s.TryPlayAsync()?.join()?;
    std::thread::sleep(Duration::from_millis(800));
    println!("player: {}", state(&player)?);

    println!("-> TrySkipNextAsync");
    s.TrySkipNextAsync()?.join()?;
    std::thread::sleep(Duration::from_millis(800));

    let sess = player.PlaybackSession()?;
    let dur = sess.NaturalDuration()?.Duration;
    let t1 = std::time::Instant::now();
    sess.SetPosition(windows::Foundation::TimeSpan { Duration: dur * 6 / 10 })?;
    std::thread::sleep(Duration::from_millis(1500));
    println!("after seek to 60%: {} ({:?})", state(&player)?, t1.elapsed());
    let mut c = windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS::default();
    unsafe { let _ = windows::Win32::System::ProcessStatus::K32GetProcessMemoryInfo(windows::Win32::System::Threading::GetCurrentProcess(), &mut c, size_of_val(&c) as u32); }
    println!("memory: {:.1} MB ws / {:.1} MB private", c.WorkingSetSize as f64 / 1048576.0, c.PagefileUsage as f64 / 1048576.0);

    if std::env::args().any(|a| a == "--hold") {
        println!("holding 30s: press media keys / open the volume flyout");
        std::thread::sleep(Duration::from_secs(30));
    }
    Ok(())
}
