//! Regenerates the app icon files in `assets/` from `src/icon.rs`.
//! Run: cargo run --example gen_icons

use clipbridge::icon::{render_macos_app_icon, render_tile_icon};
use image::codecs::ico::{IcoEncoder, IcoFrame};

fn main() -> anyhow::Result<()> {
    std::fs::create_dir_all("assets")?;

    // macOS .app icon (rounded square on Apple's grid), turned into .icns at packaging.
    let big = render_macos_app_icon(1024);
    image::save_buffer(
        "assets/icon-1024.png",
        &big,
        1024,
        1024,
        image::ExtendedColorType::Rgba8,
    )?;

    let frames = [16, 24, 32, 48, 64, 128, 256]
        .into_iter()
        .map(|s| IcoFrame::as_png(&render_tile_icon(s), s, s, image::ExtendedColorType::Rgba8))
        .collect::<Result<Vec<_>, _>>()?;
    IcoEncoder::new(std::fs::File::create("assets/icon.ico")?).encode_images(&frames)?;

    println!("wrote assets/icon-1024.png and assets/icon.ico");
    Ok(())
}
