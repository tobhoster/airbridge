//! Performance baselines. CI runs these on the base and head commits and
//! fails on a >20% regression (`scripts/bench-gate.sh`).
//!
//! - `capabilities_http`: full HTTP round trip for ScannerCapabilities
//!   (the request macOS repeats most); p99 must stay under 5 ms.
//! - `jpeg_600dpi_letter_rgb`: encode a 600 dpi US Letter RGB page
//!   (5100x6600) through the scan pipeline's JPEG encoder. AirSane's encoder
//!   (libjpeg-turbo, quality 90) on the same input is the reference, see
//!   `scripts/airsane-jpeg-baseline.sh`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airbridge::app::{App, Deps};
use airbridge::backend::fake::FakeBackend;
use airbridge::config::Config;
use airbridge::imaging::{self, DocFormat, RowSource};
use airbridge::mdns::fake::FakeNet;
use airbridge::net::addrs::{FakeAddrs, IfAddr};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LETTER_600: (usize, usize) = (5100, 6600);

/// The same deterministic content the fake scanner produces.
struct Page {
    w: usize,
    h: usize,
    y: usize,
}

impl RowSource for Page {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> Option<usize> {
        Some(self.h)
    }
    fn channels(&self) -> usize {
        3
    }
    fn next_row(&mut self, row: &mut [u8]) -> io::Result<bool> {
        if self.y == self.h {
            return Ok(false);
        }
        for (i, px) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let base = (i * 3 + self.y * 5) % 256;
            px[0] = base as u8;
            px[1] = ((base + 85) % 256) as u8;
            px[2] = ((base + 170) % 256) as u8;
        }
        self.y += 1;
        Ok(true)
    }
}

fn jpeg(c: &mut Criterion) {
    let (w, h) = LETTER_600;
    let mut g = c.benchmark_group("encode");
    g.sample_size(10);
    g.throughput(Throughput::Elements((w * h) as u64));
    g.bench_function("jpeg_600dpi_letter_rgb", |b| {
        let mut out = Vec::with_capacity(8 << 20);
        b.iter(|| {
            out.clear();
            imaging::encode(DocFormat::Jpeg, &mut Page { w, h, y: 0 }, 600, &mut out).unwrap();
            out.len()
        });
    });
    g.finish();
}

async fn get(addr: SocketAddr, buf: &mut Vec<u8>) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(
        b"GET /eSCL/ScannerCapabilities HTTP/1.1\r\nHost: bench\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    buf.clear();
    s.read_to_end(buf).await.unwrap();
    assert!(buf.starts_with(b"HTTP/1.1 200"));
}

fn capabilities(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let ip = Ipv4Addr::new(127, 0, 2, 1);
    let app: App = rt.block_on(async {
        let mut cfg = Config::defaults("bench");
        cfg.port = port;
        let deps = Deps {
            backend: Arc::new(FakeBackend::demo()),
            addrs: FakeAddrs::new(
                vec![IfAddr {
                    iface: "lan0".into(),
                    ip: IpAddr::V4(ip),
                    prefix: 24,
                    index: 2,
                }],
                vec!["lan0".into()],
            ),
            mdns: FakeNet::new().host("airbridge-bench"),
            brscan: None,
            spooler: None,
        };
        let app = App::start_with(cfg, deps, Duration::from_millis(1))
            .await
            .unwrap();
        while app.scanners().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        app
    });
    let addr = SocketAddr::new(IpAddr::V4(ip), port);

    // p99 gate, measured outside criterion's statistics.
    let mut lat = rt.block_on(async {
        let mut buf = Vec::new();
        let mut lat = Vec::with_capacity(2000);
        for _ in 0..2000 {
            let t = Instant::now();
            get(addr, &mut buf).await;
            lat.push(t.elapsed());
        }
        lat
    });
    lat.sort();
    let p99 = lat[lat.len() * 99 / 100];
    eprintln!("capabilities p50 {:?} p99 {:?}", lat[lat.len() / 2], p99);
    assert!(
        p99 < Duration::from_millis(5),
        "capabilities p99 {p99:?} >= 5 ms"
    );

    c.bench_function("capabilities_http", |b| {
        let mut buf = Vec::new();
        b.to_async(&rt).iter(|| {
            let mut local = std::mem::take(&mut buf);
            async move {
                get(addr, &mut local).await;
                local.len()
            }
        });
    });
    rt.block_on(app.shutdown());
}

criterion_group!(benches, jpeg, capabilities);
criterion_main!(benches);
