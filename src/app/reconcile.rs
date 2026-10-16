//! One reconcile pass (runs on a blocking thread): register Brother
//! scanners, re-probe SANE when needed and only while no scan runs, create
//! print queues, then bring announcements in line.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use super::{Registration, Shared};
use crate::discovery::{self, BrotherTarget, DiscoveryConfig, Plan, scanners};
use crate::escl::jobs::ScannerService;
use crate::escl::xml::{self, escape};
use crate::mdns::ServiceSpec;
use crate::scanner::Profile;

fn slug(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

impl Shared {
    fn base_url(&self) -> String {
        format!(
            "http://{}:{}",
            self.deps.mdns.host_name().trim_end_matches('.'),
            self.cfg.port
        )
    }

    /// Returns true when something should be retried soon.
    pub(super) fn reconcile(&self) -> bool {
        let stopped = self.stopped.lock();
        if *stopped {
            return false;
        }
        let dcfg = DiscoveryConfig {
            own_host: self.deps.mdns.host_name(),
            no_scan: self.cfg.no_scan.clone(),
            no_print: self.cfg.no_print.clone(),
            model_map: self.cfg.model_map.clone(),
        };
        let plan = {
            let reg = self.registry.lock();
            let brscan = self.deps.brscan.clone();
            discovery::plan(&reg, &dcfg, &move |m| {
                brscan.as_ref().is_some_and(|b| b.supported(m))
            })
        };
        let mut retry = self.register_brother(&plan);
        retry |= self.sync_scanners(&plan);
        let printers = self.sync_printers(&plan);
        self.update_status_page(&plan, &printers);
        retry
    }

    fn register_brother(&self, plan: &Plan) -> bool {
        let Some(brscan) = &self.deps.brscan else {
            return false;
        };
        let mut retry = false;
        let mut registered = self.registered.lock();
        for t in &plan.brother {
            let want = Registration {
                model: t.model.clone(),
                ip: t.ip,
                display: t.display.clone(),
            };
            if registered.get(&t.name) == Some(&want) {
                continue;
            }
            match brscan.register(&t.name, &t.model, t.ip) {
                Ok(()) => {
                    registered.insert(t.name.clone(), want);
                    *self.sane_dirty.lock() = true;
                }
                Err(e) => {
                    tracing::warn!(name = %t.name, error = %e, "brscan4 registration failed");
                    retry = true;
                }
            }
        }
        retry
    }

    fn all_idle(&self) -> bool {
        self.http.scanners.read().iter().all(|s| s.idle())
    }

    /// Returns true when a probe was postponed or a device could not be opened.
    fn sync_scanners(&self, plan: &Plan) -> bool {
        let dirty = *self.sane_dirty.lock();
        if dirty {
            if !self.all_idle() {
                tracing::info!("scan in progress; postponing scanner re-probe");
                return true;
            }
            match self.probe(plan) {
                Ok(retry) => {
                    *self.sane_dirty.lock() = retry;
                    *self.last_probe.lock() = Some(Instant::now());
                    self.announce_scanners();
                    return retry;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "SANE probe failed");
                    return true;
                }
            }
        }
        self.announce_scanners();
        false
    }

    fn probe(&self, plan: &Plan) -> Result<bool, crate::backend::BackendError> {
        let backend = &self.deps.backend;
        if let Err(e) = backend.reload() {
            tracing::warn!(error = %e, "SANE reload skipped");
        }
        let devices = backend.devices()?;
        let brother: Vec<BrotherTarget> = self
            .registered
            .lock()
            .iter()
            .map(|(name, r)| BrotherTarget {
                name: name.clone(),
                model: r.model.clone(),
                ip: r.ip,
                display: r.display.clone(),
            })
            .collect();
        let chosen = scanners::choose(
            &devices,
            &brother,
            &plan.native_scanner_ips,
            &self.cfg.no_scan,
        );
        let existing = self.http.scanners.read().clone();
        let mut next: Vec<Arc<ScannerService>> = Vec::new();
        let mut retry = false;
        for c in &chosen {
            if let Some(s) = existing
                .iter()
                .find(|s| s.profile.device.name == c.device.name && s.name == c.display)
            {
                next.push(Arc::clone(s));
                continue;
            }
            let profile = backend
                .open(&c.device.name)
                .map(|session| Profile::from_options(c.device.clone(), &session.options()));
            match profile {
                Ok(Some(profile)) => {
                    // A replacement for the same scanner (e.g. WSD -> brother4)
                    // keeps its path; otherwise the first scanner gets /eSCL.
                    let inherited = existing
                        .iter()
                        .find(|s| slug(&s.name) == slug(&c.key))
                        .map(|s| s.root.clone());
                    let root = inherited.unwrap_or_else(|| {
                        if next.iter().chain(&existing).any(|s| s.root == "/eSCL") {
                            format!("/scanners/{}/eSCL", slug(&c.key))
                        } else {
                            "/eSCL".to_owned()
                        }
                    });
                    let uuid = Uuid::new_v5(
                        &Uuid::NAMESPACE_URL,
                        format!("airbridge:scanner:{}", c.key).as_bytes(),
                    );
                    tracing::info!(device = %c.device.name, name = %c.display, %root, "serving scanner");
                    next.push(ScannerService::new(
                        root,
                        uuid,
                        c.display.clone(),
                        profile,
                        Arc::clone(backend),
                        self.cfg.jobs.clone(),
                        Arc::clone(&self.health),
                    ));
                }
                Ok(None) => {
                    tracing::info!(device = %c.device.name, "not an image scanner; skipped")
                }
                Err(e) => {
                    tracing::warn!(device = %c.device.name, error = %e, "cannot open scanner; will retry");
                    retry = true;
                    // Keep serving a previously known instance (a sleeping device).
                    if let Some(s) = existing.iter().find(|s| slug(&s.name) == slug(&c.key)) {
                        next.push(Arc::clone(s));
                    }
                }
            }
        }
        // A device that dropped out of the SANE list (asleep, WSD silent)
        // keeps its scanner unless something else took its identity or it
        // now speaks AirScan itself.
        for s in &existing {
            let listed = devices.iter().any(|d| d.name == s.profile.device.name);
            let superseded = next.iter().any(|n| slug(&n.name) == slug(&s.name));
            if !listed && !superseded {
                next.push(Arc::clone(s));
            }
        }
        for s in &existing {
            if !next.iter().any(|n| Arc::ptr_eq(n, s)) {
                tracing::info!(name = %s.name, "no longer serving scanner");
                self.health
                    .remove(&format!("scanner:{}", s.profile.device.name));
            }
        }
        *self.http.scanners.write() = next;
        Ok(retry)
    }

    fn announce_scanners(&self) {
        let scanners = self.http.scanners.read().clone();
        let base = self.base_url();
        let mut keys = BTreeSet::new();
        for s in &scanners {
            let key = format!("scan:{}", s.uuid);
            let rs = s.root.trim_start_matches('/').to_owned();
            let txt = xml::uscan_txt(
                &s.profile,
                &s.name,
                &self.cfg.location,
                &s.uuid.to_string(),
                &rs,
                &base,
            );
            self.announcer.set(
                &key,
                ServiceSpec {
                    instance: s.name.clone(),
                    ty: "_uscan._tcp".into(),
                    subtype: None,
                    port: self.cfg.port,
                    txt,
                },
            );
            keys.insert(key);
        }
        for key in self.announcer.keys() {
            if key.starts_with("scan:") && !keys.contains(&key) {
                self.announcer.remove(&key);
            }
        }
    }

    /// Creates queues for discovered printers and announces every shared
    /// queue. Returns (queue, description) rows for the status page.
    fn sync_printers(&self, plan: &Plan) -> Vec<(String, String)> {
        let Some(spooler) = &self.deps.spooler else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for t in &plan.printers {
            match spooler.driver_for(&t.device_id) {
                Ok(Some(driver)) => {
                    if let Err(e) = spooler.ensure_queue(t, &driver, &self.cfg.location) {
                        tracing::warn!(queue = %t.queue, error = %e, "cannot create print queue");
                    }
                }
                Ok(None) => rows.push((
                    t.display.clone(),
                    format!("no CUPS driver matches {}", t.device_id),
                )),
                Err(e) => rows.push((t.display.clone(), e.to_string())),
            }
        }
        let queues = match spooler.queues() {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list print queues");
                return rows;
            }
        };
        let mut keys = BTreeSet::new();
        for q in &queues {
            let key = format!("print:{}", q.name);
            let uuid = Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("airbridge:printer:{}", q.name).as_bytes(),
            );
            let txt = discovery::cups::airprint_txt(
                q,
                &self.deps.mdns.host_name(),
                &self.cfg.location,
                &uuid.to_string(),
            );
            self.announcer.set(
                &key,
                ServiceSpec {
                    instance: q.info.clone(),
                    ty: "_ipp._tcp".into(),
                    subtype: Some("_universal".into()),
                    port: 631,
                    txt,
                },
            );
            rows.push((
                q.info.clone(),
                format!("AirPrint queue {} -> {}", q.name, q.uri),
            ));
            keys.insert(key);
        }
        for key in self.announcer.keys() {
            if key.starts_with("print:") && !keys.contains(&key) {
                self.announcer.remove(&key);
            }
        }
        rows
    }

    fn update_status_page(&self, plan: &Plan, printers: &[(String, String)]) {
        let mut html = String::from("<h2>Printers (AirPrint)</h2><table>");
        for (name, what) in printers {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                escape(name),
                escape(what)
            ));
        }
        html.push_str("</table><h2>Seen, not bridged</h2><table>");
        for (host, why) in &plan.skipped {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                escape(host),
                escape(why)
            ));
        }
        html.push_str("</table>");
        *self.http.status_extra.write() = html;
    }
}
