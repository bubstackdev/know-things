#!/usr/bin/env python3
"""Own-child GPU diagnostic pressure protection, without loading models."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT=Path(__file__).resolve().parent.parent
sys.path.insert(0,str(ROOT/'scripts'))
from pressure_monitor import protect_owned_work


class PressureProtection(unittest.TestCase):
    def test_warning_stops_only_selected_owned_child_and_preserves_receipt(self):
        children=[subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']) for _ in range(2)]
        try:
            with tempfile.TemporaryDirectory() as directory:
                receipt=Path(directory)/'pressure.json'
                with patch('pressure_monitor.memory_pressure_level',side_effect=[1,2]):
                    with protect_owned_work(SimpleNamespace(process=children[0]),receipt):
                        self.assertNotEqual(children[0].wait(timeout=4),0)
                        self.assertIsNone(children[1].poll())
                report=json.loads(receipt.read_text())
                self.assertEqual([row['level'] for row in report['observations']],[1,2])
                self.assertEqual(report['observations'][-1]['action'],'terminate_owned_core')
                self.assertFalse(report['otherProcessesControlled'])
        finally:
            for child in children:
                if child.poll() is None:child.terminate()
                child.wait(timeout=3)

    def test_probe_failure_stops_owned_child_and_records_failure(self):
        child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
        try:
            with tempfile.TemporaryDirectory() as directory:
                receipt=Path(directory)/'pressure.json'
                with patch('pressure_monitor.memory_pressure_level',side_effect=OSError('synthetic probe failure')):
                    with protect_owned_work(SimpleNamespace(process=child),receipt):
                        self.assertNotEqual(child.wait(timeout=3),0)
                observation=json.loads(receipt.read_text())['observations'][0]
                self.assertEqual(observation['errorType'],'OSError')
                self.assertEqual(observation['action'],'terminate_owned_core')
        finally:
            if child.poll() is None:child.terminate()
            child.wait(timeout=3)


if __name__=='__main__':unittest.main()
