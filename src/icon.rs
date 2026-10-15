//! The ClipBridge icon, drawn in code at any size: a clipboard silhouette whose colour
//! and glyph show the state. Used for the tray and to generate the app icon files
//! (`cargo run --example gen_icons`).

use crate::app::State;

fn state_color(state: State) -> [f32; 3] {
    let [r, g, b] = match state {
        State::Active => [0x2e, 0xa0, 0x5b],
        State::Paused => [0x8a, 0x8f, 0x98],
        State::NotConfigured | State::Starting => [0xe0, 0x9a, 0x2a],
        State::Error => [0xd6, 0x45, 0x45],
    };
    [r as f32, g as f32, b as f32]
}

/// Axis-aligned rounded rectangle in the 32×32 design grid.
struct RRect(f32, f32, f32, f32, f32);

impl RRect {
    fn contains(&self, x: f32, y: f32) -> bool {
        let RRect(x0, y0, x1, y1, r) = *self;
        if x < x0 || x > x1 || y < y0 || y > y1 {
            return false;
        }
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    }
}

const BODY: RRect = RRect(4.0, 4.0, 28.0, 31.5, 4.0);
/// The clip on top: same colour as the body (so the silhouette reads on any
/// background) with a slot cut into it.
const CLIP: RRect = RRect(10.5, 0.5, 21.5, 8.0, 2.5);
const SLOT: RRect = RRect(13.5, 2.5, 18.5, 4.5, 1.0);

fn glyph(state: State) -> &'static [RRect] {
    const LINES: &[RRect] = &[
        RRect(9.0, 12.0, 23.0, 14.0, 1.0),
        RRect(9.0, 18.0, 23.0, 20.0, 1.0),
        RRect(9.0, 24.0, 19.0, 26.0, 1.0),
    ];
    const PAUSE: &[RRect] = &[
        RRect(11.0, 11.0, 14.0, 25.0, 1.0),
        RRect(18.0, 11.0, 21.0, 25.0, 1.0),
    ];
    const BANG: &[RRect] = &[
        RRect(14.5, 10.0, 17.5, 21.0, 1.5),
        RRect(14.5, 23.5, 17.5, 26.5, 1.5),
    ];
    match state {
        State::Active => LINES,
        State::Paused => PAUSE,
        _ => BANG,
    }
}

fn in_shape(x: f32, y: f32) -> bool {
    BODY.contains(x, y) || CLIP.contains(x, y)
}

/// White marks on the shape: the state glyph and the slot in the clip.
fn in_marks(state: State, x: f32, y: f32) -> bool {
    SLOT.contains(x, y) || (BODY.contains(x, y) && glyph(state).iter().any(|s| s.contains(x, y)))
}

/// Colour + alpha (0..1) of one sample point in design-grid coordinates.
fn sample(state: State, x: f32, y: f32) -> [f32; 4] {
    if !in_shape(x, y) {
        return [0.0; 4];
    }
    if in_marks(state, x, y) {
        return [255.0, 255.0, 255.0, 1.0];
    }
    let [r, g, b] = state_color(state);
    [r, g, b, 1.0]
}

/// Monochrome "template" sample for the macOS menu bar: the system recolours it to
/// match a light or dark bar, so only alpha matters. The marks are cut out.
fn sample_template(state: State, x: f32, y: f32) -> [f32; 4] {
    // Paused / starting look dimmed, like other inactive menu-bar items.
    let alpha = match state {
        State::Active => 1.0,
        _ => 0.5,
    };
    if in_shape(x, y) && !in_marks(state, x, y) {
        [0.0, 0.0, 0.0, alpha]
    } else {
        [0.0; 4]
    }
}

/// RGBA8 pixels, `size`×`size`, anti-aliased with 4×4 supersampling.
pub fn render(state: State, size: u32) -> Vec<u8> {
    supersample(size, |u, v| sample(state, u * 32.0, v * 32.0))
}

/// Monochrome template variant (see [`sample_template`]).
pub fn render_template(state: State, size: u32) -> Vec<u8> {
    supersample(size, |u, v| sample_template(state, u * 32.0, v * 32.0))
}

/// macOS application icon: the clipboard on a light rounded square laid out on
/// Apple's 1024 grid (824 px shape, 100 px margin). macOS 26 puts icons that lack
/// such a shape on a plain plate of its own; drawing it ourselves keeps the Dock,
/// Spotlight and Finder consistent.
pub fn render_macos_app_icon(size: u32) -> Vec<u8> {
    const INSET: f32 = 100.0 / 1024.0;
    const HALF: f32 = 0.5 - INSET;
    // Clipboard height relative to the whole canvas.
    const ART: f32 = 0.56;
    supersample(size, |u, v| {
        // Squircle (superellipse, n = 5) approximates Apple's continuous corners.
        let (dx, dy) = ((u - 0.5) / HALF, (v - 0.5) / HALF);
        let edge = dx.abs().powi(5) + dy.abs().powi(5);
        if edge > 1.0 {
            return [0.0; 4];
        }
        // Map into the design grid, centring the artwork (x 4..28, y 0.5..31.5).
        let k = 31.0 / ART;
        let (x, y) = ((u - 0.5) * k + 16.0, (v - 0.5) * k + 16.0);
        let fg = sample(State::Active, x, y);
        if fg[3] > 0.0 {
            return fg;
        }
        // Soft top-to-bottom light gradient, with a faint rim so the plate stays
        // visible on light backgrounds.
        let rim = if edge > 0.955 { 22.0 } else { 0.0 };
        let shade = 252.0 - 14.0 * v - rim;
        [shade, shade, shade, 1.0]
    })
}

/// Windows application icon (exe, taskbar, installer): a full-bleed green rounded
/// tile with a white clipboard, so it lines up with neighbouring app tiles.
pub fn render_tile_icon(size: u32) -> Vec<u8> {
    // Tile almost fills the square; radius ~22% like Windows 11 app tiles.
    const TILE: RRect = RRect(0.5, 0.5, 31.5, 31.5, 7.0);
    // Clipboard height relative to the tile.
    const ART: f32 = 0.72;
    let [r, g, b] = state_color(State::Active);
    supersample(size, |u, v| {
        let (tx, ty) = (u * 32.0, v * 32.0);
        if !TILE.contains(tx, ty) {
            return [0.0; 4];
        }
        let k = 31.0 / ART;
        let (x, y) = ((u - 0.5) * k + 16.0, (v - 0.5) * k + 16.0);
        if in_shape(x, y) && !in_marks(State::Active, x, y) {
            [255.0, 255.0, 255.0, 1.0]
        } else {
            [r, g, b, 1.0]
        }
    })
}

/// Places `art`×`art` RGBA pixels centred on a transparent `canvas`×`canvas` square.
pub fn pad_center(rgba: &[u8], art: u32, canvas: u32) -> Vec<u8> {
    let off = (canvas - art) / 2;
    let mut out = vec![0u8; (canvas * canvas * 4) as usize];
    let row = (art * 4) as usize;
    for y in 0..art {
        let src = (y * art * 4) as usize;
        let dst = (((y + off) * canvas + off) * 4) as usize;
        out[dst..dst + row].copy_from_slice(&rgba[src..src + row]);
    }
    out
}

/// Whether the macOS menu bar should show the monochrome template icon for this state.
/// Problems stay coloured so they stand out.
pub fn uses_template(state: State) -> bool {
    matches!(state, State::Active | State::Paused | State::Starting)
}

/// Renders `f(u, v)` over the unit square (u, v in 0..1) with 4×4 supersampling.
fn supersample(size: u32, f: impl Fn(f32, f32) -> [f32; 4]) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let [r, g, b, a] = f(u, v);
                    acc[0] += r * a;
                    acc[1] += g * a;
                    acc[2] += b * a;
                    acc[3] += a;
                }
            }
            let n = (SS * SS) as f32;
            let unpremul = |c: f32| {
                if acc[3] > 0.0 {
                    (c / acc[3]).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                }
            };
            out.extend_from_slice(&[
                unpremul(acc[0]),
                unpremul(acc[1]),
                unpremul(acc[2]),
                (acc[3] / n * 255.0).round() as u8,
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_differ_per_state_and_scale() {
        let states = [
            State::Active,
            State::Paused,
            State::NotConfigured,
            State::Error,
        ];
        let icons: Vec<_> = states.iter().map(|s| render(*s, 32)).collect();
        for (i, a) in icons.iter().enumerate() {
            assert_eq!(a.len(), 32 * 32 * 4);
            for b in &icons[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let big = render(State::Active, 256);
        assert_eq!(big.len(), 256 * 256 * 4);
        // Corners transparent, centre of the body opaque.
        assert_eq!(big[3], 0);
        let centre = ((128 * 256 + 128) * 4 + 3) as usize;
        assert_eq!(big[centre], 255);
    }

    #[test]
    fn macos_icon_has_margin_and_opaque_plate() {
        let px = render_macos_app_icon(128);
        let alpha = |x: u32, y: u32| px[((y * 128 + x) * 4 + 3) as usize];
        assert_eq!(alpha(2, 2), 0, "outside the rounded square");
        assert_eq!(alpha(64, 15), 255, "plate near the top edge");
        assert_eq!(alpha(64, 64), 255);
    }
}
