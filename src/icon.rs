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
const CLIP: RRect = RRect(11.0, 1.0, 21.0, 8.0, 2.0);

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

/// Colour (premultiplied, 0..1 alpha) of one sample point in design-grid coordinates.
fn sample(state: State, x: f32, y: f32) -> [f32; 4] {
    let [r, g, b] = state_color(state);
    if glyph(state).iter().any(|s| s.contains(x, y)) && BODY.contains(x, y) {
        [255.0, 255.0, 255.0, 1.0]
    } else if CLIP.contains(x, y) {
        [r * 0.55, g * 0.55, b * 0.55, 1.0]
    } else if BODY.contains(x, y) {
        [r, g, b, 1.0]
    } else {
        [0.0; 4]
    }
}

/// RGBA8 pixels, `size`×`size`, anti-aliased with 4×4 supersampling.
pub fn render(state: State, size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let scale = 32.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    let [r, g, b, a] = sample(state, x, y);
                    acc[0] += r * a;
                    acc[1] += g * a;
                    acc[2] += b * a;
                    acc[3] += a;
                }
            }
            let n = (SS * SS) as f32;
            let alpha = acc[3] / n;
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
                (alpha * 255.0).round() as u8,
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
}
