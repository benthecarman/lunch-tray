//! The Lunch Tray mark: a wide rounded tray with two compartments. One
//! geometry feeds the tray icon, the window header, the window icon, and
//! the launcher icon, so the app looks the same everywhere.

use std::path::Path;

use anyhow::{Context, Result};

/// Supersampling grid per pixel.
const SS: usize = 4;

fn sd_round_rect(x: f32, y: f32, half: f32, radius: f32) -> f32 {
    let qx = x.abs() - half + radius;
    let qy = y.abs() - half + radius;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// True when the unit-square point is on the tray mark: the rim or the
/// divider between the two compartments.
/// Vertical center of the mark in its own unit square.
const MARK_CENTER_Y: f32 = 0.56;

pub fn on_mark(x: f32, y: f32) -> bool {
    let rim = sd_round_rect(x - 0.5, (y - MARK_CENTER_Y) * 1.35, 0.44, 0.16);
    let in_rim = (-0.1..=0.0).contains(&rim);
    let inside = rim < -0.1;
    let in_divider = inside && (x - 0.4).abs() <= 0.045;
    in_rim || in_divider
}

/// Badge in the top-right corner, and the gap kept around it.
pub fn on_badge(x: f32, y: f32) -> (bool, bool) {
    let d = ((x - 0.82).powi(2) + (y - 0.2).powi(2)).sqrt();
    (d <= 0.19, d <= 0.26)
}

/// Coverage of `f` over one pixel, 0..=1.
fn coverage(x: usize, y: usize, size: usize, f: impl Fn(f32, f32) -> bool) -> f32 {
    let mut hits = 0;
    for sy in 0..SS {
        for sx in 0..SS {
            let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
            let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
            hits += f(px, py) as u32;
        }
    }
    hits as f32 / (SS * SS) as f32
}

/// The mark alone, in `rgb`, on a transparent background, with an optional
/// badge in `badge` color. Straight (unmultiplied) RGBA.
pub fn mark_rgba(size: usize, rgb: [u8; 3], badge: Option<[u8; 3]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let (dot, near) = if badge.is_some() {
                (
                    coverage(x, y, size, |px, py| on_badge(px, py).0),
                    coverage(x, y, size, |px, py| on_badge(px, py).1) > 0.0,
                )
            } else {
                (0.0, false)
            };
            if dot > 0.0 {
                let c = badge.unwrap();
                out.extend_from_slice(&[c[0], c[1], c[2], (dot * 255.0) as u8]);
            } else {
                let g = if near {
                    0.0
                } else {
                    coverage(x, y, size, on_mark)
                };
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (g * 255.0) as u8]);
            }
        }
    }
    out
}

// The dark tray palette: slate tile, mint mark.
const ICON_BG: [u8; 3] = [0x1d, 0x23, 0x20];
const ICON_EDGE: [u8; 3] = [0x3f, 0x49, 0x43];
const ICON_INK: [u8; 3] = [0xe9, 0xed, 0xe8];

/// The launcher icon: the mint mark on a rounded slate tile.
pub fn app_icon_rgba(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * size * 4);
    let tile = |px: f32, py: f32| sd_round_rect(px - 0.5, py - 0.5, 0.47, 0.11) <= 0.0;
    let edge = |px: f32, py: f32| {
        let d = sd_round_rect(px - 0.5, py - 0.5, 0.47, 0.11);
        (-0.02..=0.0).contains(&d)
    };
    // The mark sits centered at 64% of the tile. Its own geometry is drawn
    // a little low to leave room for the tray badge, so lift it back.
    let mark =
        |px: f32, py: f32| on_mark((px - 0.5) / 0.64 + 0.5, (py - 0.5) / 0.64 + MARK_CENTER_Y);
    for y in 0..size {
        for x in 0..size {
            let t = coverage(x, y, size, tile);
            let e = coverage(x, y, size, edge);
            let m = coverage(x, y, size, mark);
            let mut c = ICON_BG;
            if e > 0.0 {
                c = blend(c, ICON_EDGE, e);
            }
            if m > 0.0 {
                c = blend(c, ICON_INK, m);
            }
            out.extend_from_slice(&[c[0], c[1], c[2], (t * 255.0) as u8]);
        }
    }
    out
}

fn blend(base: [u8; 3], top: [u8; 3], t: f32) -> [u8; 3] {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    [
        mix(base[0], top[0]),
        mix(base[1], top[1]),
        mix(base[2], top[2]),
    ]
}

/// The launcher icon as SVG, the same tile and mark as the raster version,
/// for the sizes GNOME renders on the fly.
pub fn app_icon_svg() -> String {
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    // Mark strokes are centered on their path: the rim's half extent is
    // 0.44 with a 0.1 band inside it, so the centerline sits at 0.39.
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128" width="128" height="128">
  <rect x="3.84" y="3.84" width="120.32" height="120.32" rx="14.08" fill="{bg}" stroke="{edge}" stroke-width="2.56"/>
  <g transform="translate(64 64) scale(81.92) scale(1 0.7407)" fill="none" stroke="{ink}" stroke-linecap="butt">
    <rect x="-0.39" y="-0.39" width="0.78" height="0.78" rx="0.11" stroke-width="0.1"/>
    <path d="M -0.1 -0.34 V 0.34" stroke-width="0.09"/>
  </g>
</svg>
"##,
        bg = hex(ICON_BG),
        edge = hex(ICON_EDGE),
        ink = hex(ICON_INK),
    )
}

/// Write launcher icons into a hicolor theme directory: PNGs for every
/// common size and an SVG for the rest.
pub fn export_icons(hicolor: &Path) -> Result<()> {
    let scalable = hicolor.join("scalable").join("apps");
    std::fs::create_dir_all(&scalable)?;
    std::fs::write(scalable.join("lunch-tray.svg"), app_icon_svg())?;
    for size in [16usize, 22, 24, 32, 48, 64, 128, 256, 512] {
        let dir = hicolor.join(format!("{size}x{size}")).join("apps");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("lunch-tray.png");
        let img = image::RgbaImage::from_raw(size as u32, size as u32, app_icon_rgba(size))
            .context("icon buffer")?;
        img.save(&path)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_icon_have_visible_pixels() {
        let mark = mark_rgba(22, [255, 255, 255], None);
        assert_eq!(mark.len(), 22 * 22 * 4);
        assert!(mark.chunks(4).filter(|p| p[3] > 200).count() > 40);
        let badged = mark_rgba(22, [255, 255, 255], Some([0xe0, 0xb2, 0x3a]));
        assert!(
            badged
                .chunks(4)
                .filter(|p| p[3] > 200 && p[0] == 0xe0)
                .count()
                > 4
        );
        let icon = app_icon_rgba(64);
        let ink = icon
            .chunks(4)
            .filter(|p| p[0] == 0xe9 && p[3] == 255)
            .count();
        let bg = icon
            .chunks(4)
            .filter(|p| p[0] == 0x1d && p[3] == 255)
            .count();
        assert!(ink > 100 && bg > 1000, "ink {ink} bg {bg}");
        // Corners are transparent.
        assert_eq!(icon[3], 0);
    }

    #[test]
    fn app_icon_is_vertically_centered() {
        let size = 128;
        let icon = app_icon_rgba(size);
        let ink_rows: Vec<usize> = (0..size)
            .filter(|y| {
                (0..size).any(|x| {
                    let i = (y * size + x) * 4;
                    icon[i] == 0xe9 && icon[i + 3] == 255
                })
            })
            .collect();
        let top = *ink_rows.first().unwrap();
        let bottom = size - 1 - *ink_rows.last().unwrap();
        assert!(
            (top as i64 - bottom as i64).abs() <= 1,
            "top {top} bottom {bottom}"
        );
    }

    #[test]
    fn svg_icon_is_well_formed() {
        let svg = app_icon_svg();
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("#e9ede8") && svg.contains("#1d2320"));
    }
}
