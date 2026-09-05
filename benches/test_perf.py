#!/usr/bin/env python3
"""Regression tests for the process/terminal benchmark harness itself."""

import importlib.util
from pathlib import Path
import sys
import unittest


PERF_PATH = Path(__file__).with_name("perf.py")
SPEC = importlib.util.spec_from_file_location("fz_perf", PERF_PATH)
assert SPEC is not None and SPEC.loader is not None
perf = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = perf
SPEC.loader.exec_module(perf)


def row(text, columns=80):
    return list(text.ljust(columns))


class InteractiveRenderTests(unittest.TestCase):
    def test_new_prompt_with_old_rows_is_not_a_completed_search(self):
        """fzy draws input bytes before it computes the batch's new results."""

        screen = perf.Screen(rows=6, cols=80)
        screen.cells[0] = row("> old")
        screen.cells[1] = row("[2/2]")
        screen.cells[2] = row("old-first")
        screen.cells[3] = row("old-second")
        expected = perf.TerminalExpectation("fresh", 1, 2, ("fresh-first",))

        # This is the stale fzy state: it has already echoed the full query,
        # while its info and result rows still describe the previous search.
        screen.feed(b"\x1b[1G> fresh\x1b[K")
        self.assertFalse(screen.matches(expected))

        # Only the subsequent score-and-redraw pass may complete the sample.
        screen.feed(b"\x1b[1G> fresh\x1b[K\r\n[1/2]\x1b[K\r\nfresh-first\x1b[K\r\n\x1b[K\r\n\x1b[K")
        self.assertTrue(screen.matches(expected))


def ratio_report():
    """A complete, synthetic report whose measured ratios equal 1.1."""

    return {
        "noninteractive": [
            {
                "dataset": "small",
                "query": "exact",
                "mode": "default",
                "fz": {"wall_seconds": 1.1, "cpu_seconds": 1.1, "peak_rss_bytes": 110},
                "fzy": {"wall_seconds": 1.0, "cpu_seconds": 1.0, "peak_rss_bytes": 100},
            }
        ],
        "interactive": [
            {
                "mode": "default",
                "operation": "dense",
                "fz_redraw_seconds": 1.1,
                "fzy_redraw_seconds": 1.0,
            },
            {
                "mode": "default",
                "fz_rss_after_bytes": 110,
                "fzy_rss_after_bytes": 100,
                "fz_peak_rss_bytes": 110,
                "fzy_peak_rss_bytes": 100,
            },
        ],
    }


class RatioCheckTests(unittest.TestCase):
    def test_boundary_ratio_passes(self):
        perf.check_report_ratio(ratio_report(), 1.1)

    def test_cpu_memory_and_latency_ratio_failures(self):
        cases = (
            ("cpu_seconds", "cpu_seconds"),
            ("peak_rss_bytes", "peak_rss_bytes"),
            ("fz_redraw_seconds", "redraw_seconds"),
            ("fz_rss_after_bytes", "rss_after_bytes"),
        )
        for field, expected in cases:
            with self.subTest(field=field):
                report = ratio_report()
                if field in report["noninteractive"][0]["fz"]:
                    report["noninteractive"][0]["fz"][field] = 111
                else:
                    report["interactive"][0 if field == "fz_redraw_seconds" else 1][field] = 111
                with self.assertRaisesRegex(perf.BenchmarkError, expected):
                    perf.check_report_ratio(report, 1.1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
