#[path = "src/icon_raster.rs"]
mod icon_raster;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint build failed");

    println!("cargo:rerun-if-changed=src/icon_raster.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let ico_path = out.join("yusic.ico");
        write_ico(&ico_path);
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        res.set("FileDescription", "Yusic");
        res.set("ProductName", "Yusic");
        res.compile().expect("failed to embed icon resource");
    }
}

fn write_ico(path: &std::path::Path) {
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    let frames: Vec<IcoFrame> = [16u32, 24, 32, 48, 64, 128, 256]
        .iter()
        .map(|&s| IcoFrame::as_png(&icon_raster::rgba(s), s, s, image::ExtendedColorType::Rgba8).unwrap())
        .collect();
    let file = std::fs::File::create(path).unwrap();
    IcoEncoder::new(file).encode_images(&frames).unwrap();
}
