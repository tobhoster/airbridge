//! One listening socket per served address, reconciled whenever addresses
//! change: an address that appears gets a listener, one that disappears loses
//! it. Connections already open keep running.
//!
//! Every connection is bounded: a global connection cap, a header read
//! timeout (slowloris), a capped read buffer, and the access policy checked
//! on each request.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::http::{Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::access::AccessPolicy;

#[derive(Clone, Debug)]
pub struct HttpLimits {
    pub max_connections: usize,
    pub header_timeout: Duration,
    pub max_buf: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_connections: 128,
            header_timeout: Duration::from_secs(10),
            max_buf: 64 * 1024,
        }
    }
}

pub struct Listeners {
    port: u16,
    router: Router,
    policy: Arc<RwLock<AccessPolicy>>,
    limits: HttpLimits,
    conns: Arc<Semaphore>,
    active: Mutex<BTreeMap<SocketAddr, JoinHandle<()>>>,
}

fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

impl Listeners {
    pub fn new(
        port: u16,
        router: Router,
        policy: Arc<RwLock<AccessPolicy>>,
        limits: HttpLimits,
    ) -> Arc<Self> {
        Arc::new(Self {
            port,
            router,
            conns: Arc::new(Semaphore::new(limits.max_connections)),
            policy,
            limits,
            active: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Bring listeners in line with `wanted` (IPs). Returns addresses that
    /// failed to bind; they are retried on the next reconcile.
    pub fn reconcile(&self, wanted: &BTreeSet<std::net::IpAddr>) -> Vec<(SocketAddr, io::Error)> {
        let wanted: BTreeSet<SocketAddr> = wanted
            .iter()
            .map(|ip| SocketAddr::new(*ip, self.port))
            .collect();
        let mut active = self.active.lock();
        active.retain(|addr, task| {
            let keep = wanted.contains(addr) && !task.is_finished();
            if !keep {
                tracing::info!(%addr, "stopped listening");
                task.abort();
            }
            keep
        });
        let mut failed = Vec::new();
        for addr in wanted {
            if active.contains_key(&addr) {
                continue;
            }
            match bind(addr) {
                Ok(listener) => {
                    tracing::info!(%addr, "listening");
                    active.insert(addr, tokio::spawn(self.accept_loop(listener)));
                }
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "bind failed, will retry");
                    failed.push((addr, e));
                }
            }
        }
        failed
    }

    pub fn bound(&self) -> Vec<SocketAddr> {
        self.active.lock().keys().copied().collect()
    }

    /// Abort every accept loop; await the returned handles to know the
    /// sockets are closed (a successor can then bind the port at once).
    pub fn shutdown(&self) -> Vec<JoinHandle<()>> {
        let active = std::mem::take(&mut *self.active.lock());
        active
            .into_values()
            .inspect(|task| {
                task.abort();
            })
            .collect()
    }

    fn accept_loop(&self, listener: TcpListener) -> impl Future<Output = ()> + Send + 'static {
        let router = self.router.clone();
        let policy = Arc::clone(&self.policy);
        let limits = self.limits.clone();
        let conns = Arc::clone(&self.conns);
        async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // EMFILE and friends: back off instead of spinning.
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&conns).try_acquire_owned() else {
                    tracing::warn!(%peer, "connection limit reached, dropping");
                    continue;
                };
                let router = router.clone();
                let policy = Arc::clone(&policy);
                let limits = limits.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let service = hyper::service::service_fn(
                        move |mut req: hyper::Request<hyper::body::Incoming>| {
                            let allowed = policy.read().allows(peer.ip());
                            req.extensions_mut().insert(ConnectInfo(peer));
                            let router = router.clone();
                            async move {
                                if !allowed {
                                    tracing::debug!(%peer, "denied by access policy");
                                    return Ok(Response::builder()
                                        .status(StatusCode::FORBIDDEN)
                                        .body(axum::body::Body::from("forbidden\n"))
                                        .unwrap_or_default());
                                }
                                router.oneshot(req.map(axum::body::Body::new)).await
                            }
                        },
                    );
                    let conn = hyper::server::conn::http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(limits.header_timeout)
                        .max_buf_size(limits.max_buf)
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(e) = conn.await {
                        tracing::debug!(%peer, error = %e, "connection ended");
                    }
                });
            }
        }
    }
}
