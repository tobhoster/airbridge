//! eSCL protocol conformance against captured AirSane behaviour (AirSane is
//! known to work with macOS Image Capture).

mod common;

use airbridge::backend::Backend;
use common::{Ticket, decode, start};
use hyper::StatusCode;

/// Masks the values of `tags` (host-, time- or id-dependent) and keeps every
/// other byte, line endings included, so the comparison is otherwise exact.
fn mask(xml: &str, tags: &[&str]) -> String {
    xml.split_inclusive('\n')
        .map(|line| {
            for tag in tags {
                if line.starts_with(&format!("<{tag}>")) {
                    let end = &line[line.trim_end_matches(['\r', '\n']).len()..];
                    return format!("<{tag}>*</{tag}>{end}");
                }
            }
            line.to_owned()
        })
        .collect()
}

fn normalize_caps(xml: &str) -> String {
    mask(xml, &["scan:UUID", "scan:AdminURI", "scan:IconURI"])
}

fn normalize_status(xml: &str) -> String {
    mask(xml, &["pwg:JobUri", "pwg:JobUuid", "scan:Age"])
}

#[tokio::test]
async fn capabilities_match_airsane_golden() {
    let h = start(Default::default()).await;
    let r = h.get("/eSCL/ScannerCapabilities").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "text/xml");
    let golden = include_str!("fixtures/airsane-capabilities.xml");
    assert_eq!(normalize_caps(&r.text()), normalize_caps(golden));
    // Spot checks of what the golden file pins down.
    let t = r.text();
    for needle in [
        "<scan:Platen>",
        "<scan:Adf>",
        "<scan:ColorMode>Grayscale8</scan:ColorMode>",
        "<scan:ColorMode>RGB24</scan:ColorMode>",
        "<scan:XResolution>100</scan:XResolution>",
        "<scan:XResolution>9600</scan:XResolution>",
        "<scan:MaxWidth>2503</scan:MaxWidth>",
        "<scan:MaxHeight>4200</scan:MaxHeight>",
    ] {
        assert!(t.contains(needle), "{needle}");
    }
    assert!(t.contains(&format!(
        "<scan:AdminURI>http://{}/eSCL</scan:AdminURI>",
        h.addr()
    )));
}

#[tokio::test]
async fn status_matches_airsane_golden_idle_pending_completed() {
    let h = start(Default::default()).await;
    let idle = h.get("/eSCL/ScannerStatus").await;
    assert_eq!(
        idle.text(),
        include_str!("fixtures/airsane-status-idle.xml")
    );

    let job = h.create_job(&Ticket::default().xml()).await;
    let pending = h.get("/eSCL/ScannerStatus").await.text();
    assert_eq!(
        normalize_status(&pending),
        normalize_status(include_str!("fixtures/airsane-status-pending.xml"))
    );
    assert!(pending.contains(&format!("<pwg:JobUri>{job}</pwg:JobUri>")));

    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(doc.status, StatusCode::OK);
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::NOT_FOUND
    );
    let done = h.get("/eSCL/ScannerStatus").await.text();
    assert_eq!(
        normalize_status(&done),
        normalize_status(include_str!("fixtures/airsane-status-completed.xml"))
    );
}

#[tokio::test]
async fn job_lifecycle_post_201_next_200_then_404_delete_cancels() {
    let h = start(Default::default()).await;
    let job = h.create_job(&Ticket::default().xml()).await;
    assert!(job.starts_with("/eSCL/ScanJobs/"), "{job}");
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(doc.status, StatusCode::OK);
    assert_eq!(doc.headers["content-type"], "image/jpeg");
    assert!(
        doc.headers
            .get("transfer-encoding")
            .is_some_and(|v| v == "chunked")
    );
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::NOT_FOUND
    );

    // DELETE before any page: the job is cancelled and yields nothing.
    let job = h.create_job(&Ticket::default().xml()).await;
    assert_eq!(h.delete(&job).await.status, StatusCode::OK);
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::NOT_FOUND
    );
    let status = h.get("/eSCL/ScannerStatus").await.text();
    assert!(
        status.contains("<pwg:JobState>Canceled</pwg:JobState>"),
        "{status}"
    );
    assert!(status.contains("JobCanceledByUser"));
    assert_eq!(
        h.delete("/eSCL/ScanJobs/00000000-0000-0000-0000-000000000000")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.backend.open_handles(), 0);
}

#[tokio::test]
async fn invalid_tickets_are_409_never_a_panic() {
    let h = start(Default::default()).await;
    let cases = [
        (
            "region beyond the platen",
            Ticket {
                region: Some((2400, 0, 300, 300)),
                ..Default::default()
            }
            .xml(),
        ),
        (
            "zero-size region",
            Ticket {
                region: Some((0, 0, 0, 300)),
                ..Default::default()
            }
            .xml(),
        ),
        (
            "overflowing offset",
            Ticket {
                region: Some((u32::MAX, 0, 10, 10)),
                ..Default::default()
            }
            .xml(),
        ),
        (
            "unsupported resolution",
            Ticket {
                resolution: 123,
                ..Default::default()
            }
            .xml(),
        ),
        (
            "unknown color mode",
            Ticket {
                color: "CMYK64",
                ..Default::default()
            }
            .xml(),
        ),
        (
            "unknown input source",
            Ticket {
                source: "Camera",
                ..Default::default()
            }
            .xml(),
        ),
        (
            "unsupported format",
            Ticket {
                format: "image/tiff",
                ..Default::default()
            }
            .xml(),
        ),
        ("not XML", "garbage<".to_owned()),
        ("wrong root", "<a><b>1</b></a>".to_owned()),
        ("empty", String::new()),
        (
            "doctype",
            "<!DOCTYPE x [<!ENTITY a 'b'>]><scan:ScanSettings/>".to_owned(),
        ),
    ];
    for (what, body) in cases {
        let r = h.post("/eSCL/ScanJobs", &body).await;
        assert_eq!(r.status, StatusCode::CONFLICT, "{what}: {}", r.text());
        assert!(r.text().starts_with("InvalidScanTicket"), "{what}");
    }
    // The server is still fine afterwards.
    let job = h.create_job(&Ticket::default().xml()).await;
    assert_eq!(
        h.get(&format!("{job}/NextDocument")).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn region_in_three_hundredths_of_an_inch_sets_the_image_size() {
    let h = start(Default::default()).await;
    // 1 x 1.5 inch at 200 dpi = 200 x 300 px.
    let t = Ticket {
        resolution: 200,
        format: "image/png",
        region: Some((150, 300, 300, 450)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(decode("image/png", &doc.body), (200, 300, 3, 8));
}

#[tokio::test]
async fn document_formats_decode_with_the_right_shape() {
    let h = start(Default::default()).await;
    for (format, color, channels) in [
        ("image/jpeg", "RGB24", 3),
        ("image/jpeg", "Grayscale8", 1),
        ("image/png", "RGB24", 3),
        ("image/png", "Grayscale8", 1),
    ] {
        let t = Ticket {
            format,
            color,
            resolution: 100,
            region: Some((0, 0, 255, 330)),
            ..Default::default()
        };
        let job = h.create_job(&t.xml()).await;
        let doc = h.get(&format!("{job}/NextDocument")).await;
        assert_eq!(doc.status, StatusCode::OK, "{format} {color}");
        assert_eq!(doc.headers["content-type"], format);
        assert_eq!(
            decode(format, &doc.body),
            (85, 110, channels, 8),
            "{format} {color}"
        );
    }
    let t = Ticket {
        format: "application/pdf",
        resolution: 100,
        region: Some((0, 0, 255, 330)),
        ..Default::default()
    };
    let job = h.create_job(&t.xml()).await;
    let doc = h.get(&format!("{job}/NextDocument")).await;
    assert_eq!(doc.headers["content-type"], "application/pdf");
    let pdf = String::from_utf8_lossy(&doc.body);
    assert!(pdf.starts_with("%PDF-1.4") && pdf.trim_end().ends_with("%%EOF"));
    // 85x110 px at 100 dpi = 61.2 x 79.2 pt; the image inside is a JPEG.
    assert!(
        pdf.contains("/MediaBox [0 0 61.20 79.20]"),
        "{}",
        &pdf[..400.min(pdf.len())]
    );
    assert!(pdf.contains("/Width 85 /Height 110 /ColorSpace /DeviceRGB"));
    let jpeg_start = doc.body.windows(2).position(|w| w == [0xFF, 0xD8]).unwrap();
    let jpeg_end = doc
        .body
        .windows(2)
        .rposition(|w| w == [0xFF, 0xD9])
        .unwrap()
        + 2;
    assert_eq!(
        decode("image/jpeg", &doc.body[jpeg_start..jpeg_end]),
        (85, 110, 3, 8)
    );
}

#[tokio::test]
async fn unknown_paths_and_scanner_icon() {
    let h = start(Default::default()).await;
    assert_eq!(h.get("/eSCL/Nope").await.status, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/nothing/here").await.status, StatusCode::NOT_FOUND);
    let icon = h.get("/eSCL/ScannerIcon").await;
    assert_eq!(icon.headers["content-type"], "image/png");
    let decoder = png::Decoder::new(std::io::Cursor::new(&icon.body[..]));
    let info = decoder.read_info().unwrap();
    assert_eq!(
        (info.info().width, info.info().height),
        (128, 128),
        "macOS needs a power-of-two icon"
    );
}
