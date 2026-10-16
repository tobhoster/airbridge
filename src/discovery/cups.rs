//! Printing: CUPS does the work (drivers, filters, IPP); airbridge creates
//! queues for discovered non-AirPrint printers and announces every shared
//! queue as AirPrint itself (CUPS runs without avahi).

use std::process::Command;

use super::PrinterTarget;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpoolError {
    #[error("{0}")]
    Command(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueInfo {
    pub name: String,
    /// The human name (CUPS printer-info), e.g. "HP LaserJet P2055dn".
    pub info: String,
    /// The driver's make and model string.
    pub make_model: String,
    pub uri: String,
    pub color: bool,
    pub duplex: bool,
}

pub trait Spooler: Send + Sync + 'static {
    /// Best driver (PPD name) for an IEEE-1284 device id, if any.
    fn driver_for(&self, device_id: &str) -> Result<Option<String>, SpoolError>;
    /// Create the queue, or update its URI when the device moved.
    fn ensure_queue(
        &self,
        target: &PrinterTarget,
        driver: &str,
        location: &str,
    ) -> Result<(), SpoolError>;
    /// Shared queues, including ones an admin added by hand.
    fn queues(&self) -> Result<Vec<QueueInfo>, SpoolError>;
}

/// AirPrint TXT for a CUPS queue served on port 631 by this host.
pub fn airprint_txt(q: &QueueInfo, host: &str, note: &str, uuid: &str) -> Vec<(String, String)> {
    let host = host.trim_end_matches('.');
    [
        ("txtvers", "1".to_owned()),
        ("qtotal", "1".to_owned()),
        ("rp", format!("printers/{}", q.name)),
        ("ty", q.info.clone()),
        ("product", format!("({})", q.info)),
        ("note", note.to_owned()),
        ("adminurl", format!("http://{host}:631/printers/{}", q.name)),
        ("priority", "0".to_owned()),
        (
            "pdl",
            "application/octet-stream,application/pdf,application/postscript,image/jpeg,image/png,image/pwg-raster,image/urf"
                .to_owned(),
        ),
        // What CUPS itself reports in urf-supported for a driver queue.
        (
            "URF",
            format!(
                "V1.4,CP1,W8,{}RS300{}",
                if q.color { "SRGB24," } else { "" },
                if q.duplex { ",DM1" } else { "" }
            ),
        ),
        ("UUID", uuid.to_owned()),
        ("Color", if q.color { "T" } else { "F" }.to_owned()),
        ("Duplex", if q.duplex { "T" } else { "F" }.to_owned()),
        ("Transparent", "T".to_owned()),
        ("Binary", "T".to_owned()),
        ("kind", "document,envelope,photo".to_owned()),
        ("printer-state", "3".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

pub struct CupsCli;

fn run(cmd: &str, args: &[&str]) -> Result<String, SpoolError> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| SpoolError::Command(format!("{cmd}: {e}")))?;
    if !out.status.success() {
        return Err(SpoolError::Command(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// First driver from `lpinfo -m` output (`<ppd> <make and model>` per line),
/// preferring a non-generic one.
pub fn pick_driver(lpinfo: &str) -> Option<String> {
    let drivers: Vec<&str> = lpinfo
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    drivers
        .iter()
        .find(|d| !d.contains("generic") && **d != "everywhere" && **d != "raw")
        .or_else(|| drivers.first())
        .map(|d| (*d).to_owned())
}

/// `lpoptions -l` output -> (color, duplex).
pub fn parse_options(text: &str) -> (bool, bool) {
    let mut color = false;
    let mut duplex = false;
    for line in text.lines() {
        let (key, values) = line.split_once(':').unwrap_or((line, ""));
        let key = key.split('/').next().unwrap_or(key);
        let values = values.to_ascii_lowercase();
        if key == "ColorModel" || key == "print-color-mode" {
            color |= values.contains("rgb") || values.contains("color") || values.contains("cmyk");
        }
        if key == "Duplex" || key == "sides" {
            duplex |= values.contains("duplex") || values.contains("two-sided");
        }
    }
    (color, duplex)
}

impl Spooler for CupsCli {
    fn driver_for(&self, device_id: &str) -> Result<Option<String>, SpoolError> {
        let out = run("lpinfo", &["--device-id", device_id, "-m"]).unwrap_or_default();
        Ok(pick_driver(&out))
    }

    fn ensure_queue(
        &self,
        t: &PrinterTarget,
        driver: &str,
        location: &str,
    ) -> Result<(), SpoolError> {
        let existing = run("lpstat", &["-v", &t.queue]).unwrap_or_default();
        if existing.contains(&t.uri) {
            return Ok(());
        }
        if existing.is_empty() {
            run(
                "lpadmin",
                &[
                    "-p",
                    &t.queue,
                    "-E",
                    "-v",
                    &t.uri,
                    "-m",
                    driver,
                    "-D",
                    &t.display,
                    "-L",
                    location,
                    "-o",
                    "printer-is-shared=true",
                ],
            )?;
            tracing::info!(queue = %t.queue, uri = %t.uri, driver, "created print queue");
        } else {
            run("lpadmin", &["-p", &t.queue, "-v", &t.uri])?;
            tracing::info!(queue = %t.queue, uri = %t.uri, "printer moved, queue updated");
        }
        Ok(())
    }

    fn queues(&self) -> Result<Vec<QueueInfo>, SpoolError> {
        let mut out = Vec::new();
        for line in run("lpstat", &["-v"]).unwrap_or_default().lines() {
            // "device for NAME: URI"
            let Some(rest) = line.strip_prefix("device for ") else {
                continue;
            };
            let Some((name, uri)) = rest.split_once(": ") else {
                continue;
            };
            let opts = run("lpoptions", &["-p", name]).unwrap_or_default();
            if !opts.contains("printer-is-shared=true") {
                continue;
            }
            let quoted = |key: &str| {
                opts.split(&format!("{key}='"))
                    .nth(1)
                    .and_then(|s| s.split('\'').next())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            let make_model = quoted("printer-make-and-model").unwrap_or_else(|| name.to_owned());
            let info = quoted("printer-info").unwrap_or_else(|| make_model.clone());
            let (color, duplex) =
                parse_options(&run("lpoptions", &["-p", name, "-l"]).unwrap_or_default());
            out.push(QueueInfo {
                name: name.to_owned(),
                info,
                make_model,
                uri: uri.to_owned(),
                color,
                duplex,
            });
        }
        Ok(out)
    }
}
