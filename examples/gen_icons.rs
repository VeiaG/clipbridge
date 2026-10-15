//! Regenerates the app icon files in `assets/` from `src/icon.rs`.
//! Run: cargo run --example gen_icons

use clipbridge::app::State;
use clipbridge::icon::render;
use image::codecs::ico::{IcoEncoder, IcoFrame};

fn main() -> anyhow::Result<()> {
    std::fs::create_dir_all("assets")?;

    // macOS icons keep a margin around the artwork (Big Sur grid: ~10% each side).
    let (canvas, art, off) = (1024u32, 824u32, 100u32);
    let small = render(State::Active, art);
    let mut big = vec![0u8; (canvas * canvas * 4) as usize];
    for y in 0..art {
        let src = (y * art * 4) as usize;
        let dst = (((y + off) * canvas + off) * 4) as usize;
        big[dst..dst + (art * 4) as usize].copy_from_slice(&small[src..src + (art * 4) as usize]);
    }
    image::save_buffer(
        "assets/icon-1024.png",
        &big,
        1024,
        1024,
        image::ExtendedColorType::Rgba8,
    )?;

    let frames = [16, 24, 32, 48, 64, 128, 256]
        .into_iter()
        .map(|s| {
            IcoFrame::as_png(
                &render(State::Active, s),
                s,
                s,
                image::ExtendedColorType::Rgba8,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    IcoEncoder::new(std::fs::File::create("assets/icon.ico")?).encode_images(&frames)?;

    println!("wrote assets/icon-1024.png and assets/icon.ico");
    Ok(())
}
