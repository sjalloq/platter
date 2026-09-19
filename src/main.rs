mod commission;
mod device;
mod io;
mod log;
mod scan;
mod smart;
mod wipe;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use device::Device;
use serde_json::json;

#[derive(Parser)]
#[command(name = "platter", version, about = "Commission, wipe, verify and log spinning-rust drives")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show identity and SMART summary, log nothing.
    Id { device: String },

    /// Non-destructive check: has this drive been wiped? Logs the result.
    Scan {
        device: String,
        /// Random 1 MiB chunks to sample in addition to head and tail.
        #[arg(short = 'n', long, default_value_t = 64)]
        samples: usize,
        /// Read the entire drive and confirm every byte is zero (slow, conclusive).
        #[arg(long)]
        full: bool,
        #[arg(long)]
        note: Option<String>,
    },

    /// Acceptance-test a new drive: SMART baseline, self-tests, full read, SMART delta.
    Commission {
        device: String,
        /// Destructive: write zeros over the whole drive and verify instead of read-only pass.
        #[arg(long)]
        write: bool,
        /// Also run the SMART extended self-test at the end (hours).
        #[arg(long)]
        long: bool,
        /// Skip the SMART conveyance/short self-tests.
        #[arg(long)]
        skip_selftests: bool,
        /// Warn if power-on hours exceed this (a "new" drive should be near zero).
        #[arg(long, default_value_t = 24)]
        max_hours_new: u64,
        /// Skip the interactive confirmation (only with --write).
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        note: Option<String>,
    },

    /// Destructive: overwrite the whole drive, verify the last pass, write a signature, log it.
    Wipe {
        device: String,
        #[arg(short, long, value_enum, default_value_t = wipe::Method::Zero)]
        method: wipe::Method,
        /// Do not write the PLATTER-WIPED marker into sector 0.
        #[arg(long)]
        no_signature: bool,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        note: Option<String>,
    },

    /// Append a free-text note against a drive (device path or serial).
    Note { target: String, text: String },

    /// One line per known drive.
    List,

    /// Event history for one serial.
    Show {
        serial: String,
        /// Dump the full JSON records.
        #[arg(short, long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Id { device } => cmd_id(&device),
        Cmd::Scan { device, samples, full, note } => cmd_scan(&device, samples, full, note),
        Cmd::Commission { device, write, long, skip_selftests, max_hours_new, yes, note } => {
            cmd_commission(
                &device,
                commission::Options { write, long, skip_selftests, max_hours_new },
                yes,
                note,
            )
        }
        Cmd::Wipe { device, method, no_signature, yes, note } => {
            cmd_wipe(&device, method, no_signature, yes, note)
        }
        Cmd::Note { target, text } => cmd_note(&target, &text),
        Cmd::List => cmd_list(),
        Cmd::Show { serial, verbose } => cmd_show(&serial, verbose),
    }
}

/// Root is required for real block devices; plain image files are fine for testing.
fn need_root(path: &str) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let is_block = std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(true);
    if is_block && !device::is_root() {
        bail!("{path} is a block device - run with sudo");
    }
    Ok(())
}

fn banner(id: &device::Identity) {
    eprintln!(
        "{}: {} serial={} {:.0} GB{}",
        id.device,
        id.model,
        id.serial,
        id.size_bytes as f64 / 1e9,
        id.transport.as_deref().map(|t| format!(" via {t}")).unwrap_or_default()
    );
}

fn cmd_id(path: &str) -> Result<()> {
    let dev = Device::open(path, false)?;
    let id = device::identify(path, dev.size);
    let s = smart::snapshot(path).unwrap_or_default();
    banner(&id);
    println!("{}", serde_json::to_string_pretty(&json!({ "identity": id, "smart": s }))?);
    Ok(())
}

fn cmd_scan(path: &str, samples: usize, full: bool, note: Option<String>) -> Result<()> {
    need_root(path)?;
    let dev = Device::open(path, false)?;
    let id = device::identify(path, dev.size);
    banner(&id);
    let s = smart::snapshot(path).unwrap_or_default();
    let mut r = scan::scan(&dev, samples)?;
    let mut full_pass = None;
    if full {
        let mut v = io::verify_pass(&dev, "full zero check", io::Pattern::Zero)?;
        // The signature sector legitimately makes block 0 differ from zeros.
        if r.signature.is_some() && v.mismatched_blocks == 1 && v.first_mismatch_offset == Some(0) {
            v.mismatched_blocks = 0;
            v.first_mismatch_offset = None;
        }
        r.verdict = match (v.clean(), r.signature.is_some()) {
            (true, true) => "WIPED (signature present, every byte zero)".into(),
            (true, false) => "WIPED (every byte zero, no signature)".into(),
            (false, _) => format!(
                "NOT WIPED (full read: {} nonzero 4 MiB blocks, {} io errors)",
                v.mismatched_blocks, v.io_errors
            ),
        };
        full_pass = Some(v);
    }
    eprintln!(
        "  smart_healthy={:?} power_on_hours={:?} critical={:?}",
        s.healthy,
        s.power_on_hours,
        smart::nonzero_critical(&s)
    );
    eprintln!(
        "  signature={} nonzero_chunks={}/{}",
        r.signature.as_deref().unwrap_or("none"),
        r.nonzero_chunks,
        r.sampled_chunks
    );
    if !r.wipefs.is_empty() {
        eprintln!("  wipefs:\n    {}", r.wipefs.replace('\n', "\n    "));
    }
    eprintln!("  VERDICT: {}", r.verdict);
    let p = log::append(
        &id.serial,
        "scan",
        json!({ "identity": id, "smart": s, "scan": r, "full": full_pass, "verdict": r.verdict, "note": note }),
    )?;
    eprintln!("  logged to {}", p.display());
    Ok(())
}

fn cmd_commission(path: &str, opts: commission::Options, yes: bool, note: Option<String>) -> Result<()> {
    need_root(path)?;
    device::check_not_in_use(path)?;
    let dev = Device::open(path, opts.write)?;
    let id = device::identify(path, dev.size);
    banner(&id);
    if opts.write && !yes {
        wipe::confirm(&id.serial, &id.model, path, "write-test (zero fill)")?;
    }
    let _ = log::append(&id.serial, "commission-start", json!({ "identity": id, "write": opts.write, "note": note }));
    let r = commission::commission(&dev, &opts)?;
    for w in &r.warnings {
        eprintln!("  warning: {w}");
    }
    for f in &r.failures {
        eprintln!("  FAIL: {f}");
    }
    eprintln!("  VERDICT: {}", r.verdict);
    let p = log::append(
        &id.serial,
        "commission",
        json!({ "identity": id, "commission": r, "verdict": r.verdict, "note": note }),
    )?;
    eprintln!("  logged to {}", p.display());
    if r.verdict == "FAIL" {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_wipe(path: &str, method: wipe::Method, no_signature: bool, yes: bool, note: Option<String>) -> Result<()> {
    need_root(path)?;
    device::check_not_in_use(path)?;
    let dev = Device::open(path, true)?;
    let id = device::identify(path, dev.size);
    banner(&id);
    if !yes {
        wipe::confirm(&id.serial, &id.model, path, &format!("wipe ({})", method.name()))?;
    }
    let before = smart::snapshot(path).unwrap_or_default();
    let _ = log::append(&id.serial, "wipe-start", json!({ "identity": id, "method": method.name(), "note": note }));
    let r = wipe::wipe(&dev, method, no_signature)?;
    let after = smart::snapshot(path).unwrap_or_default();
    let delta = smart::delta(&before, &after);
    let verdict = if r.ok { "WIPED" } else { "WIPE FAILED (errors or verify mismatch)" };
    for p in &r.passes {
        eprintln!("  {}: {:.0} MB/s, {} errors, {:.1} min", p.kind, p.mb_per_s, p.io_errors, p.seconds / 60.0);
    }
    eprintln!(
        "  verify: {:.0} MB/s, {} errors, {} mismatches",
        r.verify.mb_per_s, r.verify.io_errors, r.verify.mismatched_blocks
    );
    if !delta.is_empty() {
        eprintln!("  SMART moved during wipe: {delta:?}");
    }
    eprintln!("  VERDICT: {verdict}");
    let p = log::append(
        &id.serial,
        "wipe",
        json!({ "identity": id, "wipe": r, "smart_after": after, "smart_delta": delta, "verdict": verdict, "note": note }),
    )?;
    eprintln!("  logged to {}", p.display());
    if !r.ok {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_note(target: &str, text: &str) -> Result<()> {
    let serial = if target.starts_with("/dev/") {
        let dev = Device::open(target, false)?;
        device::identify(target, dev.size).serial
    } else {
        target.to_string()
    };
    let p = log::append(&serial, "note", json!({ "note": text }))?;
    eprintln!("noted for {serial} in {}", p.display());
    Ok(())
}

fn cmd_list() -> Result<()> {
    let serials = log::serials();
    if serials.is_empty() {
        eprintln!("no drives logged in {}", log::dir().display());
        return Ok(());
    }
    println!("{:<22} {:<26} {:>7} {:<17} {:<12} VERDICT", "SERIAL", "MODEL", "GB", "LAST", "EVENT");
    for s in serials {
        let recs = log::load(&s);
        let last = recs.iter().rev().find(|r| r.get("verdict").is_some());
        let (model, gb) = recs
            .iter()
            .rev()
            .find_map(|r| r.get("identity"))
            .map(|i| {
                (
                    i["model"].as_str().unwrap_or("").to_string(),
                    i["size_bytes"].as_f64().unwrap_or(0.0) / 1e9,
                )
            })
            .unwrap_or_default();
        let (ts, ev, verdict) = last
            .map(|r| {
                (
                    r["ts"].as_str().unwrap_or("").chars().take(16).collect::<String>(),
                    r["event"].as_str().unwrap_or("").to_string(),
                    r["verdict"].as_str().unwrap_or("").to_string(),
                )
            })
            .unwrap_or_default();
        println!("{s:<22} {model:<26} {gb:>7.0} {ts:<17} {ev:<12} {verdict}  [{} events]", recs.len());
    }
    Ok(())
}

fn cmd_show(serial: &str, verbose: bool) -> Result<()> {
    let recs = log::load(serial);
    if recs.is_empty() {
        bail!("no records for {serial} in {}", log::dir().display());
    }
    for r in recs {
        let detail = r["verdict"].as_str().or(r["note"].as_str()).unwrap_or("");
        println!(
            "{}  {:<16} {}",
            r["ts"].as_str().unwrap_or(""),
            r["event"].as_str().unwrap_or(""),
            detail
        );
        if verbose {
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
    }
    Ok(())
}
