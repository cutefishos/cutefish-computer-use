// The desktop as one image: every output scaled into the logical layout, then
// down to a size a model reads whole.

// Anthropic's guidance for screenshots: the long edge and the pixel count a
// model sees without scaling the image itself.
const MAX_EDGE: f64 = 1568.0;
const MAX_PIXELS: f64 = 1_150_000.0;
// Claude Desktop refuses a tool result over 1 MB, and base64 adds a third.
const MAX_BYTES: usize = 700_000;
const JPEG_QUALITIES: [u8; 4] = [85, 70, 55, 40];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

// Maps screenshot pixels to the compositor's global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub bounds: Rect,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

impl Layout {
    pub fn of(outputs: &[Rect]) -> Option<Layout> {
        let usable: Vec<&Rect> = outputs.iter().filter(|r| r.width > 0 && r.height > 0).collect();
        let left = usable.iter().map(|r| r.x).min()?;
        let top = usable.iter().map(|r| r.y).min()?;
        let right = usable.iter().map(|r| r.x + r.width).max()?;
        let bottom = usable.iter().map(|r| r.y + r.height).max()?;
        let bounds = Rect { x: left, y: top, width: right - left, height: bottom - top };
        let (w, h) = (bounds.width as f64, bounds.height as f64);
        let scale = 1f64.min(MAX_EDGE / w.max(h)).min((MAX_PIXELS / (w * h)).sqrt());
        Some(Layout {
            bounds,
            scale,
            width: ((w * scale).round() as u32).max(1),
            height: ((h * scale).round() as u32).max(1),
        })
    }

    // The centre of a screenshot pixel, in logical coordinates.
    pub fn to_logical(&self, x: f64, y: f64) -> (f64, f64) {
        let x = x.clamp(0.0, self.width as f64 - 1.0);
        let y = y.clamp(0.0, self.height as f64 - 1.0);
        (
            self.bounds.x as f64 + (x + 0.5) / self.scale,
            self.bounds.y as f64 + (y + 0.5) / self.scale,
        )
    }

    fn to_image(&self, rect: &Rect) -> (usize, usize, usize, usize) {
        let edge = |value: i32, origin: i32, limit: u32| {
            ((((value - origin) as f64) * self.scale).round().max(0.0) as usize).min(limit as usize)
        };
        (
            edge(rect.x, self.bounds.x, self.width),
            edge(rect.y, self.bounds.y, self.height),
            edge(rect.x + rect.width, self.bounds.x, self.width),
            edge(rect.y + rect.height, self.bounds.y, self.height),
        )
    }
}

// Pixels of one captured output: rows of 32-bit XRGB in memory order B, G, R, X.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub data: Vec<u8>,
}

// Area weights: each target pixel averages the source span it covers.
fn weights(source: usize, target: usize) -> Vec<(usize, Vec<f32>)> {
    let ratio = source as f64 / target as f64;
    (0..target)
        .map(|i| {
            let start = i as f64 * ratio;
            let end = (i + 1) as f64 * ratio;
            let first = start.floor() as usize;
            let last = (end.ceil() as usize).min(source);
            let span = end - start;
            let weights = (first..last)
                .map(|j| ((end.min(j as f64 + 1.0) - start.max(j as f64)) / span) as f32)
                .collect();
            (first, weights)
        })
        .collect()
}

// Draws `frame` into `rect` of an RGB canvas the size of `layout`.
pub fn draw(canvas: &mut [u8], layout: &Layout, rect: &Rect, frame: &Frame) {
    let (x0, y0, x1, y1) = layout.to_image(rect);
    let (width, height) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if width == 0 || height == 0 || frame.width == 0 || frame.height == 0 {
        return;
    }
    let columns = weights(frame.width, width);
    let rows = weights(frame.height, height);

    // Horizontal pass into floats, then vertical into the canvas.
    let mut narrowed = vec![0f32; frame.height * width * 3];
    for y in 0..frame.height {
        let line = &frame.data[y * frame.stride..];
        for (i, (first, column)) in columns.iter().enumerate() {
            let mut sum = [0f32; 3];
            for (k, weight) in column.iter().enumerate() {
                let pixel = &line[(first + k) * 4..];
                sum[0] += weight * pixel[2] as f32;
                sum[1] += weight * pixel[1] as f32;
                sum[2] += weight * pixel[0] as f32;
            }
            narrowed[(y * width + i) * 3..][..3].copy_from_slice(&sum);
        }
    }
    let canvas_width = layout.width as usize;
    for (j, (first, row)) in rows.iter().enumerate() {
        let out = &mut canvas[((y0 + j) * canvas_width + x0) * 3..][..width * 3];
        for i in 0..width {
            let mut sum = [0f32; 3];
            for (k, weight) in row.iter().enumerate() {
                let pixel = &narrowed[((first + k) * width + i) * 3..];
                for c in 0..3 {
                    sum[c] += weight * pixel[c];
                }
            }
            for c in 0..3 {
                out[i * 3 + c] = sum[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

// PNG keeps text exact, but a photo wallpaper makes it too big; JPEG fits then.
pub fn encode(rgb: &[u8], width: u32, height: u32) -> Result<(Vec<u8>, &'static str), String> {
    let png = encode_png(rgb, width, height)?;
    if png.len() <= MAX_BYTES {
        return Ok((png, "image/png"));
    }
    let mut jpeg = Vec::new();
    for quality in JPEG_QUALITIES {
        jpeg = encode_jpeg(rgb, width, height, quality)?;
        if jpeg.len() <= MAX_BYTES {
            break;
        }
    }
    Ok((jpeg, "image/jpeg"))
}

fn encode_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_desktop_keeps_its_logical_size() {
        let layout = Layout::of(&[Rect { x: 0, y: 0, width: 1280, height: 800 }]).unwrap();
        assert_eq!((layout.width, layout.height), (1280, 800));
        assert_eq!(layout.to_logical(10.0, 20.0), (10.5, 20.5));
    }

    #[test]
    fn a_large_desktop_is_scaled_to_the_pixel_budget() {
        let outputs = [
            Rect { x: 0, y: 0, width: 1920, height: 1080 },
            Rect { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let layout = Layout::of(&outputs).unwrap();
        assert!(layout.width <= 1568);
        assert!((layout.width * layout.height) as f64 <= MAX_PIXELS * 1.01);
        let (x, _) = layout.to_logical(layout.width as f64 - 1.0, 0.0);
        assert!(x < 3840.0 && x > 3835.0);
    }

    #[test]
    fn downscaling_averages_the_covered_pixels() {
        let layout = Layout::of(&[Rect { x: 0, y: 0, width: 1, height: 1 }]).unwrap();
        // Two pixels wide at scale 2: a white one and a black one make grey.
        let frame = Frame {
            width: 2,
            height: 2,
            stride: 8,
            data: vec![255, 255, 255, 0, 0, 0, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0],
        };
        let mut canvas = vec![0u8; 3];
        draw(&mut canvas, &layout, &Rect { x: 0, y: 0, width: 1, height: 1 }, &frame);
        assert_eq!(canvas, vec![128, 128, 128]);
    }

    #[test]
    fn a_busy_desktop_falls_back_to_jpeg_within_the_budget() {
        let (width, height) = (1568u32, 733u32);
        let flat = vec![200u8; (width * height * 3) as usize];
        assert_eq!(encode(&flat, width, height).unwrap().1, "image/png");

        // A gradient with noise on it, like a photo.
        let mut seed = 1u32;
        let busy: Vec<u8> = (0..width * height * 3)
            .map(|i| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((i / 3 % width) * 200 / width + (seed >> 16) % 32) as u8
            })
            .collect();
        let (data, mime_type) = encode(&busy, width, height).unwrap();
        assert_eq!(mime_type, "image/jpeg");
        assert!(data.len() <= MAX_BYTES);
    }
}
