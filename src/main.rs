//! airbridge daemon entry point.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use airbridge::app::{App, Deps};
use airbridge::backend::Backend;
use airbridge::backend::fake::FakeBackend;
use airbridge::backend::sane::SaneBackend;
use airbridge::config::{BackendKind, Config, system_hostname};
use airbridge::discovery::brscan::{Brsaneconfig4, Brscan};
use airbridge::discovery::cups::{CupsCli, Spooler};
use airbridge::mdns::real::RealMdns;
use airbridge::net::addrs::SystemAddrs;
use anyhow::Context;

fn find_cupsd() -> Option<&'static str> {
    ["/usr/sbin/cupsd", "/usr/local/sbin/cupsd"]
        .into_iter()
        .find(|p| Path::new(p).exists())
}

/// Runs cupsd in the foreground; if it exits, so does airbridge (the
/// orchestrator restarts the container with a clean CUPS).
async fn supervise_cupsd(path: &str) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new(path)
        .arg("-f")
        .kill_on_drop(true)
        .spawn()
        .context("starting cupsd")?;
    let status = child.wait().await.context("waiting for cupsd")?;
    anyhow::bail!("cupsd exited ({status})")
}

async fn run() -> anyhow::Result<()> {
    let started = Instant::now();
    let hostname = system_hostname();
    let vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("AIRBRIDGE_"))
        .collect();
    let cfg = Config::from_vars(
        &hostname,
        vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    )?;

    let backend: Arc<dyn Backend> = match cfg.backend {
        BackendKind::Fake => {
            tracing::warn!("AIRBRIDGE_BACKEND=fake: serving a simulated scanner");
            Arc::new(FakeBackend::demo())
        }
        BackendKind::Sane => {
            let lib = cfg.libsane.clone();
            Arc::new(tokio::task::spawn_blocking(move || SaneBackend::load(&lib)).await??)
        }
    };
    let brscan: Option<Arc<dyn Brscan>> =
        Brsaneconfig4::detect(&cfg.brscan_root).map(|b| Arc::new(b) as Arc<dyn Brscan>);
    let cupsd = match cfg.printing {
        Some(false) => None,
        Some(true) => {
            Some(find_cupsd().context("AIRBRIDGE_PRINTING=on but cupsd is not installed")?)
        }
        None => find_cupsd(),
    };
    let spooler: Option<Arc<dyn Spooler>> = cupsd.map(|_| Arc::new(CupsCli) as Arc<dyn Spooler>);
    let mdns = Arc::new(RealMdns::start(&cfg.host_label)?);
    let deps = Deps {
        backend,
        addrs: Arc::new(SystemAddrs::start(Duration::from_secs(60))),
        mdns,
        brscan,
        spooler,
    };
    tracing::info!(
        port = cfg.port,
        host = %cfg.host_label,
        printing = cupsd.is_some(),
        brscan4 = deps.brscan.is_some(),
        "starting"
    );
    let app = App::start(cfg, deps).await?;
    tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "ready");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        r = async {
            match cupsd {
                Some(path) => supervise_cupsd(path).await,
                None => std::future::pending().await,
            }
        } => r,
        _ = sigterm.recv() => { tracing::info!("SIGTERM"); Ok(()) }
        _ = tokio::signal::ctrl_c() => { tracing::info!("interrupted"); Ok(()) }
    };
    app.shutdown().await;
    result
}

/// `airbridge healthcheck`: exit 0 when the local daemon reports healthy.
/// For container HEALTHCHECKs, so the image needs no curl.
fn healthcheck() -> ExitCode {
    use std::io::{Read, Write};
    let port = std::env::var("AIRBRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8090);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let check = || -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.1 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("healthcheck: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mdns_sd=warn".into()),
        )
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("airbridge: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
