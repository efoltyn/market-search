#!/usr/bin/env python3
"""Profile the Kalshi and Polymarket paths of a market-search binary.

Runs a fixed list of agent workloads once each, through the CLI and through
the MCP server (stdio), with the binary's request trace on, and reports for
each one: wall time, HTTP requests, decoded bytes, and where the time went.

This is a profile, not a load test. Each workload runs once. Requests are
spaced by the binary's own gate (here one request per 550 ms per venue host
unless --spacing-ms says otherwise), and the run stops as soon as a venue
refuses a request.

    python3 bench/odds_bench.py run --bin target/debug/market-search \
        --sandbox /some/empty/dir --out before
    python3 bench/odds_bench.py compare before/results.json after/results.json

The binary runs with HOME and TMPDIR inside --sandbox and an otherwise empty
environment, so it cannot touch the real cache, catalog or activity trail and
sees no credentials. Workloads are in odds_workloads.json next to this file;
the tickers in it go stale as markets settle, so refresh them when a workload
starts returning nothing.

How to read the columns:

    wall     seconds from process start to exit (CLI) or request to response (MCP)
    reqs     HTTP requests the binary made (from its trace)
    kB       decoded response bytes
    net      time with at least one request on the wire
    gate     time spent only waiting for the per-host spacing
    local    wall - net - gate: process start, parsing, disk, everything else
    est      wall re-estimated at the product's default spacing
             (local + net + gate * default/spacing); an estimate, not a
             measurement
"""
import argparse
import json
import os
import select
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
VENUE_HOSTS = {
    "api.elections.kalshi.com": 125,
    "external-api.kalshi.com": 125,
    "gamma-api.polymarket.com": 100,
    "clob.polymarket.com": 100,
    "data-api.polymarket.com": 100,
}
CONNECT_PROBES = [
    ("api.elections.kalshi.com", "https://api.elections.kalshi.com/trade-api/v2/historical/cutoff"),
    ("gamma-api.polymarket.com", "https://gamma-api.polymarket.com/markets?limit=1&closed=false"),
    ("clob.polymarket.com", "https://clob.polymarket.com/time"),
    ("data-api.polymarket.com", "https://data-api.polymarket.com/"),
]


def sandbox_env(sandbox, trace, spacing_ms, pause_secs):
    home = os.path.join(sandbox, "home")
    tmp = os.path.join(sandbox, "tmp")
    os.makedirs(home, exist_ok=True)
    os.makedirs(tmp, exist_ok=True)
    return {
        "HOME": home,
        "TMPDIR": tmp,
        "PATH": "/usr/bin:/bin",
        "ELI_AUDIT_LOG": "0",
        "MARKET_SEARCH_HTTP_TRACE": trace,
        "MARKET_SEARCH_HTTP_MIN_INTERVAL_MS": str(spacing_ms),
        "MARKET_SEARCH_HTTP_PAUSE_SECS": str(pause_secs),
    }


def read_trace(path, since_ms=0, until_ms=None):
    rows = []
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                ts = row.get("ts", 0)
                if ts < since_ms or (until_ms is not None and ts > until_ms):
                    continue
                rows.append(row)
    except OSError:
        pass
    return rows


def union_ms(intervals):
    total, end = 0, None
    for lo, hi in sorted(intervals):
        if end is None or lo > end:
            total += hi - lo
            end = hi
        elif hi > end:
            total += hi - end
            end = hi
    return total


def subtract_ms(intervals, cover):
    """Length of `intervals` (as a union) not covered by `cover`."""
    return union_ms(intervals + cover) - union_ms(cover)


def summarize(rows, wall_ms, spacing_ms, started_ms=None):
    reqs = [r for r in rows if "host" in r]
    events = [r for r in rows if "event" in r]
    by_host, statuses, versions = {}, {}, {}
    wire, waits = [], []
    gate_by_host = {}
    bytes_total, refused, paused = 0, 0, 0
    for r in reqs:
        host = r.get("host", "?")
        by_host[host] = by_host.get(host, 0) + (0 if r.get("paused") else 1)
        status = str(r.get("status", "err"))
        statuses[status] = statuses.get(status, 0) + 1
        if r.get("paused"):
            paused += 1
            continue
        if r.get("status") == 429:
            refused += 1
        if r.get("http"):
            versions[r["http"]] = versions.get(r["http"], 0) + 1
        bytes_total += r.get("bytes", 0)
        end = r.get("ts", 0)
        sent = end - r.get("body_ms", 0) - r.get("ttfb_ms", 0)
        asked = sent - r.get("wait_ms", 0)
        wire.append((sent, end))
        if r.get("wait_ms", 0) > 0:
            waits.append((asked, sent))
            gate_by_host.setdefault(host, []).append((asked, sent))
    net_ms = union_ms(wire)
    gate_ms = subtract_ms(waits, wire)
    local_ms = max(0, wall_ms - net_ms - gate_ms)
    # Peak number of requests on the wire at once, per host: with HTTP/1.1
    # that is how many connections (and handshakes) the run needed.
    peak = {}
    for host in by_host:
        marks = []
        for r in reqs:
            if r.get("host") != host or r.get("paused"):
                continue
            end = r.get("ts", 0)
            sent = end - r.get("body_ms", 0) - r.get("ttfb_ms", 0)
            marks.append((sent, 1))
            marks.append((end, -1))
        level = best = 0
        for _, step in sorted(marks, key=lambda m: (m[0], m[1])):
            level += step
            best = max(best, level)
        peak[host] = best
    # Re-estimate the gate share at the product's default spacing.
    est_gate = 0.0
    for host, spans in gate_by_host.items():
        default = VENUE_HOSTS.get(host, spacing_ms)
        share = subtract_ms(spans, wire)
        est_gate += share * min(1.0, default / float(spacing_ms))
    first_ttfb = {}
    for r in sorted(reqs, key=lambda r: r.get("ts", 0)):
        if r.get("paused") or "ttfb_ms" not in r:
            continue
        first_ttfb.setdefault((r.get("pid"), r.get("host")), r["ttfb_ms"])
    # Time before the first request was asked for and after the last response
    # was read: start-up, argument handling and local reads on one side;
    # parsing, ranking, disk writes and output on the other.
    pre_ms = post_ms = None
    spans = wire + waits
    if started_ms is not None and spans:
        pre_ms = max(0, min(lo for lo, _ in spans) - started_ms)
        post_ms = max(0, started_ms + wall_ms - max(hi for _, hi in spans))
    return {
        "pre_ms": pre_ms,
        "post_ms": post_ms,
        "requests": sum(by_host.values()),
        "by_host": by_host,
        "bytes": bytes_total,
        "statuses": statuses,
        "http_versions": versions,
        "refused": refused,
        "paused": paused,
        "wait_sum_ms": sum(r.get("wait_ms", 0) for r in reqs),
        "net_ms": net_ms,
        "gate_ms": gate_ms,
        "local_ms": local_ms,
        "est_default_ms": int(local_ms + net_ms + est_gate),
        "peak_in_flight": peak,
        "first_ttfb_ms": sorted(first_ttfb.values()),
        "ttfb_ms": sorted(r["ttfb_ms"] for r in reqs if "ttfb_ms" in r and not r.get("paused")),
        "cache_events": len(events),
    }


def shape(text):
    """Small description of a JSON output, to see that before and after return the same things."""
    try:
        doc = json.loads(text)
    except ValueError:
        return {"json": False, "chars": len(text)}
    out = {"json": True, "chars": len(text)}
    if isinstance(doc, dict):
        for key, value in doc.items():
            if isinstance(value, list):
                out["n_" + key] = len(value)
        if isinstance(doc.get("series"), list):
            out["candles"] = sum(
                len(s.get("candles", [])) for s in doc["series"] if isinstance(s, dict)
            )
        if doc.get("status"):
            out["status"] = doc["status"]
        if isinstance(doc.get("error"), str):
            out["error"] = doc["error"][:200]
    return out


def take_saved_output(text, dest):
    """Move a large MCP output out of the shared temp dir.

    The stdio server saves outputs that are too large to return inline as
    eli_<tool>_<ms>.json in the system temp dir (not TMPDIR) and answers with
    {"_file": path, "_chars": n, ...summary}. That file is outside the
    sandbox, so the bench moves it next to its other results. Returns the size
    of the full output in characters, or None when the output came inline.
    """
    try:
        doc = json.loads(text)
    except ValueError:
        return None
    path = doc.get("_file") if isinstance(doc, dict) else None
    if not isinstance(path, str):
        return None
    base = os.path.basename(path)
    if not (base.startswith("eli_") and base.endswith(".json")):
        return None
    try:
        with open(path, "r", encoding="utf-8") as fh:
            body = fh.read()
        with open(dest, "w", encoding="utf-8") as fh:
            fh.write(body)
        os.remove(path)
        return len(body)
    except OSError:
        return doc.get("_chars") if isinstance(doc.get("_chars"), int) else None


def run_cli(binary, args, env, timeout):
    started = time.time()
    try:
        proc = subprocess.run(
            [binary] + args, env=env, stdin=subprocess.DEVNULL,
            capture_output=True, timeout=timeout,
        )
        code, out, err = proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as exc:
        code, out, err = -1, exc.stdout or b"", (exc.stderr or b"") + b"\n[bench] timed out"
    wall_ms = int((time.time() - started) * 1000)
    return wall_ms, code, out.decode("utf-8", "replace"), err.decode("utf-8", "replace"), int(started * 1000)


class McpClient:
    def __init__(self, binary, env):
        self.proc = subprocess.Popen(
            [binary, "mcp"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.next_id = 1
        self.buf = b""

    def call(self, method, params, timeout):
        rid = self.next_id
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        started = time.time()
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()
        deadline = started + timeout
        fd = self.proc.stdout.fileno()
        while True:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if not line.strip():
                    continue
                try:
                    doc = json.loads(line)
                except ValueError:
                    continue
                if doc.get("id") == rid:
                    return int((time.time() - started) * 1000), doc
            left = deadline - time.time()
            if left <= 0:
                raise TimeoutError(method)
            ready, _, _ = select.select([fd], [], [], left)
            if not ready:
                raise TimeoutError(method)
            chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError("mcp server closed its output")
            self.buf += chunk

    def notify(self, method):
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", "method": method}) + "\n").encode())
        self.proc.stdin.flush()

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def connect_profile():
    """One curl per venue host: DNS, TCP and TLS time for a fresh connection."""
    rows = {}
    fmt = "%{http_code} %{http_version} %{time_namelookup} %{time_connect} %{time_appconnect} %{time_starttransfer} %{time_total}"
    for host, url in CONNECT_PROBES:
        time.sleep(1.0)
        try:
            res = subprocess.run(
                ["curl", "-sS", "--compressed", "--max-time", "20", "-o", "/dev/null", "-w", fmt, url],
                capture_output=True, text=True, timeout=30,
            )
            parts = res.stdout.split()
            rows[host] = {
                "status": parts[0], "http": parts[1],
                "dns_ms": round(float(parts[2]) * 1000, 1),
                "tcp_ms": round((float(parts[3]) - float(parts[2])) * 1000, 1),
                "tls_ms": round((float(parts[4]) - float(parts[3])) * 1000, 1),
                "server_ms": round((float(parts[5]) - float(parts[4])) * 1000, 1),
                "total_ms": round(float(parts[6]) * 1000, 1),
            }
        except Exception as exc:  # no curl, no network
            rows[host] = {"error": str(exc)[:200]}
    return rows


def cmd_run(opts):
    binary = os.path.abspath(opts.bin)
    outdir = os.path.abspath(opts.out)
    os.makedirs(outdir, exist_ok=True)
    with open(opts.workloads, "r", encoding="utf-8") as fh:
        spec = json.load(fh)
    subs = spec.get("vars", {})

    def fill(value):
        if isinstance(value, str):
            for key, sub in subs.items():
                value = value.replace("{" + key + "}", sub)
            return value
        if isinstance(value, list):
            return [fill(v) for v in value]
        if isinstance(value, dict):
            return {k: fill(v) for k, v in value.items()}
        return value

    only = set(opts.only.split(",")) if opts.only else None
    skip = set(opts.skip.split(",")) if opts.skip else set()
    results = {
        "label": opts.label, "binary": os.path.basename(binary), "profile": opts.profile,
        "spacing_ms": opts.spacing_ms, "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "workloads": {},
    }
    result_path = os.path.join(outdir, "results.json")
    if (only or skip) and os.path.exists(result_path):
        with open(result_path, "r", encoding="utf-8") as fh:
            results = json.load(fh)

    def save():
        with open(result_path, "w", encoding="utf-8") as fh:
            json.dump(results, fh, indent=1, sort_keys=True)

    def stop_if_refused(name, summary):
        # One refusal that the binary waited out and retried is normal on a
        # shared IP. Three, or a pause, means stop and leave the venue alone.
        if summary["paused"] or summary["refused"] >= 3:
            save()
            sys.exit(
                "[bench] %s: the venue refused %d request(s) (%d more held back by the pause). "
                "Stopping; wait for the pause to end before running again."
                % (name, summary["refused"], summary["paused"])
            )
        if summary["refused"]:
            print("[bench] %s: %d request(s) refused and retried; resting 15s" % (name, summary["refused"]))
            sys.stdout.flush()
            time.sleep(15)

    if (not only or "connect" in only) and "connect" not in skip:
        results["connect"] = connect_profile()
        time.sleep(1.0)

    if (not only or "start" in only) and "start" not in skip:
        env = sandbox_env(opts.sandbox, os.path.join(outdir, "start.jsonl"), opts.spacing_ms, opts.pause_secs)
        walls = [run_cli(binary, ["--version"], env, 60)[0] for _ in range(5)]
        results["start_ms"] = sorted(walls)

    for item in spec.get("cli", []):
        name = item["name"]
        if (only and name not in only) or name in skip:
            continue
        if item.get("slow") and not only and not opts.slow:
            continue
        trace = os.path.join(outdir, name + ".jsonl")
        if os.path.exists(trace):
            os.remove(trace)
        env = sandbox_env(opts.sandbox, trace, opts.spacing_ms, opts.pause_secs)
        args = fill(item["args"])
        wall_ms, code, out, err, started_ms = run_cli(binary, args, env, opts.timeout)
        with open(os.path.join(outdir, name + ".out"), "w", encoding="utf-8") as fh:
            fh.write(out)
        with open(os.path.join(outdir, name + ".err"), "w", encoding="utf-8") as fh:
            fh.write(err)
        summary = summarize(read_trace(trace), wall_ms, opts.spacing_ms, started_ms)
        summary.update({"wall_ms": wall_ms, "exit": code, "out": shape(out), "via": "cli"})
        results["workloads"][name] = summary
        print_row(name, summary)
        save()
        stop_if_refused(name, summary)
        time.sleep(opts.gap)

    mcp_items = [
        m for m in spec.get("mcp", []) if (not only or m["name"] in only) and m["name"] not in skip
    ]
    if mcp_items:
        trace = os.path.join(outdir, "mcp.jsonl")
        env = sandbox_env(opts.sandbox, trace, opts.spacing_ms, opts.pause_secs)
        client = McpClient(binary, env)
        try:
            wall_ms, _ = client.call("initialize", {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "odds-bench", "version": "0"},
            }, 30)
            client.notify("notifications/initialized")
            results["mcp_initialize_ms"] = wall_ms
            wall_ms, listed = client.call("tools/list", None, 30)
            results["mcp_tools_list_ms"] = wall_ms
            results["mcp_tools"] = len(listed.get("result", {}).get("tools", []))
            for item in mcp_items:
                name = item["name"]
                t0 = int(time.time() * 1000)
                wall_ms, doc = client.call(
                    "tools/call", {"name": item["tool"], "arguments": fill(item["arguments"])}, opts.timeout
                )
                t1 = int(time.time() * 1000)
                text = ""
                for part in doc.get("result", {}).get("content", []):
                    if part.get("type") == "text":
                        text += part.get("text", "")
                with open(os.path.join(outdir, name + ".out"), "w", encoding="utf-8") as fh:
                    fh.write(text)
                saved = take_saved_output(text, os.path.join(outdir, name + ".saved.json"))
                summary = summarize(read_trace(trace, t0, t1 + 50), wall_ms, opts.spacing_ms, t0)
                if saved is not None:
                    summary["saved_chars"] = saved
                summary.update({
                    "wall_ms": wall_ms, "out": shape(text), "via": "mcp",
                    "exit": 1 if doc.get("error") or doc.get("result", {}).get("isError") else 0,
                })
                results["workloads"][name] = summary
                print_row(name, summary)
                save()
                stop_if_refused(name, summary)
                time.sleep(opts.gap)
        finally:
            client.close()
    save()
    print("[bench] wrote " + result_path)


def print_row(name, s):
    hosts = " ".join("%s=%d" % (h.split(".")[0], n) for h, n in sorted(s["by_host"].items()))
    edges = ""
    if s.get("pre_ms") is not None:
        edges = " (before first request %.2fs, after last %.2fs)" % (s["pre_ms"] / 1000.0, s["post_ms"] / 1000.0)
    print(
        "%-18s wall %6.2fs  reqs %4d  %8.1f kB  net %6.2fs gate %6.2fs local %5.2fs%s  est %6.2fs  exit %s  [%s]"
        % (name, s["wall_ms"] / 1000.0, s["requests"], s["bytes"] / 1000.0, s["net_ms"] / 1000.0,
           s["gate_ms"] / 1000.0, s["local_ms"] / 1000.0, edges, s["est_default_ms"] / 1000.0, s["exit"], hosts)
    )
    sys.stdout.flush()


def cmd_compare(opts):
    with open(opts.before, "r", encoding="utf-8") as fh:
        before = json.load(fh)
    with open(opts.after, "r", encoding="utf-8") as fh:
        after = json.load(fh)
    print("before: %s (%s build, %s, spacing %s ms)" % (
        before.get("label"), before.get("profile"), before.get("started"), before.get("spacing_ms")))
    print("after:  %s (%s build, %s, spacing %s ms)" % (
        after.get("label"), after.get("profile"), after.get("started"), after.get("spacing_ms")))
    print("%-22s %-4s %15s %11s %17s %15s" % ("workload", "via", "wall s", "requests", "kB", "est s"))
    names = [n for n in before.get("workloads", {}) if n in after.get("workloads", {})]
    for name in names:
        b, a = before["workloads"][name], after["workloads"][name]
        print("%-22s %-4s %6.2f -> %6.2f %4d -> %4d %7.1f -> %7.1f %6.2f -> %6.2f" % (
            name, a.get("via", ""), b["wall_ms"] / 1000.0, a["wall_ms"] / 1000.0,
            b["requests"], a["requests"], b["bytes"] / 1000.0, a["bytes"] / 1000.0,
            b["est_default_ms"] / 1000.0, a["est_default_ms"] / 1000.0))
    for label, doc in (("before", before), ("after", after)):
        if "start_ms" in doc:
            print("%s process start (--version, 5 runs, ms): %s" % (label, doc["start_ms"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run")
    run.add_argument("--bin", required=True)
    run.add_argument("--sandbox", required=True, help="directory for the binary's HOME and TMPDIR")
    run.add_argument("--out", required=True, help="directory for traces, outputs and results.json")
    run.add_argument("--workloads", default=os.path.join(HERE, "odds_workloads.json"))
    run.add_argument("--label", default="run")
    run.add_argument("--profile", default="dev", help="build profile of the binary, for the record")
    run.add_argument("--spacing-ms", type=int, default=550)
    run.add_argument("--pause-secs", type=int, default=600)
    run.add_argument("--gap", type=float, default=1.0, help="seconds to rest between workloads")
    run.add_argument("--timeout", type=int, default=900)
    run.add_argument("--only", default="", help="comma-separated workload names (also: connect, start)")
    run.add_argument("--skip", default="", help="comma-separated workload names to leave out; results merge into an existing results.json")
    run.add_argument("--slow", action="store_true", help="also run the workloads marked slow (the catalog sync)")
    run.set_defaults(func=cmd_run)
    cmp_ = sub.add_parser("compare")
    cmp_.add_argument("before")
    cmp_.add_argument("after")
    cmp_.set_defaults(func=cmd_compare)
    opts = parser.parse_args()
    opts.func(opts)


if __name__ == "__main__":
    main()
