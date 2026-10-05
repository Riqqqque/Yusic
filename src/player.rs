//! Audio playback through Windows.Media.Playback.MediaPlayer. Its built-in
//! SystemMediaTransportControls integration gives us hardware media keys and
//! the Windows media flyout without extra code.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::Core::MediaSource;
use windows::Media::MediaPlaybackType;
use windows::Media::Playback::{
    MediaCommandEnablingRule, MediaPlaybackCommandManager,
    MediaPlaybackCommandManagerNextReceivedEventArgs,
    MediaPlaybackCommandManagerPreviousReceivedEventArgs, MediaPlaybackItem, MediaPlaybackSession, MediaPlaybackState,
    MediaPlayer, MediaPlayerAudioCategory, MediaPlayerFailedEventArgs,
};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::core::{HSTRING, IInspectable};

#[derive(Debug)]
pub enum PlayerEvent {
    Playing(bool),
    Ended,
    Failed(String),
    Next,
    Previous,
}

pub struct Meta<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub thumb: &'a str,
}

pub struct Player {
    mp: MediaPlayer,
}

impl Player {
    pub fn new(on_event: impl Fn(PlayerEvent) + Send + Sync + 'static) -> Result<Self> {
        let ev = Arc::new(on_event);
        let mp = MediaPlayer::new()?;
        mp.SetAudioCategory(MediaPlayerAudioCategory::Media)?;
        mp.SetAutoPlay(false)?;

        let cm = mp.CommandManager()?;
        cm.NextBehavior()?.SetEnablingRule(MediaCommandEnablingRule::Always)?;
        cm.PreviousBehavior()?.SetEnablingRule(MediaCommandEnablingRule::Always)?;
        let e = ev.clone();
        cm.NextReceived(&TypedEventHandler::<MediaPlaybackCommandManager, MediaPlaybackCommandManagerNextReceivedEventArgs>::new(move |_, args| {
            if let Some(a) = args.as_ref() {
                a.SetHandled(true)?;
            }
            e(PlayerEvent::Next);
            Ok(())
        }))?;
        let e = ev.clone();
        cm.PreviousReceived(&TypedEventHandler::<MediaPlaybackCommandManager, MediaPlaybackCommandManagerPreviousReceivedEventArgs>::new(move |_, args| {
            if let Some(a) = args.as_ref() {
                a.SetHandled(true)?;
            }
            e(PlayerEvent::Previous);
            Ok(())
        }))?;

        let e = ev.clone();
        mp.MediaEnded(&TypedEventHandler::<MediaPlayer, IInspectable>::new(move |_, _| {
            e(PlayerEvent::Ended);
            Ok(())
        }))?;
        let e = ev.clone();
        mp.MediaFailed(&TypedEventHandler::<MediaPlayer, MediaPlayerFailedEventArgs>::new(move |_, args| {
            let msg = args
                .as_ref()
                .and_then(|a| a.ErrorMessage().ok())
                .map(|m| m.to_string())
                .unwrap_or_else(|| "playback failed".into());
            e(PlayerEvent::Failed(msg));
            Ok(())
        }))?;
        let e = ev;
        mp.PlaybackSession()?.PlaybackStateChanged(&TypedEventHandler::<MediaPlaybackSession, IInspectable>::new(move |s, _| {
            if let Some(s) = s.as_ref() {
                e(PlayerEvent::Playing(s.PlaybackState()? == MediaPlaybackState::Playing));
            }
            Ok(())
        }))?;
        Ok(Self { mp })
    }

    pub fn load(&self, path: &Path, meta: &Meta) -> Result<()> {
        let uri = Uri::CreateUri(&HSTRING::from(file_uri(path)))?;
        let item = MediaPlaybackItem::Create(&MediaSource::CreateFromUri(&uri)?)?;
        let props = item.GetDisplayProperties()?;
        props.SetType(MediaPlaybackType::Music)?;
        let music = props.MusicProperties()?;
        music.SetTitle(&HSTRING::from(meta.title))?;
        music.SetArtist(&HSTRING::from(meta.artist))?;
        music.SetAlbumTitle(&HSTRING::from(meta.album))?;
        if meta.thumb.starts_with("https://") {
            if let Ok(u) = Uri::CreateUri(&HSTRING::from(meta.thumb)) {
                props.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&u)?)?;
            }
        }
        item.ApplyDisplayProperties(&props)?;
        self.mp.SetSource(&item)?;
        self.mp.Play()?;
        Ok(())
    }

    pub fn play(&self) {
        let _ = self.mp.Play();
    }

    pub fn pause(&self) {
        let _ = self.mp.Pause();
    }

    pub fn is_playing(&self) -> bool {
        self.mp
            .PlaybackSession()
            .and_then(|s| s.PlaybackState())
            .is_ok_and(|s| s == MediaPlaybackState::Playing)
    }

    pub fn toggle(&self) {
        if self.is_playing() { self.pause() } else { self.play() }
    }

    /// (position, duration) in seconds.
    pub fn position(&self) -> (f64, f64) {
        let Ok(s) = self.mp.PlaybackSession() else { return (0.0, 0.0) };
        let pos = s.Position().map(|t| t.Duration).unwrap_or(0) as f64 / 1e7;
        let dur = s.NaturalDuration().map(|t| t.Duration).unwrap_or(0) as f64 / 1e7;
        (pos, dur)
    }

    pub fn seek_secs(&self, secs: f64) {
        if let Ok(s) = self.mp.PlaybackSession() {
            let _ = s.SetPosition(TimeSpan { Duration: (secs.max(0.0) * 1e7) as i64 });
        }
    }

    pub fn seek_fraction(&self, f: f32) {
        let (_, dur) = self.position();
        if dur > 0.0 {
            self.seek_secs(dur * f.clamp(0.0, 1.0) as f64);
        }
    }

    /// Unloads the current track (stops playback and releases its file).
    pub fn clear(&self) {
        let _ = self.mp.Pause();
        let _ = self.mp.SetSource(None::<&windows::Media::Playback::IMediaPlaybackSource>);
    }

    /// Releases the current file so it can be deleted.
    pub fn close(&self) {
        let _ = self.mp.Close();
    }

    pub fn set_volume(&self, v: f32) {
        let _ = self.mp.SetVolume(v.clamp(0.0, 1.0) as f64);
    }
}

/// `file:///C:/path%20with%20spaces/x.m4a`
fn file_uri(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let p = p.trim_start_matches("//?/");
    let mut out = String::from("file:///");
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_uri_escapes() {
        let u = super::file_uri(std::path::Path::new(r"C:\Users\A B\x#1.m4a"));
        assert_eq!(u, "file:///C:/Users/A%20B/x%231.m4a");
    }
}
