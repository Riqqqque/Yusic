# Yusic

A lightweight native YouTube Music client for Windows, written in Rust.

- UI: [Slint](https://slint.dev) with the software renderer. No browser engine runs.
- Data: [rustypipe](https://crates.io/crates/rustypipe) for albums, artists, playlists, search and radio. Home and Explore use a small InnerTube browse parser (`src/yt/browse.rs`).
- Audio: [yt-dlp](https://github.com/yt-dlp/yt-dlp) resolves the stream, Yusic downloads it, and `Windows.Media.Playback.MediaPlayer` plays it. Media keys and the Windows media flyout work through the player's built-in SMTC integration.

## Requirements

- Windows 10/11
- Node.js on `PATH` (yt-dlp needs a JavaScript runtime for YouTube)
- `yt-dlp.exe`, placed next to `yusic.exe`, in a `tools\` folder beside it, or on `PATH`. In this checkout it lives in `tools\yt-dlp.exe`, which is gitignored. Update it with `tools\yt-dlp.exe -U`.

## Build and run

```
cargo build --release
target\release\yusic.exe
```

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

`examples/` holds the feasibility spikes (stream resolution, MediaPlayer/SMTC, UI memory) and `smtc_list`, which lists Windows media sessions.

## Notes

- YouTube's DASH audio is fragmented MP4. When a file carries an edit list, Media Foundation jumps to the end of the track, so the resolver neutralizes the `edts` box after download (see `disable_fragmented_edit_list`).
- rustypipe's combined search mis-parses song rows, so songs and videos come from the filtered searches.
- Likes and playlist editing are not implemented yet.
- The app icon is drawn in code (`src/icon_raster.rs`). `build.rs` turns it into the `.ico` embedded in the exe.
