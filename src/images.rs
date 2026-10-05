//! Thumbnail loading. Downloads and decoding run on tokio; the UI thread
//! owns a byte-capped LRU of decoded images and the pending callbacks.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

const CACHE_BYTES: usize = 24 << 20;

struct Entry {
    image: Image,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, Entry>,
    bytes: usize,
    tick: u64,
}

impl Cache {
    fn get(&mut self, key: &str) -> Option<Image> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|e| {
            e.used = tick;
            e.image.clone()
        })
    }

    fn insert(&mut self, key: String, image: Image, bytes: usize) {
        self.tick += 1;
        self.bytes += bytes;
        if let Some(old) = self.map.insert(key, Entry { image, bytes, used: self.tick }) {
            self.bytes -= old.bytes;
        }
        while self.bytes > CACHE_BYTES && self.map.len() > 1 {
            let oldest = self.map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone());
            match oldest.and_then(|k| self.map.remove(&k)) {
                Some(e) => self.bytes -= e.bytes,
                None => break,
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }
}

type Callback = Box<dyn FnOnce(Image)>;

struct Loader {
    http: reqwest::Client,
    rt: tokio::runtime::Handle,
    limit: Arc<Semaphore>,
}

thread_local! {
    static LOADER: RefCell<Option<Loader>> = const { RefCell::new(None) };
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    static PENDING: RefCell<HashMap<String, Vec<Callback>>> = RefCell::new(HashMap::new());
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

pub fn init(http: reqwest::Client, rt: tokio::runtime::Handle) {
    LOADER.with(|l| {
        *l.borrow_mut() = Some(Loader { http, rt, limit: Arc::new(Semaphore::new(6)) });
    });
}

pub fn scale() -> f32 {
    SCALE.with(Cell::get)
}

pub fn set_scale(scale: f32) {
    SCALE.with(|s| s.set(scale.clamp(1.0, 4.0)));
}

/// Drops cached decoded images (used when the window is hidden).
pub fn trim() {
    CACHE.with(|c| c.borrow_mut().clear());
}

/// Requests `url` decoded at `w`x`h` logical pixels. `done` runs on the UI
/// thread, immediately if the image is cached.
pub fn request(url: &str, w: u32, h: u32, done: impl FnOnce(Image) + 'static) {
    request_shaped(url, w, h, false, done);
}

/// Like [`request`]; `round` masks the image to a circle (the software
/// renderer does not clip images to rounded rectangles).
pub fn request_shaped(url: &str, w: u32, h: u32, round: bool, done: impl FnOnce(Image) + 'static) {
    if url.is_empty() {
        return;
    }
    let scale = SCALE.with(Cell::get);
    let (pw, ph) = ((w as f32 * scale).round() as u32, (h as f32 * scale).round() as u32);
    let src = sized_url(url, pw, ph);
    let key = format!("{pw}x{ph}{}|{src}", if round { "o" } else { "" });
    if let Some(img) = CACHE.with(|c| c.borrow_mut().get(&key)) {
        done(img);
        return;
    }
    let first = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let list = p.entry(key.clone()).or_default();
        list.push(Box::new(done));
        list.len() == 1
    });
    if !first {
        return;
    }
    LOADER.with(|l| {
        let l = l.borrow();
        let Some(l) = l.as_ref() else { return };
        let (http, limit) = (l.http.clone(), l.limit.clone());
        l.rt.spawn(async move {
            let buf = {
                let _permit = limit.acquire().await;
                fetch(&http, &src, pw, ph, round).await
            };
            let _ = slint::invoke_from_event_loop(move || deliver(key, buf));
        });
    });
}

fn deliver(key: String, buf: Option<SharedPixelBuffer<Rgba8Pixel>>) {
    let callbacks = PENDING.with(|p| p.borrow_mut().remove(&key)).unwrap_or_default();
    let Some(buf) = buf else { return };
    let bytes = buf.as_bytes().len();
    let img = Image::from_rgba8(buf);
    CACHE.with(|c| c.borrow_mut().insert(key, img.clone(), bytes));
    for cb in callbacks {
        cb(img.clone());
    }
}

async fn fetch(http: &reqwest::Client, url: &str, w: u32, h: u32, round: bool) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let bytes = http.get(url).send().await.ok()?.error_for_status().ok()?.bytes().await.ok()?;
    tokio::task::spawn_blocking(move || decode(&bytes, w, h, round)).await.ok()?
}

fn decode(bytes: &[u8], w: u32, h: u32, round: bool) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() == w && img.height() == h {
        img
    } else {
        img.resize_to_fill(w, h, image::imageops::FilterType::Triangle)
    };
    let mut rgba = img.to_rgba8();
    if round {
        circle_mask(&mut rgba);
    }
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

fn circle_mask(img: &mut image::RgbaImage) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let (cx, cy, r) = (w / 2.0, h / 2.0, w.min(h) / 2.0);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
        let cover = (r - d + 0.5).clamp(0.0, 1.0);
        px[3] = (px[3] as f32 * cover) as u8;
    }
}

/// Asks Google's image servers for the size we display instead of the largest variant.
pub fn sized_url(url: &str, w: u32, h: u32) -> String {
    let resizable = url.contains("googleusercontent.com") || url.contains("ggpht.com");
    if !resizable {
        return url.to_owned();
    }
    match url.rfind('=') {
        Some(i) if !url[i..].contains('/') => format!("{}=w{w}-h{h}-l90-rj", &url[..i]),
        _ => format!("{url}=w{w}-h{h}-l90-rj"),
    }
}

#[cfg(test)]
mod tests {
    use super::sized_url;

    #[test]
    fn rewrites_google_sizes() {
        assert_eq!(
            sized_url("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj", 120, 120),
            "https://lh3.googleusercontent.com/abc=w120-h120-l90-rj"
        );
        assert_eq!(
            sized_url("https://yt3.ggpht.com/xyz=s576", 60, 60),
            "https://yt3.ggpht.com/xyz=w60-h60-l90-rj"
        );
        assert_eq!(sized_url("https://i.ytimg.com/vi/x/hqdefault.jpg", 60, 60), "https://i.ytimg.com/vi/x/hqdefault.jpg");
    }
}
