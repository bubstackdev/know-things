#!/usr/bin/env python3
"""Pure dependency/path guards; full CPU capture is an explicitly recorded experiment."""
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0,str(Path(__file__).resolve().parent.parent/'scripts'))
import capture_embedding_runtime as capture


class RuntimeCapture(unittest.TestCase):
    def test_only_known_system_prefixes_are_external_allowlisted(self):
        self.assertTrue(capture.system_dependency('/usr/lib/libSystem.B.dylib'))
        self.assertTrue(capture.system_dependency('/System/Library/Frameworks/Metal.framework/Metal'))
        for value in ['/opt/homebrew/lib/libpython.dylib','/Users/person/lib.dylib','/usr/library/other','@rpath/lib.dylib']:
            self.assertFalse(capture.system_dependency(value))

    def test_native_dependencies_require_a_unique_existing_resolution(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory).resolve()
            source=root/'module.so';source.write_bytes(b'fixture')
            for folder in ['one','two']:
                (root/folder).mkdir();(root/folder/'library.dylib').write_bytes(b'fixture')
            executable=root/'bin/python'
            self.assertEqual(capture.resolve_dependency('@loader_path/one/library.dylib',source,[],executable),root/'one/library.dylib')
            self.assertEqual(capture.resolve_dependency('@rpath/library.dylib',source,['@loader_path/one'],executable),root/'one/library.dylib')
            with self.assertRaises(ValueError):
                capture.resolve_dependency('@rpath/library.dylib',source,['@loader_path/one','@loader_path/two'],executable)
            with self.assertRaises(ValueError):
                capture.resolve_dependency('@rpath/missing.dylib',source,['@loader_path/one'],executable)

    def test_macho_detection_does_not_treat_python_or_static_archives_as_native_load_images(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'payload'
            for data in [b'#!/usr/bin/env python3\n',b'!<arch>\nstatic archive',b'']:
                path.write_bytes(data);self.assertIsNone(capture.macho_type(path))
            path.write_bytes(b'\xcf\xfa\xed\xfe'+bytes(8)+struct.pack('<I',2))
            self.assertEqual(capture.macho_type(path),2)


if __name__=='__main__':unittest.main()
