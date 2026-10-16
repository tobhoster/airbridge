//! The two ends of a page transfer: `SaneRows` turns SANE lines into 8-bit
//! rows, `ChannelSink` hands encoded bytes to the async HTTP body.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::backend::{BackendError, FrameFormat, FrameParams, Session};
use crate::imaging::{self, RowSource};
use crate::scanner::ColorMode;

pub const CHUNK: usize = 64 * 1024;

fn stamp(progress: &AtomicU64, epoch: Instant) {
    progress.store(
        u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX),
        Ordering::SeqCst,
    );
}

/// Bridges the blocking encoder to the async HTTP body. A client that stops
/// reading for `timeout` counts as gone.
pub struct ChannelSink<'a> {
    pub tx: mpsc::Sender<io::Result<Bytes>>,
    pub buf: Vec<u8>,
    pub runtime: &'a tokio::runtime::Handle,
    pub timeout: Duration,
    pub closed: bool,
    pub progress: &'a AtomicU64,
    pub epoch: Instant,
}

impl ChannelSink<'_> {
    fn send(&mut self, item: io::Result<Bytes>) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "client gone"));
        }
        let sent = self
            .runtime
            .block_on(async { tokio::time::timeout(self.timeout, self.tx.send(item)).await });
        if let Ok(Ok(())) = sent {
            stamp(self.progress, self.epoch);
            Ok(())
        } else {
            self.closed = true;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "client gone"))
        }
    }

    /// Terminate the HTTP body with an error so the client sees a broken
    /// transfer rather than a silently truncated image.
    pub fn fail(&mut self, msg: &str) {
        // Best effort: when the client is already gone nobody is left to tell.
        let _ = self.send(Err(io::Error::other(msg.to_owned())));
    }
}

impl io::Write for ChannelSink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
            self.send(Ok(chunk))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = Bytes::from(std::mem::take(&mut self.buf));
            self.send(Ok(chunk))?;
        }
        Ok(())
    }
}

/// Reads SANE lines and converts them to 8-bit rows in the requested mode:
/// 1-bit data (some "gray" modes) expands to 0/255, 16-bit keeps the high
/// byte, RGB<->gray conversion happens here, then the gamma table applies.
pub struct SaneRows<'a> {
    session: &'a mut dyn Session,
    params: FrameParams,
    want: ColorMode,
    lut: [u8; 256],
    raw: Vec<u8>,
    lines: usize,
    progress: &'a AtomicU64,
    epoch: Instant,
    pub error: Option<BackendError>,
}

impl<'a> SaneRows<'a> {
    pub fn new(
        session: &'a mut dyn Session,
        params: FrameParams,
        want: ColorMode,
        gamma: f64,
        progress: &'a AtomicU64,
        epoch: Instant,
    ) -> Self {
        if params.format == FrameFormat::Gray && params.depth == 1 && want == ColorMode::Gray8 {
            tracing::info!("backend delivered 1-bit data for a gray scan; expanding to 8-bit");
        }
        Self {
            raw: vec![0; params.bytes_per_line],
            session,
            params,
            want,
            lut: imaging::gamma_table(gamma),
            lines: 0,
            progress,
            epoch,
            error: None,
        }
    }

    fn sample(&self, x: usize, c: usize) -> u8 {
        let p = self.params;
        let channels = if p.format == FrameFormat::Rgb { 3 } else { 1 };
        let i = x * channels + c;
        match p.depth {
            1 => {
                let bit = (self.raw[i / 8] >> (7 - i % 8)) & 1;
                if bit == 1 { 0 } else { 255 }
            }
            16 => u16::from_ne_bytes([self.raw[2 * i], self.raw[2 * i + 1]]).to_be_bytes()[0],
            _ => self.raw[i],
        }
    }
}

impl RowSource for SaneRows<'_> {
    fn width(&self) -> usize {
        self.params.pixels_per_line
    }
    fn height(&self) -> Option<usize> {
        self.params.lines
    }
    fn channels(&self) -> usize {
        self.want.channels()
    }

    fn next_row(&mut self, row: &mut [u8]) -> io::Result<bool> {
        if self.params.lines.is_some_and(|l| self.lines >= l) {
            return Ok(false);
        }
        let mut filled = 0;
        while filled < self.raw.len() {
            match self.session.read(&mut self.raw[filled..]) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    filled += n;
                    stamp(self.progress, self.epoch);
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.error = Some(e);
                    return Err(io::Error::other(msg));
                }
            }
        }
        let rgb_in = self.params.format == FrameFormat::Rgb;
        for x in 0..self.params.pixels_per_line {
            match (self.want, rgb_in) {
                (ColorMode::Gray8, false) => row[x] = self.lut[usize::from(self.sample(x, 0))],
                (ColorMode::Gray8, true) => {
                    let (r, g, b) = (
                        u32::from(self.sample(x, 0)),
                        u32::from(self.sample(x, 1)),
                        u32::from(self.sample(x, 2)),
                    );
                    let y = (54 * r + 183 * g + 19 * b) >> 8;
                    row[x] = self.lut[y.min(255) as usize];
                }
                (ColorMode::Rgb24, true) => {
                    for c in 0..3 {
                        row[3 * x + c] = self.lut[usize::from(self.sample(x, c))];
                    }
                }
                (ColorMode::Rgb24, false) => {
                    let v = self.lut[usize::from(self.sample(x, 0))];
                    row[3 * x..3 * x + 3].fill(v);
                }
            }
        }
        self.lines += 1;
        Ok(true)
    }
}
