//! Whole-disk sequential passes: read (surface scan), write (pattern fill),
//! verify (read back and compare against a regenerable pattern).

use crate::device::Device;
use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rand::{rngs::SmallRng, RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt;
use std::time::Instant;

pub const BLOCK: usize = 4 * 1024 * 1024;
const SUBBLOCK: usize = 64 * 1024;
const ZONES: usize = 100;
const MAX_RECORDED_ERRORS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    Zero,
    Ones,
    Random(u64),
}

impl Pattern {
    pub fn name(&self) -> String {
        match self {
            Pattern::Zero => "0x00".into(),
            Pattern::Ones => "0xff".into(),
            Pattern::Random(seed) => format!("random(seed={seed:#x})"),
        }
    }
    /// Fill `buf` with the expected content of block `index`. Random data is
    /// derived from (seed, index) so a verify pass can regenerate it exactly.
    pub fn fill(&self, index: u64, buf: &mut [u8]) {
        match self {
            Pattern::Zero => buf.fill(0),
            Pattern::Ones => buf.fill(0xff),
            Pattern::Random(seed) => {
                SmallRng::seed_from_u64(seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15)).fill_bytes(buf)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassReport {
    pub kind: String,
    pub pattern: Option<String>,
    pub bytes: u64,
    pub seconds: f64,
    pub mb_per_s: f64,
    /// Sequential throughput per 1% zone, outer to inner tracks.
    pub zone_mb_per_s: Vec<f64>,
    pub slowest_zone_mb_per_s: f64,
    pub io_errors: u64,
    /// Byte offsets of the first failing 64 KiB sub-blocks (capped).
    pub error_offsets: Vec<u64>,
    pub mismatched_blocks: u64,
    pub first_mismatch_offset: Option<u64>,
}

impl PassReport {
    pub fn clean(&self) -> bool {
        self.io_errors == 0 && self.mismatched_blocks == 0
    }
}

enum Op<'a> {
    Read,
    Write(Pattern),
    Verify(Pattern, &'a mut Vec<u8>),
}

fn bar(total: u64, msg: &str) -> ProgressBar {
    let b = ProgressBar::new(total);
    b.set_style(
        ProgressStyle::with_template(
            "  {msg:<14} [{bar:30}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    b.set_message(msg.to_string());
    b
}

fn do_pass(dev: &Device, kind: &str, mut op: Op) -> Result<PassReport> {
    let size = dev.size;
    let nblocks = (size + BLOCK as u64 - 1) / BLOCK as u64;
    let zone_len = (size / ZONES as u64).max(1);
    let mut buf = vec![0u8; BLOCK];
    let mut zone_bytes = vec![0u64; ZONES];
    let mut zone_secs = vec![0f64; ZONES];
    let mut rep = PassReport {
        kind: kind.into(),
        pattern: match &op {
            Op::Read => None,
            Op::Write(p) | Op::Verify(p, _) => Some(p.name()),
        },
        bytes: 0,
        seconds: 0.0,
        mb_per_s: 0.0,
        zone_mb_per_s: vec![],
        slowest_zone_mb_per_s: 0.0,
        io_errors: 0,
        error_offsets: vec![],
        mismatched_blocks: 0,
        first_mismatch_offset: None,
    };
    let pb = bar(size, kind);
    let t0 = Instant::now();
    let mut last_drop = Instant::now();

    for i in 0..nblocks {
        let off = i * BLOCK as u64;
        let len = ((size - off).min(BLOCK as u64)) as usize;
        let slice = &mut buf[..len];
        let tb = Instant::now();

        let ok = match &mut op {
            Op::Read => read_block(dev, off, slice, &mut rep),
            Op::Write(p) => {
                p.fill(i, slice);
                write_block(dev, off, slice, &mut rep)
            }
            Op::Verify(p, expect) => {
                let ok = read_block(dev, off, slice, &mut rep);
                p.fill(i, &mut expect[..len]);
                if slice != &expect[..len] {
                    rep.mismatched_blocks += 1;
                    rep.first_mismatch_offset.get_or_insert(off);
                }
                ok
            }
        };
        let _ = ok;

        let z = ((off / zone_len) as usize).min(ZONES - 1);
        zone_bytes[z] += len as u64;
        zone_secs[z] += tb.elapsed().as_secs_f64();
        rep.bytes += len as u64;
        pb.set_position(rep.bytes);
        if last_drop.elapsed().as_secs() > 10 {
            dev.drop_cache();
            last_drop = Instant::now();
        }
    }
    if matches!(op, Op::Write(_)) {
        dev.file.sync_all()?;
    }
    dev.drop_cache();
    pb.finish_and_clear();

    rep.seconds = t0.elapsed().as_secs_f64();
    rep.mb_per_s = rep.bytes as f64 / 1e6 / rep.seconds.max(1e-9);
    rep.zone_mb_per_s = zone_bytes
        .iter()
        .zip(&zone_secs)
        .filter(|(b, _)| **b > 0)
        .map(|(b, s)| (*b as f64 / 1e6 / s.max(1e-9) * 10.0).round() / 10.0)
        .collect();
    rep.slowest_zone_mb_per_s =
        rep.zone_mb_per_s.iter().cloned().fold(f64::INFINITY, f64::min);
    if !rep.slowest_zone_mb_per_s.is_finite() {
        rep.slowest_zone_mb_per_s = 0.0;
    }
    Ok(rep)
}

/// Read a block; on failure retry in 64 KiB pieces so one bad sector doesn't
/// discard 4 MiB. Failed pieces are zero-filled and recorded.
fn read_block(dev: &Device, off: u64, buf: &mut [u8], rep: &mut PassReport) -> bool {
    if dev.file.read_exact_at(buf, off).is_ok() {
        return true;
    }
    let mut ok = true;
    for (j, piece) in buf.chunks_mut(SUBBLOCK).enumerate() {
        let poff = off + (j * SUBBLOCK) as u64;
        if dev.file.read_exact_at(piece, poff).is_err() {
            piece.fill(0);
            record_error(rep, poff);
            ok = false;
        }
    }
    ok
}

fn write_block(dev: &Device, off: u64, buf: &[u8], rep: &mut PassReport) -> bool {
    if dev.file.write_all_at(buf, off).is_ok() {
        return true;
    }
    let mut ok = true;
    for (j, piece) in buf.chunks(SUBBLOCK).enumerate() {
        let poff = off + (j * SUBBLOCK) as u64;
        if dev.file.write_all_at(piece, poff).is_err() {
            record_error(rep, poff);
            ok = false;
        }
    }
    ok
}

fn record_error(rep: &mut PassReport, off: u64) {
    rep.io_errors += 1;
    if rep.error_offsets.len() < MAX_RECORDED_ERRORS {
        rep.error_offsets.push(off);
    }
}

pub fn read_pass(dev: &Device, label: &str) -> Result<PassReport> {
    do_pass(dev, label, Op::Read)
}

pub fn write_pass(dev: &Device, label: &str, p: Pattern) -> Result<PassReport> {
    do_pass(dev, label, Op::Write(p))
}

pub fn verify_pass(dev: &Device, label: &str, p: Pattern) -> Result<PassReport> {
    let mut expect = vec![0u8; BLOCK];
    do_pass(dev, label, Op::Verify(p, &mut expect))
}

/// Read `len` bytes at `off`; true if every byte is zero.
pub fn is_zero_at(dev: &Device, off: u64, len: usize) -> Result<bool> {
    let mut buf = vec![0u8; len];
    dev.file.read_exact_at(&mut buf, off)?;
    Ok(buf.iter().all(|b| *b == 0))
}
