//! Non-destructive "has this drive been wiped?" check.

use crate::device::{self, Device};
use crate::io;
use anyhow::Result;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;
const EDGE: u64 = 64 * MIB;
pub const SCRUB_SIG: &[u8] = b"SCRUBBED!";
pub const PLATTER_SIG: &[u8] = b"PLATTER-WIPED";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub signature: Option<String>,
    pub wipefs: String,
    pub sampled_chunks: usize,
    pub nonzero_chunks: usize,
    pub nonzero_offsets: Vec<u64>,
    pub verdict: String,
}

pub fn signature(dev: &Device) -> Result<Option<String>> {
    let mut head = vec![0u8; device::SECTOR];
    use std::os::unix::fs::FileExt;
    dev.file.read_exact_at(&mut head, 0)?;
    if head.starts_with(PLATTER_SIG) {
        let end = head.iter().position(|b| *b == 0).unwrap_or(head.len());
        return Ok(Some(String::from_utf8_lossy(&head[..end]).into_owned()));
    }
    if head.windows(SCRUB_SIG.len()).any(|w| w == SCRUB_SIG) {
        return Ok(Some("SCRUBBED! (scrub)".into()));
    }
    Ok(None)
}

pub fn wipefs(path: &str) -> String {
    device::run_cmd("wipefs", &["-n", path]).unwrap_or_default().trim().to_string()
}

/// Sample head, tail and `n` random 1 MiB chunks.
pub fn scan(dev: &Device, n: usize) -> Result<ScanReport> {
    let sig = signature(dev)?;
    let wipefs = wipefs(&dev.path);
    let size = dev.size;
    let mut offsets: Vec<u64> = Vec::new();
    // head: skip sector 0 if it holds our signature
    let head_start = if sig.is_some() { device::SECTOR as u64 } else { 0 };
    let mut nonzero = Vec::new();
    let mut count = 0usize;

    let mut check = |off: u64, len: u64| -> Result<()> {
        count += 1;
        if !io::is_zero_at(dev, off, len as usize)? {
            nonzero.push(off);
        }
        Ok(())
    };
    check(head_start, EDGE.min(size) - head_start)?;
    if size > EDGE {
        check(size - EDGE, EDGE)?;
    }
    let nchunks = size / MIB;
    if nchunks > 130 {
        let mut all: Vec<u64> = (65..nchunks - 65).collect();
        let mut rng = rand::thread_rng();
        all.shuffle(&mut rng);
        offsets.extend(all.into_iter().take(n));
        offsets.sort_unstable();
        for c in &offsets {
            check(c * MIB, MIB)?;
        }
    }

    let verdict = match (nonzero.is_empty(), sig.is_some(), wipefs.is_empty()) {
        (true, true, _) => "WIPED (signature present, all sampled chunks zero)",
        (true, false, true) => "WIPED (all sampled chunks zero, no signature)",
        (true, false, false) => "PARTIAL (zeros sampled but wipefs still sees signatures)",
        (false, true, _) => "PARTIAL (wipe signature but data found - interrupted?)",
        (false, false, false) => "NOT WIPED (filesystem/partition signatures present)",
        (false, false, true) => "NOT WIPED (data found)",
    }
    .to_string();

    Ok(ScanReport {
        signature: sig,
        wipefs,
        sampled_chunks: count,
        nonzero_chunks: nonzero.len(),
        nonzero_offsets: nonzero,
        verdict,
    })
}
