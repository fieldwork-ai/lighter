#!/usr/bin/env python3
"""Keep published report inputs pinned independently of scratch measurements."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))
import report


class SelectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.results = self.root / "results"
        self.records = self.root / "records"
        self.results.mkdir()
        self.records.mkdir()
        (self.records / "run.csv").write_text(
            "case,rep,ms\ncopy-tree,1,9\ncopy-tree,2,3\ncopy-tree,3,6\n"
        )
        (self.records / "run.tree").write_text("recorded environment\n")
        self.selection = {
            "schema": 1,
            "targets": {"lighter": {"csv": "../records/run.csv", "tree": "../records/run.tree"}},
        }
        self.write_selection()

    def write_selection(self):
        (self.results / "selection.json").write_text(json.dumps(self.selection))

    def test_selected_record_wins_over_scratch_result(self):
        (self.results / "lighter.csv").write_text("case,rep,ms\ncopy-tree,1,999\n")
        (self.results / "orbstack.csv").write_text("case,rep,ms\ncopy-tree,1,888\n")
        self.assertEqual(report.load("lighter", self.results), {"copy-tree": 6})
        self.assertEqual(report.load("orbstack", self.results), {})
        self.assertEqual(set(report.sources(self.results)), {"lighter"})

    def test_missing_selected_input_never_falls_back(self):
        (self.results / "lighter.csv").write_text("case,rep,ms\ncopy-tree,1,999\n")
        for name in ["run.csv", "run.tree"]:
            with self.subTest(name=name):
                path = self.records / name
                content = path.read_bytes()
                path.unlink()
                with self.assertRaises(FileNotFoundError):
                    report.load("lighter", self.results)
                path.write_bytes(content)

    def test_invalid_selection_fails(self):
        self.selection["schema"] = 2
        self.write_selection()
        with self.assertRaises(ValueError):
            report.sources(self.results)
        self.selection["schema"] = 1
        del self.selection["targets"]["lighter"]["csv"]
        self.write_selection()
        with self.assertRaises(ValueError):
            report.sources(self.results)

    def test_unselected_directory_reads_local_results(self):
        (self.results / "selection.json").unlink()
        (self.results / "lighter.csv").write_bytes((self.records / "run.csv").read_bytes())
        self.assertEqual(report.load("lighter", self.results), {"copy-tree": 6})
        self.assertEqual(report.load("absent", self.results), {})

    def test_repository_selections_resolve(self):
        for directory, _, _ in report.machines():
            for target in report.sources(directory):
                report.load(target, directory)


if __name__ == "__main__":
    unittest.main()
