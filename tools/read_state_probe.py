#!/usr/bin/env python3
"""复用隔离窗口执行器，验证自动已读而不触碰真实账号。"""
import argparse
from pathlib import Path
from sent_confirmation_probe import run

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/mail-ui'))
    parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
    parser.add_argument('--evidence', type=Path, default=Path('dist/verification/auto-read.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.fixture.resolve(), args.evidence,
        fixture_args=('--bodies',), probe_env='QINGMAIL_AUTO_READ_PROBE', marker='AUTO_READ PASS',
        checks=['only displayed mail becomes read; checked mail unchanged',
                'explicit mark unread survives cache refresh; reopening marks read',
                'failed and cancelled reads remain unread', 'search and list projections agree',
                'single native window; persistent offline flags; no sends'])
