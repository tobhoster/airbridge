//! The scanner icon macOS shows in Image Capture: a flatbed drawn at startup
//! (128x128 RGBA PNG; macOS only accepts power-of-two sizes).

pub fn scanner_png() -> Vec<u8> {
    const N: usize = 128;
    let mut px = vec![0u8; N * N * 4];
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, rgba: [u8; 4], radius: usize| {
        for y in y0..y1 {
            for x in x0..x1 {
                // Round the corners.
                let dx = if x < x0 + radius {
                    x0 + radius - x
                } else if x + radius >= x1 {
                    x + radius + 1 - x1
                } else {
                    0
                };
                let dy = if y < y0 + radius {
                    y0 + radius - y
                } else if y + radius >= y1 {
                    y + radius + 1 - y1
                } else {
                    0
                };
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                px[(y * N + x) * 4..(y * N + x) * 4 + 4].copy_from_slice(&rgba);
            }
        }
    };
    fill(8, 52, 120, 108, [0x5f, 0x63, 0x68, 0xff], 10); // body
    fill(8, 40, 120, 60, [0x8a, 0x8f, 0x96, 0xff], 8); // lid
    fill(18, 64, 110, 72, [0x7c, 0xd4, 0xff, 0xff], 3); // light bar
    fill(18, 88, 34, 96, [0x9c, 0xe0, 0x7a, 0xff], 3); // status LED
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, N as u32, N as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let written = enc.write_header().and_then(|mut w| w.write_image_data(&px));
    if let Err(e) = written {
        // Writing to a Vec cannot fail for I/O reasons; an empty icon only
        // means macOS shows its generic scanner glyph.
        tracing::warn!(error = %e, "icon encoding failed");
        out.clear();
    }
    out
}
