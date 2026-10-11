//! Off-screen drawing of the annotated thermal image, for snapshots and
//! videos: palette, hot-spot crosshair, numbered points and their values.

use image::{Rgb, RgbImage};

pub const SCALE: u32 = 8;

pub struct Overlay<'a> {
    pub hotspot: Option<(usize, usize, String)>,
    pub markers: &'a [(usize, usize, String)],
}

/// `colors` is the palette colour of each thermal pixel, row-major.
pub fn thermal_image(colors: &[[u8; 3]], w: usize, h: usize, overlay: &Overlay) -> RgbImage {
    let mut img = RgbImage::from_fn(w as u32 * SCALE, h as u32 * SCALE, |x, y| {
        Rgb(colors[(y / SCALE) as usize * w + (x / SCALE) as usize])
    });
    let centre = |x: usize, y: usize| ((x as u32 * SCALE + SCALE / 2) as i32, (y as u32 * SCALE + SCALE / 2) as i32);
    if let Some((x, y, label)) = &overlay.hotspot {
        let (cx, cy) = centre(*x, *y);
        crosshair(&mut img, cx, cy, 12);
        label_near(&mut img, cx, cy, 16, label);
    }
    for (n, (x, y, value)) in overlay.markers.iter().enumerate() {
        let (cx, cy) = centre(*x, *y);
        disc(&mut img, cx, cy, 5, Rgb([0, 0, 0]));
        disc(&mut img, cx, cy, 3, Rgb([80, 220, 255]));
        label_near(&mut img, cx, cy, 8, &format!("{} : {}", n + 1, value));
    }
    img
}

fn put(img: &mut RgbImage, x: i32, y: i32, c: Rgb<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        img.put_pixel(x as u32, y as u32, c);
    }
}

fn disc(img: &mut RgbImage, cx: i32, cy: i32, r: i32, c: Rgb<u8>) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(img, cx + dx, cy + dy, c);
            }
        }
    }
}

fn crosshair(img: &mut RgbImage, cx: i32, cy: i32, r: i32) {
    for (half, c) in [(2, Rgb([0, 0, 0])), (1, Rgb([255, 255, 255]))] {
        // Ring.
        for a in 0..360 {
            let t = (a as f32).to_radians();
            for d in -half + 1..half {
                let rr = r as f32 + d as f32;
                put(img, cx + (rr * t.cos()).round() as i32, cy + (rr * t.sin()).round() as i32, c);
            }
        }
        // Ticks outside the ring.
        for i in r * 6 / 10..=r * 18 / 10 {
            for d in -half + 1..half {
                put(img, cx + i, cy + d, c);
                put(img, cx - i, cy + d, c);
                put(img, cx + d, cy + i, c);
                put(img, cx + d, cy - i, c);
            }
        }
    }
}

/// 5×7 glyphs for the few characters labels use.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
        '6' => [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
        '.' => [0, 0, 0, 0, 0, 0x0c, 0x0c],
        '-' => [0, 0, 0, 0x1f, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1f, 0x04, 0x04, 0],
        ':' => [0, 0x0c, 0x0c, 0, 0x0c, 0x0c, 0],
        '°' => [0x0c, 0x12, 0x12, 0x0c, 0, 0, 0],
        'C' => [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
        _ => [0; 7],
    }
}

const PX: i32 = 2; // glyph pixel size
const ADVANCE: i32 = 6 * PX;

fn text_width(text: &str) -> i32 {
    text.chars().count() as i32 * ADVANCE
}

/// White text with a black outline, top-left at (x, y).
fn text(img: &mut RgbImage, x: i32, y: i32, s: &str) {
    for (pass, c) in [(true, Rgb([0, 0, 0])), (false, Rgb([255, 255, 255]))] {
        for (i, ch) in s.chars().enumerate() {
            let g = glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0x10 >> col) == 0 {
                        continue;
                    }
                    let px = x + i as i32 * ADVANCE + col * PX;
                    let py = y + row as i32 * PX;
                    let o = if pass { 1 } else { 0 };
                    for dy in -o..PX + o {
                        for dx in -o..PX + o {
                            put(img, px + dx, py + dy, c);
                        }
                    }
                }
            }
        }
    }
}

/// Label up-right of (cx, cy), flipped to stay inside the image.
fn label_near(img: &mut RgbImage, cx: i32, cy: i32, r: i32, s: &str) {
    let (w, h) = (text_width(s), 7 * PX);
    let x = if cx + r + w > img.width() as i32 { cx - r - w } else { cx + r };
    let y = if cy - r - h < 0 { cy + r } else { cy - r - h };
    text(img, x, y, s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_labels_inside_the_image() {
        let colors = vec![[40, 0, 80]; 80 * 60];
        let markers = [(2, 2, "35.2 °C".to_string())];
        let img = thermal_image(
            &colors,
            80,
            60,
            &Overlay { hotspot: Some((79, 0, "+1.25 °C".into())), markers: &markers },
        );
        assert_eq!(img.dimensions(), (640, 480));
        // Something white was drawn near the top-right hot spot.
        let white = (500..640).flat_map(|x| (0..60).map(move |y| (x, y))).any(|(x, y)| img.get_pixel(x, y).0 == [255; 3]);
        assert!(white);
        if let Some(dir) = std::env::var_os("FLIR_TEST_OUT") {
            img.save(std::path::Path::new(&dir).join("render.png")).unwrap();
        }
    }
}
