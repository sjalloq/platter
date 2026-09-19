# platter

Commission, wipe, verify and log spinning-rust drives. One JSONL file per
drive serial, so "did I wipe this one?" is a `platter list` away.

```
platter id         /dev/sdX                 identity + SMART summary, no logging
platter scan       /dev/sdX [-n 64] [--full] has it been wiped? (non-destructive)
platter commission /dev/sdX [--write] [--long] acceptance test for a new drive
platter wipe       /dev/sdX [-m zero|random|dod|nnsa]
platter note       /dev/sdX|SERIAL "text"
platter list
platter show       SERIAL [-v]
```

## Commissioning a new drive

```
sudo platter commission /dev/sdX --write
```

1. SMART baseline: health, power-on hours (warns if > 24 for a "new" drive),
   reallocated / pending / uncorrectable / CRC counts.
2. SMART conveyance self-test (shipping damage) if supported, then short.
3. Full-surface pass. Read-only by default; `--write` fills with zeros and
   reads every byte back. Per-1% throughput is recorded so slow zones show up.
4. `--long` adds the SMART extended self-test (hours).
5. SMART again; any critical attribute that grew is a FAIL.

Verdict is PASS, PASS with warnings, or FAIL (exit code 2).

## Wiping

```
sudo platter wipe /dev/sdX -m dod
```

Passes are written sequentially in 4 MiB blocks. The last pass is read back
and compared byte for byte (random passes are regenerated from the seed, so
they are verified too). On success a `PLATTER-WIPED method=.. ts=.. host=..`
marker is written to sector 0; `scan` recognises it and scrub's `SCRUBBED!`.
You must type the drive's serial to confirm unless `--yes`.

Rough timing for a 2 TB drive at ~150 MB/s: zero ≈ 7.5 h (1 write + 1 read),
dod ≈ 19 h (4 writes + 1 read).

## Checking a drive of unknown state

```
sudo platter scan /dev/sdX          # head, tail, 64 random MiB + wipefs + marker
sudo platter scan /dev/sdX --full   # read the whole thing
```

## Where the log lives

`/var/lib/platter/<SERIAL>.jsonl` as root, else `~/.local/share/platter/`.
Override with `PLATTER_DIR`. Every record carries `ts`, `event`, `host`,
`verdict` (where applicable), the drive identity and SMART snapshot.

## Building

```
cargo build --release                                   # this machine
cargo build --release --target x86_64-unknown-linux-musl # static, for old boxes
```

Runtime helpers, all optional: `smartctl` (SMART and self-tests), `lsblk`
(identity), `wipefs` (leftover filesystem signatures). Plain image files can
be used instead of a device for testing and do not need root.
