#!/usr/bin/env python3
"""Offline package installation/relocation checks using an explicit fixture embedder."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT/'scripts'))
from local_client import LocalClient
import package_installation as packaging


class Packaging(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix='know-things-packaging-')
        cls.root = Path(cls.temporary.name).resolve()
        fixture = cls.root/'fixture'
        cls.fixture = fixture
        fixture.mkdir()
        worker = fixture/'worker.py'
        worker.write_text('''#!/usr/bin/env python3
import hashlib,json,os,struct,sys
m=json.load(open(os.path.join(os.environ['KNOW_THINGS_ASSET_ROOT'],'bundle.json')))['manifest']
def send(value):
 data=json.dumps(value).encode();sys.stdout.buffer.write(struct.pack('>I',len(data))+data);sys.stdout.buffer.flush()
send(dict(protocolVersion=1,kind='embedding',manifestId=hashlib.sha256(json.dumps(m,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest(),runtime=m['runtime']))
while True:
 header=sys.stdin.buffer.read(4)
 if not header:break
 request=json.loads(sys.stdin.buffer.read(struct.unpack('>I',header)[0]))
 send(dict(requestId=request['requestId'],status='ok',vector=[1,0]))
''')
        worker.chmod(0o700)
        artifacts = {'worker.py': packaging.digest(worker)}
        manifest = dict(name='fixture-not-a-real-model',artifacts=artifacts,tokenizer='fixture',
            architecture='fixture',quantization='fixture',runtime='fixture',queryPrefix='',documentPrefix='',
            pooling='fixture',normalization='l2',dimensions=2,maxInputTokens=4096,license='fixture')
        (fixture/'bundle.json').write_text(json.dumps(dict(schemaVersion=1,kind='embedding',entrypoint='worker.py',
            arguments=[],artifacts=artifacts,manifest=manifest)))
        cls.built = cls.root/'built package'
        result = subprocess.run([sys.executable,str(ROOT/'scripts/package_installation.py'),
            '--destination',str(cls.built),'--embedding-bundle',str(fixture)],
            capture_output=True,text=True,timeout=120)
        if result.returncode:
            cls.temporary.cleanup()
            raise RuntimeError(result.stdout+result.stderr)
        cls.receipt = json.loads(result.stdout)

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='know-things-relocation-')
        self.root = Path(self.temporary.name).resolve()
        self.installed = self.root/"installed package '$x"
        result = subprocess.run([str(self.built/'bin/know-things'),'assets','import',str(self.built),str(self.installed)],
            capture_output=True,text=True,timeout=15)
        self.assertEqual(result.returncode,0,result.stderr)

    def tearDown(self):
        self.temporary.cleanup()

    def test_relocated_launcher_resolves_assets_for_cli_and_mcp_without_checkout_cwd(self):
        binary = self.installed/'bin/know-things'
        source = self.root/'source'
        source.mkdir()
        raw = 'Automobile policy.'
        (source/'guide.md').write_text(raw)
        with LocalClient(binary,self.root/'data',cwd=self.root) as client:
            for request in [dict(operation='register',name='kb',root=str(source)),dict(operation='reconcile',collection='kb')]:
                self.assertEqual(client.call(**request)['status'],'ok')
            result = client.call(operation='search',query='car',collections=['kb'],mode='hybrid',strictHybrid=True)
            self.assertEqual(result['status'],'ok')
            self.assertEqual(result['data']['actualMode'],'hybrid')
            reference = result['data']['evidence'][0]['reference']
            self.assertEqual(client.call(operation='context',reference=reference)['data']['text'],raw)
        messages = [dict(jsonrpc='2.0',id=1,method='initialize',params=dict(protocolVersion='2025-06-18',
            capabilities={},clientInfo=dict(name='packaging-test',version='1'))),
            dict(jsonrpc='2.0',method='notifications/initialized'),
            dict(jsonrpc='2.0',id=2,method='tools/call',params=dict(name='know_things',arguments=dict(operation='status')))]
        result = subprocess.run([str(binary),'mcp-stdio',str(self.root/'data')],cwd=self.root,
            env={'PATH':'/usr/bin:/bin'},input=''.join(json.dumps(v)+'\n' for v in messages),
            capture_output=True,text=True,timeout=15)
        self.assertEqual(result.returncode,0,result.stderr)
        responses = [json.loads(line) for line in result.stdout.splitlines()]
        response = next(v for v in responses if v['id']==2)
        status = json.loads(response['result']['content'][0]['text'])
        self.assertEqual(status['status'],'ok')
        self.assertIsNotNone(status['data']['configuredEmbeddingSpaceId'])
        self.assertEqual((source/'guide.md').read_text(),raw)

    def test_package_receipt_is_explicit_and_does_not_claim_model_or_license_qualification(self):
        bundle = json.loads((self.installed/'bundle.json').read_text())
        manifest = bundle['manifest']
        self.assertEqual(bundle['kind'],'installation')
        self.assertFalse(manifest['qualification']['releaseQualified'])
        self.assertFalse(manifest['qualification']['modelProfilesQualified'])
        self.assertFalse(manifest['qualification']['dependencyClosureValidated'])
        self.assertTrue(manifest['externalRuntimeRequirements'])
        self.assertEqual(manifest['assets'],dict(embedding='assets/embedding'))
        self.assertEqual(manifest['sourceArtifacts']['Cargo.lock'],packaging.digest(ROOT/'Cargo.lock'))
        notices = json.loads((self.installed/'dependencies.json').read_text())
        self.assertTrue(notices['dependencies'])
        self.assertTrue(all('noticeFiles' in item for item in notices['dependencies']))

    def test_transfer_tampering_unlisted_files_and_existing_destinations_are_rejected(self):
        core = self.installed/'bin/know-things-core'
        result = subprocess.run([str(core),'assets','import',str(self.built),str(self.installed)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)
        manifest_hash = packaging.digest(self.installed/'bundle.json')
        self.assertEqual(manifest_hash,packaging.digest(self.built/'bundle.json'))
        extra = self.installed/'unlisted'
        extra.write_text('unexpected file')
        result = subprocess.run([str(core),'assets','verify',str(self.installed)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)
        extra.unlink()
        (self.installed/'assets/embedding/worker.py').write_text('tampered fixture')
        result = subprocess.run([str(core),'assets','verify',str(self.installed)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)

    def test_copy_rejects_symlinks_and_profile_requires_pinned_worker(self):
        original = self.root/'original'
        original.write_text('private input')
        link = self.root/'link'
        link.symlink_to(original)
        with self.assertRaises(ValueError):
            packaging.copy_checked(link,self.root/'copy')
        destination = self.root/'invalid'
        result = subprocess.run([sys.executable,str(ROOT/'scripts/package_installation.py'),
            '--destination',str(destination),'--generation-profile',str(self.root)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)
        self.assertFalse(destination.exists())

        destination = self.root/'nested-installation'
        result = subprocess.run([sys.executable,str(ROOT/'scripts/package_installation.py'),
            '--destination',str(destination),'--embedding-bundle',str(self.root)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)
        self.assertFalse(destination.exists())

    def test_same_inputs_reproduce_manifest_and_payload_at_another_destination(self):
        destination = self.root/'rebuilt'
        result = subprocess.run([sys.executable,str(ROOT/'scripts/package_installation.py'),
            '--destination',str(destination),'--embedding-bundle',str(self.fixture)],
            capture_output=True,text=True,timeout=120)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual((destination/'bundle.json').read_bytes(),(self.built/'bundle.json').read_bytes())
        first = json.loads((self.built/'bundle.json').read_text())
        for name,expected in first['artifacts'].items():
            self.assertEqual(packaging.digest(destination/name),expected)

    def test_profile_binding_mismatch_cannot_publish_or_start_a_worker(self):
        def bundle(folder,kind,manifest):
            folder.mkdir()
            payload = folder/'payload'
            payload.write_text('fixture-not-a-model-or-worker')
            (folder/'bundle.json').write_text(json.dumps(dict(schemaVersion=1,kind=kind,
                entrypoint='payload',arguments=[],artifacts={'payload':packaging.digest(payload)},manifest=manifest)))
        flash,profile = self.root/'flash',self.root/'profile'
        bundle(flash,'flash',dict(fixture=True))
        bundle(profile,'generation_profile',dict(flashArtifactId='0'*64))
        destination = self.root/'mismatched'
        result = subprocess.run([sys.executable,str(ROOT/'scripts/package_installation.py'),
            '--destination',str(destination),'--flash-bundle',str(flash),'--generation-profile',str(profile)],
            capture_output=True,text=True,timeout=120)
        self.assertNotEqual(result.returncode,0)
        self.assertIn('does not bind',result.stderr)
        self.assertFalse(destination.exists())


if __name__ == '__main__':
    unittest.main()
