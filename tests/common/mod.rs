//! Test harness: a full airbridge instance on loopback with a fake SANE
//! backend, a fake mDNS network and fake interface addresses. Nothing here
//! touches the LAN; `scripts/netns-runner.sh` (the cargo test runner) runs
//! every test binary in an empty network namespace to enforce that.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airbridge::app::{App, Deps};
use airbridge::backend::fake::{FakeBackend, FakeDevice};
use airbridge::config::Config;
use airbridge::discovery::brscan::Brscan;
use airbridge::discovery::cups::Spooler;
use airbridge::mdns::fake::{FakeMdns, FakeNet};
use airbridge::net::addrs::{FakeAddrs, IfAddr};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use tokio::net::TcpSocket;

/// The "LAN" address of the test host (a loopback alias, no configuration needed).
pub const LAN_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 2, 1);
/// A client on the LAN.
pub const LAN_CLIENT: Ipv4Addr = Ipv4Addr::new(127, 0, 2, 50);
/// A client on a container bridge that must be refused.
pub const DOCKER_CLIENT: Ipv4Addr = Ipv4Addr::new(127, 0, 3, 50);

pub fn lan(ip: Ipv4Addr) -> IfAddr {
    IfAddr {
        iface: "lan0".into(),
        ip: IpAddr::V4(ip),
        prefix: 24,
        index: 2,
    }
}

pub fn docker() -> IfAddr {
    IfAddr {
        iface: "docker0".into(),
        ip: IpAddr::V4(Ipv4Addr::new(127, 0, 3, 1)),
        prefix: 24,
        index: 3,
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap()
}

pub struct Harness {
    pub app: App,
    pub port: u16,
    pub backend: FakeBackend,
    pub net: FakeNet,
    pub mdns: Arc<FakeMdns>,
    pub addrs: Arc<FakeAddrs>,
}

pub struct Opts {
    pub devices: Vec<FakeDevice>,
    pub brscan: Option<Arc<dyn Brscan>>,
    pub spooler: Option<Arc<dyn Spooler>>,
    pub net: FakeNet,
    pub port: u16,
    pub tweak: Box<dyn FnOnce(&mut Config)>,
    pub wait_for_scanners: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            devices: vec![FakeDevice::brother_dcp_l2540dw("fake:dell", "DellE514dw")],
            brscan: None,
            spooler: None,
            net: FakeNet::new(),
            port: free_port(),
            tweak: Box::new(|_| {}),
            wait_for_scanners: 1,
        }
    }
}

pub fn test_config(port: u16) -> Config {
    let mut cfg = Config::defaults("testhost");
    cfg.port = port;
    cfg.allow_loopback = false;
    cfg.jobs.hang_timeout = Duration::from_millis(400);
    cfg.jobs.wedge_grace = Duration::from_millis(400);
    cfg.jobs.pending_timeout = Duration::from_secs(5);
    cfg.limits.header_timeout = Duration::from_millis(300);
    cfg.body_timeout = Duration::from_millis(300);
    cfg
}

pub async fn start(opts: Opts) -> Harness {
    let backend = FakeBackend::new(opts.devices);
    let mdns = opts.net.host("airbridge-testhost");
    let addrs = FakeAddrs::new(vec![lan(LAN_IP), docker()], vec!["lan0".into()]);
    let mut cfg = test_config(opts.port);
    (opts.tweak)(&mut cfg);
    let deps = Deps {
        backend: Arc::new(backend.clone()),
        addrs: addrs.clone(),
        mdns: mdns.clone(),
        brscan: opts.brscan,
        spooler: opts.spooler,
    };
    let app = App::start_with(cfg, deps, Duration::from_millis(10))
        .await
        .unwrap();
    let h = Harness {
        port: app.port(),
        app,
        backend,
        net: opts.net,
        mdns,
        addrs,
    };
    let want = opts.wait_for_scanners;
    wait_until(Duration::from_secs(3), || h.app.scanners().len() >= want).await;
    h
}

pub async fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < limit,
            "condition not met within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A raw TCP connection from `from` (for tests that misbehave on purpose).
pub async fn connect_from(from: Ipv4Addr, to: SocketAddr) -> tokio::net::TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(IpAddr::V4(from), 0)).unwrap();
    socket.connect(to).await.unwrap()
}

/// One HTTP/1.1 request from `from` to `to`.
pub async fn request_from(
    from: Ipv4Addr,
    to: SocketAddr,
    method: Method,
    path: &str,
    body: Option<&str>,
) -> Resp {
    let stream = connect_from(from, to).await;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", format!("{to}"))
        .header("content-type", "text/xml")
        .body(Full::new(Bytes::from(body.unwrap_or_default().to_owned())))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    Resp {
        status,
        headers,
        body,
    }
}

impl Harness {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(LAN_IP), self.port)
    }

    pub async fn get(&self, path: &str) -> Resp {
        request_from(LAN_CLIENT, self.addr(), Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: &str) -> Resp {
        request_from(LAN_CLIENT, self.addr(), Method::POST, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Resp {
        request_from(LAN_CLIENT, self.addr(), Method::DELETE, path, None).await
    }

    /// POST a ticket and return the job path (from the Location header).
    pub async fn create_job(&self, ticket: &str) -> String {
        let r = self.post("/eSCL/ScanJobs", ticket).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        r.headers
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }
}

pub struct Ticket<'a> {
    pub source: &'a str,
    pub color: &'a str,
    pub resolution: u32,
    pub format: &'a str,
    /// x, y, width, height in 1/300 inch.
    pub region: Option<(u32, u32, u32, u32)>,
}

impl Default for Ticket<'_> {
    fn default() -> Self {
        Self {
            source: "Platen",
            color: "RGB24",
            resolution: 150,
            format: "image/jpeg",
            region: Some((0, 0, 300, 450)),
        }
    }
}

impl Ticket<'_> {
    pub fn xml(&self) -> String {
        let region = self.region.map_or_else(String::new, |(x, y, w, h)| {
            format!(
                "<pwg:ScanRegions><pwg:ScanRegion><pwg:Height>{h}</pwg:Height><pwg:Width>{w}</pwg:Width><pwg:XOffset>{x}</pwg:XOffset><pwg:YOffset>{y}</pwg:YOffset><pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits></pwg:ScanRegion></pwg:ScanRegions>"
            )
        });
        format!(
            "<?xml version='1.0' encoding='UTF-8'?><scan:ScanSettings xmlns:scan='http://schemas.hp.com/imaging/escl/2011/05/03' xmlns:pwg='http://www.pwg.org/schemas/2010/12/sm'><pwg:Version>2.0</pwg:Version>{region}<pwg:InputSource>{}</pwg:InputSource><scan:ColorMode>{}</scan:ColorMode><scan:XResolution>{r}</scan:XResolution><scan:YResolution>{r}</scan:YResolution><pwg:DocumentFormat>{}</pwg:DocumentFormat></scan:ScanSettings>",
            self.source,
            self.color,
            self.format,
            r = self.resolution
        )
    }
}

/// Decoded image: (width, height, channels, bit depth).
pub fn decode(mime: &str, body: &[u8]) -> (usize, usize, usize, u8) {
    match mime {
        "image/jpeg" => {
            let mut d = jpeg_decoder::Decoder::new(body);
            d.decode().unwrap();
            let info = d.info().unwrap();
            let channels = match info.pixel_format {
                jpeg_decoder::PixelFormat::L8 => 1,
                jpeg_decoder::PixelFormat::RGB24 => 3,
                other => panic!("unexpected {other:?}"),
            };
            (
                usize::from(info.width),
                usize::from(info.height),
                channels,
                8,
            )
        }
        "image/png" => {
            let decoder = png::Decoder::new(std::io::Cursor::new(body));
            let mut reader = decoder.read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut buf).unwrap();
            let channels = match info.color_type {
                png::ColorType::Grayscale => 1,
                png::ColorType::Rgb => 3,
                other => panic!("unexpected {other:?}"),
            };
            (
                info.width as usize,
                info.height as usize,
                channels,
                info.bit_depth as u8,
            )
        }
        other => panic!("cannot decode {other}"),
    }
}

/// Distinct sample values in an 8-bit gray PNG (to tell real gray from 1-bit).
pub fn png_gray_levels(body: &[u8]) -> usize {
    let decoder = png::Decoder::new(std::io::Cursor::new(body));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let mut seen = [false; 256];
    for b in &buf[..info.buffer_size()] {
        seen[usize::from(*b)] = true;
    }
    seen.iter().filter(|s| **s).count()
}
