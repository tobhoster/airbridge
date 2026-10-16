//! Interface addresses and which of them airbridge serves on.
//!
//! The system source is event driven (netlink via `if-addrs`), with a slow
//! resync as a safety net, so idle CPU stays at zero while address changes
//! (DHCP renewals, Wi-Fi roaming, a NIC coming up late at boot) apply within
//! moments instead of only at startup.

use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IfAddr {
    pub iface: String,
    pub ip: IpAddr,
    pub prefix: u8,
    pub index: u32,
}

pub trait AddrSource: Send + Sync + 'static {
    fn snapshot(&self) -> io::Result<Vec<IfAddr>>;
    /// Interfaces that carry a default route.
    fn default_route_ifaces(&self) -> Vec<String>;
    /// Notified whenever addresses may have changed.
    fn changed(&self) -> Arc<Notify>;
}

/// Interface names that are never a LAN: containers, VPNs, overlays.
const VIRTUAL_PREFIXES: [&str; 14] = [
    "lo",
    "docker",
    "veth",
    "br-",
    "cni",
    "flannel",
    "cali",
    "cilium",
    "tailscale",
    "tun",
    "wg",
    "virbr",
    "kube",
    "podman",
];

pub fn is_virtual(iface: &str) -> bool {
    VIRTUAL_PREFIXES.iter().any(|p| iface.starts_with(p)) || iface == "tel0"
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Interfaces holding a default route; if none, every non-virtual one.
    Auto,
    Named(Vec<String>),
}

/// The addresses airbridge serves and announces on.
pub fn select(all: &[IfAddr], default_ifaces: &[String], sel: &Selection) -> Vec<IfAddr> {
    let wanted: BTreeSet<&str> = match sel {
        Selection::Named(names) => names.iter().map(String::as_str).collect(),
        Selection::Auto => {
            let routed: BTreeSet<&str> = default_ifaces
                .iter()
                .map(String::as_str)
                .filter(|i| !is_virtual(i))
                .collect();
            if routed.is_empty() {
                all.iter()
                    .map(|a| a.iface.as_str())
                    .filter(|i| !is_virtual(i))
                    .collect()
            } else {
                routed
            }
        }
    };
    let candidates: Vec<IfAddr> = all
        .iter()
        // Loopback is excluded by interface name (`lo` is in VIRTUAL_PREFIXES),
        // not by address, so tests can model a LAN with 127.0.2.0/24 aliases.
        .filter(|a| wanted.contains(a.iface.as_str()) && !is_link_local(a.ip))
        .cloned()
        .collect();
    // A host-prefix address next to a real subnet address on the same
    // interface is a floating VIP (keepalived, Talos/kube-vip): it moves to
    // another machine at any time, so never announce or serve on it.
    let floating = |a: &IfAddr| {
        let host_prefix = if a.ip.is_ipv4() { 32 } else { 128 };
        a.prefix == host_prefix
            && candidates.iter().any(|b| {
                b.iface == a.iface && b.ip.is_ipv4() == a.ip.is_ipv4() && b.prefix < host_prefix
            })
    };
    let mut out: Vec<IfAddr> = candidates
        .iter()
        .filter(|a| !floating(a))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Interfaces with a default route, from `/proc/net/route`.
pub fn parse_proc_route(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let iface = f.next()?;
            let dest = f.next()?;
            let _gateway = f.next()?;
            let flags = u32::from_str_radix(f.next()?, 16).ok()?;
            // RTF_UP
            (dest == "00000000" && flags & 1 == 1).then(|| iface.to_owned())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub struct SystemAddrs {
    notify: Arc<Notify>,
}

impl SystemAddrs {
    /// Starts a thread that waits on netlink and wakes the watcher.
    pub fn start(resync: Duration) -> Self {
        let notify = Arc::new(Notify::new());
        let n = Arc::clone(&notify);
        let spawned = std::thread::Builder::new().name("ifwatch".into()).spawn(move || {
            match if_addrs::IfChangeNotifier::new() {
                Ok(mut notifier) => loop {
                    match notifier.wait(Some(resync)) {
                        Ok(_) => n.notify_one(),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => n.notify_one(),
                        Err(e) => {
                            tracing::warn!(error = %e, "interface notifier failed; polling instead");
                            break;
                        }
                    }
                },
                Err(e) => tracing::warn!(error = %e, "no interface notifications; polling instead"),
            }
            loop {
                std::thread::sleep(Duration::from_secs(5));
                n.notify_one();
            }
        });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "cannot start interface watcher thread");
        }
        Self { notify }
    }
}

impl AddrSource for SystemAddrs {
    fn snapshot(&self) -> io::Result<Vec<IfAddr>> {
        Ok(if_addrs::get_if_addrs()?
            .into_iter()
            .map(|i| {
                let (ip, prefix) = match &i.addr {
                    if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
                    if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
                };
                IfAddr {
                    iface: i.name,
                    ip,
                    prefix,
                    index: i.index.unwrap_or(0),
                }
            })
            .collect())
    }

    fn default_route_ifaces(&self) -> Vec<String> {
        std::fs::read_to_string("/proc/net/route")
            .map(|t| parse_proc_route(&t))
            .unwrap_or_default()
    }

    fn changed(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }
}

/// Test double: tests add and remove addresses at will.
#[derive(Default)]
pub struct FakeAddrs {
    addrs: Mutex<Vec<IfAddr>>,
    routes: Mutex<Vec<String>>,
    notify: Arc<Notify>,
}

impl FakeAddrs {
    pub fn new(addrs: Vec<IfAddr>, default_ifaces: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            addrs: Mutex::new(addrs),
            routes: Mutex::new(default_ifaces),
            notify: Arc::new(Notify::new()),
        })
    }

    pub fn set(&self, addrs: Vec<IfAddr>) {
        *self.addrs.lock() = addrs;
        self.notify.notify_one();
    }
}

impl AddrSource for FakeAddrs {
    fn snapshot(&self) -> io::Result<Vec<IfAddr>> {
        Ok(self.addrs.lock().clone())
    }
    fn default_route_ifaces(&self) -> Vec<String> {
        self.routes.lock().clone()
    }
    fn changed(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(iface: &str, ip: &str) -> IfAddr {
        IfAddr {
            iface: iface.into(),
            ip: ip.parse().unwrap(),
            prefix: 24,
            index: 1,
        }
    }

    #[test]
    fn proc_route_default_interfaces() {
        let text = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                    wlo1\t00000000\t0144A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                    wlo1\t0044A8C0\t00000000\t0001\t0\t0\t600\t00FCFFFF\t0\t0\t0\n\
                    docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n";
        assert_eq!(parse_proc_route(text), vec!["wlo1".to_owned()]);
    }

    #[test]
    fn floating_vip_is_never_served() {
        // node-3 of the homelab: the Kubernetes API VIP sits on the LAN NIC as a /32.
        let node = IfAddr {
            iface: "enp0s31f6".into(),
            ip: "192.168.68.64".parse().unwrap(),
            prefix: 22,
            index: 2,
        };
        let vip = IfAddr {
            iface: "enp0s31f6".into(),
            ip: "192.168.68.100".parse().unwrap(),
            prefix: 32,
            index: 2,
        };
        let got = select(
            &[node.clone(), vip],
            &["enp0s31f6".into()],
            &Selection::Auto,
        );
        assert_eq!(got, vec![node]);
        // A /32 that is the interface's only address (point-to-point setups) stays.
        let only = IfAddr {
            iface: "ppp0".into(),
            ip: "10.1.2.3".parse().unwrap(),
            prefix: 32,
            index: 5,
        };
        assert_eq!(
            select(std::slice::from_ref(&only), &["ppp0".into()], &Selection::Auto),
            vec![only]
        );
    }

    #[test]
    fn auto_selection_skips_virtual_interfaces() {
        let all = vec![
            a("lo", "127.0.0.1"),
            a("wlo1", "192.168.68.70"),
            a("docker0", "172.17.0.1"),
            a("tailscale0", "100.74.123.76"),
            a("wlo1", "fe80::1"),
        ];
        let got = select(&all, &["wlo1".into()], &Selection::Auto);
        assert_eq!(got, vec![a("wlo1", "192.168.68.70")]);
        // No default route at all (isolated LAN): every physical interface.
        let got = select(&all, &[], &Selection::Auto);
        assert_eq!(got, vec![a("wlo1", "192.168.68.70")]);
    }
}
