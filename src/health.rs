//! Component health behind `/healthz`. A component that cannot recover in
//! process reports unhealthy and the orchestrator restarts the container.

use parking_lot::Mutex;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Health {
    components: Mutex<BTreeMap<String, (bool, String)>>,
}

impl Health {
    pub fn set(&self, component: &str, ok: bool, detail: &str) {
        let mut c = self.components.lock();
        let entry = c.entry(component.to_owned()).or_insert((ok, String::new()));
        if entry.0 != ok {
            if ok {
                tracing::info!(component, "healthy again");
            } else {
                tracing::error!(component, detail, "unhealthy");
            }
        }
        *entry = (ok, detail.to_owned());
    }

    pub fn remove(&self, component: &str) {
        self.components.lock().remove(component);
    }

    pub fn healthy(&self) -> bool {
        self.components.lock().values().all(|(ok, _)| *ok)
    }

    /// `{"status":"ok","components":{"http":"ok",...}}`
    pub fn json(&self) -> String {
        let c = self.components.lock();
        let healthy = c.values().all(|(ok, _)| *ok);
        let parts: Vec<String> = c
            .iter()
            .map(|(k, (ok, d))| {
                let v = if *ok {
                    "ok".to_owned()
                } else {
                    format!("failing: {d}")
                };
                format!("\"{}\":\"{}\"", json_escape(k), json_escape(&v))
            })
            .collect();
        format!(
            "{{\"status\":\"{}\",\"components\":{{{}}}}}\n",
            if healthy { "ok" } else { "failing" },
            parts.join(",")
        )
    }
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
