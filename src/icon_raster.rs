//! The Yusic "waveform" icon: a red circle with five white rounded bars.
//! Dependency-free so build.rs can use it to produce the .exe icon too.

const RED: [u8; 3] = [0xE5, 0x20, 0x2E];

/// (x, y, w, h) of the bars on a 64x64 grid.
const BARS: [(f32, f32, f32, f32); 5] = [
    (13.0, 27.0, 5.0, 10.0),
    (21.0, 20.0, 5.0, 24.0),
    (29.5, 13.0, 5.0, 38.0),
    (38.0, 20.0, 5.0, 24.0),
    (46.0, 27.0, 5.0, 10.0),
];

/// Straight (non-premultiplied) RGBA pixels for a `size`x`size` icon.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 5;
    let scale = 64.0 / size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (mut red, mut white) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if (px - 32.0).powi(2) + (py - 32.0).powi(2) > 32.0 * 32.0 {
                        continue;
                    }
                    if BARS.iter().any(|&b| in_bar(px, py, b)) {
                        white += 1;
                    } else {
                        red += 1;
                    }
                }
            }
            let covered = red + white;
            if covered == 0 {
                continue;
            }
            let wf = white as f32 / covered as f32;
            let i = ((y * size + x) * 4) as usize;
            for c in 0..3 {
                out[i + c] = (RED[c] as f32 * (1.0 - wf) + 255.0 * wf).round() as u8;
            }
            out[i + 3] = (covered as f32 / (SS * SS) as f32 * 255.0).round() as u8;
        }
    }
    out
}

/// Rounded bar with radius w/2.
fn in_bar(px: f32, py: f32, (x, y, w, h): (f32, f32, f32, f32)) -> bool {
    let r = w / 2.0;
    let cx = px.clamp(x + r, x + w - r);
    let cy = py.clamp(y + r, y + h - r);
    (px - cx).powi(2) + (py - cy).powi(2) <= r * r
}
