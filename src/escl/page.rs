//! One page of a job: the blocking worker that drives SANE and the encoder,
//! the watchdog that aborts a page with no progress, and the bookkeeping that
//! records the outcome.
//!
//! The watchdog cancels after `hang_timeout` without data. If the backend
//! ignores the cancel for `wedge_grace`, the scanner reports unhealthy, so a
//! wedged driver ends in a container restart instead of a scanner that is
//! silently stuck forever.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::jobs::{Active, ScannerService};
use super::pipe::{CHUNK, ChannelSink, SaneRows};
use super::ticket::Ticket;
use super::xml::JobState;
use crate::backend::{BackendError, FrameFormat, Session};
use crate::imaging;
use crate::scanner::{self, ColorMode, InputSource};

pub(super) enum Outcome {
    PageDone(Box<dyn Session>),
    NoDocs,
    ClientGone,
    Failed(String),
}

type StartTx = oneshot::Sender<Result<(), BackendError>>;

impl ScannerService {
    pub(super) fn spawn_watchdog(self: &Arc<Self>, id: Uuid, active: Arc<Active>) {
        let svc = Arc::clone(self);
        let tick =
            (self.cfg.hang_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                if active.done.load(Ordering::SeqCst) {
                    return;
                }
                let idle = svc
                    .now_ms()
                    .saturating_sub(active.progress.load(Ordering::SeqCst));
                if Duration::from_millis(idle) >= svc.cfg.hang_timeout {
                    break;
                }
            }
            tracing::warn!(job = %id, "scan made no progress, aborting");
            active.cancel();
            {
                let mut t = svc.table();
                if let Some(job) = t.jobs.iter_mut().find(|j| j.id == id)
                    && !job.state.is_terminal()
                {
                    job.finish(
                        JobState::Aborted,
                        "AbortedBySystem",
                        "scanner stopped responding",
                    );
                }
            }
            let deadline = Instant::now() + svc.cfg.wedge_grace;
            while !active.done.load(Ordering::SeqCst) && Instant::now() < deadline {
                tokio::time::sleep(tick).await;
            }
            if !active.done.load(Ordering::SeqCst) {
                svc.health
                    .set(&svc.health_key, false, "scanner backend ignored cancel");
                while !active.done.load(Ordering::SeqCst) {
                    tokio::time::sleep(tick).await;
                }
                svc.health.set(&svc.health_key, true, "");
            }
        });
    }

    pub(super) fn run_page(
        &self,
        session: Option<Box<dyn Session>>,
        ticket: &Ticket,
        active: &Active,
        start_tx: StartTx,
        body_tx: mpsc::Sender<io::Result<Bytes>>,
    ) -> Outcome {
        let fail = |start_tx: StartTx, e: BackendError| {
            // The requester may have gone already; the outcome still lands in
            // the job table, so a dropped receiver loses nothing.
            let _ = start_tx.send(Err(e.clone()));
            if e == BackendError::NoDocs {
                Outcome::NoDocs
            } else {
                Outcome::Failed(e.to_string())
            }
        };
        let fresh = session.is_none();
        let mut session = match session {
            Some(s) => s,
            None => match self.backend.open(&self.profile.device.name) {
                Ok(s) => s,
                Err(e) => return fail(start_tx, e),
            },
        };
        *active.cancel.lock() = Some(session.cancel_handle());
        if active.aborted.load(Ordering::SeqCst) {
            return fail(start_tx, BackendError::Cancelled);
        }
        if fresh && let Err(e) = scanner::apply(&self.profile, session.as_mut(), &ticket.page) {
            return fail(start_tx, e);
        }
        let params = match session.start() {
            Ok(p) => p,
            Err(e) => return fail(start_tx, e),
        };
        if params.format == FrameFormat::SingleChannel || params.pixels_per_line == 0 {
            session.cancel();
            return fail(
                start_tx,
                BackendError::Failed("unsupported frame format".into()),
            );
        }
        active.progress.store(self.now_ms(), Ordering::SeqCst);
        if start_tx.send(Ok(())).is_err() {
            session.cancel();
            return Outcome::ClientGone;
        }
        let gamma = match ticket.page.color {
            ColorMode::Gray8 => self.cfg.gray_gamma,
            ColorMode::Rgb24 => self.cfg.color_gamma,
        };
        let mut sink = ChannelSink {
            tx: body_tx,
            buf: Vec::with_capacity(CHUNK),
            runtime: &self.runtime,
            timeout: self.cfg.hang_timeout,
            closed: false,
            progress: &active.progress,
            epoch: self.epoch,
        };
        // Flatbed pages come out exactly the size of the requested region
        // (see SaneRows). Feeder pages keep the paper's own length.
        let target = (ticket.page.source == InputSource::Platen).then(|| {
            let px = |units: u32| {
                (f64::from(units) * f64::from(ticket.page.resolution) / scanner::UNITS_PER_INCH)
                    .round() as usize
            };
            let (_, _, w, h) = ticket.page.region;
            (px(w).max(1), px(h).max(1))
        });
        let (result, backend_error) = {
            let mut rows = SaneRows::new(
                session.as_mut(),
                params,
                ticket.page.color,
                gamma,
                target,
                &active.progress,
                self.epoch,
            );
            let r = imaging::encode(ticket.format, &mut rows, ticket.page.resolution, &mut sink)
                .and_then(|()| {
                    use std::io::Write;
                    sink.flush()
                });
            (r, rows.error.take())
        };
        match result {
            Ok(()) => Outcome::PageDone(session),
            Err(e) => {
                session.cancel();
                let msg = backend_error.map_or_else(|| e.to_string(), |b| b.to_string());
                if sink.closed {
                    Outcome::ClientGone
                } else {
                    sink.fail(&msg);
                    Outcome::Failed(msg)
                }
            }
        }
    }

    pub(super) fn settle(&self, id: Uuid, outcome: Outcome, active: &Active) {
        let mut t = self.table();
        let mut adf_empty = None;
        if let Some(job) = t.jobs.iter_mut().find(|j| j.id == id) {
            job.active = None;
            job.last_activity = Instant::now();
            let feeder = job.ticket.page.source == InputSource::Feeder;
            match outcome {
                Outcome::PageDone(session) => {
                    job.images += 1;
                    tracing::info!(job = %id, page = job.images, "page delivered");
                    if job.state.is_terminal() {
                        // Cancelled while the last bytes went out: `session` drops (closes) here.
                    } else if feeder {
                        job.state = JobState::Pending;
                        job.reason = "None";
                        job.session = Some(session);
                        adf_empty = Some(false);
                    } else {
                        job.finish(JobState::Completed, "JobCompletedSuccessfully", "");
                    }
                }
                Outcome::NoDocs => {
                    adf_empty = Some(true);
                    if job.images == 0 {
                        job.finish(
                            JobState::Aborted,
                            "AbortedBySystem",
                            "document feeder is empty",
                        );
                    } else {
                        job.finish(JobState::Completed, "JobCompletedSuccessfully", "");
                    }
                }
                Outcome::ClientGone => {
                    if !job.state.is_terminal() {
                        job.finish(
                            JobState::Canceled,
                            "JobCanceledByUser",
                            "client disconnected",
                        );
                    }
                }
                Outcome::Failed(msg) => {
                    if !job.state.is_terminal() {
                        tracing::warn!(job = %id, error = %msg, "scan failed");
                        job.finish(JobState::Aborted, "AbortedBySystem", &msg);
                    }
                }
            }
        }
        if let Some(empty) = adf_empty {
            t.adf_empty = empty;
        }
        drop(t);
        active.done.store(true, Ordering::SeqCst);
    }
}
