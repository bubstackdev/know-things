#!/usr/bin/env python3
"""Construction guards use inert fixture bytes, never semantic-model substitutes."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT=Path(__file__).resolve().parent.parent
sys.path.insert(0,str(ROOT/'scripts'))
import package_installation as package
from bundle_layout import EMBEDDING_RUNTIME_LAUNCHER

BINARY=ROOT/'target/release/know-things'


class EmbeddingBundle(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory(prefix='know-things-embedding-builder-')
        self.root=Path(self.temporary.name).resolve()
        self.model=self.root/'model';self.model.mkdir()
        (self.model/'config.json').write_text(json.dumps(dict(model_type='bert')))
        (self.model/'model.safetensors').write_bytes(b'inert-construction-fixture-not-weights')
        (self.model/'tokenizer.json').write_text('{}')
        (self.model/'untrusted.py').write_text('raise RuntimeError("must not execute or copy")')
        self.runtime=self.root/'runtime';self.runtime.mkdir()
        worker=self.runtime/'python-runtime'
        worker.write_text("#!/bin/sh\nprintf '%s\\n' 'fixture-runtime'\n");worker.chmod(0o700)
        self.runtime_bundle=dict(schemaVersion=1,kind='embedding_runtime',entrypoint='python-runtime',arguments=[],
            artifacts={'python-runtime':package.digest(worker)},manifest=dict(runtime='fixture-runtime'))
        (self.runtime/'bundle.json').write_text(json.dumps(self.runtime_bundle))

    def tearDown(self):self.temporary.cleanup()

    def build(self,destination,*options,mode='bundled'):
        selected=['--runtime-bundle',str(self.runtime)] if mode=='bundled' else ['--python',str(self.runtime/'python-runtime')]
        return subprocess.run([sys.executable,str(ROOT/'scripts/bundle_embedding.py'),
            '--binary',str(BINARY),'--model',str(self.model),'--destination',str(destination),
            '--name','inert-builder-fixture','--license','fixture','--dimensions','16','--max-input-tokens','32',
            *selected,*options],capture_output=True,text=True,timeout=20)

    def test_bundled_runtime_launcher_identity_and_artifacts_are_pinned(self):
        destination=self.root/"bundled fixture '$x"
        source_hash=package.digest(self.runtime/'bundle.json')
        result=self.build(destination)
        self.assertEqual(result.returncode,0,result.stderr)
        bundle=json.loads((destination/'bundle.json').read_text())
        self.assertEqual(bundle['entrypoint'],'embedding')
        self.assertEqual((destination/'embedding').read_text(),EMBEDDING_RUNTIME_LAUNCHER)
        self.assertEqual(bundle['manifest']['runtime'],'fixture-runtime')
        self.assertEqual(bundle['manifest']['artifacts'],bundle['artifacts'])
        self.assertEqual(package.digest(destination/'runtime/bundle.json'),source_hash)
        self.assertFalse((destination/'model/untrusted.py').exists())
        info=package.included_embedding_runtime(BINARY,destination,bundle)
        self.assertEqual(info['bundleManifestSha256'],source_hash)
        self.assertEqual(package.digest(self.runtime/'bundle.json'),source_hash)
        self.assertEqual((self.model/'model.safetensors').read_bytes(),b'inert-construction-fixture-not-weights')

    def test_external_interpreter_mode_remains_explicit(self):
        destination=self.root/'external'
        result=self.build(destination,mode='external')
        self.assertEqual(result.returncode,0,result.stderr)
        bundle=json.loads((destination/'bundle.json').read_text())
        self.assertEqual(bundle['entrypoint'],'embedding.py')
        self.assertTrue((destination/'embedding.py').read_text().startswith('#!'+str(self.runtime/'python-runtime')+'\n'))
        self.assertIsNone(package.included_embedding_runtime(BINARY,destination,bundle))

    def test_runtime_directory_alone_cannot_hide_an_external_interpreter(self):
        destination=self.root/'misleading-runtime'
        result=self.build(destination)
        self.assertEqual(result.returncode,0,result.stderr)
        bundle=json.loads((destination/'bundle.json').read_text())
        (destination/'embedding').write_text('#!/bin/sh\nexec /external/mutable/python "$@"\n')
        bundle['artifacts']['embedding']=package.digest(destination/'embedding')
        bundle['manifest']['artifacts']=bundle['artifacts']
        (destination/'bundle.json').write_text(json.dumps(bundle))
        with self.assertRaises(ValueError):
            package.included_embedding_runtime(BINARY,destination,bundle)

    def test_failures_never_publish_partial_bundles_or_replace_existing_paths(self):
        destination=self.root/'existing';destination.mkdir()
        (destination/'keep').write_text('existing bytes')
        self.assertNotEqual(self.build(destination).returncode,0)
        self.assertEqual((destination/'keep').read_text(),'existing bytes')
        self.runtime_bundle['manifest']['runtime']='wrong-runtime'
        (self.runtime/'bundle.json').write_text(json.dumps(self.runtime_bundle))
        destination=self.root/'wrong-runtime'
        result=self.build(destination)
        self.assertNotEqual(result.returncode,0)
        self.assertIn('identity mismatch',result.stderr)
        self.assertFalse(destination.exists())

    def test_source_overlap_symlinks_and_missing_weights_fail_without_publication(self):
        destination=self.model/'output'
        self.assertNotEqual(self.build(destination).returncode,0)
        self.assertFalse(destination.exists())
        (self.model/'link.txt').symlink_to(self.runtime/'bundle.json')
        destination=self.root/'linked'
        self.assertNotEqual(self.build(destination).returncode,0)
        self.assertFalse(destination.exists())
        (self.model/'link.txt').unlink();(self.model/'model.safetensors').unlink()
        destination=self.root/'missing'
        self.assertNotEqual(self.build(destination).returncode,0)
        self.assertFalse(destination.exists())


if __name__=='__main__':unittest.main()
