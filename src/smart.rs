//! Thin wrapper over `smartctl -j`.

use anyhow::{bail, Result};
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

/// Attributes whose raw value should be zero on a good drive.
pub const CRITICAL: &[(u64, &str)] = &[
    (5, "Reallocated_Sector_Ct"),
    (187, "Reported_Uncorrect"),
    (196, "Reallocated_Event_Count"),
    (197, "Current_Pending_Sector"),
    (198, "Offline_Uncorrectable"),
    (199, "UDMA_CRC_Error_Count"),
];

fn smartctl(args: &[&str]) -> Option<Value> {
    let out = crate::device::run_cmd("smartctl", args)?;
    serde_json::from_str(&out).ok()
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

/// Kick off a SMART self-test and block until it finishes.
pub fn run_selftest(dev: &str, kind: TestKind, budget_minutes: u64) -> Result<SelfTestResult> {
    let start = smartctl(&["-j", "-t", kind.arg(), dev]);
    let exit = start.as_ref().and_then(|v| v["smartctl"]["exit_status"].as_u64()).unwrap_or(1);
    if exit & 0x04 != 0 || start.is_none() {
        bail!("smartctl could not start {} self-test", kind.arg());
    }
    let t0 = Instant::now();
    let deadline = Duration::from_secs(budget_minutes * 60 + 120);
    let bar = indicatif::ProgressBar::new(100);
    bar.set_style(
        indicatif::ProgressStyle::with_template("  {msg} [{bar:30}] {percent}% {elapsed}")
            .unwrap()
            .progress_chars("=> "),
    );
    bar.set_message(format!("SMART {} self-test", kind.arg()));
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let v = match smartctl(&["-j", "-c", dev]) {
            Some(v) => v,
            None => continue,
        };
        let st = &v["ata_smart_data"]["self_test"]["status"];
        let code = st["value"].as_u64().unwrap_or(0);
        if (0xf0..=0xff).contains(&code) {
            let remaining = st["remaining_percent"].as_u64().unwrap_or(100);
            bar.set_position(100 - remaining);
        } else {
            bar.finish_and_clear();
            let status = st["string"].as_str().unwrap_or("unknown").to_string();
            let passed = st["passed"].as_bool().unwrap_or(code == 0);
            return Ok(SelfTestResult {
                kind: kind.arg().into(),
                passed,
                status,
                minutes: t0.elapsed().as_secs_f64() / 60.0,
            });
        }
        if t0.elapsed() > deadline {
            bar.finish_and_clear();
            bail!("{} self-test did not finish within {} min", kind.arg(), budget_minutes);
        }
    }
}
