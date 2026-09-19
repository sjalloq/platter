//! Opening block devices and identifying them (lsblk / smartctl / sysfs).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

pub const SECTOR: usize = 512;

/// Static identity of a drive, gathered once per run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Identity {
    pub device: String,
    pub model: String,
    pub serial: String,
    pub firmware: Option<String>,
    pub wwn: Option<String>,
    pub transport: Option<String>,
    pub rotational: Option<bool>,
    pub rotation_rate: Option<u64>,
    pub size_bytes: u64,
}

pub struct Device {
    pub path: String,
    pub file: File,
    pub size: u64,
}

impl Device {
    pub fn open(path: &str, write: bool) -> Result<Self> {
        if !Path::new(path).exists() {
            bail!("{path} does not exist");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .with_context(|| format!("opening {path} (need root?)"))?;
        let size = block_size(&file, path)?;
        if size < 4 * 1024 * 1024 {
            bail!("{path} is only {size} bytes - refusing to treat as a drive");
        }
        Ok(Device { path: path.to_string(), file, size })
    }

    /// Drop cached pages so a whole-disk pass doesn't evict everything else.
    pub fn drop_cache(&self) {
        unsafe {
            libc::posix_fadvise(self.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

fn block_size(file: &File, path: &str) -> Result<u64> {
    let meta = file.metadata()?;
    if meta.len() > 0 {
        return Ok(meta.len()); // regular file (testing with an image)
    }
    // BLKGETSIZE64
    let mut size: u64 = 0;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), 0x8008_1272u64 as libc::c_ulong, &mut size as *mut u64) };
    if rc != 0 {
        bail!("BLKGETSIZE64 failed on {path}");
    }
    Ok(size)
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

pub fn run_cmd(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Refuse to touch anything that is mounted or otherwise in use.
pub fn check_not_in_use(path: &str) -> Result<()> {
    let name = path.trim_start_matches("/dev/");
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in mounts.lines() {
        if let Some(dev) = line.split_whitespace().next() {
            if dev.starts_with(path) {
                bail!("{dev} is mounted - unmount it first");
            }
        }
    }
    // holders/ lists dm/md devices stacked on top of this disk or its partitions
    let sys = format!("/sys/block/{name}");
    if let Ok(entries) = std::fs::read_dir(&sys) {
        for e in entries.flatten() {
            let p = e.path();
            let holders = if p.file_name().map_or(false, |n| n == "holders") {
                p
            } else if p.is_dir() && p.join("holders").exists() {
                p.join("holders")
            } else {
                continue;
            };
            if let Ok(h) = std::fs::read_dir(&holders) {
                if let Some(x) = h.flatten().next() {
                    bail!(
                        "{} is held by {} (LVM/md/dm active?) - deactivate it first",
                        holders.display(),
                        x.file_name().to_string_lossy()
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn identify(path: &str, size: u64) -> Identity {
    let mut id = Identity { device: path.to_string(), size_bytes: size, ..Default::default() };

    if let Some(out) = run_cmd(
        "lsblk",
        &["-J", "-d", "-b", "-o", "NAME,MODEL,SERIAL,TRAN,ROTA,WWN", path],
    ) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
            if let Some(d) = v["blockdevices"].get(0) {
                let s = |k: &str| d[k].as_str().map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
                id.model = s("model").unwrap_or_default();
                id.serial = s("serial").unwrap_or_default();
                id.transport = s("tran");
                id.wwn = s("wwn");
                id.rotational = d["rota"].as_bool();
            }
        }
    }
    if let Some(s) = crate::smart::snapshot(path) {
        if id.model.is_empty() {
            id.model = s.model.clone().unwrap_or_default();
        }
        if id.serial.is_empty() {
            id.serial = s.serial.clone().unwrap_or_default();
        }
        id.firmware = s.firmware.clone();
        id.rotation_rate = s.rotation_rate;
    }
    if id.serial.is_empty() {
        id.serial = "UNKNOWN".into();
    }
    id
}
