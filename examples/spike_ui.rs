//! Spike 4: Slint window with ~50 real cover thumbnails; measures working set and
//! CPU while auto-scrolling. Pick the renderer with SLINT_BACKEND, e.g.
//!   SLINT_BACKEND=winit-software / winit-femtovg
//!
//! cargo run --release --example spike_ui

use std::time::{Duration, Instant};

use anyhow::Result;
use rustypipe::client::RustyPipe;
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

slint::slint! {
    export component Spike inherits Window {
        in property <[image]> covers;
        in-out property <length> scroll-y;
        title: "Yusic UI spike";
        preferred-width: 1280px;
        preferred-height: 800px;
        background: #030303;
        Flickable {
            content-y: -root.scroll-y;
            content-height: ceil(covers.length / 6) * 260px + 40px;
            for img[i] in covers: Rectangle {
                x: 32px + mod(i, 6) * 200px;
                y: 24px + floor(i / 6) * 260px;
                width: 180px;
                height: 240px;
                Rectangle {
                    width: 180px; height: 180px; y: 0;
                    border-radius: 4px;
                    clip: true;
                    Image { source: img; width: 180px; height: 180px; image-fit: cover; }
                }
                Text { y: 188px; x: 0; text: "Track title " + i; color: #ffffff; font-size: 14px; }
                Text { y: 208px; x: 0; text: "Artist name"; color: #aaaaaa; font-size: 13px; }
            }
        }
    }
}

fn mem_mb() -> (f64, f64) {
    let mut c = PROCESS_MEMORY_COUNTERS::default();
    unsafe {
        let _ = K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
    }
    (
        c.WorkingSetSize as f64 / 1048576.0,
        c.PagefileUsage as f64 / 1048576.0,
    )
}

fn cpu_time() -> Duration {
    let (mut a, mut b, mut k, mut u) = Default::default();
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
    }
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) * 100;
    Duration::from_nanos(f(k) + f(u))
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let rgba: Vec<(u32, u32, Vec<u8>)> = rt.block_on(async {
        let rp = RustyPipe::builder()
            .storage_dir(std::env::temp_dir().join("yusic-spike"))
            .build()?;
        let http = reqwest::Client::new();
        let mut urls = Vec::new();
        for q in ["daft punk", "taylor swift", "radiohead"] {
            let r = rp.query().music_search_tracks(q).await?;
            urls.extend(r.items.items.into_iter().filter_map(|t| t.cover.last().map(|c| c.url.clone())));
        }
        let mut out = Vec::new();
        for u in urls.into_iter().take(50) {
            let bytes = http.get(&u).send().await?.bytes().await?;
            let img = image::load_from_memory(&bytes)?.thumbnail(226, 226).to_rgba8();
            out.push((img.width(), img.height(), img.into_raw()));
        }
        anyhow::Ok(out)
    })?;
    println!("covers: {}", rgba.len());
    println!("before window: {:.1} MB ws / {:.1} MB private", mem_mb().0, mem_mb().1);

    let ui = Spike::new()?;
    let imgs: Vec<Image> = rgba
        .iter()
        .map(|(w, h, px)| {
            Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(px, *w, *h))
        })
        .collect();
    drop(rgba);
    ui.set_covers(ModelRc::new(VecModel::from(imgs)));

    let start = Instant::now();
    let weak = ui.as_weak();
    let mut cpu0 = Duration::ZERO;
    let mut phase = 0;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let ui = weak.unwrap();
        let t = start.elapsed().as_secs_f32();
        if t < 3.0 {
            return;
        }
        if phase == 0 {
            let (ws, pv) = mem_mb();
            println!("idle visible: {ws:.1} MB ws / {pv:.1} MB private");
            cpu0 = cpu_time();
            phase = 1;
        }
        if t < 8.0 {
            // ~60 fps continuous scroll back and forth
            let y = ((t - 3.0) * 400.0) % 1000.0;
            ui.set_scroll_y(y);
        } else if phase == 1 {
            let cpu = cpu_time() - cpu0;
            let (ws, pv) = mem_mb();
            println!(
                "after 5s scroll: {ws:.1} MB ws / {pv:.1} MB private, CPU {:.0}% of one core",
                cpu.as_secs_f64() / 5.0 * 100.0
            );
            cpu0 = cpu_time();
            phase = 2;
        } else if t > 11.0 && phase == 2 {
            let cpu = cpu_time() - cpu0;
            println!("idle 3s: CPU {:.1}% of one core", cpu.as_secs_f64() / 3.0 * 100.0);
            slint::quit_event_loop().unwrap();
        }
    });
    ui.run()?;
    drop(rt);
    Ok(())
}
