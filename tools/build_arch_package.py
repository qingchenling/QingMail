#!/usr/bin/env python3
"""Build an Arch release from a hashed workspace snapshot; never install it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tomllib
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
VERSION = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    snapshot = output / 'snapshot' / f'qingmail-{VERSION}'
    snapshot.mkdir(parents=True)
    for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'README.md', 'crates', 'packaging'):
        source = ROOT / name
        if source.is_dir():
            shutil.copytree(source, snapshot / name, ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
        else:
            shutil.copy2(source, snapshot / name)
    (snapshot / 'tests').mkdir()
    shutil.copytree(ROOT / 'tests/fixtures', snapshot / 'tests/fixtures')
    (snapshot / 'docs').mkdir()
    shutil.copy2(ROOT / 'docs/发行说明.md', snapshot / 'docs/发行说明.md')
    if (ROOT / '.cargo').exists():
        shutil.copytree(ROOT / '.cargo', snapshot / '.cargo')
    files = {str(p.relative_to(snapshot)): sha256(p)
             for p in sorted(snapshot.rglob('*')) if p.is_file()}
    info = dict(version=VERSION, source_commit=subprocess.check_output(
        ['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        workspace_dirty=bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
        built_at=datetime.now(timezone.utc).isoformat(),
        rustc=subprocess.check_output(['rustc', '--version'], text=True).strip(),
        source_files_sha256=files)
    build_info = json.dumps(info, indent=2) + '\n'
    (snapshot / 'BUILD-INFO.json').write_text(build_info)
    (output / 'BUILD-INFO.json').write_text(build_info)
    archive = output / f'qingmail-{VERSION}.tar.gz'
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(snapshot, arcname=snapshot.name)
    recipe = ((ROOT / 'packaging/arch/PKGBUILD.in').read_text()
              .replace('@VERSION@', VERSION).replace('@SOURCE_SHA256@', sha256(archive)))
    (output / 'PKGBUILD').write_text(recipe)
    with (output / '.SRCINFO').open('w') as handle:
        subprocess.run(['makepkg', '--printsrcinfo'], cwd=output, stdout=handle, check=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / 'target'))
    subprocess.run(['makepkg', '--noconfirm'], cwd=output, env=env, check=True)
    packages = sorted(output.glob('qingmail-*.pkg.tar.zst'))
    assert packages, 'makepkg did not produce a package'
    (output / 'SHA256SUMS').write_text(''.join(f'{sha256(p)}  {p.name}\n' for p in packages + [archive]))
    print('\n'.join(str(p) for p in packages))


if __name__ == '__main__':
    main()
