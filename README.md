# Yusic

A lightweight native YouTube Music client for Windows, written in Rust.

- UI: [Slint](https://slint.dev) with the software renderer. No browser engine runs.
- Data: [rustypipe](https://crates.io/crates/rustypipe) for albums, artists, playlists, search and radio. Home and Explore use a small InnerTube browse parser (`src/yt/browse.rs`).
- Audio: [yt-dlp](https://github.com/yt-dlp/yt-dlp) resolves the stream, Yusic downloads it, and `Windows.Media.Playback.MediaPlayer` plays it. Media keys and the Windows media flyout work through the player's built-in SMTC integration.

## Install

1. Download **[Yusic.exe](https://github.com/Riqqqque/Yusic/releases/latest/download/Yusic.exe)** from the latest release.
2. Run it and choose **Yes** to install. It installs for your Windows account (no admin needed) and adds Start menu and desktop shortcuts.
3. On first start, Yusic downloads the tools it needs to play music: [yt-dlp](https://github.com/yt-dlp/yt-dlp), plus the [Deno](https://deno.com) JavaScript runtime if Node.js isn't installed. Checksums are verified.

Yusic updates itself from GitHub releases, and yt-dlp is updated daily. To uninstall, use **Settings > Apps > Installed apps > Yusic**.

Windows SmartScreen may warn the first time because the exe isn't code-signed. Choose **More info > Run anyway**. Each release lists the SHA-256 of `Yusic.exe` if you want to check it.

Requires Windows 10 or 11. Signing in uses the WebView2 runtime, which comes with Windows 11.

## Build and run

```
cargo build --release
target\release\yusic.exe
```

Development builds look for `yt-dlp.exe` in `tools\` (gitignored) and use Node.js from `PATH`.

`scripts\release.ps1 -Version x.y.z` builds the public exe (with local paths removed from the binary) and publishes it as a GitHub release.

`scripts\deploy.ps1` builds and installs `dist\yusic.exe`, which the desktop shortcut runs. It doesn't interrupt a running copy: the old exe is renamed aside, keeps playing, and gets cleaned up on a later start.

Data lives in `%LOCALAPPDATA%\Yusic` (override with the `YUSIC_DATA_DIR` environment variable). Audio is downloaded per track. Only the current song and the next one are kept on disk. Everything else is deleted as you move on, and the folder is emptied on exit.

## Signing in

**Sign in** opens a small Google sign-in window, a temporary WebView2 instance. Once YouTube Music loads signed in, Yusic copies the session cookie and closes the window. The browser processes exit, and the temporary browser profile is deleted. The cookie is stored encrypted with Windows DPAPI (`auth.bin`), and so is rustypipe's cache (`rustypipe\cache.bin`). Signing in enables Library (playlists, liked music, albums, artists, recently played), the playlist sidebar, a personalized Home, and private playlists. Sign out is in the account menu (top right).

Closing the window hides Yusic to the tray, and playback continues. Quit from the tray menu. Launching it a second time brings the existing window back.

### Command-line flags

| Flag | Purpose |
|---|---|
| `--minimized` | Start hidden in the tray |
| `--route <r>` | Open a page: `home`, `explore`, `library`, `search:<q>`, `album:<id>`, `playlist:<id>`, `artist:<id>` |
| `--play <videoId>` | Start playing a track (with radio) |
| `--volume <0-1>` | Volume for this run only (not saved) |
| `--snapshot <png> [--delay ms]` | Render the window to a PNG and exit (for visual checks) |
| `--exit-after <s>` | Quit after N seconds |
| `--sign-in` | Open the sign-in window at startup |
| `--lyrics` | Open the Now Playing panel on the Lyrics tab |

Test runs (`--snapshot`, `--exit-after`) refuse to start next to a running Yusic unless `YUSIC_DATA_DIR` points them at their own folder.

`examples/` holds the feasibility spikes (stream resolution, MediaPlayer/SMTC, UI memory), `smtc_list` (lists Windows media sessions), and read-only probes for lyrics, page timing and the "Save to playlist" list.

## Notes

- YouTube's DASH audio is fragmented MP4. When a file carries an edit list, Media Foundation jumps to the end of the track, so the resolver neutralizes the `edts` box after download (see `disable_fragmented_edit_list`).
- rustypipe's combined search mis-parses song rows, so songs and videos come from the filtered searches.
- Audio: the best format available is chosen, in this order: 256 kbps Opus/AAC (YouTube Music Premium, when signed in), ~160 kbps Opus, then 128 kbps AAC. If Windows can't decode a file, that track is fetched again as AAC.
- Lyrics: time-synced lyrics come from [LRCLIB](https://lrclib.net), with YouTube Music's own lyrics as a fallback. Covers and re-uploads get the original song's words without the timing.
- When signed in: like or dislike songs (player bar), create, edit and delete playlists, save songs to playlists, remove songs from your own playlists, save albums and playlists to your library, and subscribe to artists. Right-click a song or card, or use its ⋮ button, for Play next, Add to queue, Start radio, Go to artist or album, and Copy link.
- Settings (sidebar or account menu): audio quality, autoplay, preparing the next song, LRCLIB lyrics and text size, accent color, content region, close-to-tray, start with Windows, game mode, automatic updates, and account.
- The app icon is drawn in code (`src/icon_raster.rs`). `build.rs` turns it into the `.ico` embedded in the exe.
