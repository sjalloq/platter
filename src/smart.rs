//! Thin wrapper over `smartctl -j`.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The handful of SMART fields that matter for deciding whether a drive is
/// trustworthy. Attribute raw values are keyed by attribute name.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub available: bool,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub rotation_rate: Option<u64>,
    pub healthy: Option<bool>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    pub temperature_c: Option<i64>,
    pub attrs: BTreeMap<String, u64>,
    pub selftest_supported: bool,
    pub conveyance_supported: bool,
    pub short_minutes: Option<u64>,
    pub extended_minutes: Option<u64>,
    pub conveyance_minutes: Option<u64>,
}

/// Render an optional value for humans: "true", "13367", or "n/a".
pub fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_else(|| "n/a".into())
}

/// Render nonzero critical attributes as "Reallocated_Sector_Ct=3, ..." or "none".
pub fn critical_summary(s: &Snapshot) -> String {
    let v = nonzero_critical(s);
    if v.is_empty() {
        "none".into()
    } else {
        v.iter().map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(", ")
    }
}

/// Attributes whose raw value should be zero on a good drive.
pub const CRITICAL: &[(u64, &str)] = &[
    (5, "Reallocated_Sector_Ct"),
    (187, "Reported_Uncorrect"),
    (196, "Reallocated_Event_Count"),
    (197, "Current_Pending_Sector"),
    (198, "Offline_Uncorrectable"),
    (199, "UDMA_CRC_Error_Count"),
];

/// Run smartctl and parse its `-j` output. The error explains *why* there is no
/// JSON, which is usually the only clue worth having.
fn smartctl_json(args: &[&str]) -> Result<Value> {
    let out = crate::device::run_cmd_full("smartctl", args)
        .context("could not run smartctl (is smartmontools installed?)")?;
    serde_json::from_str(&out.stdout).map_err(|e| {
        let err = out.stderr.trim();
        if err.is_empty() {
            anyhow!("smartctl produced no usable JSON: {e}")
        } else {
            anyhow!("smartctl failed: {}", one_line(err))
        }
    })
}

fn smartctl(args: &[&str]) -> Option<Value> {
    smartctl_json(args).ok()
}

fn one_line(s: &str) -> String {
    s.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ")
}

/// smartctl's own explanation of a refusal, e.g. "Can't start self-test without
/// aborting current test (90% remaining)".
fn messages(v: &Value) -> String {
    let m = v["smartctl"]["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["string"].as_str())
                .map(one_line)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if m.is_empty() {
        "no reason given".into()
    } else {
        m
    }
}

pub fn snapshot(dev: &str) -> Option<Snapshot> {
    let v = smartctl(&["-j", "-i", "-H", "-A", "-c", dev])?;
    if v.get("device").is_none() {
        return None;
    }
    let mut s = Snapshot { available: true, ..Default::default() };
    s.model = v["model_name"].as_str().map(String::from);
    s.serial = v["serial_number"].as_str().map(String::from);
    s.firmware = v["firmware_version"].as_str().map(String::from);
    s.rotation_rate = v["rotation_rate"].as_u64();
    s.healthy = v["smart_status"]["passed"].as_bool();
    s.power_on_hours = v["power_on_time"]["hours"].as_u64();
    s.power_cycles = v["power_cycle_count"].as_u64();
    s.temperature_c = v["temperature"]["current"].as_i64();
    if let Some(table) = v["ata_smart_attributes"]["table"].as_array() {
        for a in table {
            let id = a["id"].as_u64().unwrap_or(0);
            if let Some((_, name)) = CRITICAL.iter().find(|(i, _)| *i == id) {
                // raw.value can be composite on some attributes; low 16 bits is the count
                let raw = a["raw"]["value"].as_u64().unwrap_or(0);
                s.attrs.insert(name.to_string(), if id == 199 { raw } else { raw & 0xffff });
            }
        }
    }
    let caps = &v["ata_smart_data"]["capabilities"];
    s.selftest_supported = caps["self_tests_supported"].as_bool().unwrap_or(false);
    s.conveyance_supported = caps["conveyance_self_test_supported"].as_bool().unwrap_or(false);
    let pm = &v["ata_smart_data"]["self_test"]["polling_minutes"];
    s.short_minutes = pm["short"].as_u64();
    s.extended_minutes = pm["extended"].as_u64();
    s.conveyance_minutes = pm["conveyance"].as_u64();
    Some(s)
}

/// Raw-attribute deltas between two snapshots, only for those that moved.
pub fn delta(before: &Snapshot, after: &Snapshot) -> BTreeMap<String, i64> {
    let mut d = BTreeMap::new();
    for (k, a) in &after.attrs {
        let b = before.attrs.get(k).copied().unwrap_or(0);
        if *a as i64 != b as i64 {
            d.insert(k.clone(), *a as i64 - b as i64);
        }
    }
    d
}

pub fn nonzero_critical(s: &Snapshot) -> Vec<(String, u64)> {
    s.attrs.iter().filter(|(_, v)| **v > 0).map(|(k, v)| (k.clone(), *v)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    Short,
    Conveyance,
    Long,
}

impl TestKind {
    fn arg(self) -> &'static str {
        match self {
            TestKind::Short => "short",
            TestKind::Conveyance => "conveyance",
            TestKind::Long => "long",
        }
    }
    pub fn name(self) -> &'static str {
        self.arg()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfTestResult {
    pub kind: String,
    pub passed: bool,
    pub status: String,
    pub minutes: f64,
}

/// Percent remaining if a self-test is running on the drive right now.
fn selftest_remaining(dev: &str) -> Option<u64> {
    let v = smartctl(&["-j", "-c", dev])?;
    let st = &v["ata_smart_data"]["self_test"]["status"];
    let code = st["value"].as_u64()?;
    (0xf0..=0xff)
        .contains(&code)
        .then(|| st["remaining_percent"].as_u64().unwrap_or(100))
}

/// Poll until no self-test is running, drawing a progress bar, and return the
/// status the drive settled on.
fn wait_until_idle(dev: &str, label: &str, budget_minutes: u64) -> Result<(String, bool)> {
    let t0 = Instant::now();
    let deadline = Duration::from_secs(budget_minutes * 60 + 120);
    let bar = indicatif::ProgressBar::new(100);
    bar.set_style(
        indicatif::ProgressStyle::with_template("  {msg} [{bar:30}] {percent}% {elapsed}")
            .unwrap()
            .progress_chars("=> "),
    );
    bar.set_message(label.to_string());
    loop {
        std::thread::sleep(Duration::from_secs(15));
        if let Some(v) = smartctl(&["-j", "-c", dev]) {
            let st = &v["ata_smart_data"]["self_test"]["status"];
            let code = st["value"].as_u64().unwrap_or(0);
            if !(0xf0..=0xff).contains(&code) {
                bar.finish_and_clear();
                let status = st["string"].as_str().unwrap_or("unknown").to_string();
                return Ok((status, st["passed"].as_bool().unwrap_or(code == 0)));
            }
            bar.set_position(100 - st["remaining_percent"].as_u64().unwrap_or(100));
        }
        if t0.elapsed() > deadline {
            bar.finish_and_clear();
            bail!("self-test still running after {budget_minutes} min (abort it with `smartctl -X {dev}`)");
        }
    }
}

/// Kick off a SMART self-test and block until it finishes.
///
/// A test left behind by an interrupted run is still running on the drive, and
/// the firmware will refuse to start another one, so wait it out first.
pub fn run_selftest(dev: &str, kind: TestKind, budget_minutes: u64) -> Result<SelfTestResult> {
    if let Some(remaining) = selftest_remaining(dev) {
        eprintln!("  a self-test is already running ({remaining}% remaining) - waiting for it");
        let (status, _) = wait_until_idle(dev, "previous SMART self-test", budget_minutes)?;
        eprintln!("  previous self-test finished: {status}");
    }
    let start = smartctl_json(&["-j", "-t", kind.arg(), dev])
        .map_err(|e| anyhow!("could not start it: {e}"))?;
    let exit = start["smartctl"]["exit_status"].as_u64().unwrap_or(1);
    if exit & 0x04 != 0 {
        bail!("smartctl refused to start it: {}", messages(&start));
    }
    let t0 = Instant::now();
    let (status, passed) =
        wait_until_idle(dev, &format!("SMART {} self-test", kind.arg()), budget_minutes)?;
    Ok(SelfTestResult {
        kind: kind.arg().into(),
        passed,
        status,
        minutes: t0.elapsed().as_secs_f64() / 60.0,
    })
}
