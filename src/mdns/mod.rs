//! mDNS/DNS-SD: announcing our scanners and printer queues, and browsing the
//! LAN for devices. `real` wraps the mdns-sd responder (no avahi, no dbus);
//! `fake` is an in-process network used by the test suite.

pub mod announcer;
pub mod fake;
pub mod real;

use std::collections::BTreeMap;
use std::net::IpAddr;

use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    pub instance: String,
    /// e.g. `_uscan._tcp`
    pub ty: String,
    /// e.g. `_universal` for AirPrint
    pub subtype: Option<String>,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

impl ServiceSpec {
    pub fn fullname(&self) -> String {
        fullname(&self.instance, &self.ty)
    }
}

pub fn fullname(instance: &str, ty: &str) -> String {
    format!("{instance}.{ty}.local.")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub ty: String,
    pub instance: String,
    pub fullname: String,
    /// e.g. `DELL6D6F61.local.`
    pub host: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub txt: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdnsEvent {
    Resolved(Resolved),
    Removed {
        ty: String,
        fullname: String,
    },
    /// The responder renamed one of our services after a conflict.
    NameChanged {
        original: String,
        new: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MdnsError {
    #[error("mdns responder: {0}")]
    Responder(String),
}

pub trait Mdns: Send + Sync + 'static {
    /// Our own host name, e.g. `airbridge-node-1.local.`
    fn host_name(&self) -> String;
    /// Publish (or re-publish) a service on `addrs`; returns its full name.
    fn publish(&self, spec: &ServiceSpec, addrs: &[IpAddr]) -> Result<String, MdnsError>;
    /// Withdraw a service (sends goodbye packets).
    fn withdraw(&self, fullname: &str) -> Result<(), MdnsError>;
    fn browse(&self, ty: &str) -> Result<(), MdnsError>;
    /// The event stream; can be taken once.
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<MdnsEvent>>;
    fn set_interfaces(&self, ifaces: &[String]);
    fn alive(&self) -> bool;
}

/// Service types airbridge watches on the LAN.
pub const BROWSE_TYPES: [&str; 7] = [
    "_scanner._tcp",
    "_uscan._tcp",
    "_uscans._tcp",
    "_ipp._tcp",
    "_ipps._tcp",
    "_pdl-datastream._tcp",
    "_printer._tcp",
];
