"""Bounded synchronous client for local evaluation scripts; owns only its child."""
import json
import os
import select
import subprocess
import time

MAX_FRAME = 1024*1024


class LocalClient:
    def __init__(self, binary, store, options=(), cwd=None, stderr=None):
        self.process = subprocess.Popen([str(binary), 'local', str(store), *map(str, options)],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, cwd=cwd)
        os.set_blocking(self.process.stdin.fileno(), False)
        os.set_blocking(self.process.stdout.fileno(), False)
        self.serial = 0
        self.buffer = bytearray()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.terminate()  # This client's own child only.
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        self.process.stdout.close()

    def _ready(self, stream, writing, deadline):
        remaining = deadline-time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Coordinator response deadline exceeded')
        ready = select.select([] if writing else [stream], [stream] if writing else [], [], remaining)
        if not ready[1 if writing else 0]:
            raise TimeoutError('Coordinator response deadline exceeded')

    def _line(self, deadline):
        while True:
            end = self.buffer.find(b'\n')
            if end >= 0:
                if end > MAX_FRAME:
                    raise ValueError('Coordinator response exceeds frame limit')
                line = bytes(self.buffer[:end])
                del self.buffer[:end+1]
                return json.loads(line)
            if len(self.buffer) > MAX_FRAME:
                raise ValueError('Coordinator response exceeds frame limit')
            self._ready(self.process.stdout, False, deadline)
            try:
                block = os.read(self.process.stdout.fileno(), 65536)
            except BlockingIOError:
                continue
            if not block:
                raise RuntimeError('Coordinator closed its response stream')
            self.buffer.extend(block)

    def call(self, operation, timeout=125, progress=None, **fields):
        self.serial += 1
        request_id = str(self.serial)
        data = (json.dumps(dict(schemaVersion=1, requestId=request_id,
                                operation=operation, **fields))+'\n').encode()
        if len(data) > MAX_FRAME:
            raise ValueError('Request exceeds frame limit')
        deadline = time.monotonic()+timeout
        view = memoryview(data)
        while view:
            self._ready(self.process.stdin, True, deadline)
            try:
                written = os.write(self.process.stdin.fileno(), view)
            except BlockingIOError:
                continue
            if written == 0:
                raise RuntimeError('Coordinator closed its request stream')
            view = view[written:]
        while True:
            response = self._line(deadline)
            if response.get('requestId') != request_id:
                raise ValueError('Coordinator response ID mismatch')
            if progress:
                progress(response)
            if response.get('status') != 'streaming':
                return response
