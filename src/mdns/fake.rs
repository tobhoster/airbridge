//! An in-process mDNS "network". Every `FakeMdns` is one host on it; foreign
//! devices (a printer that already speaks AirPrint, a rival announcing our
//! name) are injected with `FakeNet::announce`. It mirrors the behaviour of
//! the real responder that airbridge relies on: browse results include our own
//! services, and a name clash is resolved by renaming to `Name (2)`.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::Mutex;

use tokio::sync::mpsc;

use super::{Mdns, MdnsError, MdnsEvent, Resolved, ServiceSpec, fullname};

#[derive(Clone, Debug)]
struct Reg {
    owner: u64,
    spec: ServiceSpec,
    host: String,
    addrs: Vec<IpAddr>,
}

#[derive(Default)]
struct NetState {
    regs: BTreeMap<String, Reg>,
    browsers: Vec<(u64, String, mpsc::UnboundedSender<MdnsEvent>)>,
    next_owner: u64,
}

#[derive(Clone, Default)]
pub struct FakeNet {
    state: Arc<Mutex<NetState>>,
}

fn resolved(fullname: &str, r: &Reg) -> Resolved {
    Resolved {
        ty: r.spec.ty.clone(),
        instance: r.spec.instance.clone(),
        fullname: fullname.to_owned(),
        host: r.host.clone(),
        addrs: r.addrs.clone(),
        port: r.spec.port,
        txt: r
            .spec
            .txt
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect(),
    }
}

impl FakeNet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn host(&self, host_label: &str) -> Arc<FakeMdns> {
        let mut s = self.state.lock();
        s.next_owner += 1;
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(FakeMdns {
            net: self.clone(),
            owner: s.next_owner,
            host: format!("{host_label}.local."),
            tx,
            rx: Mutex::new(Some(rx)),
            alive: Mutex::new(true),
        })
    }

    /// A foreign device announces a service. Returns the full name.
    pub fn announce(&self, host: &str, spec: ServiceSpec, addrs: Vec<IpAddr>) -> String {
        self.register(0, host, &spec, &addrs).0
    }

    pub fn remove(&self, fullname: &str) {
        let mut s = self.state.lock();
        if let Some(reg) = s.regs.remove(fullname) {
            for (_, ty, tx) in &s.browsers {
                if *ty == reg.spec.ty {
                    // A browser whose host is gone no longer cares.
                    let _ = tx.send(MdnsEvent::Removed {
                        ty: ty.clone(),
                        fullname: fullname.to_owned(),
                    });
                }
            }
        }
    }

    /// Everything currently announced: (fullname, host, addrs).
    pub fn published(&self, ty: &str) -> Vec<(String, String, Vec<IpAddr>)> {
        let s = self.state.lock();
        s.regs
            .iter()
            .filter(|(_, r)| r.spec.ty == ty)
            .map(|(n, r)| (n.clone(), r.host.clone(), r.addrs.clone()))
            .collect()
    }

    pub fn spec(&self, fullname: &str) -> Option<ServiceSpec> {
        let s = self.state.lock();
        s.regs.get(fullname).map(|r| r.spec.clone())
    }

    /// Register under `spec.instance`, renaming on a clash with another owner.
    /// Returns (fullname, renamed-from).
    fn register(
        &self,
        owner: u64,
        host: &str,
        spec: &ServiceSpec,
        addrs: &[IpAddr],
    ) -> (String, Option<String>) {
        let mut s = self.state.lock();
        let original = fullname(&spec.instance, &spec.ty);
        let mut spec = spec.clone();
        let mut n = 1;
        while s
            .regs
            .get(&fullname(&spec.instance, &spec.ty))
            .is_some_and(|r| r.owner != owner)
        {
            n += 1;
            let base = fullname_instance(&original, &spec.ty);
            spec.instance = format!("{base} ({n})");
        }
        let name = fullname(&spec.instance, &spec.ty);
        let reg = Reg {
            owner,
            spec,
            host: host.to_owned(),
            addrs: addrs.to_vec(),
        };
        for (_, ty, tx) in &s.browsers {
            if *ty == reg.spec.ty {
                // A browser whose host is gone no longer cares.
                let _ = tx.send(MdnsEvent::Resolved(resolved(&name, &reg)));
            }
        }
        s.regs.insert(name.clone(), reg);
        let renamed = (name != original).then_some(original);
        (name, renamed)
    }
}

fn fullname_instance(full: &str, ty: &str) -> String {
    full.strip_suffix(&format!(".{ty}.local."))
        .unwrap_or(full)
        .to_owned()
}

pub struct FakeMdns {
    net: FakeNet,
    owner: u64,
    host: String,
    tx: mpsc::UnboundedSender<MdnsEvent>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<MdnsEvent>>>,
    alive: Mutex<bool>,
}

impl FakeMdns {
    pub fn kill(&self) {
        *self.alive.lock() = false;
    }
}

impl Mdns for FakeMdns {
    fn host_name(&self) -> String {
        self.host.clone()
    }

    fn publish(&self, spec: &ServiceSpec, addrs: &[IpAddr]) -> Result<String, MdnsError> {
        let (name, renamed) = self.net.register(self.owner, &self.host, spec, addrs);
        if let Some(original) = renamed {
            // The real responder reports renames through its monitor channel.
            let _ = self.tx.send(MdnsEvent::NameChanged {
                original,
                new: name.clone(),
            });
        }
        Ok(name)
    }

    fn withdraw(&self, fullname: &str) -> Result<(), MdnsError> {
        let owned = {
            let s = self.net.state.lock();
            s.regs.get(fullname).is_some_and(|r| r.owner == self.owner)
        };
        if owned {
            self.net.remove(fullname);
        }
        Ok(())
    }

    fn browse(&self, ty: &str) -> Result<(), MdnsError> {
        let mut s = self.net.state.lock();
        for (name, reg) in &s.regs {
            if reg.spec.ty == ty {
                // Receiver lives as long as this host; ignore a closed one.
                let _ = self.tx.send(MdnsEvent::Resolved(resolved(name, reg)));
            }
        }
        s.browsers
            .push((self.owner, ty.to_owned(), self.tx.clone()));
        Ok(())
    }

    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<MdnsEvent>> {
        self.rx.lock().take()
    }

    fn set_interfaces(&self, _ifaces: &[String]) {}

    fn alive(&self) -> bool {
        *self.alive.lock()
    }
}
