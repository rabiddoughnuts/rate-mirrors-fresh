#!/usr/bin/env python3
"""Produce a shareable report on rate-mirrors search depth.

One option changes at a time around the stock v0.31 defaults. The benchmark
never passes --save and removes RATE_MIRRORS_* environment overrides.
"""

import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path


# (stock default, one below / default / two above, phase). These defaults were
# checked against the stock rate-mirrors v0.31.0 executable, not this fork.
AXES = {
    "max-jumps": (7, (4, 7, 10, 14), "exploration"),
    "country-test-mirrors-per-country": (2, (1, 2, 4, 8), "exploration"),
    "country-neighbors-per-country": (3, (1, 3, 5, 8), "exploration"),
}
STOCK_RETEST_DEFAULT = 5
BENCHMARK_RETEST_COUNT = 0
GEO_URL = "https://ipapi.co/json/"
SPEED_RE = re.compile(r"speed:\s*([0-9]+(?:\.[0-9]+)?)\s*([KMGTPE]?B)/s")
RESULT_RE = re.compile(r"SpeedTestResult .*? -> (\S+)")
PROBE_RE = re.compile(r"PROBING MIRROR (\S+)")
FINAL_RE = re.compile(r"^#\s*\d+\.\s+.*SpeedTestResult ")
JUMP_RE = re.compile(r"^# JUMP #\d+")
PEER_RE = re.compile(
    r"^(?:#\s*)?(\S+) peer build age: ([+-]?[\d.]+) days; "
    r"freshness quality: ([\d.]+) \((\d+) missing, (\d+)s total build lag; "
    r"latest package build: ([^)]+)\)"
)
UNITS = {"B": 1, "KB": 1_000, "MB": 1_000_000, "GB": 1_000_000_000,
         "TB": 1_000_000_000_000, "PB": 1_000_000_000_000_000,
         "EB": 1_000_000_000_000_000_000}


def utc_now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def speed_mb(line):
    match = SPEED_RE.search(line)
    return float(match.group(1)) * UNITS[match.group(2)] / 1_000_000 if match else None


def parse_log(log, top_n):
    attempted, initial_speeds, final, freshness = set(), {}, [], {}
    in_retest = in_results = False
    probes = retest_probes = jumps = 0
    for line in log.splitlines():
        if "RE-TESTING TOP MIRRORS" in line:
            in_retest = True
        if "==== RESULTS" in line:
            in_results = True
        if JUMP_RE.match(line):
            jumps += 1
        probe = PROBE_RE.search(line)
        if probe:
            if in_retest:
                retest_probes += 1
            else:
                probes += 1
                attempted.add(probe.group(1))
        peer = PEER_RE.search(line)
        if peer:
            freshness[peer.group(1)] = {
                "age_days_vs_frontier": float(peer.group(2)),
                "quality": float(peer.group(3)),
                "missing_packages": int(peer.group(4)),
                "total_build_lag_seconds": int(peer.group(5)),
                "latest_package_build_utc": peer.group(6),
            }
        result = RESULT_RE.search(line)
        speed = speed_mb(line)
        if result and speed is not None:
            url = result.group(1)
            if in_results and FINAL_RE.match(line):
                final.append({"url": url, "speed_mb_s": speed})
            elif not in_retest and not in_results:
                initial_speeds[url] = speed
    for mirror in final:
        mirror["freshness"] = freshness.get(mirror["url"])
    speed_rank = sorted(final, key=lambda item: (-item["speed_mb_s"], item["url"]))
    freshness_rank = sorted((item for item in final if item["freshness"] is not None),
                            key=lambda item: (-item["freshness"]["quality"],
                                              -item["freshness"]["age_days_vs_frontier"],
                                              -item["speed_mb_s"], item["url"]))
    selected_speeds = [item["speed_mb_s"] for item in final[:top_n]]
    return {
        "initial_attempted_urls": sorted(attempted),
        "initial_successful_speeds_mb_s": initial_speeds,
        "initial_probes": probes, "retest_probes": retest_probes, "jumps": jumps,
        "selected_order": final,
        "speed_ranked_mirrors": speed_rank,
        "freshness_ranked_mirrors": freshness_rank,
        "selected_top_median_mb_s": median(selected_speeds),
        "fastest_top_median_mb_s": median([item["speed_mb_s"] for item in speed_rank[:top_n]]),
        "freshest_top_median_quality": median(
            [item["freshness"]["quality"] for item in freshness_rank[:top_n]]),
        "instrumentation_missing": bool(final) and not attempted,
    }


def matrix(axes):
    for axis in axes:
        default, values, stage = AXES[axis]
        for value in values:
            knobs = {name: spec[0] for name, spec in AXES.items()}
            knobs[axis] = value
            knobs["top-mirrors-number-to-retest"] = BENCHMARK_RETEST_COUNT
            yield {"mode": "freshness", "axis": axis, "value": value,
                   "stock_default": default, "baseline": value == default,
                   "stage": stage, "settings": knobs}


def command(binary, target, settings, freshness_weight, entry_country):
    args = [str(binary)]
    for name, value in settings.items():
        args.extend((f"--{name}", str(value)))
    args.extend(("--entry-country", entry_country))
    args.append(f"--freshness-check={freshness_weight}")
    args.append(target)
    return args


def approximate_location(enabled):
    location = {"status": "skipped" if not enabled else "unavailable", "source": GEO_URL,
                "city": None, "region": None, "country": None, "country_code": None,
                "asn": None}
    if not enabled:
        return location
    try:
        request = urllib.request.Request(GEO_URL, headers={"User-Agent": "rate-mirrors-benchmark/1"})
        with urllib.request.urlopen(request, timeout=5) as response:
            body = response.read(8193)
        if len(body) > 8192:
            return location
        data = json.loads(body)
        code = data.get("country_code") or data.get("country")
        if not isinstance(code, str) or not re.fullmatch(r"[A-Za-z]{2}", code):
            return location
        location["status"] = "approximate"
        location["country_code"] = code.upper()
        for source, dest in (("city", "city"), ("region", "region"),
                             ("country_name", "country"), ("asn", "asn")):
            value = data.get(source)
            if isinstance(value, str):
                location[dest] = value[:100]
    except (OSError, ValueError, TypeError):
        pass  # A geo service failure must not prevent benchmarking.
    return location  # Never retain the public IP or full provider response.


def binary_metadata(binary):
    digest = hashlib.sha256()
    with binary.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    result = subprocess.run([str(binary), "--version"], stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, timeout=5, check=False)
    version = result.stdout.splitlines()[0].strip() if result.stdout else "unknown"
    return {"name": binary.name, "version": version[:200],
            "sha256": digest.hexdigest()}


def run_case(args, case, repeat, destination, entry_country):
    cmd = command(args.binary, args.target, case["settings"],
                  args.freshness_weight, entry_country)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("RATE_MIRRORS_")}
    started_utc = utc_now()
    start = time.monotonic()
    try:
        process = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 text=True, errors="replace", timeout=args.timeout_seconds,
                                 env=env, check=False)
        output, exit_code, timed_out = process.stdout, process.returncode, False
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout or b""
        if isinstance(output, bytes):
            output = output.decode(errors="replace")
        exit_code, timed_out = None, True
    elapsed = time.monotonic() - start
    log_name = None
    if args.keep_logs:
        log_name = f"{case['mode']}-{case['axis']}-{case['value']}-repeat-{repeat}.log"
        (destination / log_name).write_text(output, encoding="utf-8")
    return {**case, **parse_log(output, args.top), "repeat": repeat,
            "arguments": cmd[1:], "entry_country": entry_country,
            "started_utc": started_utc, "ended_utc": utc_now(),
            "elapsed_seconds": elapsed, "exit_code": exit_code,
            "timed_out": timed_out, "log": log_name}


def median(values):
    values = [value for value in values if value is not None]
    return statistics.median(values) if values else None


def valid(row):
    return (row["exit_code"] == 0 and not row["timed_out"]
            and row["retest_probes"] == 0 and bool(row["selected_order"])
            and bool(row["freshness_ranked_mirrors"]))


def summarize(rows):
    summaries = []
    for mode, axis in dict.fromkeys((row["mode"], row["axis"]) for row in rows):
        prior = None
        for value in AXES[axis][1]:
            trials = [row for row in rows if row["mode"] == mode and
                      row["axis"] == axis and row["value"] == value]
            if not trials:
                continue
            good = [row for row in trials if valid(row)]
            summary = {
                "mode": mode, "axis": axis, "value": value,
                "stock_default": AXES[axis][0], "runs": len(trials),
                "valid_runs": len(good),
                "elapsed_seconds": median([r["elapsed_seconds"] for r in good]),
                "initial_attempted": median([len(r["initial_attempted_urls"]) for r in good]),
                "initial_successful": median([len(r["initial_successful_speeds_mb_s"]) for r in good]),
                "retest_probes": median([r["retest_probes"] for r in good]),
                "jumps": median([r["jumps"] for r in good]),
                "final_mirrors": median([len(r["selected_order"]) for r in good]),
                "selected_top_median_mb_s": median([r["selected_top_median_mb_s"] for r in good]),
                "fastest_top_median_mb_s": median([r["fastest_top_median_mb_s"] for r in good]),
                "freshest_top_median_quality": median([r["freshest_top_median_quality"] for r in good]),
                "new_attempted": None, "new_successful_median_mb_s": None,
                "delta_elapsed_seconds": None, "delta_fastest_top_mb_s": None,
            }
            if prior and good:
                earlier = {row["repeat"]: row for row in prior["good"]}
                pairs = [(row, earlier[row["repeat"]]) for row in good
                         if row["repeat"] in earlier]
                summary["new_attempted"] = median([
                    len(set(current["initial_attempted_urls"]) - set(previous["initial_attempted_urls"]))
                    for current, previous in pairs])
                new_speeds = [speed for current, previous in pairs
                              for url, speed in current["initial_successful_speeds_mb_s"].items()
                              if url not in previous["initial_successful_speeds_mb_s"]]
                summary["new_successful_median_mb_s"] = median(new_speeds)
                for field, delta in (("elapsed_seconds", "delta_elapsed_seconds"),
                                     ("fastest_top_median_mb_s", "delta_fastest_top_mb_s")):
                    now, before = summary[field], prior["summary"][field]
                    if now is not None and before is not None:
                        summary[delta] = now - before
            summaries.append(summary)
            prior = {"good": good, "summary": summary} if good else None
    return summaries


def fmt(value, digits=2):
    return "—" if value is None else f"{value:.{digits}f}"


def markdown_report(report):
    meta, summary, rows = report["metadata"], report["summary"], report["runs"]
    place = meta["location"]
    location = ", ".join(part for part in (place["city"], place["region"],
                                            place["country"] or place["country_code"]) if part)
    lines = ["# Rate-mirrors exploration report", "",
             f"Run: {meta['started_utc']} to {meta['ended_utc']}.",
             f"Approximate network location: {location or 'unavailable'} "
             f"(ASN {place['asn'] or 'unknown'}; lookup {place['status']}).",
             f"Entry country used: {meta['entry_country']} ({meta['entry_country_source']}).",
             f"Target: {meta['target']}; repeats: {meta['repeats']}; top-N: {meta['top']}.",
             f"Binary: {meta['binary']['version']} (SHA-256 {meta['binary']['sha256']}).",
             "Location is inferred from the public network exit and may reflect a VPN or proxy.",
             "The report does not store the public IP. No pacman mirrorlist was saved.", "",
             "## Setting comparison", "",
             "The exploration baseline uses stock v0.31 defaults (7 jumps, 2 mirrors/country,",
             "3 neighbors/country). Each axis tests one lower value, the stock default, and",
             "two higher values. Freshness is enabled for every run. The normal retest",
             f"default is {STOCK_RETEST_DEFAULT}, but this benchmark forces retests to 0",
             "to isolate the initial exploration pass.", "",
             "| Mode | Axis | Value | Valid/total | Checked | New | New mirror median MB/s | Final | Fastest top median MB/s | Δ fastest MB/s | Freshest top quality | Time s | Δ time s |",
             "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for item in summary:
        lines.append("| " + " | ".join((item["mode"], item["axis"], str(item["value"]),
                     f"{item['valid_runs']}/{item['runs']}", fmt(item["initial_attempted"], 0),
                     fmt(item["new_attempted"], 0),
                     fmt(item["new_successful_median_mb_s"]), fmt(item["final_mirrors"], 0),
                     fmt(item["fastest_top_median_mb_s"]), fmt(item["delta_fastest_top_mb_s"]),
                     fmt(item["freshest_top_median_quality"], 4),
                     fmt(item["elapsed_seconds"]), fmt(item["delta_elapsed_seconds"]))) + " |")
    lines += ["", "New and delta columns compare to the next lower value of the same axis/mode.",
              "Mirror sets need not be nested: changing depth can change the exploration path,",
              "and mirrors can change between runs. 'New' means absent from the lower-value run.",
              "A small speed gain for much more time suggests diminishing returns,",
              "not a proven global optimum. Compare repeated reports from multiple networks.",
              "Speeds are rounded by rate-mirrors' display output; freshness quality is a",
              "package-build-date proxy, not a mirror's actual last-sync time. Each run",
              "constructs its own package frontier, so quality values across runs are not",
              "directly comparable; use the per-run rankings and mirror overlap instead.",
              "Every speed shown here comes from the initial combined .files + .db probe.", ""]
    for row in rows:
        lines += [f"## {row['mode']} / {row['axis']}={row['value']} / repeat {row['repeat']}", "",
                  f"UTC: {row['started_utc']} to {row['ended_utc']}; elapsed {fmt(row['elapsed_seconds'])} s; "
                  f"exit {row['exit_code']}; timed out {row['timed_out']}.",
                  f"Settings: {', '.join(f'{key}={value}' for key, value in row['settings'].items())}; "
                  f"entry-country={row['entry_country']}; freshness weight="
                  f"{meta['freshness_weight']}.",
                  f"Initial mirrors checked: {len(row['initial_attempted_urls'])} "
                  f"({len(row['initial_successful_speeds_mb_s'])} speed successes); "
                  f"retest probes: {row['retest_probes']}; jumps: {row['jumps']}; "
                  f"final mirrors: {len(row['selected_order'])}.", ""]
        if row["instrumentation_missing"]:
            lines += ["Warning: this binary did not emit probe markers; checked count is incomplete.", ""]
        if not valid(row):
            lines += ["This run has no valid initial-only final mirror list; inspect exit status,",
                      "unexpected retest count, or optional log.", ""]
            continue
        lines += ["Speed ranking (all eligible final mirrors):", "",
                  "| Rank | MB/s | Mirror |", "| ---: | ---: | --- |"]
        for number, mirror in enumerate(row["speed_ranked_mirrors"], 1):
            lines.append(f"| {number} | {fmt(mirror['speed_mb_s'])} | {mirror['url']} |")
        lines.append("")
        lines += ["Freshness ranking (quality is higher when more packages match the",
                  "per-package newest-build frontier):", "",
                  "| Rank | Quality | Age vs frontier (days) | Missing packages | Latest package build (UTC) | Mirror |",
                  "| ---: | ---: | ---: | ---: | --- | --- |"]
        for number, mirror in enumerate(row["freshness_ranked_mirrors"], 1):
            fresh = mirror["freshness"]
            lines.append(f"| {number} | {fmt(fresh['quality'], 4)} | "
                         f"{fmt(fresh['age_days_vs_frontier'], 4)} | "
                         f"{fresh['missing_packages']} | "
                         f"{fresh['latest_package_build_utc']} | {mirror['url']} |")
        if not row["freshness_ranked_mirrors"]:
            lines.append("No freshness scores were parsed for this run.")
        lines.append("")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/rate_mirrors"))
    parser.add_argument("--target", default="arch", choices=("arch", "cachyos"))
    parser.add_argument("--entry-country", help="two-letter override; otherwise use geo country or US")
    parser.add_argument("--no-location-lookup", action="store_true",
                        help="do not contact ipapi.co; location will be unavailable")
    parser.add_argument("--axes", nargs="+", choices=tuple(AXES), default=list(AXES))
    parser.add_argument("--freshness-weight", type=float, default=0.5,
                        help="speed priority for weighted pruning, 0..1 (default: 0.5)")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--top", type=int, default=10)
    parser.add_argument("--timeout-seconds", type=int, default=300)
    parser.add_argument("--output-dir", type=Path, default=Path("benchmark-results"))
    parser.add_argument("--keep-logs", action="store_true", help="also retain raw tool output")
    parser.add_argument("--dry-run", action="store_true", help="print matrix; no network or files")
    args = parser.parse_args(argv)
    if args.repeats < 1 or args.top < 1 or args.timeout_seconds < 1:
        parser.error("--repeats, --top, and --timeout-seconds must be positive")
    if not 0 <= args.freshness_weight <= 1:
        parser.error("--freshness-weight must be between 0 and 1")
    if args.entry_country:
        if not re.fullmatch(r"[A-Za-z]{2}", args.entry_country):
            parser.error("--entry-country must be a two-letter country code")
        args.entry_country = args.entry_country.upper()
    cases = list(matrix(dict.fromkeys(args.axes)))
    if args.dry_run:
        for case in cases:
            print(" ".join(command(args.binary, args.target, case["settings"],
                                   args.freshness_weight, args.entry_country or "US")))
        print(f"{len(cases)} configurations × {args.repeats} repeat(s)")
        return 0
    args.binary = args.binary.expanduser().resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error(f"binary is missing or not executable: {args.binary}")
    started_utc = utc_now()
    location = approximate_location(not args.no_location_lookup)
    entry_country = args.entry_country or location["country_code"] or "US"
    entry_source = "explicit" if args.entry_country else (
        "network lookup" if location["country_code"] else "fallback US")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    destination = Path(tempfile.mkdtemp(prefix="exploration-", dir=args.output_dir))
    rows = []
    for case in cases:
        for repeat in range(1, args.repeats + 1):
            print(f"{case['mode']} {case['axis']}={case['value']} "
                  f"repeat {repeat}/{args.repeats}", flush=True)
            row = run_case(args, case, repeat, destination, entry_country)
            rows.append(row)
            print(f"  {row['elapsed_seconds']:.1f}s; checked="
                  f"{len(row['initial_attempted_urls'])}; final={len(row['selected_order'])}; "
                  f"exit={row['exit_code']}", flush=True)
    report = {
        "schema_version": 2,
        "metadata": {"started_utc": started_utc, "ended_utc": utc_now(),
                     "location": location, "entry_country": entry_country,
                     "entry_country_source": entry_source, "target": args.target,
                     "repeats": args.repeats, "top": args.top,
                     "freshness_weight": args.freshness_weight,
                     "binary": binary_metadata(args.binary),
                     "stock_defaults": {**{name: spec[0] for name, spec in AXES.items()},
                                        "top-mirrors-number-to-retest": STOCK_RETEST_DEFAULT},
                     "benchmark_retest_count": BENCHMARK_RETEST_COUNT,
                     "geo_lookup_sends_public_ip": not args.no_location_lookup},
        "summary": summarize(rows), "runs": rows,
    }
    (destination / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    (destination / "report.md").write_text(markdown_report(report), encoding="utf-8")
    print(f"Report: {destination / 'report.md'}")
    return 0 if all(valid(row) for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
