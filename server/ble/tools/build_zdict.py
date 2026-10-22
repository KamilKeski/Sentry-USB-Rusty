#!/usr/bin/env python3
"""Build the preset deflate dictionary for BLE binary replies.

The dictionary is the synthetic response fixtures in zdict-fixtures/ laid end
to end, least-fetched first: deflate finds matches by distance, so the
responses the app fetches most sit at the end, nearest the data. Fixtures are
synthetic (no VINs, coordinates, SSIDs, hostnames or real timestamps).

    build_zdict.py           print size and SHA-256 of the build
    build_zdict.py --write   write ../zdict/v1.bin and embed it in
                             ../sentryusb-ble.py

A shipped version is frozen (the iOS app bundles identical bytes), so --write
refuses to change an existing v1.bin; a new dictionary is a new version.
"""
import base64
import hashlib
from pathlib import Path
import re
import sys
import zlib

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / 'zdict-fixtures'
OUTPUT = HERE.parent / 'zdict' / 'v1.bin'
DAEMON = HERE.parent / 'sentryusb-ble.py'
MAX_SIZE = 20 * 1024

# Rarely fetched or cached by the app: first in line to be trimmed.
LEAST_FETCHED = [
    'api-setup-config.json',
]

# Polled by the app over Bluetooth, in increasing frequency; these go last.
# Every other fixture sits between the two lists, in name order.
MOST_FETCHED = [
    'api-status-storage.json',            # every 5 min
    'api-system-keep-accessory-gps.json',  # every ~36 s
    'api-away-mode-status-auto.json',
    'api-away-mode-status.json',
    'api-keep-awake-status-idle.json',
    'api-keep-awake-status.json',
    'api-charging-current-idle.json',
    'api-charging-current.json',
    'api-system-ble-latest-sample-empty.json',
    'api-system-ble-latest-sample.json',
    'api-dashboard-snapshot.json',         # every 12 s
    'api-drives-status-archiving.json',
    'api-drives-status.json',
    'api-status.json',
]


def build():
    names = sorted(p.name for p in FIXTURES.glob('*.json'))
    ordered = [n for n in LEAST_FETCHED if n in names]
    ordered += [n for n in names if n not in LEAST_FETCHED + MOST_FETCHED]
    ordered += [n for n in MOST_FETCHED if n in names]
    data = b''.join((FIXTURES / n).read_bytes().strip() for n in ordered)
    # Over budget, the least-fetched start is what goes.
    return data[-MAX_SIZE:]


def embed(data):
    blob = base64.b85encode(zlib.compress(data, 9)).decode('ascii')
    lines = [blob[i:i + 72] for i in range(0, len(blob), 72)]
    block = (
        '# BEGIN ZDICT V1\n'
        f"ZDICT_V1_SHA256 = '{hashlib.sha256(data).hexdigest()}'\n"
        'ZDICT_V1_B85 = (\n'
        + ''.join(f"    '{line}'\n" for line in lines)
        + ')\n'
        '# END ZDICT V1\n'
    )
    source = DAEMON.read_text()
    updated, count = re.subn(r'# BEGIN ZDICT V1\n.*?# END ZDICT V1\n',
                             lambda _: block, source, flags=re.S)
    if count != 1:
        sys.exit('ZDICT V1 markers not found in the daemon')
    DAEMON.write_text(updated)


def main():
    data = build()
    print(f'{len(data)} bytes  sha256 {hashlib.sha256(data).hexdigest()}')
    if '--write' not in sys.argv[1:]:
        return
    if OUTPUT.exists() and OUTPUT.read_bytes() != data:
        sys.exit(f'{OUTPUT} is frozen; build a new version instead')
    OUTPUT.parent.mkdir(exist_ok=True)
    OUTPUT.write_bytes(data)
    embed(data)
    print(f'wrote {OUTPUT} and embedded it in {DAEMON.name}')


if __name__ == '__main__':
    main()
