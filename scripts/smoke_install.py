#!/usr/bin/env python3
"""Verify an installed retrieval binary on disposable sources, including restore."""
import argparse
import hashlib
import json
import subprocess
import tempfile
import time
from pathlib import Path
from local_client import LocalClient


def session(binary, store, requests):
    # Import must finish before the dependent query/backup. NDJSON request IDs,
    # rather than submission order, define response correlation.
    with LocalClient(binary, store) as client:
        responses = [client.call(timeout=30, **request) for request in requests]
    for response in responses:
        assert response['status'] == 'ok', response
    return responses


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='know-things-install-') as directory:
        root = Path(directory)
        source, data, restored = root/'source', root/'data', root/'restored'
        source.mkdir()
        raw = '# Fixture policy\n\nRedis PersistentCache migration guidance.\n'
        (source/'guide.md').write_text(raw)
        (source/'secret').mkdir()
        (source/'secret'/'invalid.txt').write_bytes(b'\xff')
        results = session(args.binary, data, [
            dict(operation='register', name='kb', root=str(source), policy=dict(excludedPaths=['secret'], retention=dict(retainRevisions=2, intervalMs=86400000))),
            dict(operation='reconcile', collection='kb'),
            dict(operation='rebuild_lexical', deadlineMs=5000),
            dict(operation='search', query='PersistentCache', collections=['kb'], mode='lexical'),
            dict(operation='integrity'),
            dict(operation='backup', destination=str(root/'backup.sqlite')),
        ])
        assert results[1]['data']['filteredEntries'] == 1
        assert results[1]['data']['failures'] == []
        evidence = next(e for e in results[3]['data']['evidence'] if 'PersistentCache' in e['text'])
        resolved = session(args.binary, data, [dict(operation='context', reference=evidence['reference'])])
        assert resolved[0]['data']['text'] == evidence['text']
        with LocalClient(args.binary, data) as client:
            def call(**request):
                response = client.call(timeout=30, **request)
                assert response['status'] == 'ok', response
                return response['data']
            assert call(operation='collection_policy', collection='kb')['excludedPaths'] == ['secret']
            call(operation='set_collection_policy', collection='kb',
                 policy=dict(excludedPaths=['guide.md', 'secret']))
            assert not call(operation='search', query='PersistentCache', collections=['kb'], mode='lexical')['evidence']
            call(operation='set_collection_policy', collection='kb', policy=dict(excludedPaths=['secret']))
            assert not call(operation='search', query='PersistentCache', collections=['kb'], mode='lexical')['evidence']
            call(operation='reconcile', collection='kb')
            assert call(operation='context', reference=evidence['reference'])['text'] == evidence['text']
            call(operation='set_collection_policy', collection='kb',
                 policy=dict(excludedPaths=['secret'], reconcileIntervalMs=1000))
            (source/'guide.md').write_text(raw+'\nPeriodicScannerMarker\n')
            deadline = time.monotonic()+8
            while not call(operation='search', query='PeriodicScannerMarker', collections=['kb'], mode='lexical')['evidence']:
                assert time.monotonic() < deadline, 'Periodic edit was not discovered'
                time.sleep(.05)
            call(operation='set_collection_policy', collection='kb', policy=dict(excludedPaths=['secret']))
            call(operation='integrity')
            (source/'guide.md').write_text(raw)
            call(operation='reconcile', collection='kb')
            pruned = call(operation='prune', retainRevisions=1, maxRevisions=1, deadlineMs=5000)
            assert pruned['revisionsRemoved'] == 1
            assert not pruned['moreRemaining']
            assert call(operation='context', reference=evidence['reference'])['text'] == evidence['text']
            call(operation='integrity')
            (source/'guide.md').write_text(raw+'\nRetirementMarker\n')
            call(operation='reconcile', collection='kb')
            obsolete = call(operation='search', query='RetirementMarker', collections=['kb'], mode='lexical')['evidence'][0]['reference']
            (source/'guide.md').write_text(raw)
            call(operation='reconcile', collection='kb')
            call(operation='set_collection_policy', collection='kb',
                 policy=dict(excludedPaths=['secret'], retention=dict(retainRevisions=1, intervalMs=1000, maxRevisions=1)))
        # A new owned coordinator must resume persisted retention, independently
        # of source scanning. The active original reference remains resolvable.
        with LocalClient(args.binary, data) as client:
            deadline = time.monotonic()+8
            while True:
                result = client.call(operation='context', reference=obsolete)
                if result['status'] == 'error':
                    assert result['error']['code'] == 'evidence_version_unavailable', result
                    break
                assert time.monotonic() < deadline, 'Persisted retention did not remove obsolete history'
                time.sleep(.05)
            assert client.call(operation='context', reference=evidence['reference'])['data']['text'] == evidence['text']
            assert client.call(operation='integrity')['status'] == 'ok'
        subprocess.run([args.binary, 'restore', str(root/'backup.sqlite'), str(restored)], check=True,
                       capture_output=True, text=True, timeout=30)
        recovered = session(args.binary, restored, [dict(operation='context', reference=evidence['reference']),
                                                  dict(operation='collection_policy', collection='kb'),
                                                  dict(operation='remove_collection', collection='kb'),
                                                  dict(operation='integrity')])
        assert recovered[0]['data']['text'] == evidence['text']
        assert recovered[1]['data'] == dict(excludedPaths=['secret'], reconcileIntervalMs=None,
                                           retention=dict(retainRevisions=2, intervalMs=86400000, maxRevisions=128, deadlineMs=1000))
        assert (source/'guide.md').read_text() == raw
        assert (source/'secret'/'invalid.txt').read_bytes() == b'\xff'
    print(json.dumps(dict(installedRetrievalSmoke='passed', sourceUnchanged=True,
                          binarySha256=hashlib.sha256(Path(args.binary).read_bytes()).hexdigest(),
                          workflows=['filtered import', 'rebuild lexical', 'query', 'context', 'integrity', 'backup', 'policy tighten and relax', 'periodic edit discovery', 'bounded pruning', 'persisted retention after restart', 'restore with retention policy', 'remove collection'],
                          limitations=['No embedding/generation assets installed by this smoke test'])))

if __name__ == '__main__':
    main()
