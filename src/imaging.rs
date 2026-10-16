//! Streaming encoders: rows come from the scanner one at a time and leave as
//! JPEG/PNG/PDF bytes without holding the page in memory (pages of unknown
//! length are the exception: JPEG and PNG need the height up front).

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocFormat {
    Jpeg,
    Png,
    Pdf,
}

impl DocFormat {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Pdf => "application/pdf",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime.trim().to_ascii_lowercase().as_str() {
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "application/pdf" => Some(Self::Pdf),
            _ => None,
        }
    }
}

/// A page delivered row by row. Rows are 8-bit, `channels` bytes per pixel.
pub trait RowSource {
    fn width(&self) -> usize;
    fn height(&self) -> Option<usize>;
    fn channels(&self) -> usize;
    /// Fill `row`; `Ok(false)` at the end of the page.
    fn next_row(&mut self, row: &mut [u8]) -> io::Result<bool>;
}

/// A fully buffered page (used when the backend does not know the height).
pub struct Buffered {
    width: usize,
    channels: usize,
    data: Vec<u8>,
    next: usize,
}

impl Buffered {
    pub fn drain(src: &mut dyn RowSource) -> io::Result<Self> {
        let width = src.width();
        let channels = src.channels();
        let stride = width * channels;
        let mut data = Vec::new();
        let mut row = vec![0u8; stride];
        while src.next_row(&mut row)? {
            data.extend_from_slice(&row);
        }
        Ok(Self {
            width,
            channels,
            data,
            next: 0,
        })
    }
}

impl RowSource for Buffered {
    fn width(&self) -> usize {
        self.width
    }
    fn height(&self) -> Option<usize> {
        Some(self.data.len() / (self.width * self.channels).max(1))
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn next_row(&mut self, row: &mut [u8]) -> io::Result<bool> {
        let stride = self.width * self.channels;
        if self.next + stride > self.data.len() {
            return Ok(false);
        }
        row.copy_from_slice(&self.data[self.next..self.next + stride]);
        self.next += stride;
        Ok(true)
    }
}

pub const JPEG_QUALITY: u8 = 90;

pub fn encode(
    format: DocFormat,
    src: &mut dyn RowSource,
    dpi: u32,
    out: &mut dyn Write,
) -> io::Result<()> {
    if src.height().is_none() {
        let mut page = Buffered::drain(src)?;
        return encode(format, &mut page, dpi, out);
    }
    match format {
        DocFormat::Jpeg => encode_jpeg(src, dpi, out),
        DocFormat::Png => encode_png(src, dpi, out),
        DocFormat::Pdf => encode_pdf(src, dpi, out),
    }
}

fn dims(src: &dyn RowSource) -> io::Result<(u16, u16)> {
    let h = src.height().unwrap_or(0);
    let w = src.width();
    match (u16::try_from(w), u16::try_from(h)) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("page {w}x{h} exceeds JPEG limits"),
        )),
    }
}

/// Remembers the first write error, so a failure inside the C encoder (which
/// unwinds) still surfaces as the original `io::Error` (e.g. BrokenPipe).
struct ErrorKeeping<'a> {
    out: &'a mut dyn Write,
    error: Option<io::Error>,
}

impl Write for ErrorKeeping<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf).inspect_err(|e| {
            self.error
                .get_or_insert_with(|| io::Error::new(e.kind(), e.to_string()));
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush().inspect_err(|e| {
            self.error
                .get_or_insert_with(|| io::Error::new(e.kind(), e.to_string()));
        })
    }
}

/// libjpeg-turbo (through mozjpeg's "fastest" profile, which is plain
/// libjpeg-turbo behaviour) fed one scanline at a time: the same encoder
/// AirSane uses, so throughput matches it, and the page is never buffered.
fn encode_jpeg(src: &mut dyn RowSource, dpi: u32, out: &mut dyn Write) -> io::Result<()> {
    let (width, height) = dims(src)?;
    let channels = src.channels();
    let mut sink = ErrorKeeping { out, error: None };
    let mut row = vec![0u8; usize::from(width) * channels];
    let density = u16::try_from(dpi).unwrap_or(u16::MAX);
    // mozjpeg reports libjpeg errors (including a failing writer) by
    // unwinding; catch it here and turn it back into an io::Error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
        let space = if channels == 1 {
            mozjpeg::ColorSpace::JCS_GRAYSCALE
        } else {
            mozjpeg::ColorSpace::JCS_RGB
        };
        let mut comp = mozjpeg::Compress::new(space);
        comp.set_fastest_defaults();
        comp.set_size(usize::from(width), usize::from(height));
        comp.set_quality(f32::from(JPEG_QUALITY));
        comp.set_pixel_density(mozjpeg::PixelDensity {
            unit: mozjpeg::PixelDensityUnit::Inches,
            x: density,
            y: density,
        });
        let mut started = comp.start_compress(&mut sink)?;
        for _ in 0..height {
            if !src.next_row(&mut row)? {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "page shorter than announced",
                ));
            }
            started.write_scanlines(&row)?;
        }
        started.finish()?;
        Ok(())
    }));
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(sink.error.take().unwrap_or(e)),
        Err(_) => Err(sink
            .error
            .take()
            .unwrap_or_else(|| io::Error::other("JPEG encoder failed"))),
    }
}

fn encode_png(src: &mut dyn RowSource, dpi: u32, out: &mut dyn Write) -> io::Result<()> {
    let height = src.height().unwrap_or(0);
    let width = src.width();
    let channels = src.channels();
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page too large",
        ));
    };
    let mut enc = png::Encoder::new(out, w, h);
    enc.set_color(if channels == 1 {
        png::ColorType::Grayscale
    } else {
        png::ColorType::Rgb
    });
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let ppm = (f64::from(dpi) / 0.0254).round() as u32;
    enc.set_pixel_dims(Some(png::PixelDimensions {
        xppu: ppm,
        yppu: ppm,
        unit: png::Unit::Meter,
    }));
    let mut writer = enc.write_header().map_err(io::Error::other)?;
    let mut stream = writer.stream_writer().map_err(io::Error::other)?;
    let mut row = vec![0u8; width * channels];
    let mut rows = 0;
    while rows < height && src.next_row(&mut row)? {
        stream.write_all(&row)?;
        rows += 1;
    }
    if rows < height {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "page shorter than announced",
        ));
    }
    stream.finish().map_err(io::Error::other)
}

/// Counts bytes so the PDF cross-reference table can point at each object.
struct Counting<'a> {
    out: &'a mut dyn Write,
    n: usize,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.n += n;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// One page, the JPEG stream embedded as a DCTDecode image; its length is an
/// indirect object written after the stream so nothing is buffered.
fn encode_pdf(src: &mut dyn RowSource, dpi: u32, out: &mut dyn Write) -> io::Result<()> {
    let (width, height) = dims(src)?;
    let gray = src.channels() == 1;
    let pt = |px: u16| f64::from(px) * 72.0 / f64::from(dpi.max(1));
    let (pw, ph) = (pt(width), pt(height));
    let mut w = Counting { out, n: 0 };
    let mut offsets = [0usize; 7];
    w.write_all(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")?;
    offsets[1] = w.n;
    w.write_all(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n")?;
    offsets[2] = w.n;
    w.write_all(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n")?;
    offsets[3] = w.n;
    write!(
        w,
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 6 0 R >>\nendobj\n"
    )?;
    offsets[4] = w.n;
    write!(
        w,
        "4 0 obj\n<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /{} /BitsPerComponent 8 /Filter /DCTDecode /Length 5 0 R >>\nstream\n",
        if gray { "DeviceGray" } else { "DeviceRGB" }
    )?;
    let start = w.n;
    encode_jpeg(src, dpi, &mut w)?;
    let len = w.n - start;
    w.write_all(b"\nendstream\nendobj\n")?;
    offsets[5] = w.n;
    write!(w, "5 0 obj\n{len}\nendobj\n")?;
    let content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q");
    offsets[6] = w.n;
    write!(
        w,
        "6 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
        content.len()
    )?;
    let xref = w.n;
    w.write_all(b"xref\n0 7\n0000000000 65535 f \n")?;
    for off in &offsets[1..] {
        writeln!(w, "{off:010} 00000 n ")?;
    }
    write!(
        w,
        "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )?;
    w.flush()
}

/// Gamma lookup table; 1.0 is the identity.
pub fn gamma_table(gamma: f64) -> [u8; 256] {
    let mut t = [0u8; 256];
    for (i, v) in t.iter_mut().enumerate() {
        *v = if (gamma - 1.0).abs() < f64::EPSILON {
            i as u8
        } else {
            (255.0 * (i as f64 / 255.0).powf(gamma))
                .round()
                .clamp(0.0, 255.0) as u8
        };
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gradient {
        w: usize,
        h: usize,
        c: usize,
        y: usize,
    }

    impl RowSource for Gradient {
        fn width(&self) -> usize {
            self.w
        }
        fn height(&self) -> Option<usize> {
            Some(self.h)
        }
        fn channels(&self) -> usize {
            self.c
        }
        fn next_row(&mut self, row: &mut [u8]) -> io::Result<bool> {
            if self.y == self.h {
                return Ok(false);
            }
            for (i, b) in row.iter_mut().enumerate() {
                *b = ((i + self.y) % 256) as u8;
            }
            self.y += 1;
            Ok(true)
        }
    }

    #[test]
    fn pdf_xref_offsets_point_at_objects() {
        let mut out = Vec::new();
        encode(
            DocFormat::Pdf,
            &mut Gradient {
                w: 33,
                h: 17,
                c: 3,
                y: 0,
            },
            300,
            &mut out,
        )
        .unwrap();
        // Offsets are byte positions: never index a lossily decoded string.
        let tail_at = out.windows(10).rposition(|w| w == b"startxref\n").unwrap() + 10;
        let tail = std::str::from_utf8(&out[tail_at..]).unwrap();
        let xref_at: usize = tail.lines().next().unwrap().parse().unwrap();
        assert!(out[xref_at..].starts_with(b"xref"));
        let table = std::str::from_utf8(&out[xref_at..]).unwrap();
        let entries: Vec<usize> = table
            .lines()
            .skip(3)
            .take(6)
            .map(|l| l[..10].parse().unwrap())
            .collect();
        for (i, off) in entries.iter().enumerate() {
            assert!(
                out[*off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()),
                "object {}",
                i + 1
            );
        }
    }

    #[test]
    fn short_page_is_an_error_not_padding() {
        struct Short(usize);
        impl RowSource for Short {
            fn width(&self) -> usize {
                16
            }
            fn height(&self) -> Option<usize> {
                Some(64)
            }
            fn channels(&self) -> usize {
                1
            }
            fn next_row(&mut self, _row: &mut [u8]) -> io::Result<bool> {
                self.0 += 1;
                Ok(self.0 < 10)
            }
        }
        for f in [DocFormat::Jpeg, DocFormat::Png] {
            assert!(
                encode(f, &mut Short(0), 150, &mut Vec::new()).is_err(),
                "{f:?}"
            );
        }
    }
}
