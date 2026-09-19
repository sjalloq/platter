//! Out-of-the-packet acceptance test for a new (or new-to-you) drive.

use crate::device::Device;
use crate::io::{self, Pattern};
use crate::smart::{self, Snapshot, TestKind};
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub struct Options {
    pub write: bool,
    pub long: bool,
    pub skip_selftests: bool,
    pub max_hours_new: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionReport {
    pub smart_before: Snapshot,
    pub smart_after: Snapshot,
    pub smart_delta: std::collections::BTreeMap<String, i64>,
    pub selftests: Vec<smart::SelfTestResult>,
    pub read: Option<io::PassReport>,
    pub write: Option<io::PassReport>,
    pub verify: Option<io::PassReport>,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
    pub verdict: String,
}

pub fn commission(dev: &Device, opts: &Options) -> Result<CommissionReport> {
    let mut warnings = Vec::new();
    let mut failures = Vec::new();

    let before = smart::snapshot(&dev.path).unwrap_or_default();
    if !before.available {
        warnings.push("smartctl unavailable or drive has no SMART - self-tests skipped".into());
    } else {
        if before.healthy == Some(false) {
            failures.push("SMART overall health FAILED before testing".into());
        }
        if let Some(h) = before.power_on_hours {
            if h > opts.max_hours_new {
                warnings.push(format!("power-on hours = {h}, not factory fresh"));
            }
        }
        for (k, v) in smart::nonzero_critical(&before) {
            warnings.push(format!("{k} = {v} before testing"));
        }
        eprintln!(
            "  SMART baseline: healthy={:?} hours={:?} cycles={:?} temp={:?}C",
            before.healthy, before.power_on_hours, before.power_cycles, before.temperature_c
        );
    }

    let mut selftests = Vec::new();
    if before.available && before.selftest_supported && !opts.skip_selftests {
        let mut plan = vec![];
        if before.conveyance_supported {
            plan.push((TestKind::Conveyance, before.conveyance_minutes.unwrap_or(5)));
        }
        plan.push((TestKind::Short, before.short_minutes.unwrap_or(2)));
        for (kind, mins) in plan {
            match smart::run_selftest(&dev.path, kind, mins.max(2) * 3) {
                Ok(r) => {
                    eprintln!("  {} self-test: {} ({:.1} min)", r.kind, r.status, r.minutes);
                    if !r.passed {
                        failures.push(format!("{} self-test: {}", r.kind, r.status));
                    }
                    selftests.push(r);
                }
                Err(e) => warnings.push(format!("{} self-test: {e}", kind.name())),
            }
        }
    }

    let read = if opts.write {
        None
    } else {
        let r = io::read_pass(dev, "surface read")?;
        report_pass(&r, &mut failures);
        Some(r)
    };

    let (write, verify) = if opts.write {
        let w = io::write_pass(dev, "write 0x00", Pattern::Zero)?;
        report_pass(&w, &mut failures);
        let v = io::verify_pass(dev, "verify", Pattern::Zero)?;
        report_pass(&v, &mut failures);
        (Some(w), Some(v))
    } else {
        (None, None)
    };

    if opts.long && before.available && before.selftest_supported {
        let mins = before.extended_minutes.unwrap_or(600);
        match smart::run_selftest(&dev.path, TestKind::Long, mins * 2) {
            Ok(r) => {
                eprintln!("  long self-test: {} ({:.1} min)", r.status, r.minutes);
                if !r.passed {
                    failures.push(format!("long self-test: {}", r.status));
                }
                selftests.push(r);
            }
            Err(e) => warnings.push(format!("long self-test: {e}")),
        }
    }

    let after = smart::snapshot(&dev.path).unwrap_or_default();
    let delta = smart::delta(&before, &after);
    for (k, d) in &delta {
        if *d > 0 {
            failures.push(format!("{k} grew by {d} during testing"));
        }
    }
    if after.healthy == Some(false) {
        failures.push("SMART overall health FAILED after testing".into());
    }
    if let Some(t) = after.temperature_c {
        if t >= 55 {
            warnings.push(format!("drive reached {t}C - check airflow"));
        }
    }

    let verdict = if !failures.is_empty() {
        "FAIL"
    } else if !warnings.is_empty() {
        "PASS with warnings"
    } else {
        "PASS"
    }
    .to_string();

    Ok(CommissionReport {
        smart_before: before,
        smart_after: after,
        smart_delta: delta,
        selftests,
        read,
        write,
        verify,
        warnings,
        failures,
        verdict,
    })
}

fn report_pass(r: &io::PassReport, failures: &mut Vec<String>) {
    eprintln!(
        "  {}: {:.0} MB/s avg, slowest zone {:.0} MB/s, {} io errors, {} mismatches, {:.1} min",
        r.kind,
        r.mb_per_s,
        r.slowest_zone_mb_per_s,
        r.io_errors,
        r.mismatched_blocks,
        r.seconds / 60.0
    );
    if r.io_errors > 0 {
        failures.push(format!("{}: {} I/O errors (first at {})", r.kind, r.io_errors, r.error_offsets[0]));
    }
    if r.mismatched_blocks > 0 {
        failures.push(format!(
            "{}: {} blocks read back wrong (first at {})",
            r.kind,
            r.mismatched_blocks,
            r.first_mismatch_offset.unwrap_or(0)
        ));
    }
}
