//! Hardware suite: run by hand on the LAN against a live airbridge and a real
//! scanner. Every test is `#[ignore]`d so the default suite stays hermetic.
//!
//!   AIRBRIDGE_ALLOW_NETWORK=1 cargo test --test hardware -- --ignored hardware
//!
//! Environment:
//!   AIRBRIDGE_URL      eSCL base of the bridge, e.g. http://192.168.68.67:8090
//!                      (default: found via mDNS)
//!   AIRBRIDGE_SCANNER  name of the bridged scanner (default "Dell MFP E514dw")
//!   AIRBRIDGE_PRINTER_HOST  mDNS host of the printer whose printing must stay
//!                      native (default "DELL6D6F61.local.")

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

fn scanner_name() -> String {
    std::env::var("AIRBRIDGE_SCANNER").unwrap_or_else(|_| "Dell MFP E514dw".into())
}

/// (host, addresses, port, txt) of one resolved service.
type Seen = (String, Vec<IpAddr>, u16, BTreeMap<String, String>);

/// fullname -> what was resolved, collected for `secs`.
fn browse(ty: &str, secs: u64) -> BTreeMap<String, Seen> {
    let daemon = ServiceDaemon::new().unwrap();
    let rx = daemon.browse(&format!("{ty}.local.")).unwrap();
    let mut out = BTreeMap::new();
    let end = Instant::now() + Duration::from_secs(secs);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(r)) => {
                let txt = r
                    .txt_properties
                    .iter()
                    .map(|p| (p.key().to_ascii_lowercase(), p.val_str().to_owned()))
                    .collect();
                let addrs = r.addresses.iter().map(|a| a.to_ip_addr()).collect();
                out.insert(r.fullname.clone(), (r.host.clone(), addrs, r.port, txt));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    out
}

/// eSCL root URL of the bridged scanner.
fn bridge_url() -> String {
    if let Ok(url) = std::env::var("AIRBRIDGE_URL") {
        return format!("{}/eSCL", url.trim_end_matches('/'));
    }
    let name = scanner_name();
    let found = browse("_uscan._tcp", 6);
    let (_, (host, addrs, port, txt)) = found
        .iter()
        .find(|(n, (h, ..))| n.starts_with(&name) && h.starts_with("airbridge-"))
        .unwrap_or_else(|| panic!("no airbridge scanner named {name:?} on the LAN: {found:#?}"));
    let ip = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .unwrap_or_else(|| panic!("{host} has no IPv4 address"));
    format!(
        "http://{ip}:{port}/{}",
        txt.get("rs").map_or("eSCL", String::as_str)
    )
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(180))
        .build()
}

fn ticket(source: &str, color: &str, res: u32, w: u32, h: u32) -> String {
    format!(
        "<?xml version='1.0' encoding='UTF-8'?><scan:ScanSettings xmlns:scan='http://schemas.hp.com/imaging/escl/2011/05/03' xmlns:pwg='http://www.pwg.org/schemas/2010/12/sm'><pwg:Version>2.0</pwg:Version><pwg:ScanRegions><pwg:ScanRegion><pwg:Height>{h}</pwg:Height><pwg:Width>{w}</pwg:Width><pwg:XOffset>0</pwg:XOffset><pwg:YOffset>0</pwg:YOffset><pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits></pwg:ScanRegion></pwg:ScanRegions><pwg:InputSource>{source}</pwg:InputSource><scan:ColorMode>{color}</scan:ColorMode><scan:XResolution>{res}</scan:XResolution><scan:YResolution>{res}</scan:YResolution><pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat></scan:ScanSettings>"
    )
}

/// POST a job, fetch one page. Returns (status of NextDocument, body, job URL).
fn scan_once(base: &str, body: &str) -> (u16, Vec<u8>, String) {
    let a = agent();
    let resp = a
        .post(&format!("{base}/ScanJobs"))
        .set("Content-Type", "text/xml")
        .send_string(body)
        .unwrap();
    assert_eq!(resp.status(), 201);
    let location = resp.header("Location").unwrap().to_owned();
    let origin = base
        .split("/eSCL")
        .next()
        .unwrap()
        .split("/scanners/")
        .next()
        .unwrap();
    let job = format!("{origin}{location}");
    match a.get(&format!("{job}/NextDocument")).call() {
        Ok(r) => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut r.into_reader(), &mut buf).unwrap();
            (200, buf, job)
        }
        Err(ureq::Error::Status(code, _)) => (code, Vec::new(), job),
        Err(e) => panic!("{e}"),
    }
}

fn jpeg_shape(body: &[u8]) -> (usize, usize, usize) {
    let mut d = jpeg_decoder::Decoder::new(body);
    d.decode().unwrap();
    let info = d.info().unwrap();
    let channels = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1,
        jpeg_decoder::PixelFormat::RGB24 => 3,
        other => panic!("{other:?}"),
    };
    (usize::from(info.width), usize::from(info.height), channels)
}

#[test]
#[ignore = "hardware: needs the LAN, a running airbridge and the scanner"]
fn hardware_flatbed_color_and_gray_at_150_300_600() {
    let base = bridge_url();
    // 2 x 2 inch keeps 600 dpi quick.
    for res in [150u32, 300, 600] {
        for (color, channels) in [("RGB24", 3), ("Grayscale8", 1)] {
            let t0 = Instant::now();
            let (status, body, job) = scan_once(&base, &ticket("Platen", color, res, 600, 600));
            assert_eq!(status, 200, "{color} {res} dpi");
            // brscan4 rounds the area itself: `scanimage -x 50.8 -y 50.8
            // --resolution 300` returns 576x575, not 600x600. Accept the
            // driver's rounding (within 5%), never a wrong shape.
            let want = (res * 2) as usize;
            let (w, h, c) = jpeg_shape(&body);
            assert_eq!(c, channels, "{color} {res} dpi");
            for got in [w, h] {
                assert!(
                    got <= want && got * 100 >= want * 95,
                    "{color} {res} dpi: {w}x{h}, want ~{want}"
                );
            }
            eprintln!(
                "{color} {res} dpi: {} bytes in {:?}",
                body.len(),
                t0.elapsed()
            );
            let (next, ..) = {
                let r = agent().get(&format!("{job}/NextDocument")).call();
                match r {
                    Err(ureq::Error::Status(c, _)) => (c, (), ()),
                    Ok(_) => (200, (), ()),
                    Err(e) => panic!("{e}"),
                }
            };
            assert_eq!(next, 404, "flatbed job ends after one page");
        }
    }
}

#[test]
#[ignore = "hardware: needs the LAN, a running airbridge and the scanner"]
fn hardware_adf_scan_if_paper_is_loaded() {
    let base = bridge_url();
    let (status, body, _) = scan_once(&base, &ticket("Feeder", "RGB24", 150, 2550, 3300));
    match status {
        200 => {
            let (w, _, c) = jpeg_shape(&body);
            assert!(
                w <= 1275 && w * 100 >= 1275 * 95 && c == 3,
                "feeder page {w} px wide, {c} channels"
            );
            eprintln!("feeder page: {} bytes", body.len());
        }
        409 => eprintln!(
            "feeder empty (409 ScannerAdfEmpty): nothing to scan, which is the correct answer"
        ),
        other => panic!("feeder scan returned {other}"),
    }
}

#[test]
#[ignore = "hardware: needs the LAN"]
fn hardware_mdns_shows_exactly_one_scanner() {
    let name = scanner_name();
    let found = browse("_uscan._tcp", 8);
    let matching: Vec<_> = found
        .iter()
        .filter(|(n, (.., txt))| {
            n.starts_with(&name)
                || txt
                    .get("ty")
                    .is_some_and(|t| t.contains("E514dw") || *t == name)
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected one AirScan service for {name:?}: {matching:#?}"
    );
}

#[test]
#[ignore = "hardware: needs the LAN"]
fn hardware_dell_is_not_reshared_as_a_printer() {
    let printer_host =
        std::env::var("AIRBRIDGE_PRINTER_HOST").unwrap_or_else(|_| "DELL6D6F61.local.".into());
    let found = browse("_ipp._tcp", 8);
    let dell: Vec<_> = found
        .iter()
        .filter(|(n, (.., txt))| {
            n.contains("E514dw") || txt.get("ty").is_some_and(|t| t.contains("E514dw"))
        })
        .collect();
    assert!(
        !dell.is_empty(),
        "the printer's own AirPrint service should be visible"
    );
    for (name, (host, ..)) in &dell {
        assert!(
            host.eq_ignore_ascii_case(&printer_host),
            "{name} is announced by {host}, not by the printer itself"
        );
    }
}
