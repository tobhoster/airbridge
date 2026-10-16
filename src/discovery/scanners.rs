//! Which SANE devices become AirScan scanners.
//!
//! One physical scanner can show up twice (brother4 and sane-airscan's WSD
//! discovery both find a Brother MFP). Devices are matched by IP; the vendor
//! backend wins over WSD, and anything that is already eSCL is skipped (it is
//! native AirScan, and re-exporting it would loop).

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use super::BrotherTarget;
use crate::backend::DeviceInfo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub device: DeviceInfo,
    /// Name to announce.
    pub display: String,
    /// Stable identity (drives the UUID): the same physical scanner keeps it
    /// across backend changes and DHCP changes.
    pub key: String,
}

fn wsd_ip(kind: &str) -> Option<IpAddr> {
    kind.split_whitespace()
        .find_map(|w| w.strip_prefix("ip="))
        .and_then(|ip| ip.parse().ok())
}

/// Lower is preferred.
fn rank(name: &str) -> Option<u8> {
    let backend = name.split(':').next().unwrap_or(name);
    match backend {
        "escl" | "v4l" | "test" => None,
        "airscan" if name.starts_with("airscan:e") => None,
        "airscan" => Some(2),
        _ => Some(0),
    }
}

pub fn choose(
    devices: &[DeviceInfo],
    brother: &[BrotherTarget],
    native_ips: &BTreeSet<IpAddr>,
    no_scan: &[String],
) -> Vec<Chosen> {
    // ip -> (rank, chosen); devices without a known IP are kept as they are.
    let mut by_ip: BTreeMap<IpAddr, (u8, Chosen)> = BTreeMap::new();
    let mut no_ip = Vec::new();
    for d in devices {
        let Some(r) = rank(&d.name) else { continue };
        let (ip, display) = if d.name.starts_with("brother4:") {
            match brother.iter().find(|t| t.name == d.model) {
                Some(t) => (Some(IpAddr::V4(t.ip)), t.display.clone()),
                None => (None, format!("{} {}", d.vendor, d.model)),
            }
        } else if d.name.starts_with("airscan:") {
            (wsd_ip(&d.kind), d.model.clone())
        } else {
            (None, format!("{} {}", d.vendor, d.model).trim().to_owned())
        };
        let ip_s = ip.map(|i| i.to_string()).unwrap_or_default();
        let hay = [d.name.as_str(), display.as_str(), ip_s.as_str()];
        if no_scan.iter().filter(|p| !p.is_empty()).any(|p| {
            let p = p.to_ascii_lowercase();
            hay.iter().any(|h| h.to_ascii_lowercase().contains(&p))
        }) {
            continue;
        }
        let chosen = Chosen {
            device: d.clone(),
            key: display.to_ascii_lowercase(),
            display,
        };
        match ip {
            Some(ip) if native_ips.contains(&ip) => {}
            Some(ip) => match by_ip.get(&ip) {
                Some((existing, _)) if *existing <= r => {}
                _ => {
                    by_ip.insert(ip, (r, chosen));
                }
            },
            None => no_ip.push(chosen),
        }
    }
    let mut out: Vec<Chosen> = by_ip.into_values().map(|(_, c)| c).chain(no_ip).collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out.dedup_by(|a, b| a.key == b.key);
    out
}
