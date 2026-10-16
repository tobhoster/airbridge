//! Keeps the set of announced services in line with what airbridge serves
//! and the addresses it serves on. An address change withdraws every service
//! (goodbye packets for the old addresses) and announces it again on the new
//! ones; a responder rename after a name conflict is recorded, not fatal.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use super::{Mdns, ServiceSpec};

struct Entry {
    spec: ServiceSpec,
    published: Option<String>,
}

#[derive(Default)]
struct State {
    addrs: Vec<IpAddr>,
    entries: BTreeMap<String, Entry>,
}

pub struct Announcer {
    mdns: Arc<dyn Mdns>,
    state: Mutex<State>,
}

impl Announcer {
    pub fn new(mdns: Arc<dyn Mdns>) -> Self {
        Self {
            mdns,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    fn publish(&self, addrs: &[IpAddr], entry: &mut Entry) {
        if addrs.is_empty() {
            return;
        }
        match self.mdns.publish(&entry.spec, addrs) {
            Ok(name) => {
                tracing::info!(service = %name, ?addrs, "announced");
                entry.published = Some(name);
            }
            Err(e) => {
                tracing::error!(service = %entry.spec.instance, error = %e, "announce failed")
            }
        }
    }

    fn withdraw(&self, entry: &mut Entry) {
        if let Some(name) = entry.published.take() {
            if let Err(e) = self.mdns.withdraw(&name) {
                tracing::warn!(service = %name, error = %e, "withdraw failed");
            } else {
                tracing::info!(service = %name, "withdrawn");
            }
        }
    }

    /// Announce `spec` under `key`, re-announcing only if it changed.
    pub fn set(&self, key: &str, spec: ServiceSpec) {
        let mut st = self.state();
        let addrs = st.addrs.clone();
        match st.entries.get_mut(key) {
            Some(e) if e.spec == spec && (e.published.is_some() || addrs.is_empty()) => {}
            Some(e) => {
                self.withdraw(e);
                e.spec = spec;
                self.publish(&addrs, e);
            }
            None => {
                let mut e = Entry {
                    spec,
                    published: None,
                };
                self.publish(&addrs, &mut e);
                st.entries.insert(key.to_owned(), e);
            }
        }
    }

    pub fn remove(&self, key: &str) {
        let mut st = self.state();
        if let Some(mut e) = st.entries.remove(key) {
            self.withdraw(&mut e);
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.state().entries.keys().cloned().collect()
    }

    pub fn set_addrs(&self, addrs: Vec<IpAddr>) {
        let mut st = self.state();
        if st.addrs == addrs {
            return;
        }
        st.addrs = addrs.clone();
        for e in st.entries.values_mut() {
            self.withdraw(e);
        }
        for e in st.entries.values_mut() {
            self.publish(&addrs, e);
        }
    }

    pub fn on_name_change(&self, original: &str, new: &str) {
        let mut st = self.state();
        for e in st.entries.values_mut() {
            if e.published.as_deref() == Some(original) {
                tracing::warn!(original, new, "service renamed after a name conflict");
                e.published = Some(new.to_owned());
            }
        }
    }

    /// (key, published full name)
    pub fn published(&self) -> Vec<(String, String)> {
        self.state()
            .entries
            .iter()
            .filter_map(|(k, e)| e.published.clone().map(|p| (k.clone(), p)))
            .collect()
    }
}
