#!/usr/bin/env python3
"""Exercise real pacman transactions in a disposable root, never the host database."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--upgrade', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise SystemExit('Run with fakeroot, not sudo: fakeroot -- python3 tools/arch_package_probe.py ...')
    packages = [p.resolve() for p in (args.package, args.upgrade)]
    assert all(p.is_file() for p in packages)
    versions = [subprocess.check_output(['pacman', '-Qp', str(p)], text=True).strip() for p in packages]
    assert versions[0] != versions[1], 'upgrade requires different package revisions'
    assert all(v.startswith('qingmail ') for v in versions)
    assert subprocess.check_output(['vercmp', versions[1].split()[1], versions[0].split()[1]], text=True).strip() == '1'
    evidence = dict(passed=False, packages=[dict(path=str(p), sha256=hashlib.sha256(p.read_bytes()).hexdigest(), version=v)
                                          for p, v in zip(packages, versions)],
                    host_install=False, dependency_resolution_tested=False,
                    note='Isolated pacman root; dependencies and hooks skipped; upgrade uses package revision only.')
    with tempfile.TemporaryDirectory(prefix='qingmail-arch-transaction-') as tmp:
        root = Path(tmp)
        for name in ('var/lib/pacman', 'var/cache/pacman/pkg', 'empty-hooks', 'home/test'):
            (root / name).mkdir(parents=True)
        sentinels = ['home/test/.local/share/qingmail/mail.db', 'home/test/.config/qingmail/settings.txt',
                     'home/test/.cache/qingmail/resource', 'home/test/.config/autostart/org.qingmail.QingMail.desktop',
                     'home/test/.local/share/keyrings/test.keyring']
        for name in sentinels:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('user-owned fixture: must survive\n')
        (root / 'pacman.conf').write_text('[options]\nArchitecture = auto\nSigLevel = Never\n')
        base = ['pacman', '--config', str(root / 'pacman.conf'), '--root', str(root), '--dbpath', str(root / 'var/lib/pacman'),
                '--cachedir', str(root / 'var/cache/pacman/pkg'), '--logfile', str(root / 'pacman.log'),
                '--hookdir', str(root / 'empty-hooks'), '--noconfirm']
        transactions = []
        for index, package in enumerate(packages):
            result = subprocess.run(base + ['-U', '--nodeps', '--nodeps', '--noscriptlet', str(package)],
                                    check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            transactions.append(result.stdout)
            installed = subprocess.check_output(base + ['-Q', 'qingmail'], text=True).strip()
            assert installed == versions[index], installed
            for executable in ('mail-ui', 'mail-daemon'):
                path = root / 'usr/bin' / executable
                assert path.is_file() and path.stat().st_mode & 0o777 == 0o755
                links = subprocess.check_output(['ldd', str(path)], text=True)
                assert 'not found' not in links, links
            desktop = root / 'usr/share/applications/org.qingmail.QingMail.desktop'
            subprocess.run(['desktop-file-validate', str(desktop)], check=True)
            assert 'Exec=/usr/bin/mail-ui\n' in desktop.read_text()
            assert (root / 'usr/share/icons/hicolor/scalable/apps/org.qingmail.QingMail.svg').is_file()
            assert not (root / 'etc/xdg/autostart').exists()
            assert not (root / 'usr/lib/systemd').exists()
            evidence['installed_version_output'] = subprocess.check_output(
                [str(root / 'usr/bin/mail-ui'), '--version'], text=True).strip()
        result = subprocess.run(base + ['-R', '--nodeps', '--nodeps', '--noscriptlet', 'qingmail'], check=True,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        transactions.append(result.stdout)
        assert not (root / 'usr/bin/mail-ui').exists()
        assert not (root / 'usr/bin/mail-daemon').exists()
        assert not (root / 'usr/share/applications/org.qingmail.QingMail.desktop').exists()
        assert not (root / 'usr/share/icons/hicolor/scalable/apps/org.qingmail.QingMail.svg').exists()
        assert all((root / name).read_text() == 'user-owned fixture: must survive\n' for name in sentinels)
        evidence.update(passed=True, transactions=transactions, user_fixtures_retained=sentinels,
                        linked_libraries_resolved_on_build_host=True)
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(evidence, indent=2) + '\n')
    print('PASS: isolated install, package-revision upgrade, uninstall, user fixtures retained')


if __name__ == '__main__':
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(error.stdout or '', flush=True)
        raise
