#!/usr/bin/env python3
"""BLE API reply framing of the real daemon, with D-Bus and GLib stubbed out."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import struct
import sys
import types
import unittest
import zlib

REPO = Path(__file__).resolve().parents[1]
ZDICT_V1 = REPO / 'server/ble/zdict/v1.bin'


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_ble_daemon():
    """Import the real daemon with just enough python3-dbus and gi stubs."""
    class DBusObject:
        def __init__(self, *args, **kwargs):
            pass

    def decorator(*args, **kwargs):
        return lambda f: f

    dbus = types.ModuleType('dbus')
    dbus.service = types.ModuleType('dbus.service')
    dbus.service.Object = DBusObject
    dbus.service.method = decorator
    dbus.service.signal = decorator
    dbus.exceptions = types.ModuleType('dbus.exceptions')
    dbus.exceptions.DBusException = Exception
    dbus.mainloop = types.ModuleType('dbus.mainloop')
    dbus.mainloop.glib = types.ModuleType('dbus.mainloop.glib')
    dbus.Array = lambda items, signature=None: bytes(items)
    dbus.Byte = int
    sys.modules.update({'dbus': dbus, 'dbus.service': dbus.service,
                        'dbus.exceptions': dbus.exceptions, 'dbus.mainloop': dbus.mainloop,
                        'dbus.mainloop.glib': dbus.mainloop.glib})
    # Main-context scheduling runs inline so a request completes synchronously.
    glib = types.SimpleNamespace(idle_add=lambda fn, *a: fn(*a),
                                 timeout_add=lambda ms, fn, *a: fn(*a))
    gi = types.ModuleType('gi')
    gi.repository = types.ModuleType('gi.repository')
    gi.repository.GLib = glib
    sys.modules.update({'gi': gi, 'gi.repository': gi.repository})

    module = load_module('sentryusb_ble', REPO / 'server/ble/sentryusb-ble.py')
    module.log.disabled = True
    module.threading = types.SimpleNamespace(
        Thread=lambda target, daemon: types.SimpleNamespace(start=target))
    module.is_authenticated = lambda options: True
    return module


ble = load_ble_daemon()
SERVICE = types.SimpleNamespace(path='/test/service', get_path=lambda: '/test/service')

# Compact, as the Rust server sends it; json.dumps would add spaces.
STATUS_BODY = b'[' + b','.join([
    b'{"cpu_temp":"48312","drives_active":"yes","free_space":"100000000",'
    b'"num_snapshots":"12","total_space":"500000000","uptime":"3600",'
    b'"wifi_ip":"","wifi_ssid":"","wifi_strength":"0/70"}'] * 4) + b']'


def decode(frames, zdicts=None):
    """Reassemble and decode binary frames the way the app does."""
    header = struct.Struct('>BHHH')
    parts = {}
    for frame in frames:
        magic, rid, idx, count = header.unpack(frame[:7])
        assert magic == 0xB1
        parts[idx] = (rid, count, frame[7:])
    rids = {p[0] for p in parts.values()}
    counts = {p[1] for p in parts.values()}
    assert len(rids) == 1 and len(counts) == 1
    count = counts.pop()
    assert sorted(parts) == list(range(count))
    payload = b''.join(parts[i][2] for i in range(count))
    encoding, status = payload[0], struct.unpack('>H', payload[1:3])[0]
    body = payload[3:]
    if encoding == 1:
        body = zlib.decompress(body, -15)
    elif encoding == 2:
        dict_id = struct.unpack('>I', body[2:6])[0]
        zdict = (zdicts or {}).get(dict_id)
        assert zdict is not None, 'unknown dictionary id'
        d = zlib.decompressobj(15, zdict=zdict)
        body = d.decompress(body) + d.flush()
    return rids.pop(), status, encoding, body


class FakeResponse:
    def __init__(self):
        self.sent = []

    def send_notification(self, value):
        self.sent.append(bytes(value))


class BleFramesTest(unittest.TestCase):
    def setUp(self):
        self.response = FakeResponse()
        self.api = ble.APIRequestCharacteristic(None, 0, SERVICE, self.response)
        self.reply = {'status': 200, 'raw': STATUS_BODY}
        ble.proxy_api_request = lambda method, path, body=None: {
            'status': self.reply['status'],
            'body': json.loads(self.reply['raw']) if self.reply['raw'] else '',
            'raw': self.reply['raw']}

    def request(self, mtu=185, **fields):
        self.response.sent.clear()
        msg = {'id': 7, 'method': 'GET', 'path': '/status', **fields}
        self.api.WriteValue(list(json.dumps(msg).encode()), {'mtu': mtu})
        return list(self.response.sent)

    def dict_ids(self):
        return {zlib.adler32(z): z for z in ble.ZDICTS.values()}

    def test_dictionary_v1_is_embedded_intact(self):
        data = ZDICT_V1.read_bytes()
        self.assertEqual(ble.ZDICTS.get(1), data)
        self.assertEqual(hashlib.sha256(data).hexdigest(), ble.ZDICT_V1_SHA256)
        self.assertLessEqual(len(data), 32 * 1024)

    def test_missing_or_corrupt_dictionary_is_left_out(self):
        saved = ble.ZDICT_V1_B85, ble.ZDICT_V1_SHA256
        self.addCleanup(lambda: (setattr(ble, 'ZDICT_V1_B85', saved[0]),
                                 setattr(ble, 'ZDICT_V1_SHA256', saved[1])))
        ble.ZDICT_V1_SHA256 = '0' * 64
        self.assertEqual(ble.load_zdicts(), {})
        ble.ZDICT_V1_B85 = ''
        self.assertEqual(ble.load_zdicts(), {})

    def test_dictionary_v1_is_reproducible(self):
        builder = load_module('build_zdict', REPO / 'server/ble/tools/build_zdict.py')
        self.assertEqual(builder.build(), ZDICT_V1.read_bytes(),
                         'v1 is frozen: a changed dictionary must become v2')

    def test_binary_reply_with_dictionary_round_trips_verbatim(self):
        frames = self.request(bin=1, zd=1, gz=1)
        self.assertTrue(all(f[0] == 0xB1 and len(f) <= 185 - 8 for f in frames))
        rid, status, encoding, body = decode(frames, self.dict_ids())
        self.assertEqual((rid, status, encoding, body), (7, 200, 2, STATUS_BODY))

    def test_unknown_dictionary_falls_back_to_raw_deflate(self):
        for zd in (None, 99, 'x'):
            fields = {'bin': 1} if zd is None else {'bin': 1, 'zd': zd}
            _, _, encoding, body = decode(self.request(**fields))
            self.assertEqual((encoding, body), (1, STATUS_BODY))

    def test_small_or_incompressible_bodies_stay_plain(self):
        self.reply['raw'] = b'{"ts":null}'
        frames = self.request(bin=1, zd=1)
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0][:7], bytes([0xB1, 0, 7, 0, 0, 0, 1]))
        self.assertEqual(decode(frames)[1:], (200, 0, b'{"ts":null}'))

        noise = os.urandom(600)
        payload, encoding = ble.encode_bin_payload(200, noise, None)
        self.assertEqual((encoding, payload[3:]), (0, noise))

    def test_errors_use_binary_frames_too(self):
        self.reply.update(status=404, raw=b'{"error":"not found"}')
        self.assertEqual(decode(self.request(bin=1, zd=1))[1:], (404, 0, b'{"error":"not found"}'))

    def test_frame_header_layout_and_size_budget(self):
        payload = bytes(range(256)) * 10
        frames = ble.bin_frames(0x12345, payload, 50)
        self.assertEqual(len(frames), -(-len(payload) // 43))
        for idx, frame in enumerate(frames):
            self.assertLessEqual(len(frame), 50)
            self.assertEqual(struct.unpack('>BHHH', frame[:7]),
                             (0xB1, 0x2345, idx, len(frames)))
        self.assertEqual(b''.join(f[7:] for f in frames), payload)
        self.assertEqual(ble.bin_frames(1, b'', 50), [bytes([0xB1, 0, 1, 0, 0, 0, 1])])

    def test_resend_replays_the_binary_frames(self):
        frames = self.request(bin=1, zd=1)
        self.response.sent.clear()
        self.api.WriteValue(list(b'{"resend": 7}'), {'mtu': 185})
        self.assertEqual(self.response.sent, frames)

    def test_clients_without_bin_get_the_json_envelope_unchanged(self):
        self.reply['raw'] = b'{"ts":null}'
        self.assertEqual(self.request(), [b'{"id": 7, "status": 200, "body": {"ts": null}}'])

        self.reply['raw'] = STATUS_BODY
        frames = self.request(mtu=512, gz=1)
        chunks = [json.loads(f) for f in frames]
        envelope = json.loads(''.join(c['data'] for c in chunks)) if 'chunks' in chunks[0] else chunks[0]
        self.assertTrue(envelope['gz'])
        self.assertEqual(json.loads(zlib.decompress(
            __import__('base64').b64decode(envelope['body_gz']), -15)), json.loads(STATUS_BODY))


if __name__ == '__main__':
    unittest.main()
