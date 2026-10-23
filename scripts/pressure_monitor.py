"""Fail-closed macOS pressure monitor controlling only its caller-owned child."""
import json
import os
import subprocess
import sys
import threading
import time
from contextlib import contextmanager


def write_receipt(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with os.fdopen(os.open(path, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600), 'w') as stream:
        json.dump(value, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def memory_pressure_level():
    if sys.platform != 'darwin':
        raise RuntimeError('This GPU diagnostic requires the macOS pressure monitor')
    probe = subprocess.run(['/usr/sbin/sysctl', '-n', 'kern.memorystatus_vm_pressure_level'],
                           text=True, capture_output=True, check=True, timeout=2)
    return int(probe.stdout.strip())


@contextmanager
def protect_owned_work(client, receipt):
    stopped = threading.Event()
    observations = []
    def monitor():
        while not stopped.is_set():
            try:
                level = memory_pressure_level()
                observations.append(dict(elapsedMs=(time.monotonic()-started)*1000, level=level))
                if level != 1:
                    observations[-1]['action'] = 'terminate_owned_core'
                    client.process.terminate()  # Popen owns this child and checks its exit state.
                    return
            except Exception as error:
                observations.append(dict(elapsedMs=(time.monotonic()-started)*1000,errorType=type(error).__name__, action='terminate_owned_core'))
                client.process.terminate()
                return
            stopped.wait(1)
    started = time.monotonic()
    thread = threading.Thread(target=monitor, daemon=True)
    thread.start()
    try:
        yield
    finally:
        stopped.set()
        thread.join(timeout=3)
        write_receipt(receipt, dict(kind='owned-gpu-diagnostic-pressure-monitor',
            observations=observations, failClosed=True, otherProcessesControlled=False,
            nativeMemoryGuardDisabled=False))
