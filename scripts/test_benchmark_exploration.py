import json
import unittest
from unittest.mock import patch

import benchmark_exploration as bench


class BenchmarkTests(unittest.TestCase):
    def test_stock_defaults_and_four_point_sweeps(self):
        self.assertEqual({key: value[0] for key, value in bench.AXES.items()},
                         {"max-jumps": 7, "country-test-mirrors-per-country": 2,
                          "country-neighbors-per-country": 3})
        cases = list(bench.matrix(bench.AXES))
        self.assertEqual(len(cases), 12)
        for case in cases:
            default, values, _ = bench.AXES[case["axis"]]
            self.assertEqual(len(values), 4)
            self.assertLess(values[0], default)
            self.assertEqual(values[1], default)
            self.assertGreater(values[2], default)
            self.assertGreater(values[3], values[2])
            differences = [name for name in bench.AXES
                           if case["settings"][name] != bench.AXES[name][0]]
            self.assertEqual(differences, [] if case["baseline"] else [case["axis"]])
            self.assertEqual(case["mode"], "freshness")
            self.assertEqual(case["settings"]["top-mirrors-number-to-retest"], 0)

    def test_command_always_enables_freshness_and_skips_retest(self):
        case = next(bench.matrix(["max-jumps"]))
        fresh = bench.command("rate_mirrors", "cachyos", case["settings"], 0.5, "GB")
        self.assertIn("--freshness-check=0.5", fresh)
        self.assertIn("--top-mirrors-number-to-retest 0", " ".join(fresh))
        self.assertEqual(fresh[-1], "cachyos")
        self.assertNotIn("--save", " ".join(fresh))
        self.assertIn("--entry-country GB", " ".join(fresh))

    def test_log_has_independent_speed_and_freshness_rankings(self):
        log = """# JUMP #1
# PROBING MIRROR https://a.example/
# [US] SpeedTestResult { speed: 2.0 MB/s; downloaded: 2 MB; elapsed: 1s; connection_time: 4ms } -> https://a.example/
# PROBING MIRROR https://b.example/
# [US] SpeedTestResult { speed: 4.0 MB/s; downloaded: 4 MB; elapsed: 1s; connection_time: 4ms } -> https://b.example/
#     https://a.example/ peer build age: -0.1000 days; freshness quality: 0.9000 (0 missing, 3600s total build lag; latest package build: 2026-01-01 00:00:00 UTC)
#     https://b.example/ peer build age: -1.0000 days; freshness quality: 0.5000 (0 missing, 86400s total build lag; latest package build: 2025-12-31 00:00:00 UTC)
# ==== RESULTS (top re-tested) ====
#   1. [US] SpeedTestResult { speed: 2.5 MB/s; downloaded: 2 MB; elapsed: 1s; connection_time: 4ms } -> https://a.example/
#   2. [US] SpeedTestResult { speed: 3.5 MB/s; downloaded: 4 MB; elapsed: 1s; connection_time: 4ms } -> https://b.example/
# FINISHED AT: today
"""
        parsed = bench.parse_log(log, 2)
        self.assertEqual(parsed["initial_probes"], 2)
        self.assertEqual(parsed["jumps"], 1)
        self.assertEqual(parsed["speed_ranked_mirrors"][0]["url"], "https://b.example/")
        self.assertEqual(parsed["freshness_ranked_mirrors"][0]["url"], "https://a.example/")
        self.assertEqual(parsed["selected_top_median_mb_s"], 3.0)
        self.assertEqual(parsed["fastest_top_median_mb_s"], 3.0)
        self.assertEqual(parsed["freshest_top_median_quality"], 0.7)
        row = {**parsed, "mode": "freshness", "axis": "max-jumps", "value": 7,
               "repeat": 1, "started_utc": "2026-01-01T00:00:00+00:00",
               "ended_utc": "2026-01-01T00:00:01+00:00", "elapsed_seconds": 1.0,
               "exit_code": 0, "timed_out": False,
               "settings": {name: spec[0] for name, spec in bench.AXES.items()},
               "entry_country": "US"}
        report = {"metadata": {"started_utc": row["started_utc"],
                 "ended_utc": row["ended_utc"], "location": bench.approximate_location(False),
                 "entry_country": "US", "entry_country_source": "fallback US",
                 "target": "arch", "repeats": 1, "top": 2, "freshness_weight": 0.5,
                 "binary": {"version": "rate-mirrors config 0.33.0", "sha256": "abc"}},
                  "summary": [], "runs": [row]}
        markdown = bench.markdown_report(report)
        self.assertIn("Speed ranking", markdown)
        self.assertIn("Freshness ranking", markdown)
        self.assertIn("2026-01-01T00:00:00+00:00", markdown)
        self.assertLess(markdown.index("| 1 | 3.50 | https://b.example/ |"),
                        markdown.index("| 2 | 2.50 | https://a.example/ |"))

    def test_location_keeps_only_approximate_fields(self):
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *_):
                pass

            def read(self, _):
                return json.dumps({"ip": "203.0.113.7", "city": "Sample City",
                                   "region": "Sample Region", "country_code": "GB",
                                   "country_name": "United Kingdom", "asn": "AS123"}).encode()

        with patch.object(bench.urllib.request, "urlopen", return_value=Response()):
            location = bench.approximate_location(True)
        self.assertEqual(location["country_code"], "GB")
        self.assertEqual(location["city"], "Sample City")
        self.assertNotIn("ip", location)
        self.assertNotIn("203.0.113.7", json.dumps(location))
        self.assertEqual(bench.approximate_location(False)["status"], "skipped")

    def test_incomplete_instrumentation_is_not_valid(self):
        row = {"exit_code": 0, "timed_out": False, "retest_probes": 0,
               "selected_order": [{"url": "https://a.example/"}],
               "freshness_ranked_mirrors": [{"url": "https://a.example/"}],
               "instrumentation_missing": True}
        self.assertFalse(bench.valid(row))
        row["instrumentation_missing"] = False
        self.assertTrue(bench.valid(row))
        row["freshness_ranked_mirrors"] = []
        self.assertFalse(bench.valid(row))

    def test_incremental_comparison(self):
        def row(value, attempted, speeds, elapsed, top):
            return {"mode": "freshness", "axis": "max-jumps", "value": value,
                    "exit_code": 0, "timed_out": False, "repeat": 1,
                    "initial_attempted_urls": attempted,
                    "initial_successful_speeds_mb_s": speeds,
                    "retest_probes": 0, "jumps": value,
                    "selected_order": [{"url": "a", "speed_mb_s": top}],
                    "freshness_ranked_mirrors": [{"url": "a", "speed_mb_s": top,
                                                  "freshness": {"quality": 1.0}}],
                    "selected_top_median_mb_s": top,
                    "fastest_top_median_mb_s": top,
                    "freshest_top_median_quality": None,
                    "elapsed_seconds": elapsed}

        summaries = bench.summarize([
            row(4, ["a"], {"a": 20.0}, 10.0, 20.0),
            row(7, ["a", "b"], {"a": 20.0, "b": 5.0}, 16.0, 20.0),
        ])
        self.assertEqual(summaries[1]["new_attempted"], 1)
        self.assertEqual(summaries[1]["new_successful_median_mb_s"], 5.0)
        self.assertEqual(summaries[1]["delta_elapsed_seconds"], 6.0)
        self.assertEqual(summaries[1]["delta_fastest_top_mb_s"], 0.0)


if __name__ == "__main__":
    unittest.main()
