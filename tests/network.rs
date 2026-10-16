//! Addresses, access control, hostile clients and restarts.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use airbridge::app::{App, Deps};
use airbridge::backend::fake::FakeBackend;
use airbridge::mdns::fake::FakeNet;
use airbridge::net::addrs::FakeAddrs;
use common::{
    DOCKER_CLIENT, LAN_CLIENT, LAN_IP, Opts, connect_from, docker, lan, request_from, start,
    wait_until,
};
use hyper::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NEW_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 4, 1);

fn uscan_addrs(net: &FakeNet) -> Vec<IpAddr> {
    net.published("_uscan._tcp")
        .into_iter()
        .flat_map(|(_, _, a)| a)
        .collect()
}

#[tokio::test]
async fn address_changes_move_listeners_and_announcements() {
    let h = start(Default::default()).await;
    let old = SocketAddr::new(IpAddr::V4(LAN_IP), h.port);
    let new = SocketAddr::new(IpAddr::V4(NEW_IP), h.port);
    assert_eq!(uscan_addrs(&h.net), vec![IpAddr::V4(LAN_IP)]);

    // DHCP hands out a second address: served and announced within moments.
    h.addrs.set(vec![lan(LAN_IP), lan(NEW_IP), docker()]);
    wait_until(Duration::from_secs(2), || h.app.bound().contains(&new)).await;
    assert_eq!(
        request_from(
            Ipv4Addr::new(127, 0, 4, 9),
            new,
            Method::GET,
            "/healthz",
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    wait_until(Duration::from_secs(2), || {
        uscan_addrs(&h.net).contains(&IpAddr::V4(NEW_IP))
    })
    .await;

    // The old address goes away: listener closed, withdrawn from mDNS.
    h.addrs.set(vec![lan(NEW_IP), docker()]);
    wait_until(Duration::from_secs(2), || !h.app.bound().contains(&old)).await;
    wait_until(Duration::from_secs(2), || {
        uscan_addrs(&h.net) == vec![IpAddr::V4(NEW_IP)]
    })
    .await;
    let refused = tokio::net::TcpStream::connect(old).await;
    assert!(
        refused.is_err(),
        "old address must no longer accept connections"
    );
    assert_eq!(
        h.net.published("_uscan._tcp").len(),
        1,
        "announced once, not twice"
    );
}

#[tokio::test]
async fn interface_without_addresses_then_back() {
    let h = start(Default::default()).await;
    h.addrs.set(vec![docker()]);
    wait_until(Duration::from_secs(2), || h.app.bound().is_empty()).await;
    wait_until(Duration::from_secs(2), || {
        h.net.published("_uscan._tcp").is_empty()
    })
    .await;
    h.addrs.set(vec![lan(LAN_IP), docker()]);
    wait_until(Duration::from_secs(2), || !h.app.bound().is_empty()).await;
    wait_until(Duration::from_secs(2), || {
        h.net.published("_uscan._tcp").len() == 1
    })
    .await;
    assert_eq!(h.get("/eSCL/ScannerStatus").await.status, StatusCode::OK);
}

#[tokio::test]
async fn access_control_follows_interfaces() {
    let h = start(Default::default()).await;
    let caps = "/eSCL/ScannerCapabilities";
    assert_eq!(
        request_from(LAN_CLIENT, h.addr(), Method::GET, caps, None)
            .await
            .status,
        StatusCode::OK
    );
    let denied = request_from(DOCKER_CLIENT, h.addr(), Method::GET, caps, None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    // A tailscale-style CGNAT-range peer is refused too (mapped onto loopback here).
    assert_eq!(
        request_from(
            Ipv4Addr::new(127, 100, 64, 1),
            h.addr(),
            Method::GET,
            caps,
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // The LAN interface now also carries the 127.0.3.0/24 network: allowed.
    h.addrs.set(vec![
        lan(LAN_IP),
        lan(Ipv4Addr::new(127, 0, 3, 2)),
        docker(),
    ]);
    wait_until(Duration::from_secs(2), || h.app.bound().len() == 2).await;
    assert_eq!(
        request_from(DOCKER_CLIENT, h.addr(), Method::GET, caps, None)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn slowloris_headers_are_cut_off() {
    let h = start(Default::default()).await;
    let mut s = connect_from(LAN_CLIENT, h.addr()).await;
    s.write_all(b"GET /eSCL/ScannerStatus HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let t0 = Instant::now();
    let mut buf = [0u8; 512];
    // Header timeout is 300 ms in the harness; the server must hang up.
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .expect("server kept the connection");
    assert!(n.is_ok());
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
}

#[tokio::test]
async fn slow_or_oversized_bodies_are_refused() {
    let h = start(Default::default()).await;
    // Body promised, never delivered.
    let mut s = connect_from(LAN_CLIENT, h.addr()).await;
    s.write_all(b"POST /eSCL/ScanJobs HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\n<scan:")
        .await
        .unwrap();
    let mut buf = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 408"),
        "{}",
        String::from_utf8_lossy(&buf[..n])
    );

    // 1 MiB of ScanSettings.
    let big = format!(
        "<scan:ScanSettings>{}</scan:ScanSettings>",
        "x".repeat(1 << 20)
    );
    let r = h.post("/eSCL/ScanJobs", &big).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn garbage_and_huge_headers_do_not_hurt_the_server() {
    let h = start(Default::default()).await;
    for junk in [
        b"\x00\x01\x02 not http\r\n\r\n".to_vec(),
        format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(200_000)).into_bytes(),
        b"GET /eSCL/ScanJobs/../../etc/passwd HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
    ] {
        let mut s = connect_from(LAN_CLIENT, h.addr()).await;
        // The server may close before reading everything; that is the point.
        let _ = s.write_all(&junk).await;
        let mut buf = vec![0u8; 256];
        let _ = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
    }
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
}

#[tokio::test]
async fn connection_flood_is_capped_and_recovers() {
    let h = start(Opts {
        tweak: Box::new(|c| c.limits.max_connections = 8),
        ..Default::default()
    })
    .await;
    let mut idle = Vec::new();
    for _ in 0..40 {
        idle.push(connect_from(LAN_CLIENT, h.addr()).await);
    }
    // Idle connections hit the header timeout and free their slots.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
    drop(idle);
}

/// A fresh instance on a fixed port, as after a container restart.
async fn boot(port: u16, net: &FakeNet) -> App {
    let mut cfg = common::test_config(port);
    cfg.allow_loopback = false;
    let deps = Deps {
        backend: Arc::new(FakeBackend::new(vec![
            airbridge::backend::fake::FakeDevice::brother_dcp_l2540dw("fake:dell", "DellE514dw"),
        ])),
        addrs: FakeAddrs::new(vec![lan(LAN_IP)], vec!["lan0".into()]),
        mdns: net.host("airbridge-testhost"),
        brscan: None,
        spooler: None,
    };
    App::start_with(cfg, deps, Duration::from_millis(10))
        .await
        .unwrap()
}

async fn healthy(addr: SocketAddr) -> bool {
    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_millis(200), async {
        let s = tokio::net::TcpSocket::new_v4()?;
        s.bind(SocketAddr::new(IpAddr::V4(LAN_CLIENT), 0))?;
        s.connect(addr).await
    })
    .await
    else {
        return false;
    };
    drop(stream);
    request_from(LAN_CLIENT, addr, Method::GET, "/healthz", None)
        .await
        .status
        == StatusCode::OK
}

#[tokio::test]
async fn restart_idle_and_under_load_is_healthy_within_5s() {
    let port = common::free_port();
    let addr = SocketAddr::new(IpAddr::V4(LAN_IP), port);
    let net = FakeNet::new();

    let app = boot(port, &net).await;
    wait_until(Duration::from_secs(5), || !app.scanners().is_empty()).await;
    app.shutdown().await;
    let t0 = Instant::now();
    let app = boot(port, &net).await;
    while !healthy(addr).await {
        assert!(t0.elapsed() < Duration::from_secs(5), "idle restart");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let idle = t0.elapsed();

    // Under load: clients hammer the server across the restart.
    let stop = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicUsize::new(0));
    let mut load = Vec::new();
    for _ in 0..16 {
        let (stop, served) = (Arc::clone(&stop), Arc::clone(&served));
        load.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), async {
                    let sock = tokio::net::TcpSocket::new_v4()?;
                    sock.bind(SocketAddr::new(IpAddr::V4(LAN_CLIENT), 0))?;
                    sock.connect(addr).await
                })
                .await
                {
                    drop(s);
                    served.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.shutdown().await;
    let t1 = Instant::now();
    let app = boot(port, &net).await;
    while !healthy(addr).await {
        assert!(t1.elapsed() < Duration::from_secs(5), "restart under load");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let loaded = t1.elapsed();
    stop.store(true, Ordering::Relaxed);
    for l in load {
        let _ = l.await;
    }
    assert!(served.load(Ordering::Relaxed) > 0);
    eprintln!("healthy after restart: idle {idle:?}, under load {loaded:?}");
    app.shutdown().await;
}
