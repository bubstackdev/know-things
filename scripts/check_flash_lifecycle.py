#!/usr/bin/env python3
"""Native worker ownership checks without loading model weights or using the GPU."""
import argparse
import json
import os
import select
import struct
import subprocess
import time
from pathlib import Path


def read_exact(stream, size, deadline):
    output = bytearray()
    while len(output) < size:
        remaining = deadline-time.monotonic()
        if remaining <= 0 or not select.select([stream], [], [], remaining)[0]:
            raise TimeoutError('Worker startup deadline')
        data = os.read(stream.fileno(), size-len(output))
        if not data:
            raise RuntimeError('Worker closed its handshake pipe')
        output.extend(data)
    return bytes(output)


def check(root, action):
    bundle = json.loads((root/'bundle.json').read_text())
    read_fd, write_fd = os.pipe()
    env = dict(os.environ, KNOW_THINGS_ASSET_ROOT=str(root), KNOW_THINGS_CONTROL_FD=str(read_fd))
    process = subprocess.Popen([str(root/bundle['entrypoint'])], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, pass_fds=(read_fd,), env=env)
    os.close(read_fd)
    try:
        deadline = time.monotonic()+10
        size = struct.unpack('>I', read_exact(process.stdout, 4, deadline))[0]
        assert 0 < size <= 1024*1024
        hello = json.loads(read_exact(process.stdout, size, deadline))
        assert hello['flashArtifactId'] == bundle['manifest']['flashArtifactId']
        started = time.monotonic()
        if action == 'control_eof':
            os.close(write_fd)
            write_fd = None
            expected = 0
        else:
            message = json.dumps(dict(operation='unknown_mandatory_control')).encode()
            os.write(write_fd, struct.pack('>I', len(message))+message)
            expected = 2
        # stdin deliberately stays open: only the control path can terminate the idle worker.
        code = process.wait(timeout=5)
        assert code == expected, (action, code)
        return dict(action=action, exitCode=code, elapsedMs=(time.monotonic()-started)*1000, passed=True)
    finally:
        if write_fd is not None:
            os.close(write_fd)
        if process.poll() is None:
            process.terminate()  # This script's own child only.
            process.wait(timeout=5)
        process.stdin.close()
        process.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker-bundle', type=Path, required=True)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = args.worker_bundle.resolve()
    subprocess.run([args.binary, 'assets', 'verify', str(root)], check=True, capture_output=True, timeout=30)
    bundle = json.loads((root/'bundle.json').read_text())
    summary = dict(kind='native-worker-control-lifecycle', flashArtifactId=bundle['manifest']['flashArtifactId'],
                   workerBuild=bundle['manifest']['workerBuild'],
                   checks=[check(root, action) for action in ['control_eof', 'malformed_control']],
                   limitations=['No model loaded; active GPU cancellation and reset remain unqualified'])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps(summary))

if __name__ == '__main__':
    main()
