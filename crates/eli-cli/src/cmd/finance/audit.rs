// ---- Local activity trail ----
//
// A books-and-records style audit trail of every finance tool invocation,
// designed so the information basis of any piece of research can be
// reconstructed and proven after the fact:
//
// 1. RECORD — one JSONL line per invocation: sequence number, timestamp,
//    user, session, full argv, duration, data-level status, result summary.
// 2. PAYLOAD — the full response the tool actually returned, archived
//    content-addressed (sha256) and gzipped under `payloads/`. The record
//    carries the hash, so "what did the data say at that moment" is
//    answerable exactly, not approximately. Identical responses dedupe to
//    one archive file automatically.
// 3. TAMPER EVIDENCE — records are hash-chained: each record's `hash` covers
//    its own content plus the previous record's hash. Any edit, deletion, or
//    reorder breaks the chain from that point forward. `finance log --verify`
//    walks the chain and reports the first break.
// 4. NO SILENT LOSS — the trail is append-only and never rotates records
//    away. Malformed invocations (bad args) are recorded too.
// 5. EXPORT — `finance log --export DIR [--since --until]` produces an
//    examiner bundle: the record slice, every referenced payload, and a
//    manifest with the chain-verification result.
//
// Controls: ELI_AUDIT_LOG=0 disables the trail entirely;
// ELI_AUDIT_ARCHIVE=0 keeps records but skips payload archiving;
// ELI_SESSION_ID tags records with a caller-chosen session label.
//
// Files (platform data dir):
//   logs/activity.jsonl        — the chained record stream
//   logs/payloads/ab/<sha>.json.gz — content-addressed response archives

static AUDIT_SUMMARY: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);
static AUDIT_PAYLOAD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Called by tool output funnels to attach a structured summary of what the
/// invocation returned (row counts, sources, bytes). Best-effort: poisoned
/// locks are ignored rather than ever failing a data call.
pub(crate) fn audit_set_summary(v: serde_json::Value) {
    if let Ok(mut cell) = AUDIT_SUMMARY.lock() {
        *cell = Some(v);
    }
}

/// Called by the shared stdout funnel with the exact serialized response the
/// caller saw. This is what gets archived.
pub(crate) fn audit_set_payload(payload: &str) {
    if let Ok(mut cell) = AUDIT_PAYLOAD.lock() {
        *cell = Some(payload.to_string());
    }
}

fn audit_take_summary() -> Option<serde_json::Value> {
    AUDIT_SUMMARY.lock().ok().and_then(|mut cell| cell.take())
}

fn audit_take_payload() -> Option<String> {
    AUDIT_PAYLOAD.lock().ok().and_then(|mut cell| cell.take())
}

fn audit_dir() -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "eli")?;
    Some(dirs.data_local_dir().join("logs"))
}

pub(crate) fn audit_log_path() -> Option<std::path::PathBuf> {
    Some(audit_dir()?.join("activity.jsonl"))
}

fn audit_enabled() -> bool {
    std::env::var("ELI_AUDIT_LOG").map(|v| v != "0").unwrap_or(true)
}

fn audit_archive_enabled() -> bool {
    std::env::var("ELI_AUDIT_ARCHIVE")
        .map(|v| v != "0")
        .unwrap_or(true)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Archive a payload content-addressed and gzipped; returns (sha256, bytes,
/// relative path). Identical payloads share one file.
fn archive_payload(dir: &std::path::Path, payload: &str) -> std::io::Result<(String, usize, String)> {
    use std::io::Write;
    let hash = sha256_hex(payload.as_bytes());
    let shard = &hash[..2];
    let rel = format!("payloads/{shard}/{hash}.json.gz");
    let path = dir.join(&rel);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(&path)?;
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        enc.write_all(payload.as_bytes())?;
        enc.finish()?;
    }
    Ok((hash, payload.len(), rel))
}

fn read_payload_archive(dir: &std::path::Path, rel: &str) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(dir.join(rel))?;
    let mut dec = flate2::read::GzDecoder::new(file);
    let mut out = String::new();
    dec.read_to_string(&mut out)?;
    Ok(out)
}

/// Advisory lock so concurrent invocations serialize their chain append
/// (two writers reading the same prev_hash would fork the chain).
struct AuditLock(std::path::PathBuf);
impl AuditLock {
    fn acquire(dir: &std::path::Path) -> Option<AuditLock> {
        let path = dir.join("activity.lock");
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Some(AuditLock(path)),
                Err(_) => {
                    // Steal locks older than 10s (crashed writer).
                    if let Ok(meta) = std::fs::metadata(&path) {
                        if let Ok(modified) = meta.modified() {
                            if modified.elapsed().map(|e| e.as_secs() > 10).unwrap_or(false) {
                                let _ = std::fs::remove_file(&path);
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        None
    }
}
impl Drop for AuditLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One-time migration: records written before the hash chain existed have no
/// `hash` field and would (correctly) fail verification forever. Move them
/// aside — preserved, never deleted — so the chain starts clean from genesis.
fn migrate_legacy_trail(log_path: &std::path::Path) {
    let Ok(raw) = std::fs::read_to_string(log_path) else {
        return;
    };
    let Some(first) = raw.lines().find(|l| !l.trim().is_empty()) else {
        return;
    };
    let is_legacy = serde_json::from_str::<serde_json::Value>(first)
        .map(|v| v.get("hash").is_none())
        .unwrap_or(true);
    if is_legacy {
        let mut dest = log_path.with_file_name("activity-legacy.jsonl");
        let mut n = 1;
        while dest.exists() {
            dest = log_path.with_file_name(format!("activity-legacy.{n}.jsonl"));
            n += 1;
        }
        let _ = std::fs::rename(log_path, dest);
    }
}

/// External-anchor checkpoint: the chain head (seq + hash) is mirrored into
/// the CONFIG directory tree on every append — a different location than the
/// log itself, so truncating or wholesale-replacing the log without also
/// updating the checkpoint is detectable by --verify. This raises the bar;
/// it is not immutability: an attacker with write access to both trees can
/// still forge both. For a true external anchor, record the head hash that
/// --verify prints somewhere off this machine periodically.
fn checkpoint_path() -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "eli")?;
    Some(dirs.config_dir().join("audit-chain-head.json"))
}

fn write_checkpoint(seq: u64, hash: &str) {
    let Some(path) = checkpoint_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let v = serde_json::json!({
        "seq": seq,
        "hash": hash,
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    let _ = std::fs::write(path, v.to_string());
}

fn read_checkpoint() -> Option<(u64, String)> {
    let raw = std::fs::read_to_string(checkpoint_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some((
        v.get("seq")?.as_u64()?,
        v.get("hash")?.as_str()?.to_string(),
    ))
}

fn last_chain_state(log_path: &std::path::Path) -> (u64, String) {
    // (next_seq, prev_hash). Genesis prev_hash is 64 zeros.
    let genesis = "0".repeat(64);
    let Ok(raw) = std::fs::read_to_string(log_path) else {
        return (1, genesis);
    };
    let Some(last) = raw.lines().rev().find(|l| !l.trim().is_empty()) else {
        return (1, genesis);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(last) else {
        return (1, genesis);
    };
    let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
    let hash = v
        .get("hash")
        .and_then(|h| h.as_str())
        .unwrap_or(&genesis)
        .to_string();
    (seq + 1, hash)
}

/// Compute the chained hash of a record: sha256 of the record serialized
/// WITHOUT its `hash` field (serde_json maps are BTree-ordered, so the
/// serialization is canonical). `prev_hash` is part of the record, which is
/// what links the chain.
fn record_hash(record: &serde_json::Value) -> String {
    let mut without = record.clone();
    if let Some(obj) = without.as_object_mut() {
        obj.remove("hash");
    }
    sha256_hex(without.to_string().as_bytes())
}

fn append_chained_record(mut record: serde_json::Value) {
    let Some(dir) = audit_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Some(log_path) = audit_log_path() else {
        return;
    };
    let Some(_lock) = AuditLock::acquire(&dir) else {
        return;
    };
    migrate_legacy_trail(&log_path);
    let (seq, prev_hash) = last_chain_state(&log_path);
    if let Some(obj) = record.as_object_mut() {
        obj.insert("seq".to_string(), serde_json::json!(seq));
        obj.insert("prev_hash".to_string(), serde_json::json!(prev_hash));
    }
    let hash = record_hash(&record);
    if let Some(obj) = record.as_object_mut() {
        obj.insert("hash".to_string(), serde_json::json!(hash));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            f,
            "{}",
            serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string())
        );
        write_checkpoint(seq, &hash);
    }
}

fn base_record(ts: chrono::DateTime<chrono::Utc>, argv: Vec<String>) -> serde_json::Value {
    let mut record = serde_json::json!({
        "ts": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "argv": argv,
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let Ok(user) = std::env::var("USER") {
        record["user"] = serde_json::json!(user);
    }
    if let Ok(session) = std::env::var("ELI_SESSION_ID") {
        record["session"] = serde_json::json!(session);
    }
    record
}

/// Wraps the finance dispatcher with the activity trail. Recording is
/// best-effort: a failure to write the trail never affects the data call.
async fn run_finance_with_audit(cmd: FinanceCommand) -> Result<()> {
    // The log query command doesn't log itself — reading the trail is not
    // research activity, and self-logging would bury the signal.
    let cmd = match cmd {
        FinanceCommand::Log(args) => return cmd_finance_log(args),
        other => other,
    };
    if !audit_enabled() {
        return cmd_finance(cmd).await;
    }
    let started = std::time::Instant::now();
    let ts = chrono::Utc::now();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = cmd_finance(cmd).await;
    let summary = audit_take_summary();
    let payload = audit_take_payload();
    // Status reflects the DATA outcome, not just the process exit: a call
    // that printed {"status":"error","series":[]} and exited 0 is a failed
    // data pull and must not read as "ok" in the trail.
    let status = if result.is_err() {
        "error"
    } else {
        match summary
            .as_ref()
            .and_then(|s| s.get("status"))
            .and_then(|v| v.as_str())
        {
            Some("error") => "error",
            Some("partial") => "partial",
            _ => "ok",
        }
    };
    let mut record = base_record(ts, argv);
    record["duration_ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
    record["status"] = serde_json::json!(status);
    if let Err(e) = &result {
        record["error"] = serde_json::json!(format!("{e:#}"));
    }
    if let Some(summary) = summary {
        record["summary"] = summary;
    }
    match payload {
        Some(payload) if audit_archive_enabled() => {
            if let Some(dir) = audit_dir() {
                if let Ok((hash, bytes, rel)) = archive_payload(&dir, &payload) {
                    record["payload"] = serde_json::json!({
                        "sha256": hash,
                        "bytes": bytes,
                        "path": rel,
                    });
                }
            }
        }
        Some(_) => {
            record["payload"] = serde_json::json!({"archived": false, "reason": "ELI_AUDIT_ARCHIVE=0"});
        }
        None => {
            // Honest gap marker: this tool's output path doesn't flow through
            // the shared funnel yet, so the response content wasn't captured.
            record["payload"] = serde_json::json!({"archived": false, "reason": "not_captured"});
        }
    }
    append_chained_record(record);
    result
}

/// Log a malformed invocation that clap rejected before any command ran —
/// otherwise agent-generated bad args (the most common real source) leave
/// zero trace in the trail.
pub(crate) fn audit_log_arg_error(message: &str) {
    if !audit_enabled() {
        return;
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) != Some("finance") {
        return;
    }
    let mut record = base_record(chrono::Utc::now(), argv);
    record["status"] = serde_json::json!("arg_error");
    record["error"] = serde_json::json!(message.lines().next().unwrap_or(message));
    append_chained_record(record);
}

fn verify_chain(raw: &str) -> serde_json::Value {
    let genesis = "0".repeat(64);
    let mut prev = genesis;
    let mut expected_seq: u64 = 1;
    let mut records: u64 = 0;
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return serde_json::json!({
                "intact": false, "records_verified": records,
                "break_at_line": i + 1, "reason": "unparseable record",
            });
        };
        let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        let rec_prev = v.get("prev_hash").and_then(|h| h.as_str()).unwrap_or("");
        let rec_hash = v.get("hash").and_then(|h| h.as_str()).unwrap_or("");
        if seq != expected_seq {
            return serde_json::json!({
                "intact": false, "records_verified": records,
                "break_at_line": i + 1,
                "reason": format!("sequence gap: expected seq {expected_seq}, found {seq} (records removed or reordered)"),
            });
        }
        if rec_prev != prev {
            return serde_json::json!({
                "intact": false, "records_verified": records,
                "break_at_line": i + 1,
                "reason": "prev_hash mismatch (prior record altered or removed)",
            });
        }
        if record_hash(&v) != rec_hash {
            return serde_json::json!({
                "intact": false, "records_verified": records,
                "break_at_line": i + 1,
                "reason": "record hash mismatch (record content altered)",
            });
        }
        prev = rec_hash.to_string();
        expected_seq += 1;
        records += 1;
    }
    serde_json::json!({"intact": true, "records_verified": records})
}

fn cmd_finance_log(args: FinanceLogArgs) -> Result<()> {
    let path = audit_log_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve platform data dir for activity trail"))?;
    if args.r#where {
        println!("{}", path.display());
        return Ok(());
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            println!(
                "{}",
                serde_json::json!({
                    "entries": [],
                    "note": format!("no activity trail yet at {}", path.display())
                })
            );
            return Ok(());
        }
    };

    if args.verify {
        let mut result = verify_chain(&raw);
        result["log_path"] = serde_json::json!(path.display().to_string());
        // Head of the trail as present on disk — record this value somewhere
        // OFF this machine periodically to gain a true external anchor.
        let mut head: Option<(u64, String)> = None;
        let mut by_seq: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(seq), Some(hash)) = (
                    v.get("seq").and_then(|s| s.as_u64()),
                    v.get("hash").and_then(|h| h.as_str()),
                ) {
                    by_seq.insert(seq, hash.to_string());
                    if head.as_ref().map_or(true, |(hs, _)| seq > *hs) {
                        head = Some((seq, hash.to_string()));
                    }
                }
            }
        }
        if let Some((seq, hash)) = &head {
            result["head"] = serde_json::json!({"seq": seq, "hash": hash});
        }
        // Cross-check against the config-dir checkpoint: a truncated or
        // wholesale-replaced log is internally consistent, so the in-file
        // chain alone can't see it — the mirrored head can.
        match read_checkpoint() {
            Some((cp_seq, cp_hash)) => {
                let trail_hash_at_cp = by_seq.get(&cp_seq);
                let checkpoint_ok = trail_hash_at_cp == Some(&cp_hash);
                result["checkpoint"] = serde_json::json!({
                    "seq": cp_seq,
                    "matches_trail": checkpoint_ok,
                });
                if !checkpoint_ok {
                    result["intact"] = serde_json::json!(false);
                    result["reason"] = serde_json::json!(if trail_hash_at_cp.is_none() {
                        format!("checkpoint seq {cp_seq} is missing from the trail — records were TRUNCATED or the log was replaced")
                    } else {
                        format!("trail record at checkpoint seq {cp_seq} does not match the mirrored head hash — the log was altered or replaced")
                    });
                }
            }
            None => {
                result["checkpoint"] = serde_json::json!({"present": false, "note": "no mirrored head yet (config dir); truncation of the newest records would be undetectable until the next append"});
            }
        }
        println!("{result}");
        return Ok(());
    }

    let since = args
        .since
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .map_err(|e| anyhow::anyhow!("invalid --since (want ISO-8601 UTC): {e}"))
        })
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .map_err(|e| anyhow::anyhow!("invalid --until (want ISO-8601 UTC): {e}"))
        })
        .transpose()?;
    let grep = args.grep.as_deref().map(str::to_ascii_lowercase);

    let in_window = |line: &str| -> bool {
        if since.is_none() && until.is_none() {
            return true;
        }
        let ts = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| {
                v.get("ts")
                    .and_then(|t| t.as_str())
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            })
            .map(|t| t.with_timezone(&chrono::Utc));
        let Some(ts) = ts else { return false };
        if let Some(since) = &since {
            if ts < *since {
                return false;
            }
        }
        if let Some(until) = &until {
            if ts > *until {
                return false;
            }
        }
        true
    };

    let matches: Vec<&str> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            if let Some(g) = &grep {
                if !line.to_ascii_lowercase().contains(g.as_str()) {
                    return false;
                }
            }
            in_window(line)
        })
        .collect();

    // Examiner bundle: record slice + referenced payloads + manifest with
    // the chain-verification result over the FULL trail.
    if let Some(export_dir) = args.export.as_deref() {
        let dir = std::path::Path::new(export_dir);
        if dir.join("manifest.json").exists() {
            anyhow::bail!(
                "{} already contains an examiner bundle — refusing to overwrite a chain-of-custody artifact; export to a fresh directory",
                dir.display()
            );
        }
        std::fs::create_dir_all(dir.join("payloads"))?;
        let audit_base = audit_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve platform data dir"))?;
        let mut exported_payloads = 0usize;
        let mut missing_payloads = 0usize;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(dir.join("activity.jsonl"))?;
            for line in &matches {
                writeln!(f, "{line}")?;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(rel) = v.pointer("/payload/path").and_then(|p| p.as_str()) {
                        let src = audit_base.join(rel);
                        let dst = dir.join(rel);
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        if std::fs::copy(&src, &dst).is_ok() {
                            exported_payloads += 1;
                        } else {
                            missing_payloads += 1;
                        }
                    }
                }
            }
        }
        let manifest = serde_json::json!({
            "exported_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "records": matches.len(),
            "payload_files": exported_payloads,
            "payload_files_missing": missing_payloads,
            "window": {"since": args.since, "until": args.until},
            "filter": args.grep,
            "chain_verification_full_trail": verify_chain(&raw),
            "source_log": path.display().to_string(),
            "note": "payload files are gzipped JSON, content-addressed by sha256; each record's payload.sha256 must match the decompressed file's hash",
        });
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest)?,
        )?;
        println!(
            "{}",
            serde_json::json!({"ok": true, "export_dir": export_dir, "records": matches.len(), "payload_files": exported_payloads})
        );
        return Ok(());
    }

    if args.stats {
        // Aggregate: per-tool call counts, durations, error rates.
        let mut per_tool: std::collections::BTreeMap<String, (u64, u64, u64)> =
            std::collections::BTreeMap::new(); // (calls, total_ms, errors)
        for line in &matches {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let tool = v
                .get("argv")
                .and_then(|a| a.get(1))
                .and_then(|t| t.as_str())
                .unwrap_or("?")
                .to_string();
            let ms = v.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0);
            let err = matches!(
                v.get("status").and_then(|s| s.as_str()),
                Some("error") | Some("arg_error")
            );
            let e = per_tool.entry(tool).or_insert((0, 0, 0));
            e.0 += 1;
            e.1 += ms;
            e.2 += err as u64;
        }
        let stats: serde_json::Map<String, serde_json::Value> = per_tool
            .into_iter()
            .map(|(tool, (calls, total_ms, errors))| {
                (
                    tool,
                    serde_json::json!({
                        "calls": calls,
                        "avg_ms": if calls > 0 { total_ms / calls } else { 0 },
                        "errors": errors,
                    }),
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "entries_matched": matches.len(),
                "per_tool": stats,
                "log_path": path.display().to_string(),
            })
        );
        return Ok(());
    }

    // Replay a single archived payload: show exactly what the tool returned.
    if let Some(seq) = args.show_payload {
        let audit_base = audit_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve platform data dir"))?;
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("seq").and_then(|s| s.as_u64()) == Some(seq) {
                let Some(rel) = v.pointer("/payload/path").and_then(|p| p.as_str()) else {
                    anyhow::bail!("record seq {seq} has no archived payload");
                };
                let payload = read_payload_archive(&audit_base, rel)
                    .map_err(|e| anyhow::anyhow!("read payload archive: {e}"))?;
                // Integrity check on replay: the content must still match
                // the hash recorded at capture time.
                let expected = v
                    .pointer("/payload/sha256")
                    .and_then(|h| h.as_str())
                    .unwrap_or("");
                let actual = sha256_hex(payload.as_bytes());
                if actual != expected {
                    anyhow::bail!(
                        "payload integrity FAILURE for seq {seq}: archived content hash {actual} != recorded {expected}"
                    );
                }
                println!("{payload}");
                return Ok(());
            }
        }
        anyhow::bail!("no record with seq {seq}");
    }

    let start = matches.len().saturating_sub(args.tail);
    for line in &matches[start..] {
        println!("{line}");
    }
    Ok(())
}
