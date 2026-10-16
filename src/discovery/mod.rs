//! What is on the LAN and what airbridge should do about it.
//!
//! Rules: a device that already speaks AirScan (`_uscan._tcp`) or AirPrint
//! (`_ipp._tcp` with a `URF` key) is never bridged for that role. Everything
//! is keyed by mDNS host name, so a DHCP address change updates the target
//! instead of creating a second device.

pub mod brscan;
pub mod cups;
pub mod scanners;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use crate::mdns::{MdnsEvent, Resolved};

#[derive(Default, Debug)]
pub struct Registry {
    /// host -> fullname -> service
    hosts: BTreeMap<String, BTreeMap<String, Resolved>>,
}

impl Registry {
    /// Returns true if anything changed.
    pub fn apply(&mut self, event: &MdnsEvent) -> bool {
        match event {
            MdnsEvent::Resolved(r) => {
                // A service that moved to another host record (rare) must not linger.
                let mut changed = false;
                for (host, services) in &mut self.hosts {
                    if *host != r.host && services.remove(&r.fullname).is_some() {
                        changed = true;
                    }
                }
                let slot = self.hosts.entry(r.host.clone()).or_default();
                if slot.get(&r.fullname) != Some(r) {
                    slot.insert(r.fullname.clone(), r.clone());
                    changed = true;
                }
                self.hosts.retain(|_, s| !s.is_empty());
                changed
            }
            MdnsEvent::Removed { fullname, .. } => {
                let mut changed = false;
                for services in self.hosts.values_mut() {
                    changed |= services.remove(fullname).is_some();
                }
                self.hosts.retain(|_, s| !s.is_empty());
                changed
            }
            MdnsEvent::NameChanged { .. } => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryConfig {
    /// Our own mDNS host; never bridge ourselves.
    pub own_host: String,
    /// Case-insensitive substrings of host, name or IP: never bridge as scanner.
    pub no_scan: Vec<String>,
    /// Same, for printing.
    pub no_print: Vec<String>,
    /// Rebadged models: advertised name -> Brother model (e.g. Dell MFP E514dw -> DCP-L2540DW).
    pub model_map: Vec<(String, String)>,
}

/// Rebadges verified on real hardware.
pub const KNOWN_REBADGES: [(&str, &str); 1] = [("Dell MFP E514dw", "DCP-L2540DW")];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrotherTarget {
    /// brsaneconfig4 friendly name (the mDNS host label).
    pub name: String,
    pub model: String,
    pub ip: Ipv4Addr,
    /// Name to announce, e.g. "Dell MFP E514dw".
    pub display: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterTarget {
    pub queue: String,
    pub display: String,
    pub uri: String,
    pub device_id: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub brother: Vec<BrotherTarget>,
    pub printers: Vec<PrinterTarget>,
    /// Addresses of devices that already speak AirScan.
    pub native_scanner_ips: BTreeSet<IpAddr>,
    /// (host, reason) for devices seen but not bridged, for the status page.
    pub skipped: Vec<(String, String)>,
}

pub fn host_label(host: &str) -> &str {
    host.trim_end_matches('.')
        .trim_end_matches(".local")
        .split('.')
        .next()
        .unwrap_or(host)
}

fn matches_any(patterns: &[String], haystacks: &[&str]) -> bool {
    patterns.iter().filter(|p| !p.is_empty()).any(|p| {
        let p = p.to_ascii_lowercase();
        haystacks
            .iter()
            .any(|h| h.to_ascii_lowercase().contains(&p))
    })
}

fn sanitize_queue(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').chars().take(127).collect()
}

fn brother_model(mfg: &str, mdl: &str, display: &str, map: &[(String, String)]) -> Option<String> {
    let lookup = |name: &str| {
        map.iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .chain(KNOWN_REBADGES)
            .find(|(from, _)| from.eq_ignore_ascii_case(name))
            .map(|(_, to)| to.to_owned())
    };
    if let Some(m) = lookup(display).or_else(|| lookup(&format!("{mfg} {mdl}"))) {
        return Some(m);
    }
    if mfg.eq_ignore_ascii_case("brother") || display.to_ascii_lowercase().starts_with("brother") {
        let m = mdl.trim();
        let m = m.strip_prefix("Brother ").unwrap_or(m);
        let m = m.strip_suffix(" series").unwrap_or(m);
        return (!m.is_empty()).then(|| m.to_owned());
    }
    None
}

pub fn plan(
    reg: &Registry,
    cfg: &DiscoveryConfig,
    brother_supported: &dyn Fn(&str) -> bool,
) -> Plan {
    let mut plan = Plan::default();
    for (host, services) in &reg.hosts {
        if host.eq_ignore_ascii_case(&cfg.own_host) {
            continue;
        }
        let label = host_label(host);
        let all: Vec<&Resolved> = services.values().collect();
        let of = |ty: &str| all.iter().copied().find(|r| r.ty == ty);
        let addrs: BTreeSet<IpAddr> = all.iter().flat_map(|r| r.addrs.iter().copied()).collect();
        let ipv4 = addrs.iter().find_map(|a| match a {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        });
        let txt = |key: &str| {
            all.iter()
                .find_map(|r| r.txt.get(key).filter(|v| !v.is_empty()).cloned())
        };
        let display = txt("ty").unwrap_or_else(|| {
            all.first()
                .map_or_else(|| label.to_owned(), |r| r.instance.clone())
        });
        let ip_strings: Vec<String> = addrs.iter().map(IpAddr::to_string).collect();
        let mut hay: Vec<&str> = vec![host.as_str(), label, display.as_str()];
        hay.extend(ip_strings.iter().map(String::as_str));

        let native_scan = of("_uscan._tcp").or(of("_uscans._tcp")).is_some();
        let native_print = all.iter().any(|r| {
            (r.ty == "_ipp._tcp" || r.ty == "_ipps._tcp")
                && r.txt
                    .get("urf")
                    .is_some_and(|u| !u.is_empty() && u != "none")
        });
        if native_scan {
            plan.native_scanner_ips.extend(addrs.iter().copied());
        }

        if let Some(scanner) = of("_scanner._tcp") {
            let reason = if native_scan {
                Some("already speaks AirScan".to_owned())
            } else if matches_any(&cfg.no_scan, &hay) {
                Some("excluded from scanning by configuration".to_owned())
            } else {
                let mfg = scanner.txt.get("mfg").map_or("", String::as_str);
                let mdl = scanner.txt.get("mdl").map_or("", String::as_str);
                match (brother_model(mfg, mdl, &display, &cfg.model_map), ipv4) {
                    (Some(model), Some(ip)) if brother_supported(&model) => {
                        plan.brother.push(BrotherTarget {
                            name: label.to_owned(),
                            model,
                            ip,
                            display: display.clone(),
                        });
                        None
                    }
                    (Some(model), Some(_)) => Some(format!("brscan4 does not support {model}")),
                    (Some(_), None) => Some("no IPv4 address".to_owned()),
                    (None, _) => Some(format!("no network scan driver for {display}")),
                }
            };
            if let Some(r) = reason {
                plan.skipped.push((host.clone(), format!("scanner: {r}")));
            }
        }

        let raw = of("_pdl-datastream._tcp");
        let ipp = of("_ipp._tcp");
        let lpd = of("_printer._tcp");
        if raw.is_some() || ipp.is_some() || lpd.is_some() {
            if native_print {
                plan.skipped
                    .push((host.clone(), "printer: already speaks AirPrint".to_owned()));
            } else if matches_any(&cfg.no_print, &hay) {
                plan.skipped.push((
                    host.clone(),
                    "printer: excluded by configuration".to_owned(),
                ));
            } else if let Some(ip) = ipv4 {
                let rp = |r: &Resolved| r.txt.get("rp").cloned().unwrap_or_default();
                let uri = if let Some(r) = raw {
                    format!("socket://{ip}:{}", r.port)
                } else if let Some(r) = ipp {
                    format!("ipp://{ip}:{}/{}", r.port, rp(r))
                } else if let Some(r) = lpd {
                    format!("lpd://{ip}/{}", rp(r))
                } else {
                    continue;
                };
                let mfg = txt("usb_mfg").unwrap_or_default();
                let mdl = txt("usb_mdl").unwrap_or_default();
                let cmd = txt("usb_cmd").unwrap_or_default();
                let device_id = format!("MFG:{mfg};MDL:{mdl};CMD:{cmd};");
                plan.printers.push(PrinterTarget {
                    queue: sanitize_queue(&display),
                    display: display.clone(),
                    uri,
                    device_id,
                });
            } else {
                plan.skipped
                    .push((host.clone(), "printer: no IPv4 address".to_owned()));
            }
        }
    }
    plan
}
