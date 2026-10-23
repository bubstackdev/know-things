#!/usr/bin/env python3
"""Build a private offline installation candidate from pinned local inputs.

Builds the core with Cargo.lock/offline, captures source identity and dependency
notices, and publishes through the core's verified no-overwrite bundle importer.
No model is downloaded, loaded, or declared qualified. Distribution licensing and
external embedding runtime provisioning remain separate qualification gates.
"""
import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path
from bundle_layout import EMBEDDING_RUNTIME_LAUNCHER

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(65536), b''):
            value.update(block)
    return value.hexdigest()


def command(arguments, timeout=120):
    result = subprocess.run(list(map(str, arguments)), cwd=ROOT, capture_output=True,
                            text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{arguments[0]} failed: {result.stderr.strip()}')
    return result.stdout


def source_identity():
    inputs = [ROOT/'Cargo.toml', ROOT/'Cargo.lock', Path(__file__).resolve(), ROOT/'scripts/bundle_layout.py']
    inputs += sorted((ROOT/'src').rglob('*.rs'))+sorted((ROOT/'migrations').glob('*.sql'))
    return {str(path.relative_to(ROOT)): digest(path) for path in sorted(inputs)}


def copy_checked(source, target):
    if source.is_symlink() or not source.is_file():
        raise ValueError('Package inputs must be regular files without symlinks')
    expected = digest(source)
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(0o700 if source.stat().st_mode & 0o111 else 0o600)
    if digest(target) != expected or digest(source) != expected:
        raise ValueError('Input changed during capture')


def capture_bundle(binary, source, target, kind):
    command([binary, 'assets', 'verify', source], timeout=600)
    expected = json.loads((source/'bundle.json').read_text())
    if expected['kind'] != kind:
        raise ValueError(f'Expected {kind} bundle')
    command([binary, 'assets', 'import', source, target], timeout=600)
    if json.loads((target/'bundle.json').read_text()) != expected:
        raise ValueError('Selected bundle changed during capture')
    return expected


def included_embedding_runtime(binary, root, bundle):
    if 'runtime/bundle.json' not in bundle['artifacts']:
        return None
    if bundle['entrypoint'] != 'embedding' or bundle['arguments'] or (root/'embedding').read_text() != EMBEDDING_RUNTIME_LAUNCHER:
        raise ValueError('A bundled runtime requires the relative embedding launcher')
    command([binary,'assets','verify',root/'runtime'])
    captured=json.loads((root/'runtime/bundle.json').read_text())
    if captured['kind'] != 'embedding_runtime' or captured['entrypoint'] != 'python-runtime' or captured['arguments']:
        raise ValueError('Unsupported bundled embedding runtime')
    if captured['manifest']['runtime'] != bundle['manifest']['runtime']:
        raise ValueError('Embedding and captured runtime identities differ')
    return dict(path='assets/embedding/runtime',bundleManifestSha256=digest(root/'runtime/bundle.json'),runtime=captured['manifest']['runtime'])


def launcher(assets):
    options = []
    for key, flag in [('embedding', '--embedding-bundle'), ('flash', '--flash-bundle'),
                      ('generationProfile', '--generation-profile')]:
        if key in assets and (key != 'flash' or 'generationProfile' in assets):
            options += [flag, '"$package_dir/'+assets[key]+'"']
    source = '''#!/bin/sh
set -eu
script_dir=${0%/*}
if [ "$script_dir" = "$0" ]; then script_dir=.; fi
case "$script_dir" in /*) ;; *) script_dir="./$script_dir" ;; esac
package_dir=$(CDPATH= cd -P "$script_dir/.." && pwd)
case "${1-}" in
  serve|local|mcp-stdio)
    exec "$package_dir/bin/know-things-core" "$@"'''
    source += (' '+ ' '.join(options) if options else '')+'\n    ;;\n'
    if 'flash' in assets:
        source += '''  flash-probe)
    if [ "$#" -eq 1 ]; then
      exec "$package_dir/bin/know-things-core" flash-probe "$package_dir/assets/flash"
    fi
    ;;
'''
    return source+'''esac
exec "$package_dir/bin/know-things-core" "$@"
'''


def notices(metadata, stage):
    records = []
    for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
        if package['id'] in metadata['workspace_members']:
            continue
        folder = Path(package['manifest_path']).parent
        files = []
        for path in sorted(folder.iterdir()):
            if path.name.upper().startswith(('LICENSE', 'NOTICE', 'COPYRIGHT')) and path.is_file():
                relative = Path('notices')/(package['name']+'-'+package['version'])/path.name
                copy_checked(path, stage/relative)
                files.append(str(relative))
        records.append(dict(name=package['name'], version=package['version'],
                            licenseExpression=package.get('license'), source=package.get('source'),
                            noticeFiles=files))
    (stage/'dependencies.json').write_text(json.dumps(dict(
        auditStatus='metadata-and-top-level-notices-only-unreviewed', dependencies=records), indent=2)+'\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--destination', type=Path, required=True)
    parser.add_argument('--embedding-bundle', type=Path)
    parser.add_argument('--flash-bundle', type=Path)
    parser.add_argument('--generation-profile', type=Path)
    args = parser.parse_args()
    destination = Path(os.path.abspath(args.destination))
    if os.path.lexists(destination):
        parser.error('Destination must be new')
    if args.generation_profile and not args.flash_bundle:
        parser.error('A generation profile requires its pinned Flash bundle')
    resolved_destination = destination.resolve()
    for source in [args.embedding_bundle,args.flash_bundle,args.generation_profile]:
        if source and (resolved_destination == source.resolve() or source.resolve() in resolved_destination.parents):
            parser.error('Installation destination must be outside input asset bundles')
    os.umask(0o077)  # This packaging process and its children only.
    before = source_identity()
    metadata = json.loads(command(['cargo', 'metadata', '--manifest-path', ROOT/'Cargo.toml',
                                  '--locked', '--offline', '--format-version', '1']))
    command(['cargo', 'build', '--manifest-path', ROOT/'Cargo.toml', '--release', '--locked', '--offline', '-j', '2'])
    if source_identity() != before:
        raise ValueError('Source changed during build; retry from stable inputs')
    binary = Path(metadata['target_directory'])/'release/know-things'
    root_package = next(p for p in metadata['packages'] if p['id'] == metadata['resolve']['root'])
    with tempfile.TemporaryDirectory(prefix='know-things-package-') as directory:
        stage = Path(directory)/'package'
        stage.mkdir(mode=0o700)
        copy_checked(binary, stage/'bin/know-things-core')
        core = stage/'bin/know-things-core'
        schema = json.loads(command([core, 'schema']))
        (stage/'schema.json').write_text(json.dumps(schema, sort_keys=True, indent=2)+'\n')
        copy_checked(ROOT/'Cargo.lock', stage/'Cargo.lock')
        notices(metadata, stage)
        assets, bundles, external = {}, {}, []
        included_runtime = None
        for key, source, kind, relative in [
            ('embedding', args.embedding_bundle, 'embedding', 'assets/embedding'),
            ('flash', args.flash_bundle, 'flash', 'assets/flash'),
            ('generationProfile', args.generation_profile, 'generation_profile', 'assets/generation-profile'),
        ]:
            if source:
                (stage/'assets').mkdir(exist_ok=True)
                bundle = capture_bundle(core, source.resolve(), stage/relative, kind)
                assets[key], bundles[key] = relative, bundle
                if key == 'embedding':
                    included_runtime=included_embedding_runtime(core,stage/relative,bundle)
                    if included_runtime is None:
                        external.append(dict(kind='embedding-runtime', runtime=bundle['manifest'].get('runtime'),
                                             provisioning='not included; verify entrypoint interpreter and runtime closure'))
        if 'generationProfile' in bundles:
            flash_id = hashlib.sha256(json.dumps(bundles['flash'], sort_keys=True,
                separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()
            if bundles['generationProfile']['manifest']['flashArtifactId'] != flash_id:
                raise ValueError('Generation profile does not bind the selected Flash bundle')
        (stage/'bin/know-things').write_text(launcher(assets))
        (stage/'bin/know-things').chmod(0o700)
        (stage/'USAGE.txt').write_text('''Know-Things private installation candidate

Use bin/know-things local DATA_DIR for resident NDJSON, mcp-stdio DATA_DIR for MCP,
or serve DATA_DIR for the socket coordinator. Store data outside this package and
source roots. Packaged asset options are supplied by the launcher. Other commands
(schema, restore, assets, request) pass through to the compiled core unchanged.
With a Flash bundle, `bin/know-things flash-probe` checks the packaged worker.

Install into a new directory with: bin/know-things assets import PACKAGE DESTINATION
Verify transferred bytes with: bin/know-things assets verify PACKAGE
Keep versioned installations side by side; stop only your own coordinator before
switching its executable. Back up before opening existing data with a newer schema;
older binaries refuse incompatible stores. Restore backups into a new data directory.

No models are downloaded or qualified by packaging. See bundle.json for included
assets, external runtime requirements, source/build identity, and qualification gaps.
Dependency expressions/notices are unreviewed inputs to licensing review; this package
does not establish application, Flash, or model redistribution rights. Private use only.
''')
        receipt = dict(application='know-things', version=root_package['version'],
            host=dict(system=platform.system(), machine=platform.machine()),
            rustc=command(['rustc', '--version']).strip(), sourceArtifacts=before,
            assets=assets, externalRuntimeRequirements=external, includedEmbeddingRuntime=included_runtime,
            qualification=dict(releaseQualified=False, modelProfilesQualified=False,
                               dependencyClosureValidated=False, distributionLicensing='unresolved-private-candidate'))
        artifacts = {str(p.relative_to(stage)): digest(p) for p in sorted(stage.rglob('*')) if p.is_file()}
        (stage/'bundle.json').write_text(json.dumps(dict(schemaVersion=1,kind='installation',
            entrypoint='bin/know-things',arguments=[],artifacts=artifacts,manifest=receipt), indent=2)+'\n')
        destination.parent.mkdir(parents=True, exist_ok=True)
        command([core, 'assets', 'import', stage, destination], timeout=600)
    print(json.dumps(dict(installation=str(destination), packageManifestSha256=digest(destination/'bundle.json'),
                         binarySha256=digest(destination/'bin/know-things-core'), releaseQualified=False)))


if __name__ == '__main__':
    main()
