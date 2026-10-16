//! Configuration from environment variables. Every variable is optional:
//! the common case needs none.

use std::path::PathBuf;
use std::time::Duration;

use ipnet::IpNet;

use crate::escl::jobs::JobConfig;
use crate::net::addrs::Selection;
use crate::net::listen::HttpLimits;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Sane,
    /// Built-in demo scanner (smoke tests, trying airbridge without hardware).
    Fake,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub interfaces: Selection,
    pub allow: Vec<IpNet>,
    pub allow_loopback: bool,
    pub backend: BackendKind,
    pub libsane: String,
    /// `None` = auto (enabled when cupsd is installed).
    pub printing: Option<bool>,
    pub no_scan: Vec<String>,
    pub no_print: Vec<String>,
    pub model_map: Vec<(String, String)>,
    /// mDNS host label (`<label>.local.`).
    pub host_label: String,
    /// Shown as the location/note in Image Capture and the print dialog.
    pub location: String,
    pub jobs: JobConfig,
    pub rescan: Duration,
    pub brscan_root: PathBuf,
    pub limits: HttpLimits,
    pub body_timeout: Duration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{var}: {msg}")]
pub struct ConfigError {
    pub var: &'static str,
    pub msg: String,
}

fn list(v: &str) -> Vec<String> {
    v.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn secs(var: &'static str, v: &str) -> Result<Duration, ConfigError> {
    v.trim()
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|e| ConfigError {
            var,
            msg: e.to_string(),
        })
}

pub fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".to_owned())
}

impl Config {
    pub fn defaults(hostname: &str) -> Self {
        let label: String = hostname
            .split('.')
            .next()
            .unwrap_or(hostname)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        Self {
            port: 8090,
            interfaces: Selection::Auto,
            allow: Vec::new(),
            allow_loopback: true,
            backend: BackendKind::Sane,
            libsane: "libsane.so.1".to_owned(),
            printing: None,
            no_scan: Vec::new(),
            no_print: Vec::new(),
            model_map: Vec::new(),
            host_label: format!("airbridge-{label}"),
            location: hostname.to_owned(),
            jobs: JobConfig::default(),
            rescan: Duration::from_secs(600),
            brscan_root: PathBuf::from("/opt/brother/scanner/brscan4"),
            limits: HttpLimits::default(),
            body_timeout: Duration::from_secs(10),
        }
    }

    /// Apply `AIRBRIDGE_*` variables on top of the defaults.
    pub fn from_vars<'a>(
        hostname: &str,
        vars: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, ConfigError> {
        let mut c = Self::defaults(hostname);
        for (k, v) in vars {
            match k {
                "AIRBRIDGE_PORT" => {
                    c.port =
                        v.trim()
                            .parse()
                            .map_err(|e: std::num::ParseIntError| ConfigError {
                                var: "AIRBRIDGE_PORT",
                                msg: e.to_string(),
                            })?;
                }
                "AIRBRIDGE_INTERFACES" => {
                    c.interfaces = match v.trim() {
                        "" | "auto" => Selection::Auto,
                        names => Selection::Named(list(names)),
                    };
                }
                "AIRBRIDGE_ALLOW" => {
                    c.allow = list(v)
                        .iter()
                        .map(|n| {
                            n.parse::<IpNet>().map_err(|e| ConfigError {
                                var: "AIRBRIDGE_ALLOW",
                                msg: format!("{n}: {e}"),
                            })
                        })
                        .collect::<Result<_, _>>()?;
                }
                "AIRBRIDGE_BACKEND" => {
                    c.backend = match v.trim() {
                        "sane" | "" => BackendKind::Sane,
                        "fake" => BackendKind::Fake,
                        other => {
                            return Err(ConfigError {
                                var: "AIRBRIDGE_BACKEND",
                                msg: format!("{other}: expected sane or fake"),
                            });
                        }
                    };
                }
                "AIRBRIDGE_LIBSANE" => c.libsane = v.trim().to_owned(),
                "AIRBRIDGE_PRINTING" => {
                    c.printing = match v.trim() {
                        "" | "auto" => None,
                        "on" | "true" | "1" => Some(true),
                        "off" | "false" | "0" => Some(false),
                        other => {
                            return Err(ConfigError {
                                var: "AIRBRIDGE_PRINTING",
                                msg: format!("{other}: expected auto, on or off"),
                            });
                        }
                    };
                }
                "AIRBRIDGE_NO_SCAN" => c.no_scan = list(v),
                "AIRBRIDGE_NO_PRINT" => c.no_print = list(v),
                "AIRBRIDGE_MODEL_MAP" => {
                    c.model_map = list(v)
                        .iter()
                        .map(|pair| {
                            pair.split_once('=')
                                .map(|(a, b)| (a.trim().to_owned(), b.trim().to_owned()))
                                .ok_or_else(|| ConfigError {
                                    var: "AIRBRIDGE_MODEL_MAP",
                                    msg: format!(
                                        "{pair}: expected 'Advertised Name=BROTHER-MODEL'"
                                    ),
                                })
                        })
                        .collect::<Result<_, _>>()?;
                }
                "AIRBRIDGE_HOSTNAME" => c.host_label = v.trim().to_owned(),
                "AIRBRIDGE_LOCATION" => c.location = v.trim().to_owned(),
                "AIRBRIDGE_JOB_TIMEOUT" => c.jobs.hang_timeout = secs("AIRBRIDGE_JOB_TIMEOUT", v)?,
                "AIRBRIDGE_RESCAN_INTERVAL" => c.rescan = secs("AIRBRIDGE_RESCAN_INTERVAL", v)?,
                "AIRBRIDGE_GAMMA" => {
                    let g: f64 =
                        v.trim()
                            .parse()
                            .map_err(|e: std::num::ParseFloatError| ConfigError {
                                var: "AIRBRIDGE_GAMMA",
                                msg: e.to_string(),
                            })?;
                    if !(0.1..=10.0).contains(&g) {
                        return Err(ConfigError {
                            var: "AIRBRIDGE_GAMMA",
                            msg: "expected 0.1..10".into(),
                        });
                    }
                    c.jobs.gray_gamma = g;
                    c.jobs.color_gamma = g;
                }
                "AIRBRIDGE_BRSCAN4_DIR" => c.brscan_root = PathBuf::from(v.trim()),
                k if k.starts_with("AIRBRIDGE_") => {
                    tracing::warn!(var = k, "unknown setting ignored")
                }
                _ => {}
            }
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_config_defaults() {
        let c = Config::from_vars("node-1", std::iter::empty()).unwrap();
        assert_eq!(c.port, 8090);
        assert_eq!(c.interfaces, Selection::Auto);
        assert_eq!(c.host_label, "airbridge-node-1");
        assert_eq!(c.printing, None);
    }

    #[test]
    fn parses_lists_and_rejects_garbage() {
        let c = Config::from_vars(
            "devbox",
            [
                ("AIRBRIDGE_NO_PRINT", "DELL6D6F61, other"),
                ("AIRBRIDGE_MODEL_MAP", "Dell MFP E514dw=DCP-L2540DW"),
                ("AIRBRIDGE_INTERFACES", "eth0,wlan0"),
            ],
        )
        .unwrap();
        assert_eq!(c.no_print, vec!["DELL6D6F61", "other"]);
        assert_eq!(
            c.model_map,
            vec![("Dell MFP E514dw".to_owned(), "DCP-L2540DW".to_owned())]
        );
        assert_eq!(
            c.interfaces,
            Selection::Named(vec!["eth0".into(), "wlan0".into()])
        );
        assert!(Config::from_vars("h", [("AIRBRIDGE_ALLOW", "not-a-net")]).is_err());
        assert!(Config::from_vars("h", [("AIRBRIDGE_PORT", "99999")]).is_err());
        assert!(Config::from_vars("h", [("AIRBRIDGE_GAMMA", "0")]).is_err());
    }
}
