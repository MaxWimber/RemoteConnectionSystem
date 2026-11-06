//! Pointer shapes for the browser (P4-7, D-022): Desktop Duplication shapes -> RGBA -> PNG for `CursorShape`.
//!
//! Runs on the input thread, never on the capture thread. Shapes change rarely, so the allocations here are fine.
//! Shapes that invert the screen (the text cursor) cannot be drawn by a browser; they become black, which is how
//! they look on the white fields where they usually appear.

use rcs_capture::PointerShape;
use rcs_capture::capture::{SHAPE_COLOR, SHAPE_MASKED_COLOR, SHAPE_MONOCHROME};
use rcs_protocol::input::MAX_CURSOR_SHAPE_BYTES;

/// Room for the `CursorShape` header in front of the PNG.
const HEADER: usize = 11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Rgba {
    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// The shape as straight RGBA, or None for a kind or size that makes no sense.
pub fn to_rgba(shape: &PointerShape) -> Option<Rgba> {
    let (w, pitch) = (shape.width, shape.pitch as usize);
    let h = if shape.kind == SHAPE_MONOCHROME {
        shape.height / 2
    } else {
        shape.height
    };
    if w == 0 || h == 0 || w > 1024 || h > 1024 {
        return None;
    }
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let px = match shape.kind {
                SHAPE_COLOR | SHAPE_MASKED_COLOR => {
                    let i = y * pitch + x * 4;
                    let [b, g, r, a] = <[u8; 4]>::try_from(shape.data.get(i..i + 4)?).ok()?;
                    if shape.kind == SHAPE_COLOR {
                        [r, g, b, a]
                    } else if a == 0 {
                        // Mask 0: the color replaces the screen.
                        [r, g, b, 255]
                    } else if r | g | b == 0 {
                        // XOR with black leaves the screen as it is.
                        CLEAR
                    } else {
                        BLACK
                    }
                }
                SHAPE_MONOCHROME => {
                    let bit = |row: usize| {
                        let byte = *shape.data.get(row * pitch + x / 8)?;
                        Some(byte & (0x80 >> (x % 8)) != 0)
                    };
                    match (bit(y)?, bit(y + h as usize)?) {
                        (false, false) => BLACK,
                        (false, true) => WHITE,
                        (true, false) => CLEAR,
                        // Inverts the screen: drawn black.
                        (true, true) => BLACK,
                    }
                }
                _ => return None,
            };
            pixels.extend_from_slice(&px);
        }
    }
    Some(Rgba {
        width: w,
        height: h,
        pixels,
    })
}

/// Half the size, each pixel the average of four with alpha weighting.
fn halve(image: &Rgba) -> Rgba {
    let (w, h) = (image.width.div_ceil(2), image.height.div_ceil(2));
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 4];
            let mut n = 0;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (sx, sy) = (x * 2 + dx, y * 2 + dy);
                if sx < image.width && sy < image.height {
                    let p = image.pixel(sx, sy);
                    let a = u32::from(p[3]);
                    for c in 0..3 {
                        sum[c] += u32::from(p[c]) * a;
                    }
                    sum[3] += a;
                    n += 1;
                }
            }
            let a = sum[3];
            let color = |c: usize| sum[c].checked_div(a).unwrap_or(0) as u8;
            pixels.extend_from_slice(&[color(0), color(1), color(2), (a / n) as u8]);
        }
    }
    Rgba {
        width: w,
        height: h,
        pixels,
    }
}

pub fn png(image: &Rgba) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&image.pixels).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// A shape ready for `CursorShape`: PNG, its size and the hot spot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub png: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
}

/// Converts and compresses a shape, halving it until the message fits in 64 KiB.
pub fn encode(shape: &PointerShape) -> Option<Encoded> {
    let mut image = to_rgba(shape)?;
    let (mut hot_x, mut hot_y) = (shape.hot_x, shape.hot_y);
    loop {
        let png = png(&image)?;
        if HEADER + png.len() <= MAX_CURSOR_SHAPE_BYTES {
            return Some(Encoded {
                png,
                width: image.width as u16,
                height: image.height as u16,
                hot_x: hot_x.min(image.width.saturating_sub(1)) as u16,
                hot_y: hot_y.min(image.height.saturating_sub(1)) as u16,
            });
        }
        if image.width <= 1 && image.height <= 1 {
            return None;
        }
        image = halve(&image);
        hot_x /= 2;
        hot_y /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(kind: u32, width: u32, height: u32, pitch: u32, data: Vec<u8>) -> PointerShape {
        PointerShape {
            kind,
            width,
            height,
            pitch,
            hot_x: 1,
            hot_y: 0,
            data,
        }
    }

    #[test]
    fn color_shapes_swap_to_rgba() {
        // 2x1, pitch 12 with padding: a half transparent blue, an opaque red.
        let data = vec![255, 0, 0, 128, 0, 0, 255, 255, 9, 9, 9, 9];
        let img = to_rgba(&shape(SHAPE_COLOR, 2, 1, 12, data)).unwrap();
        assert_eq!(img.pixels, [0, 0, 255, 128, 255, 0, 0, 255]);
    }

    #[test]
    fn masked_shapes_replace_or_invert() {
        // Mask 0 with a color, mask 1 with black (no change), mask 1 with a color (inverts).
        let data = vec![10, 20, 30, 0, 0, 0, 0, 255, 50, 50, 50, 255];
        let img = to_rgba(&shape(SHAPE_MASKED_COLOR, 3, 1, 12, data)).unwrap();
        assert_eq!(img.pixels, [30, 20, 10, 255, 0, 0, 0, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn monochrome_shapes_use_both_masks() {
        // 4x1 visible: AND row 0b0011_0000, XOR row 0b0101_0000.
        // Pixels: (0,0) black, (0,1) white, (1,0) clear, (1,1) inverted -> black.
        let data = vec![0b0011_0000, 0b0101_0000];
        let img = to_rgba(&shape(SHAPE_MONOCHROME, 4, 2, 1, data)).unwrap();
        assert_eq!((img.width, img.height), (4, 1));
        assert_eq!(
            img.pixels,
            [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 255]
        );
    }

    #[test]
    fn short_or_strange_shapes_are_refused() {
        assert!(
            to_rgba(&shape(SHAPE_COLOR, 2, 2, 8, vec![0; 8])).is_none(),
            "short data"
        );
        assert!(
            to_rgba(&shape(7, 1, 1, 4, vec![0; 4])).is_none(),
            "unknown kind"
        );
        assert!(to_rgba(&shape(SHAPE_COLOR, 0, 1, 4, Vec::new())).is_none());
    }

    #[test]
    fn png_round_trips_and_big_shapes_shrink_to_fit() {
        let small = to_rgba(&shape(SHAPE_COLOR, 2, 1, 8, vec![1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
        let bytes = png(&small).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..8], &small.pixels[..]);

        // 256x256 of noise does not compress: it is halved until it fits, and the hot spot follows.
        let mut seed = 1u32;
        let noise: Vec<u8> = (0..256 * 256 * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect();
        let mut big = shape(SHAPE_COLOR, 256, 256, 1024, noise);
        big.hot_x = 200;
        big.hot_y = 100;
        let e = encode(&big).unwrap();
        assert!(e.png.len() + HEADER <= MAX_CURSOR_SHAPE_BYTES);
        assert!(
            e.width < 256 && e.width == e.height,
            "{}x{}",
            e.width,
            e.height
        );
        let scale = 256 / u32::from(e.width);
        assert_eq!(
            (u32::from(e.hot_x), u32::from(e.hot_y)),
            (200 / scale, 100 / scale)
        );
    }
}
