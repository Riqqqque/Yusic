pub use crate::icon_raster::rgba;

pub fn slint_image(size: u32) -> slint::Image {
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba(size), size, size);
    slint::Image::from_rgba8(buf)
}
