//! Wiring: network watcher, mDNS events, and the reconcile loop that turns
//! discovered devices into served scanners, print queues and announcements.

mod reconcile;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::config::Config;
use crate::discovery::Registry;
use crate::discovery::brscan::Brscan;
use crate::discovery::cups::Spooler;
use crate::escl::http::{self, HttpState};
use crate::escl::jobs::ScannerService;
use crate::health::Health;
use crate::mdns::announcer::Announcer;
use crate::mdns::{BROWSE_TYPES, Mdns, MdnsEvent};
use crate::net::access::AccessPolicy;
use crate::net::addrs::{self, AddrSource};
use crate::net::listen::Listeners;

pub struct Deps {
    pub backend: Arc<dyn Backend>,
    pub addrs: Arc<dyn AddrSource>,
    pub mdns: Arc<dyn Mdns>,
    pub brscan: Option<Arc<dyn Brscan>>,
    pub spooler: Option<Arc<dyn Spooler>>,
}

/// A brscan4 registration; kept when the device drops off mDNS (a sleeping
/// printer often does) so its scanner stays available.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Registration {
    model: String,
    ip: Ipv4Addr,
    display: String,
}

struct Shared {
    cfg: Config,
    deps: Deps,
    http: Arc<HttpState>,
    listeners: Arc<Listeners>,
    policy: Arc<RwLock<AccessPolicy>>,
    announcer: Announcer,
    health: Arc<Health>,
    registry: Mutex<Registry>,
    registered: Mutex<BTreeMap<String, Registration>>,
    sane_dirty: Mutex<bool>,
    /// A backend's configuration changed (brsaneconfig4): the next probe
    /// must sane_exit/sane_init. Never set by timers or retries.
    sane_reinit: Mutex<bool>,
    last_probe: Mutex<Option<std::time::Instant>>,
    kick: Notify,
    debounce: Duration,
    /// Set by shutdown; held across a reconcile pass so shutdown can wait for
    /// one in flight (a blocking pass cannot be aborted) before withdrawing.
    stopped: Mutex<bool>,
}

pub struct App {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl App {
    pub async fn start(cfg: Config, deps: Deps) -> anyhow::Result<Self> {
        Self::start_with(cfg, deps, Duration::from_millis(300)).await
    }

    /// `debounce`: how long to collect mDNS events before acting (tests use less).
    pub async fn start_with(cfg: Config, deps: Deps, debounce: Duration) -> anyhow::Result<Self> {
        let health = Arc::new(Health::default());
        let http = Arc::new(HttpState {
            scanners: RwLock::new(Vec::new()),
            health: Arc::clone(&health),
            status_extra: RwLock::new(String::new()),
            icon: Bytes::from(crate::icon::scanner_png()),
            fallback_host: deps.mdns.host_name().trim_end_matches('.').to_owned(),
            body_timeout: cfg.body_timeout,
        });
        let policy = Arc::new(RwLock::new(AccessPolicy::default()));
        let listeners = Listeners::new(
            cfg.port,
            http::router(Arc::clone(&http)),
            Arc::clone(&policy),
            cfg.limits.clone(),
        );
        let announcer = Announcer::new(Arc::clone(&deps.mdns));
        let events = deps.mdns.take_events();
        let shared = Arc::new(Shared {
            cfg,
            deps,
            http,
            listeners,
            policy,
            announcer,
            health,
            registry: Mutex::new(Registry::default()),
            registered: Mutex::new(BTreeMap::new()),
            sane_dirty: Mutex::new(true),
            sane_reinit: Mutex::new(false),
            last_probe: Mutex::new(None),
            kick: Notify::new(),
            debounce,
            stopped: Mutex::new(false),
        });
        shared
            .health
            .set("mdns", shared.deps.mdns.alive(), "responder not running");
        let bind_failed = shared.apply_net();
        for ty in BROWSE_TYPES {
            if let Err(e) = shared.deps.mdns.browse(ty) {
                tracing::warn!(ty, error = %e, "browse failed");
            }
        }
        let mut tasks = vec![
            tokio::spawn(Arc::clone(&shared).net_loop(bind_failed)),
            tokio::spawn(Arc::clone(&shared).reconcile_loop()),
            tokio::spawn(Arc::clone(&shared).housekeeping_loop()),
        ];
        if let Some(rx) = events {
            tasks.push(tokio::spawn(Arc::clone(&shared).event_loop(rx)));
        }
        shared.kick.notify_one();
        Ok(Self { shared, tasks })
    }

    pub fn port(&self) -> u16 {
        self.shared.listeners.port()
    }

    pub fn health(&self) -> Arc<Health> {
        Arc::clone(&self.shared.health)
    }

    pub fn scanners(&self) -> Vec<Arc<ScannerService>> {
        self.shared.http.scanners.read().clone()
    }

    pub fn bound(&self) -> Vec<std::net::SocketAddr> {
        self.shared.listeners.bound()
    }

    pub fn announced(&self) -> Vec<(String, String)> {
        self.shared.announcer.published()
    }

    /// Ask for a reconcile now (tests; also used after config changes).
    pub fn kick(&self) {
        self.shared.kick.notify_one();
    }

    /// Stop serving and withdraw every announcement.
    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        // Waits for a reconcile pass that is still running on a blocking
        // thread, so it cannot re-announce after the withdrawals below.
        let shared = Arc::clone(&self.shared);
        let stop = tokio::task::spawn_blocking(move || *shared.stopped.lock() = true).await;
        if let Err(e) = stop {
            tracing::error!(error = %e, "shutdown: could not stop the reconcile loop");
        }
        for listener in self.shared.listeners.shutdown() {
            // An aborted task reports JoinError::Cancelled; that is the expected end.
            let _ = listener.await;
        }
        for key in self.shared.announcer.keys() {
            self.shared.announcer.remove(&key);
        }
    }
}

impl Shared {
    /// Re-read addresses: listeners, access policy, announced addresses.
    /// Returns true when some bind failed (retried soon).
    fn apply_net(&self) -> bool {
        let all = match self.deps.addrs.snapshot() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read interface addresses");
                return true;
            }
        };
        let selected = addrs::select(
            &all,
            &self.deps.addrs.default_route_ifaces(),
            &self.cfg.interfaces,
        );
        *self.policy.write() =
            AccessPolicy::build(&selected, &self.cfg.allow, self.cfg.allow_loopback);
        let mut ips: BTreeSet<IpAddr> = selected.iter().map(|a| a.ip).collect();
        let announce: Vec<IpAddr> = ips.iter().copied().collect();
        if self.cfg.allow_loopback {
            ips.insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        let failed = self.listeners.reconcile(&ips);
        let bound = self.listeners.bound();
        self.health
            .set("http", !bound.is_empty(), "no listening socket");
        let mut ifaces: Vec<String> = selected.iter().map(|a| a.iface.clone()).collect();
        ifaces.dedup();
        self.deps.mdns.set_interfaces(&ifaces);
        self.announcer.set_addrs(announce);
        !failed.is_empty()
    }

    /// `retry`: the previous attempt left an address unbound (e.g. the port
    /// is still held by the instance being replaced); try again soon.
    async fn net_loop(self: Arc<Self>, mut retry: bool) {
        let changed = self.deps.addrs.changed();
        loop {
            let wait = if retry {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(300)
            };
            tokio::select! {
                () = changed.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
            retry = self.apply_net();
        }
    }

    async fn event_loop(self: Arc<Self>, mut rx: tokio::sync::mpsc::UnboundedReceiver<MdnsEvent>) {
        while let Some(event) = rx.recv().await {
            if let MdnsEvent::NameChanged { original, new } = &event {
                self.announcer.on_name_change(original, new);
                continue;
            }
            // Our own announcements come back through the browser; they say
            // nothing new about the LAN.
            if let MdnsEvent::Resolved(r) = &event
                && r.host.eq_ignore_ascii_case(&self.deps.mdns.host_name())
            {
                continue;
            }
            let changed = self.registry.lock().apply(&event);
            if changed {
                self.kick.notify_one();
            }
        }
        self.health.set("mdns", false, "event stream ended");
    }

    async fn reconcile_loop(self: Arc<Self>) {
        let mut wait = self.cfg.rescan;
        loop {
            tokio::select! {
                () = self.kick.notified() => {}
                () = tokio::time::sleep(wait) => {
                    *self.sane_dirty.lock() = true;
                }
            }
            tokio::time::sleep(self.debounce).await;
            let me = Arc::clone(&self);
            wait = match tokio::task::spawn_blocking(move || me.reconcile()).await {
                Ok(true) => Duration::from_secs(5),
                Ok(false) => self.cfg.rescan,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile panicked");
                    Duration::from_secs(5)
                }
            };
        }
    }

    async fn housekeeping_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let alive = self.deps.mdns.alive();
            self.health.set("mdns", alive, "responder not running");
            let scanners = self.http.scanners.read().clone();
            for s in scanners {
                s.purge();
            }
        }
    }
}
