#!/usr/bin/env python3
"""Capture and build a content-pinned Flash worker without modifying its source checkout.

Produces a private research artifact. Distribution requires resolving the source's
license and testing relocation, model/tokenizer profiles, and resource/quality gates.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(65536), b''):
            value.update(block)
    return value.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--destination', type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve()
    destination = args.destination.absolute()
    if destination.exists():
        parser.error('Destination must be new')
    if destination == source or source in destination.parents:
        parser.error('Capture must be outside the source checkout')
    destination.mkdir(parents=True, mode=0o700)
    snapshot = destination/'source'
    artifact = destination/'artifact'
    for folder in [snapshot, artifact/'lib', artifact/'kernels', artifact/'include']:
        folder.mkdir(parents=True, exist_ok=True)
    files = sorted(p for folder in ['src', 'kernels'] for p in (source/folder).iterdir()
                   if p.is_file() and p.suffix in {'.h', '.cpp', '.mm', '.metal'})
    llama = source/'third_party/llama.cpp-upstream'
    files += sorted((llama/'include').glob('*.h'))+sorted((llama/'ggml/include').glob('*.h'))
    names = {}
    originals = {}
    for original in files:
        relative = original.relative_to(source)
        target = snapshot/relative
        target.parent.mkdir(parents=True, exist_ok=True)
        expected = digest(original)
        shutil.copyfile(original, target)
        if digest(target) != expected:
            raise RuntimeError('Source changed during capture; retry with a new destination')
        names[str(relative)] = expected
        originals[original] = expected
    # Headers/kernels and tokenizer shared libraries are installed with the worker.
    for header in list((snapshot/'src').glob('*.h'))+list((snapshot/'third_party/llama.cpp-upstream/include').glob('*.h'))+list((snapshot/'third_party/llama.cpp-upstream/ggml/include').glob('*.h')):
        namespace = 'flash' if header.parent.name == 'src' else 'llama'
        (artifact/'include'/namespace).mkdir(exist_ok=True)
        shutil.copyfile(header, artifact/'include'/namespace/header.name)
    for kernel in (snapshot/'kernels').iterdir():
        shutil.copyfile(kernel, artifact/'kernels'/kernel.name)
    libraries = ['libllama', 'libggml', 'libggml-base', 'libggml-cpu', 'libggml-blas', 'libggml-metal']
    for name in libraries:
        for original in (llama/'build/bin').glob(name+'*.dylib'):
            # Include only the named library and its versioned aliases, not CLI/server helpers.
            if original.name.split('.')[0] != name:
                continue
            expected = digest(original)
            shutil.copyfile(original, artifact/'lib'/original.name)
            if digest(artifact/'lib'/original.name) != expected:
                raise RuntimeError('Tokenizer dependency changed during capture')
            names['lib/'+original.name] = expected
            originals[original] = expected
    worker_source = Path(__file__).resolve().parent.parent/'workers/flash.mm'
    shutil.copyfile(worker_source, snapshot/'worker.mm')
    names['worker.mm'] = digest(worker_source)
    helper = worker_source.with_name('reasoning_filter.h')
    shutil.copyfile(helper, snapshot/helper.name)
    names[helper.name] = digest(helper)
    worker_hash = hashlib.sha256(json.dumps({name: names[name] for name in ['worker.mm', helper.name]},
                                          sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    for original, expected in originals.items():
        if digest(original) != expected:
            raise RuntimeError('Mutable checkout changed during capture; retry')
    identity = hashlib.sha256(json.dumps(names, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    (snapshot/'capture.json').write_text(json.dumps(dict(artifactId=identity, files=names), indent=2)+'\n')
    cmake = '''cmake_minimum_required(VERSION 3.24)
project(know_things_flash_worker LANGUAGES C CXX OBJCXX)
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_OBJCXX_STANDARD 20)
set(CMAKE_BUILD_TYPE Release)
set(CMAKE_OBJCXX_FLAGS "${CMAKE_OBJCXX_FLAGS} -fobjc-arc")
add_compile_definitions(FLASH_KERNEL_DIR=".")
add_compile_definitions(KNOW_THINGS_FLASH_ARTIFACT_ID="'''+identity+'''")
add_compile_definitions(KNOW_THINGS_WORKER_BUILD="'''+worker_hash+'''")
add_library(flash STATIC src/metal.mm src/quant.cpp src/gguf.cpp src/ops.mm src/ple.cpp src/model.mm src/dense.mm src/dflash.mm src/safetensors.cpp)
target_include_directories(flash PUBLIC src)
target_link_libraries(flash PUBLIC "-framework Metal" "-framework Foundation")
add_executable(know-things-flash worker.mm)
target_include_directories(know-things-flash PRIVATE "${CMAKE_SOURCE_DIR}/../artifact/include/llama")
target_link_libraries(know-things-flash PRIVATE flash "${CMAKE_SOURCE_DIR}/../artifact/lib/libllama.dylib")
set_target_properties(know-things-flash PROPERTIES BUILD_WITH_INSTALL_RPATH TRUE INSTALL_RPATH "@loader_path/lib")
'''
    (snapshot/'CMakeLists.txt').write_text(cmake)
    subprocess.run(['cmake', '-S', str(snapshot), '-B', str(destination/'build')], check=True)
    subprocess.run(['cmake', '--build', str(destination/'build'), '--target', 'know-things-flash', '-j', '2'], check=True)
    shutil.copyfile(destination/'build/know-things-flash', artifact/'know-things-flash')
    (artifact/'know-things-flash').chmod(0o700)
    # Tokenizer dependencies may carry build-checkout RPATHs. Rewrite every captured library
    # dependency to the artifact's own lib directory, never the sibling checkout.
    for path in list((artifact/'lib').glob('*.dylib'))+[artifact/'know-things-flash']:
        if path.suffix == '.dylib':
            subprocess.run(['install_name_tool', '-id', '@rpath/'+path.name, str(path)], check=True, capture_output=True)
        dependencies = subprocess.check_output(['otool', '-L', str(path)], text=True).splitlines()[1:]
        for dependency in dependencies:
            old = dependency.strip().split(' (', 1)[0]
            local = artifact/'lib'/Path(old).name
            if local.is_file() and old != '@rpath/'+local.name:
                subprocess.run(['install_name_tool', '-change', old, '@rpath/'+local.name, str(path)], check=True, capture_output=True)
        load_commands = subprocess.check_output(['otool', '-l', str(path)], text=True).splitlines()
        for index, line in enumerate(load_commands):
            if line.strip() == 'cmd LC_RPATH':
                old_path = load_commands[index+2].strip().split(' (offset', 1)[0].removeprefix('path ')
                if old_path.startswith('/'):
                    subprocess.run(['install_name_tool', '-delete_rpath', old_path, str(path)], check=True, capture_output=True)
        # Every dylib can resolve its dependencies after relocation.
        if path.suffix == '.dylib':
            subprocess.run(['install_name_tool', '-add_rpath', '@loader_path', str(path)], check=True, capture_output=True)
        subprocess.run(['codesign', '--force', '--sign', '-', str(path)], check=True, capture_output=True)
    artifacts = {str(p.relative_to(artifact)): digest(p) for p in sorted(artifact.rglob('*')) if p.is_file()}
    bundle = dict(schemaVersion=1, kind='flash', entrypoint='know-things-flash', arguments=[], artifacts=artifacts,
                  manifest=dict(flashArtifactId=identity, workerBuild=worker_hash, sourceLicense='unresolved-private-research-artifact',
                                modelProfilesQualified=False))
    (artifact/'bundle.json').write_text(json.dumps(bundle, indent=2)+'\n')
    print(artifact)


if __name__ == '__main__':
    main()
