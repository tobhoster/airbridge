//! Who may talk to the server: the subnets of the served interfaces, plus
//! loopback and any configured extra networks. Rebuilt on every address
//! change, never frozen at startup.

use std::net::IpAddr;

use ipnet::IpNet;

use super::addrs::IfAddr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessPolicy {
    nets: Vec<IpNet>,
    loopback: bool,
}

impl AccessPolicy {
    pub fn build(selected: &[IfAddr], extra: &[IpNet], allow_loopback: bool) -> Self {
        let mut nets: Vec<IpNet> = selected
            .iter()
            .filter_map(|a| IpNet::new(a.ip, a.prefix).ok().map(|n| n.trunc()))
            .collect();
        nets.extend(extra.iter().copied());
        nets.sort();
        nets.dedup();
        Self {
            nets,
            loopback: allow_loopback,
        }
    }

    pub fn allows(&self, peer: IpAddr) -> bool {
        let peer = match peer {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer, IpAddr::V4),
            v4 => v4,
        };
        (self.loopback && peer.is_loopback()) || self.nets.iter().any(|n| n.contains(&peer))
    }

    pub fn nets(&self) -> &[IpNet] {
        &self.nets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_allowed_container_and_vpn_ranges_denied() {
        let lan = IfAddr {
            iface: "wlo1".into(),
            ip: "192.168.68.70".parse().unwrap(),
            prefix: 22,
            index: 2,
        };
        let p = AccessPolicy::build(&[lan], &[], true);
        assert!(p.allows("192.168.70.5".parse().unwrap()));
        assert!(p.allows("::ffff:192.168.68.9".parse().unwrap()));
        assert!(p.allows("127.0.0.1".parse().unwrap()));
        assert!(!p.allows("172.17.0.2".parse().unwrap()), "docker bridge");
        assert!(!p.allows("100.74.1.2".parse().unwrap()), "tailscale");
        assert!(
            !p.allows("192.168.72.1".parse().unwrap()),
            "outside the /22"
        );
    }
}
