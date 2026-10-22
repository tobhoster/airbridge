//! HTTP routes: the eSCL endpoints of every bridged scanner, `/healthz`, a
//! small status page and the scanner icon.

use std::sync::Arc;

use parking_lot::RwLock;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use uuid::Uuid;

use super::jobs::{CreateError, NextDocument, ScannerService};
use super::ticket::MAX_TICKET_BYTES;
use crate::health::Health;

pub struct HttpState {
    pub scanners: RwLock<Vec<Arc<ScannerService>>>,
    pub health: Arc<Health>,
    /// Extra HTML (printers, skipped devices) for the status page.
    pub status_extra: RwLock<String>,
    pub icon: Bytes,
    pub fallback_host: String,
    pub body_timeout: Duration,
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .fallback(dispatch)
        .with_state(state)
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

fn xml(body: String) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/xml")], body).into_response()
}

async fn healthz(State(st): State<Arc<HttpState>>) -> Response {
    let status = if st.health.healthy() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        st.health.json(),
    )
        .into_response()
}

async fn index(State(st): State<Arc<HttpState>>) -> Response {
    let scanners = st.scanners.read().clone();
    let mut html = String::from(
        "<!DOCTYPE html><html><head><meta charset='utf-8'><title>airbridge</title><style>body{font-family:sans-serif;margin:2em}td,th{padding:.2em 1em;text-align:left}</style></head><body><h1>airbridge</h1><h2>Scanners (AirScan)</h2><table><tr><th>Name</th><th>eSCL path</th><th>SANE device</th></tr>",
    );
    for s in &scanners {
        html.push_str(&format!(
            "<tr><td>{}</td><td><a href='{root}/ScannerCapabilities'>{root}</a></td><td>{}</td></tr>",
            super::xml::escape(&s.name),
            super::xml::escape(&s.profile.device.name),
            root = super::xml::escape(&s.root),
        ));
    }
    html.push_str("</table>");
    html.push_str(&st.status_extra.read());
    html.push_str(&format!(
        "<h2>Health</h2><pre>{}</pre></body></html>",
        super::xml::escape(&st.health.json())
    ));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn host_of(headers: &HeaderMap, fallback: &str) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            h.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        })
        .map_or_else(|| fallback.to_owned(), str::to_owned)
}

async fn dispatch(State(st): State<Arc<HttpState>>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let scanner = {
        let scanners = st.scanners.read();
        scanners
            .iter()
            .find(|s| {
                path.strip_prefix(s.root.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .cloned()
    };
    let Some(scanner) = scanner else {
        return text(StatusCode::NOT_FOUND, "not found\n");
    };
    let rest = path[scanner.root.len() + 1..]
        .trim_end_matches('/')
        .to_owned();
    let parts: Vec<&str> = rest.split('/').collect();
    let method = req.method().clone();
    match (&method, parts.as_slice()) {
        (&Method::GET, ["ScannerCapabilities"]) => {
            let admin = format!(
                "http://{}{}",
                host_of(req.headers(), &st.fallback_host),
                scanner.root
            );
            xml(scanner.capabilities(&admin))
        }
        (&Method::GET, ["ScannerStatus"]) => xml(scanner.status()),
        (&Method::GET, ["ScannerIcon"]) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/png")],
            st.icon.clone(),
        )
            .into_response(),
        (&Method::POST, ["ScanJobs"]) => {
            let peer = req
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string());
            let agent = req
                .headers()
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .chars()
                .take(120)
                .collect::<String>();
            let client = format!("{peer} ({agent})");
            let body = match tokio::time::timeout(
                st.body_timeout,
                axum::body::to_bytes(req.into_body(), MAX_TICKET_BYTES),
            )
            .await
            {
                Ok(Ok(b)) => b,
                Ok(Err(_)) => {
                    return text(StatusCode::PAYLOAD_TOO_LARGE, "ScanSettings too large\n");
                }
                Err(_) => return text(StatusCode::REQUEST_TIMEOUT, "request body too slow\n"),
            };
            match scanner.create_job(&body, &client) {
                Ok(id) => {
                    let location = format!("{}/ScanJobs/{id}", scanner.root);
                    let mut resp = StatusCode::CREATED.into_response();
                    if let Ok(v) = HeaderValue::from_str(&location) {
                        resp.headers_mut().insert(header::LOCATION, v);
                    }
                    resp
                }
                Err(CreateError::Busy) => {
                    let mut resp = text(StatusCode::SERVICE_UNAVAILABLE, "scanner busy\n");
                    resp.headers_mut()
                        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
                    resp
                }
                Err(CreateError::Ticket(e)) => {
                    text(StatusCode::CONFLICT, format!("InvalidScanTicket: {e}\n"))
                }
            }
        }
        (&Method::GET, ["ScanJobs", id, "NextDocument"]) => {
            let Ok(id) = Uuid::parse_str(id) else {
                return text(StatusCode::NOT_FOUND, "no such job\n");
            };
            match scanner.next_document(id).await {
                NextDocument::Page { mime, body } => {
                    let stream = futures_util::stream::unfold(body, |mut rx| async move {
                        rx.recv().await.map(|item| (item, rx))
                    });
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, mime)],
                        Body::from_stream(stream),
                    )
                        .into_response()
                }
                NextDocument::NotFound => text(StatusCode::NOT_FOUND, "no more documents\n"),
                NextDocument::Busy => text(StatusCode::SERVICE_UNAVAILABLE, "page in progress\n"),
                NextDocument::AdfEmpty => text(StatusCode::CONFLICT, "ScannerAdfEmpty\n"),
                NextDocument::Failed(msg) => text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("scan failed: {msg}\n"),
                ),
            }
        }
        (&Method::DELETE, ["ScanJobs", id]) => match Uuid::parse_str(id) {
            Ok(id) if scanner.cancel(id) => StatusCode::OK.into_response(),
            _ => text(StatusCode::NOT_FOUND, "no such job\n"),
        },
        _ => text(StatusCode::NOT_FOUND, "not found\n"),
    }
}
