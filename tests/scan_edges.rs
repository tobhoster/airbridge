//! Scan-path edge cases, each one met (or nearly met) during the AirSane setup.

mod common;

use std::time::{Duration, Instant};

use airbridge::backend::Backend;
use airbridge::backend::fake::FakeDevice;
use airbridge::escl::xml::JobState;
use common::{Opts, Ticket, decode, png_gray_levels, start, wait_until};
use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn job_id(path: &str) -> uuid::Uuid {
    path.rsplit('/').next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn one_bit_backend_data_still_yields_8bit_gray() {
    // A backend whose only gray mode delivers 1-bit data.
    let dev = FakeDevice::simple_flatbed("fake:lide", "Canon", "LiDE", "flatbed scanner");
    let h = start(Opts {
        devices: vec![dev],
        ..Default::default()
    })
    .await;
    h.backend.update(|b| b.gray_one_bit = true);
    let t = Ticket {
        color: "Grayscale8",
        format: "image/png",
        resolution: 150,
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(doc.status, StatusCode::OK);
    assert_eq!(
        decode("image/png", &doc.body),
        (150, 225, 1, 8),
        "8-bit, one channel"
    );
}

/// Image Capture's overview asks for the whole advertised flatbed (Mac log:
/// region 0,0 2502x4200 at 100 dpi) and maps the reply onto that area to
/// draw its selection boxes. brscan4 silently delivers less (291 of 355.6 mm,
/// width rounded), which put every box in the wrong place. The page must come
/// out exactly the requested size, padded with white, content anchored at
/// the top left.
#[tokio::test]
async fn flatbed_page_matches_the_requested_region_even_if_the_driver_clips() {
    let h = start(Default::default()).await;
    h.backend.update(|b| {
        b.clip_lines = Some(1146);
        b.width_delta = -7;
    });
    let t = Ticket {
        format: "image/png",
        resolution: 100,
        region: Some((0, 0, 2502, 4200)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(
        decode("image/png", &doc.body),
        (834, 1400, 3, 8),
        "exactly the requested 8.34 x 14 in"
    );

    let mut reader = png::Decoder::new(std::io::Cursor::new(&doc.body[..]))
        .read_info()
        .unwrap();
    let mut px = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut px).unwrap();
    let at = |x: usize, y: usize| &px[(y * 834 + x) * 3..(y * 834 + x) * 3 + 3];
    assert!(
        at(10, 10) != [255, 255, 255],
        "scanned content stays at the top left"
    );
    assert!(
        at(826, 1145) != [255, 255, 255],
        "last delivered column and line are content"
    );
    assert_eq!(at(827, 500), [255, 255, 255], "missing columns are white");
    assert_eq!(at(400, 1146), [255, 255, 255], "missing lines are white");
    assert_eq!(at(833, 1399), [255, 255, 255]);

    // A driver that delivers MORE than asked (brscan4 rounds widths up) is cropped.
    h.backend.update(|b| {
        b.clip_lines = None;
        b.width_delta = 8;
    });
    let t = Ticket {
        format: "image/png",
        resolution: 600,
        region: Some((300, 300, 900, 600)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(decode("image/png", &doc.body), (1800, 1200, 3, 8));
}

#[tokio::test]
async fn brother_gray_uses_true_gray_not_the_dithered_mode() {
    let h = start(Default::default()).await;
    let t = Ticket {
        color: "Grayscale8",
        format: "image/png",
        resolution: 150,
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    // "Gray[Error Diffusion]" would give two levels; True Gray gives many.
    assert!(
        png_gray_levels(&doc.body) > 64,
        "{} levels",
        png_gray_levels(&doc.body)
    );
}

#[tokio::test]
async fn empty_feeder_is_409_not_a_hang() {
    let h = start(Default::default()).await;
    let job = h
        .create_job(
            &Ticket {
                source: "Feeder",
                ..Default::default()
            }
            .xml(),
        )
        .await;
    let t0 = Instant::now();
    let r = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.text().contains("ScannerAdfEmpty"));
    assert!(t0.elapsed() < Duration::from_secs(1));
    let status = h.get("/eSCL/ScannerStatus").await.text();
    assert!(
        status.contains("<scan:AdfState>ScannerAdfEmpty</scan:AdfState>"),
        "{status}"
    );
    assert!(status.contains("<pwg:JobState>Aborted</pwg:JobState>"));
    assert_eq!(h.backend.open_handles(), 0);
}

#[tokio::test]
async fn feeder_multi_page_then_404() {
    let h = start(Default::default()).await;
    h.backend.update(|b| {
        b.adf_pages = 3;
        b.unknown_lines = true;
    });
    let job = h
        .create_job(
            &Ticket {
                source: "Feeder",
                format: "image/png",
                ..Default::default()
            }
            .xml(),
        )
        .await;
    for page in 1..=3 {
        let r = h.get(&format!("{job}/NextDocument")).await;
        assert_eq!(r.status, StatusCode::OK, "page {page}");
        assert_eq!(decode("image/png", &r.body).0, 150, "page {page}");
    }
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::NOT_FOUND
    );
    let status = h.get("/eSCL/ScannerStatus").await.text();
    assert!(
        status.contains("<pwg:ImagesCompleted>3</pwg:ImagesCompleted>"),
        "{status}"
    );
    assert!(status.contains("<pwg:JobState>Completed</pwg:JobState>"));
    wait_until(Duration::from_secs(1), || h.backend.open_handles() == 0).await;
}

#[tokio::test]
async fn scanner_unreachable_mid_scan_aborts_job_and_next_job_works() {
    let h = start(Default::default()).await;
    h.backend.update(|b| b.fail_after_bytes = Some(20_000));
    let job = h.create_job(&Ticket::default().xml()).await;
    let _ = h.get(&format!("{job}/NextDocument")).await;
    let svc = &h.app.scanners()[0];
    wait_until(Duration::from_secs(2), || {
        svc.job_state(job_id(&job))
            .is_some_and(|s| s.0.is_terminal())
    })
    .await;
    let (state, _, detail) = svc.job_state(job_id(&job)).unwrap();
    assert_eq!(state, JobState::Aborted);
    assert!(detail.contains("unreachable"), "{detail}");
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);

    h.backend.update(|b| b.fail_after_bytes = None);
    let job = h.create_job(&Ticket::default().xml()).await;
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::OK
    );
    wait_until(Duration::from_secs(1), || h.backend.open_handles() == 0).await;
}

#[tokio::test]
async fn scanner_asleep_mid_scan_is_aborted_by_the_watchdog() {
    let h = start(Default::default()).await;
    h.backend.update(|b| b.hang_after_bytes = Some(20_000));
    let job = h.create_job(&Ticket::default().xml()).await;
    let t0 = Instant::now();
    let _ = h.get(&format!("{job}/NextDocument")).await;
    let svc = &h.app.scanners()[0];
    wait_until(Duration::from_secs(3), || {
        svc.job_state(job_id(&job))
            .is_some_and(|s| s.0 == JobState::Aborted)
    })
    .await;
    // hang_timeout is 400 ms in the harness.
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    assert!(
        svc.job_state(job_id(&job))
            .unwrap()
            .2
            .contains("stopped responding")
    );
    wait_until(Duration::from_secs(1), || h.backend.open_handles() == 0).await;
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);

    h.backend.update(|b| b.hang_after_bytes = None);
    let job = h.create_job(&Ticket::default().xml()).await;
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn wedged_backend_makes_health_fail_until_it_returns() {
    let h = start(Default::default()).await;
    h.backend.update(|b| b.wedge_after_bytes = Some(20_000));
    let job = h.create_job(&Ticket::default().xml()).await;
    let fetch = {
        let path = format!("{job}/NextDocument");
        let addr = h.addr();
        tokio::spawn(async move {
            common::request_from(common::LAN_CLIENT, addr, hyper::Method::GET, &path, None).await
        })
    };
    wait_until(Duration::from_secs(3), || !h.app.health().healthy()).await;
    assert_eq!(
        h.get("/healthz").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    h.backend.release_wedged();
    wait_until(Duration::from_secs(3), || h.app.health().healthy()).await;
    let _ = fetch.await;
    wait_until(Duration::from_secs(1), || h.backend.open_handles() == 0).await;
}

#[tokio::test]
async fn second_job_while_scanning_is_503() {
    let h = start(Default::default()).await;
    h.backend
        .update(|b| b.start_delay = Duration::from_millis(300));
    let job = h.create_job(&Ticket::default().xml()).await;
    let fetch = {
        let path = format!("{job}/NextDocument");
        let addr = h.addr();
        tokio::spawn(async move {
            common::request_from(common::LAN_CLIENT, addr, hyper::Method::GET, &path, None).await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = h.post("/eSCL/ScanJobs", &Ticket::default().xml()).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.headers["retry-after"], "5");
    assert_eq!(
        fetch.await.unwrap().status,
        StatusCode::OK,
        "the first job is unaffected"
    );
    // Once it finished, a new job is accepted.
    h.create_job(&Ticket::default().xml()).await;
}

#[tokio::test]
async fn client_disconnect_mid_transfer_cancels_and_frees_the_handle() {
    let h = start(Default::default()).await;
    h.backend
        .update(|b| b.read_delay = Duration::from_millis(1));
    let t = Ticket {
        resolution: 600,
        region: Some((0, 0, 600, 900)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let mut s = common::connect_from(common::LAN_CLIENT, h.addr()).await;
    s.write_all(format!("GET {job}/NextDocument HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut first = [0u8; 256];
    let n = s.read(&mut first).await.unwrap();
    assert!(String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200"));
    assert_eq!(h.backend.open_handles(), 1, "scan in progress");
    drop(s);
    let svc = &h.app.scanners()[0];
    wait_until(Duration::from_secs(3), || h.backend.open_handles() == 0).await;
    let (state, reason, _) = svc.job_state(job_id(&job)).unwrap();
    assert_eq!((state, reason), (JobState::Canceled, "JobCanceledByUser"));
}

#[tokio::test]
async fn delete_during_a_page_cancels_the_backend() {
    let h = start(Default::default()).await;
    h.backend
        .update(|b| b.read_delay = Duration::from_millis(1));
    let t = Ticket {
        resolution: 600,
        region: Some((0, 0, 600, 900)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let fetch = {
        let path = format!("{job}/NextDocument");
        let addr = h.addr();
        tokio::spawn(async move {
            common::request_from(common::LAN_CLIENT, addr, hyper::Method::GET, &path, None).await
        })
    };
    wait_until(Duration::from_secs(2), || h.backend.open_handles() == 1).await;
    assert_eq!(h.delete(&job).await.status, StatusCode::OK);
    wait_until(Duration::from_secs(2), || h.backend.open_handles() == 0).await;
    let _ = fetch.await;
    let svc = &h.app.scanners()[0];
    assert_eq!(svc.job_state(job_id(&job)).unwrap().0, JobState::Canceled);
}
