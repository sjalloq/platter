//! Human-readable rendering of the JSONL records. The log stays machine-first;
//! this is the view for a person trying to remember what a drive did.

use serde_json::Value;

/// "2026-09-21 10:45:50  commission  PASS with warnings (1 warning)"
pub fn headline(r: &Value) -> String {
    let detail = match r["verdict"].as_str() {
        Some(v) => {
            let c = &r["commission"];
            let counts = [count(&c["failures"], "failure"), count(&c["warnings"], "warning")];
            let counts: Vec<String> = counts.into_iter().flatten().collect();
            if counts.is_empty() {
                v.to_string()
            } else {
                format!("{v} ({})", counts.join(", "))
            }
        }
        None => r["note"].as_str().unwrap_or("").to_string(),
    };
    format!("{}  {:<16} {}", ts(r), r["event"].as_str().unwrap_or(""), detail).trim_end().into()
}

/// The full story of one record: identity, SMART movement, passes, findings.
pub fn detail(r: &Value) -> String {
    let mut out = Vec::new();
    if let Some(l) = identity(&r["identity"]) {
        out.push(l);
    }
    match r["event"].as_str().unwrap_or("") {
        "commission" => {
            let c = &r["commission"];
            out.extend(smart_table(&c["smart_before"], &c["smart_after"]));
            out.extend(delta(&c["smart_delta"]));
            out.extend(selftests(&c["selftests"]));
            for p in [&c["read"], &c["write"], &c["verify"]] {
                out.extend(pass(p));
            }
            out.extend(findings("failures", &c["failures"]));
            out.extend(findings("warnings", &c["warnings"]));
        }
        "scan" => {
            out.extend(smart_table(&r["smart"], &Value::Null));
            let s = &r["scan"];
            out.push(format!(
                "  signature {}, {} of {} sampled chunks nonzero",
                s["signature"].as_str().unwrap_or("none"),
                s["nonzero_chunks"].as_u64().unwrap_or(0),
                s["sampled_chunks"].as_u64().unwrap_or(0)
            ));
            out.extend(pass(&r["full"]));
        }
        "wipe" => {
            if let Some(ps) = r["wipe"]["passes"].as_array() {
                for p in ps {
                    out.extend(pass(p));
                }
            }
            out.extend(pass(&r["wipe"]["verify"]));
            out.extend(smart_table(&Value::Null, &r["smart_after"]));
            out.extend(delta(&r["smart_delta"]));
        }
        "commission-start" => {
            out.push(match r["write"].as_bool() {
                Some(true) => "  destructive: zero-fill write and verify".into(),
                _ => "  read-only surface pass".to_string(),
            });
        }
        "wipe-start" => {
            if let Some(m) = r["method"].as_str() {
                out.push(format!("  method: {m}"));
            }
        }
        _ => {}
    }
    if let Some(v) = r["verdict"].as_str() {
        if let Some(n) = r["note"].as_str().filter(|n| !n.is_empty()) {
            out.push(format!("  note: {n}"));
        }
        out.push(format!("  VERDICT: {v}"));
    }
    out.join("\n")
}

fn ts(r: &Value) -> String {
    r["ts"].as_str().unwrap_or("").replacen('T', " ", 1).chars().take(19).collect()
}

fn count(v: &Value, noun: &str) -> Option<String> {
    match v.as_array().map(|a| a.len()).unwrap_or(0) {
        0 => None,
        1 => Some(format!("1 {noun}")),
        n => Some(format!("{n} {noun}s")),
    }
}

fn identity(id: &Value) -> Option<String> {
    let model = id["model"].as_str().filter(|m| !m.is_empty())?;
    let mut bits = vec![
        id["device"].as_str().unwrap_or("?").to_string(),
        model.to_string(),
        format!("serial {}", id["serial"].as_str().unwrap_or("?")),
        format!("{:.0} GB", id["size_bytes"].as_f64().unwrap_or(0.0) / 1e9),
    ];
    if let Some(t) = id["transport"].as_str() {
        bits.push(t.to_string());
    }
    if let Some(f) = id["firmware"].as_str() {
        bits.push(format!("fw {f}"));
    }
    match id["rotation_rate"].as_u64() {
        Some(rpm) => bits.push(format!("{rpm} rpm")),
        None if id["rotational"].as_bool() == Some(false) => bits.push("solid state".into()),
        None => {}
    }
    Some(format!("  {}", bits.join("  ")))
}

/// One line of the SMART table: a label and how to pull it out of a snapshot.
type SmartRow = (&'static str, fn(&Value) -> Option<String>);

/// Two-column before/after table; either side may be Null for a single snapshot.
fn smart_table(before: &Value, after: &Value) -> Vec<String> {
    let rows: Vec<SmartRow> = vec![
        ("health", |s| {
            s["healthy"].as_bool().map(|h| if h { "OK".into() } else { "FAILED".into() })
        }),
        ("power-on hours", |s| s["power_on_hours"].as_u64().map(|v| v.to_string())),
        ("power cycles", |s| s["power_cycles"].as_u64().map(|v| v.to_string())),
        ("temperature", |s| s["temperature_c"].as_i64().map(|v| format!("{v}C"))),
        ("critical attrs", critical),
    ];
    let usable = |s: &Value| !s.is_null() && s["available"].as_bool() != Some(false);
    let (null, before, after) = (Value::Null, usable(before).then_some(before), usable(after).then_some(after));
    let (before, after) = (before.unwrap_or(&null), after.unwrap_or(&null));
    let both = !before.is_null() && !after.is_null();
    let mut out = Vec::new();
    for (label, get) in rows {
        let (b, a) = (get(before), get(after));
        let line = match (both, &b, &a) {
            (true, Some(b), Some(a)) if b != a => format!("{b:<14} -> {a}"),
            _ => match b.or(a) {
                Some(v) => v,
                None => continue,
            },
        };
        out.push(format!("    {label:<16} {line}"));
    }
    if out.is_empty() {
        return vec!["  SMART: unavailable".into()];
    }
    out.insert(0, "  SMART".into());
    out
}

fn critical(s: &Value) -> Option<String> {
    let attrs = s["attrs"].as_object()?;
    let bad: Vec<String> = attrs
        .iter()
        .filter(|(_, v)| v.as_u64().unwrap_or(0) > 0)
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    Some(if bad.is_empty() { "all zero".into() } else { bad.join(", ") })
}

fn delta(d: &Value) -> Vec<String> {
    let moved: Vec<String> = d
        .as_object()
        .map(|o| o.iter().map(|(k, v)| format!("{k} {:+}", v.as_i64().unwrap_or(0))).collect())
        .unwrap_or_default();
    if moved.is_empty() {
        vec![]
    } else {
        vec![format!("  SMART moved: {}", moved.join(", "))]
    }
}

fn selftests(v: &Value) -> Vec<String> {
    match v.as_array() {
        Some(a) if !a.is_empty() => a
            .iter()
            .map(|t| {
                format!(
                    "  {} self-test: {} ({})",
                    t["kind"].as_str().unwrap_or("?"),
                    t["status"].as_str().unwrap_or("?"),
                    dur(t["minutes"].as_f64().unwrap_or(0.0) * 60.0)
                )
            })
            .collect(),
        _ => vec![],
    }
}

/// One surface pass: throughput, how it fell off towards the inner tracks, errors.
fn pass(p: &Value) -> Vec<String> {
    let Some(kind) = p["kind"].as_str() else { return vec![] };
    let zones = p["zone_mb_per_s"].as_array().map(|z| {
        let f = |v: Option<&Value>| v.and_then(|v| v.as_f64()).unwrap_or(0.0);
        format!(
            "outer {:.0} -> inner {:.0} MB/s over {} zones, slowest {:.0}",
            f(z.first()),
            f(z.last()),
            z.len(),
            p["slowest_zone_mb_per_s"].as_f64().unwrap_or(0.0)
        )
    });
    let mut line = format!(
        "  {:<14} {:.0} MB/s avg, {}, {} io errors, {} mismatches",
        kind,
        p["mb_per_s"].as_f64().unwrap_or(0.0),
        dur(p["seconds"].as_f64().unwrap_or(0.0)),
        p["io_errors"].as_u64().unwrap_or(0),
        p["mismatched_blocks"].as_u64().unwrap_or(0),
    );
    if let Some(z) = zones {
        line.push_str(&format!("\n  {:<14} {z}", ""));
    }
    vec![line]
}

fn findings(label: &str, v: &Value) -> Vec<String> {
    let items = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return vec![],
    };
    let mut out = vec![format!("  {label}")];
    out.extend(items.iter().filter_map(|i| i.as_str()).map(|i| format!("    - {i}")));
    out
}

/// "9h 09m", "4m 12s", "31s"
fn dur(seconds: f64) -> String {
    let s = seconds.round().max(0.0) as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
