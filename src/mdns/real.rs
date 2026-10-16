//! mdns-sd backed responder/browser. Event receivers are drained by small
//! blocking threads that forward into one tokio channel.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{DaemonEvent, DaemonStatus, IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

use super::{Mdns, MdnsError, MdnsEvent, Resolved, ServiceSpec};

pub struct RealMdns {
    daemon: ServiceDaemon,
    host: String,
    tx: mpsc::UnboundedSender<MdnsEvent>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<MdnsEvent>>>,
    ifaces: Mutex<Vec<String>>,
}

fn err(e: impl std::fmt::Display) -> MdnsError {
    MdnsError::Responder(e.to_string())
}

fn strip_domain(ty_domain: &str) -> String {
    ty_domain
        .trim_end_matches('.')
        .trim_end_matches(".local")
        .to_owned()
}

impl RealMdns {
    /// `host_label` becomes `<host_label>.local.`; pick one that no other
    /// responder on the host uses (avahi may already own the plain host name).
    pub fn start(host_label: &str) -> Result<Self, MdnsError> {
        let daemon = ServiceDaemon::new().map_err(err)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let monitor = daemon.monitor().map_err(err)?;
        let mtx = tx.clone();
        std::thread::Builder::new()
            .name("mdns-monitor".into())
            .spawn(move || {
                while let Ok(event) = monitor.recv() {
                    if let DaemonEvent::NameChange(change) = event {
                        tracing::warn!(original = %change.original, new = %change.new_name, "mDNS name conflict resolved by renaming");
                        if mtx.send(MdnsEvent::NameChanged { original: change.original, new: change.new_name }).is_err() {
                            return;
                        }
                    }
                }
            })
            .map_err(err)?;
        Ok(Self {
            daemon,
            host: format!("{host_label}.local."),
            tx,
            rx: Mutex::new(Some(rx)),
            ifaces: Mutex::new(Vec::new()),
        })
    }
}

impl Mdns for RealMdns {
    fn host_name(&self) -> String {
        self.host.clone()
    }

    fn publish(&self, spec: &ServiceSpec, addrs: &[IpAddr]) -> Result<String, MdnsError> {
        let ty_domain = match &spec.subtype {
            Some(sub) => format!("{sub}._sub.{}.local.", spec.ty),
            None => format!("{}.local.", spec.ty),
        };
        let txt: Vec<(&str, &str)> = spec
            .txt
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let info = ServiceInfo::new(
            &ty_domain,
            &spec.instance,
            &self.host,
            addrs,
            spec.port,
            &txt[..],
        )
        .map_err(err)?;
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info).map_err(err)?;
        Ok(fullname)
    }

    fn withdraw(&self, fullname: &str) -> Result<(), MdnsError> {
        // The daemon processes commands in order, so a following
        // re-registration cannot overtake the goodbye; the status receiver
        // is not needed.
        self.daemon.unregister(fullname).map_err(err)?;
        Ok(())
    }

    fn browse(&self, ty: &str) -> Result<(), MdnsError> {
        let rx = self.daemon.browse(&format!("{ty}.local.")).map_err(err)?;
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name(format!("mdns-{}", ty.trim_start_matches('_')))
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    let out = match event {
                        ServiceEvent::ServiceResolved(r) => {
                            let ty = strip_domain(&r.ty_domain);
                            let instance = r
                                .fullname
                                .strip_suffix(&format!(".{}", r.ty_domain))
                                .unwrap_or(&r.fullname)
                                .to_owned();
                            let txt: BTreeMap<String, String> = r
                                .txt_properties
                                .iter()
                                .map(|p| (p.key().to_ascii_lowercase(), p.val_str().to_owned()))
                                .collect();
                            let mut addrs: Vec<IpAddr> =
                                r.addresses.iter().map(|a| a.to_ip_addr()).collect();
                            addrs.sort();
                            MdnsEvent::Resolved(Resolved {
                                ty,
                                instance,
                                fullname: r.fullname.clone(),
                                host: r.host.clone(),
                                addrs,
                                port: r.port,
                                txt,
                            })
                        }
                        ServiceEvent::ServiceRemoved(ty_domain, fullname) => MdnsEvent::Removed {
                            ty: strip_domain(&ty_domain),
                            fullname,
                        },
                        _ => continue,
                    };
                    if tx.send(out).is_err() {
                        return;
                    }
                }
            })
            .map_err(err)?;
        Ok(())
    }

    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<MdnsEvent>> {
        self.rx.lock().take()
    }

    fn set_interfaces(&self, ifaces: &[String]) {
        let mut current = self.ifaces.lock();
        if current.as_slice() == ifaces || ifaces.is_empty() {
            return;
        }
        if let Err(e) = self.daemon.disable_interface(IfKind::All) {
            tracing::warn!(error = %e, "mdns: cannot restrict interfaces");
            return;
        }
        for name in ifaces {
            if let Err(e) = self.daemon.enable_interface(IfKind::Name(name.clone())) {
                tracing::warn!(iface = %name, error = %e, "mdns: cannot enable interface");
            }
        }
        *current = ifaces.to_vec();
    }

    fn alive(&self) -> bool {
        self.daemon
            .status()
            .ok()
            .and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            .is_some_and(|s| matches!(s, DaemonStatus::Running))
    }
}
