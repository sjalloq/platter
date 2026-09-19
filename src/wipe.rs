//! Multi-pass overwrite with a verified final pass and a signature sector.

use crate::device::{self, Device};
use crate::io::{self, Pattern};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt;

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Method {
    /// One pass of zeros, verified. Fine for drives you are keeping or reusing.
    Zero,
    /// One pass of random data, verified.
    Random,
    /// 0x00, 0xff, random, 0x00 - final pass verified (DoD 5220.22-M style).
    Dod,
    /// random, random, 0x00 - final pass verified (NNSA style).
    Nnsa,
}

impl Method {
    pub fn passes(self, seed: u64) -> Vec<Pattern> {
        match self {
            Method::Zero => vec![Pattern::Zero],
            Method::Random => vec![Pattern::Random(seed)],
            Method::Dod => vec![Pattern::Zero, Pattern::Ones, Pattern::Random(seed), Pattern::Zero],
            Method::Nnsa => vec![Pattern::Random(seed), Pattern::Random(seed ^ 0x5555), Pattern::Zero],
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Method::Zero => "zero",
            Method::Random => "random",
            Method::Dod => "dod",
            Method::Nnsa => "nnsa",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WipeReport {
    pub method: String,
    pub passes: Vec<io::PassReport>,
    pub verify: io::PassReport,
    pub signature_written: bool,
    pub ok: bool,
}

pub fn wipe(dev: &Device, method: Method, no_signature: bool) -> Result<WipeReport> {
    let seed: u64 = rand::random();
    let patterns = method.passes(seed);
    let mut passes = Vec::new();
    for (i, p) in patterns.iter().enumerate() {
        let label = format!("pass {}/{} {}", i + 1, patterns.len(), short(p));
        let r = io::write_pass(dev, &label, *p)?;
        if r.io_errors > 0 {
            eprintln!("  ! {} write errors in {label}", r.io_errors);
        }
        passes.push(r);
    }
    let last = *patterns.last().unwrap();
    let verify = io::verify_pass(dev, "verify", last)?;
    let ok = verify.clean() && passes.iter().all(|p| p.io_errors == 0);

    let mut signature_written = false;
    if ok && !no_signature {
        let text = format!(
            "{} method={} ts={} host={}",
            std::str::from_utf8(crate::scan::PLATTER_SIG).unwrap(),
            method.name(),
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            device::hostname()
        );
        let mut sector = vec![0u8; device::SECTOR];
        sector[..text.len()].copy_from_slice(text.as_bytes());
        dev.file.write_all_at(&sector, 0)?;
        dev.file.sync_all()?;
        signature_written = true;
    }
    Ok(WipeReport { method: method.name().into(), passes, verify, signature_written, ok })
}

fn short(p: &Pattern) -> &'static str {
    match p {
        Pattern::Zero => "0x00",
        Pattern::Ones => "0xff",
        Pattern::Random(_) => "random",
    }
}

/// Interactive guard: the user must type the serial back.
pub fn confirm(serial: &str, model: &str, path: &str, what: &str) -> Result<()> {
    eprintln!();
    eprintln!("  About to {what} {path}");
    eprintln!("  {model} serial {serial}");
    eprintln!("  ALL DATA WILL BE DESTROYED. Type the serial to continue: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != serial {
        bail!("serial mismatch - aborting");
    }
    Ok(())
}
