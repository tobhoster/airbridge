//! Brother network scanners need registering with brscan4 before SANE sees
//! them. `brsaneconfig4` exits 0 even when it did nothing (an existing name
//! is "already registered" and keeps its old IP), so every change is
//! remove-then-add and is verified against the config file it writes.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrscanError {
    #[error("brsaneconfig4 failed: {0}")]
    Command(String),
    #[error("brsaneconfig4 did not register {0}")]
    NotRegistered(String),
}

pub trait Brscan: Send + Sync + 'static {
    fn supported(&self, model: &str) -> bool;
    /// Register (or re-register with a new address) a network scanner.
    fn register(&self, name: &str, model: &str, ip: Ipv4Addr) -> Result<(), BrscanError>;
    fn unregister(&self, name: &str) -> Result<(), BrscanError>;
}

pub struct Brsaneconfig4 {
    bin: PathBuf,
    config: PathBuf,
    models: BTreeSet<String>,
}

/// Model names from brscan4's `models4/*.ini` (`0x0328,213,2,"DCP-L2540DW",133,4`).
pub fn parse_models(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines()
        .filter_map(|l| l.split('"').nth(1))
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

impl Brsaneconfig4 {
    /// `None` when brscan4 is not installed.
    pub fn detect(root: &Path) -> Option<Self> {
        let bin = root.join("brsaneconfig4");
        if !bin.exists() {
            return None;
        }
        Some(Self::new(
            bin,
            root.join("brsanenetdevice4.cfg"),
            &root.join("models4"),
        ))
    }

    pub fn new(bin: PathBuf, config: PathBuf, models_dir: &Path) -> Self {
        let mut models = BTreeSet::new();
        if let Ok(dir) = std::fs::read_dir(models_dir) {
            for entry in dir.flatten() {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    models.extend(parse_models(&text).map(|m| m.to_ascii_uppercase()));
                }
            }
        }
        tracing::info!(models = models.len(), "brscan4 found");
        Self {
            bin,
            config,
            models,
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, BrscanError> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .map_err(|e| BrscanError::Command(e.to_string()))?;
        if !out.status.success() {
            return Err(BrscanError::Command(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// (name, ip) entries from the config file.
    fn entries(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.config)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("DEVICE=")?;
                let name = rest.split(',').next()?.trim().to_owned();
                let ip = l
                    .split("IP-ADDRESS=")
                    .nth(1)
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default();
                Some((name, ip))
            })
            .collect()
    }
}

impl Brscan for Brsaneconfig4 {
    fn supported(&self, model: &str) -> bool {
        self.models.contains(&model.to_ascii_uppercase())
    }

    fn register(&self, name: &str, model: &str, ip: Ipv4Addr) -> Result<(), BrscanError> {
        let ip_s = ip.to_string();
        let current: Vec<(String, String)> = self
            .entries()
            .into_iter()
            .filter(|(n, _)| n == name)
            .collect();
        if current.len() == 1 && current[0].1 == ip_s {
            return Ok(());
        }
        if !current.is_empty() {
            self.unregister(name)?;
        }
        self.run(&[
            "-a",
            &format!("name={name}"),
            &format!("model={model}"),
            &format!("ip={ip_s}"),
        ])?;
        let after: Vec<(String, String)> = self
            .entries()
            .into_iter()
            .filter(|(n, _)| n == name)
            .collect();
        if after.len() == 1 && after[0].1 == ip_s {
            tracing::info!(name, model, ip = %ip_s, "registered with brscan4");
            Ok(())
        } else {
            Err(BrscanError::NotRegistered(format!("{name} at {ip_s}")))
        }
    }

    fn unregister(&self, name: &str) -> Result<(), BrscanError> {
        // brsaneconfig4 removes one entry per call; repeat until none is left.
        for _ in 0..8 {
            if !self.entries().iter().any(|(n, _)| n == name) {
                return Ok(());
            }
            self.run(&["-r", name])?;
        }
        Err(BrscanError::Command(format!("could not remove {name}")))
    }
}
