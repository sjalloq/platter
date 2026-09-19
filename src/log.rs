//! Append-only JSONL log, one file per drive serial.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("PLATTER_DIR") {
        return PathBuf::from(d);
    }
    // Root writes to /var/lib/platter. Anyone else uses it too if it is
    // already there and readable (so `platter list` works without sudo),
    // otherwise falls back to a per-user directory.
    let system = PathBuf::from("/var/lib/platter");
    if crate::device::is_root() || std::fs::read_dir(&system).is_ok() {
        return system;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/platter")
}

fn path(serial: &str) -> PathBuf {
    dir().join(format!("{}.jsonl", serial.replace('/', "_")))
}

/// Stamp common fields and append.
pub fn append(serial: &str, event: &str, mut rec: Value) -> Result<PathBuf> {
    std::fs::create_dir_all(dir())?;
    let p = path(serial);
    let obj = rec.as_object_mut().context("record must be an object")?;
    obj.insert("ts".into(), json!(chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
    obj.insert("event".into(), json!(event));
    obj.insert("host".into(), json!(crate::device::hostname()));
    obj.insert("tool".into(), json!(format!("platter {}", env!("CARGO_PKG_VERSION"))));
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
    use std::io::Write;
    writeln!(f, "{}", serde_json::to_string(&rec)?)?;
    Ok(p)
}

pub fn load(serial: &str) -> Vec<Value> {
    std::fs::read_to_string(path(serial))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn serials() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_suffix(".jsonl").map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
