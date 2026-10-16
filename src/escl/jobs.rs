//! Scan jobs for one scanner.
//!
//! One job at a time: a POST while another job is unfinished gets 503. Each
//! page runs on its own OS thread (SANE calls block, see `page.rs`).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::ticket::{self, Ticket, TicketError};
use super::xml::{self, JobState, JobView};
use crate::backend::{Backend, BackendError, CancelHandle, Session};
use crate::health::Health;
use crate::scanner::{InputSource, Profile};

#[derive(Clone, Debug)]
pub struct JobConfig {
    /// Abort a page that delivers no data for this long.
    pub hang_timeout: Duration,
    /// After cancelling, how long the backend gets before the process is
    /// declared unhealthy.
    pub wedge_grace: Duration,
    /// Cancel a job nobody fetches pages from.
    pub pending_timeout: Duration,
    /// Keep finished jobs in ScannerStatus this long.
    pub retention: Duration,
    pub gray_gamma: f64,
    pub color_gamma: f64,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            hang_timeout: Duration::from_secs(120),
            wedge_grace: Duration::from_secs(30),
            pending_timeout: Duration::from_secs(60),
            retention: Duration::from_secs(300),
            // AirSane's shipped default: compensates macOS' 1.8 display gamma.
            gray_gamma: 0.555_555,
            color_gamma: 0.555_555,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    #[error(transparent)]
    Ticket(#[from] TicketError),
    #[error("scanner busy")]
    Busy,
}

pub enum NextDocument {
    NotFound,
    Busy,
    /// Empty feeder on the first page.
    AdfEmpty,
    Failed(String),
    Page {
        mime: &'static str,
        body: mpsc::Receiver<io::Result<Bytes>>,
    },
}

pub(super) struct Active {
    pub(super) cancel: Mutex<Option<CancelHandle>>,
    /// Milliseconds since the service epoch of the last progress.
    pub(super) progress: AtomicU64,
    pub(super) done: AtomicBool,
    pub(super) aborted: AtomicBool,
}

impl Active {
    pub(super) fn cancel(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        if let Some(cancel) = self.cancel.lock().as_ref() {
            cancel();
        }
    }
}

pub(super) struct Job {
    pub(super) id: Uuid,
    created: Instant,
    pub(super) last_activity: Instant,
    pub(super) ticket: Ticket,
    pub(super) state: JobState,
    pub(super) reason: &'static str,
    detail: String,
    pub(super) images: u32,
    pub(super) session: Option<Box<dyn Session>>,
    pub(super) active: Option<Arc<Active>>,
    finished: Option<Instant>,
}

impl Job {
    pub(super) fn finish(&mut self, state: JobState, reason: &'static str, detail: &str) {
        self.state = state;
        self.reason = reason;
        self.detail = detail.to_owned();
        self.session = None;
        self.finished.get_or_insert_with(Instant::now);
    }
}

#[derive(Default)]
pub(super) struct Table {
    pub(super) jobs: Vec<Job>,
    pub(super) adf_empty: bool,
}

pub struct ScannerService {
    /// eSCL root path, e.g. `/eSCL` or `/scanners/<id>/eSCL`.
    pub root: String,
    pub uuid: Uuid,
    pub name: String,
    pub profile: Profile,
    pub(super) backend: Arc<dyn Backend>,
    pub(super) cfg: JobConfig,
    table: Mutex<Table>,
    pub(super) health: Arc<Health>,
    pub(super) health_key: String,
    pub(super) epoch: Instant,
    pub(super) runtime: tokio::runtime::Handle,
}

impl ScannerService {
    pub fn new(
        root: String,
        uuid: Uuid,
        name: String,
        profile: Profile,
        backend: Arc<dyn Backend>,
        cfg: JobConfig,
        health: Arc<Health>,
    ) -> Arc<Self> {
        let health_key = format!("scanner:{}", profile.device.name);
        health.set(&health_key, true, "");
        Arc::new(Self {
            root,
            uuid,
            name,
            profile,
            backend,
            cfg,
            table: Mutex::new(Table::default()),
            health,
            health_key,
            epoch: Instant::now(),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    pub(super) fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock()
    }

    pub(super) fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn capabilities(&self, admin_uri: &str) -> String {
        xml::capabilities(
            &self.profile,
            &xml::CapsContext {
                make_and_model: &self.name,
                uuid: &self.uuid.to_string(),
                admin_uri,
                formats: &["application/pdf", "image/jpeg", "image/png"],
            },
        )
    }

    pub fn status(&self) -> String {
        self.purge();
        let t = self.table();
        // AirSane semantics: a queued job that has not started is still Idle.
        let busy = t
            .jobs
            .iter()
            .any(|j| j.active.is_some() || (!j.state.is_terminal() && j.images > 0));
        let ids: Vec<(String, String)> = t
            .jobs
            .iter()
            .rev()
            .map(|j| (format!("{}/ScanJobs/{}", self.root, j.id), j.id.to_string()))
            .collect();
        let views: Vec<JobView<'_>> = t
            .jobs
            .iter()
            .rev()
            .zip(&ids)
            .map(|(j, (uri, uuid))| JobView {
                uri,
                uuid,
                age_secs: j.created.elapsed().as_secs(),
                state: j.state,
                images_completed: j.images,
                reason: j.reason,
            })
            .collect();
        xml::status(busy, &views, t.adf_empty)
    }

    /// True when no job is unfinished; reconfiguration waits for this.
    pub fn idle(&self) -> bool {
        self.purge();
        self.table().jobs.iter().all(|j| j.state.is_terminal())
    }

    pub fn create_job(&self, body: &[u8]) -> Result<Uuid, CreateError> {
        let raw = ticket::parse(body)?;
        let ticket = ticket::validate(&raw, &self.profile)?;
        self.purge();
        let mut t = self.table();
        if t.jobs.iter().any(|j| !j.state.is_terminal()) {
            return Err(CreateError::Busy);
        }
        let id = Uuid::new_v4();
        let now = Instant::now();
        t.jobs.push(Job {
            id,
            created: now,
            last_activity: now,
            ticket,
            state: JobState::Pending,
            reason: "JobQueued",
            detail: String::new(),
            images: 0,
            session: None,
            active: None,
            finished: None,
        });
        Ok(id)
    }

    pub fn cancel(&self, id: Uuid) -> bool {
        let mut t = self.table();
        let Some(job) = t.jobs.iter_mut().find(|j| j.id == id) else {
            return false;
        };
        if !job.state.is_terminal() {
            if let Some(active) = &job.active {
                active.cancel();
            }
            job.finish(
                JobState::Canceled,
                "JobCanceledByUser",
                "cancelled by client",
            );
        }
        true
    }

    /// Cancel abandoned jobs and forget old finished ones.
    pub fn purge(&self) {
        let mut t = self.table();
        let now = Instant::now();
        for job in &mut t.jobs {
            if !job.state.is_terminal()
                && job.active.is_none()
                && now.duration_since(job.last_activity) > self.cfg.pending_timeout
            {
                tracing::info!(job = %job.id, "cancelling abandoned job");
                job.finish(JobState::Canceled, "AbortedBySystem", "no client activity");
            }
        }
        let retention = self.cfg.retention;
        t.jobs
            .retain(|j| j.finished.is_none_or(|f| now.duration_since(f) < retention));
    }

    /// Job state, reason and failure detail (status page, tests).
    pub fn job_state(&self, id: Uuid) -> Option<(JobState, &'static str, String)> {
        self.table()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| (j.state, j.reason, j.detail.clone()))
    }

    /// Mark the job as scanning a page; synchronous so no lock spans an await.
    fn claim_page(&self, id: Uuid) -> Claim {
        let mut t = self.table();
        let Some(job) = t.jobs.iter_mut().find(|j| j.id == id) else {
            return Claim::NotFound;
        };
        if job.active.is_some() {
            return Claim::Wait;
        }
        if job.state.is_terminal() {
            return Claim::NotFound;
        }
        if job.ticket.page.source == InputSource::Platen && job.images >= 1 {
            job.finish(JobState::Completed, "JobCompletedSuccessfully", "");
            return Claim::NotFound;
        }
        let active = Arc::new(Active {
            cancel: Mutex::new(None),
            progress: AtomicU64::new(self.now_ms()),
            done: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
        });
        job.state = JobState::Processing;
        job.reason = "JobScanning";
        job.last_activity = Instant::now();
        job.active = Some(Arc::clone(&active));
        Claim::Ready {
            session: job.session.take(),
            ticket: job.ticket.clone(),
            active,
            first_page: job.images == 0,
        }
    }

    pub async fn next_document(self: &Arc<Self>, id: Uuid) -> NextDocument {
        let (start_tx, start_rx) = oneshot::channel();
        let (body_tx, body_rx) = mpsc::channel::<io::Result<Bytes>>(16);
        // A client asks for the next page the moment the previous body ends,
        // which can be a moment before the worker records the page; give the
        // worker a short grace before calling the job busy.
        let grace = Instant::now() + Duration::from_secs(5);
        let (session, ticket, active, first_page) = loop {
            match self.claim_page(id) {
                Claim::NotFound => return NextDocument::NotFound,
                Claim::Wait if Instant::now() > grace => return NextDocument::Busy,
                Claim::Wait => tokio::time::sleep(Duration::from_millis(5)).await,
                Claim::Ready {
                    session,
                    ticket,
                    active,
                    first_page,
                } => break (session, ticket, active, first_page),
            }
        };
        let mime = ticket.format.mime();
        let svc = Arc::clone(self);
        let worker_active = Arc::clone(&active);
        let spawned = std::thread::Builder::new()
            .name("scan-page".into())
            .spawn(move || {
                let outcome = svc.run_page(session, &ticket, &worker_active, start_tx, body_tx);
                svc.settle(id, outcome, &worker_active);
            });
        if let Err(e) = spawned {
            let mut t = self.table();
            if let Some(job) = t.jobs.iter_mut().find(|j| j.id == id) {
                job.active = None;
                job.finish(JobState::Aborted, "AbortedBySystem", &e.to_string());
            }
            return NextDocument::Failed(format!("cannot start scan thread: {e}"));
        }
        self.spawn_watchdog(id, Arc::clone(&active));
        let started = start_rx.await;
        if !matches!(started, Ok(Ok(()))) {
            // The page never started: let the worker record the outcome
            // first, so a client reading ScannerStatus next sees the result.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !active.done.load(Ordering::SeqCst) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        match started {
            Ok(Ok(())) => NextDocument::Page {
                mime,
                body: body_rx,
            },
            Ok(Err(BackendError::NoDocs)) if first_page => NextDocument::AdfEmpty,
            Ok(Err(BackendError::NoDocs)) => NextDocument::NotFound,
            Ok(Err(e)) => NextDocument::Failed(e.to_string()),
            // The worker ended without reporting (watchdog abort or cancel).
            Err(_) => NextDocument::Failed("scan aborted".into()),
        }
    }
}

enum Claim {
    NotFound,
    /// A page of this job is still in flight.
    Wait,
    Ready {
        session: Option<Box<dyn Session>>,
        ticket: Ticket,
        active: Arc<Active>,
        first_page: bool,
    },
}
