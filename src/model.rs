//! Plain, thread-safe data passed from the network side to the UI thread.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Song,
    Video,
    Episode,
    Album,
    Playlist,
    Artist,
    Podcast,
}

impl Kind {
    pub fn playable(self) -> bool {
        matches!(self, Kind::Song | Kind::Video | Kind::Episode)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Song => "song",
            Kind::Video => "video",
            Kind::Episode => "episode",
            Kind::Album => "album",
            Kind::Playlist => "playlist",
            Kind::Artist => "artist",
            Kind::Podcast => "podcast",
        }
    }

    pub fn from_str(s: &str) -> Kind {
        match s {
            "video" => Kind::Video,
            "episode" => Kind::Episode,
            "album" => Kind::Album,
            "playlist" => Kind::Playlist,
            "artist" => Kind::Artist,
            "podcast" => Kind::Podcast,
            _ => Kind::Song,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Link {
    pub name: String,
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Item {
    pub kind: Kind,
    /// Video id for playable items, browse id otherwise.
    pub id: String,
    /// Playlist id backing an album or playlist, if known.
    pub playlist_id: String,
    pub title: String,
    /// Free-form second line, used when `artists`/`album` are not structured.
    pub subtitle: String,
    pub thumb: String,
    pub artists: Vec<Link>,
    pub album: Option<Link>,
    pub duration: Option<u32>,
    pub wide: bool,
    /// Position id inside a playlist (needed to remove the row).
    pub set_video_id: Option<String>,
}

impl Item {
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn first_artist_id(&self) -> Option<&str> {
        self.artists.iter().find_map(|a| a.id.as_deref())
    }

    /// "Song • Artist • Album" style second line for list rows.
    pub fn row_subtitle(&self) -> String {
        if !self.subtitle.is_empty() {
            return self.subtitle.clone();
        }
        let mut parts = Vec::new();
        let artists = self.artist_names();
        if !artists.is_empty() {
            parts.push(artists);
        }
        if let Some(a) = &self.album {
            parts.push(a.name.clone());
        }
        parts.join(" • ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShelfKind {
    /// Horizontal carousel of square cards.
    Cards,
    /// "Quick picks": horizontal grid of song rows.
    List,
    /// Vertical list of two-line song rows.
    Tracks,
    /// Playlist-style columns.
    TrackList,
    /// Album-style numbered columns.
    Album,
}

impl ShelfKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShelfKind::Cards => "cards",
            ShelfKind::List => "list",
            ShelfKind::Tracks => "tracks",
            ShelfKind::TrackList => "tracklist",
            ShelfKind::Album => "album",
        }
    }

    /// Clicking a row in these shelves plays the shelf as a queue instead of a radio.
    pub fn is_collection(self) -> bool {
        matches!(self, ShelfKind::TrackList | ShelfKind::Album)
    }
}

#[derive(Clone, Debug)]
pub struct Shelf {
    pub title: String,
    pub kind: ShelfKind,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Default)]
pub enum Header {
    #[default]
    None,
    Title(String),
    Collection {
        title: String,
        line1: String,
        line2: String,
        description: String,
        thumb: String,
        round: bool,
    },
    Artist {
        title: String,
        description: String,
        thumb: String,
    },
}

#[derive(Clone, Debug)]
pub enum Continuation {
    /// InnerTube browse continuation token (Home / Explore).
    Browse(String),
    /// More tracks of a playlist.
    Tracks(rustypipe::model::paginator::Paginator<rustypipe::model::TrackItem>),
    /// More rows of a playlist read with our own InnerTube parser.
    PlaylistRows(String),
}

/// What the signed-in user can do with the page's subject.
#[derive(Clone, Debug, Default)]
pub struct PageActions {
    /// Playlist id to save/remove (albums and playlists).
    pub library_id: Option<String>,
    pub saved: Option<bool>,
    /// The user's own playlist: rename, delete, remove songs.
    pub owned_playlist: Option<String>,
    pub channel_id: Option<String>,
    pub subscribed: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub header: Header,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<Continuation>,
    /// Tracks played by the header's play / shuffle buttons.
    pub play: Vec<Item>,
    /// Radio playlist id for the header's radio button.
    pub radio: Option<String>,
    pub notice: String,
    pub actions: PageActions,
}

pub fn fmt_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => trim_1(n as f64 / 1e3) + "K",
        1_000_000..1_000_000_000 => trim_1(n as f64 / 1e6) + "M",
        _ => trim_1(n as f64 / 1e9) + "B",
    }
}

fn trim_1(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.1}");
        s.trim_end_matches(".0").to_string()
    }
}
