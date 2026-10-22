#!/usr/bin/env python3
"""BLE claim/auth rules of the real daemon, with D-Bus and GLib stubbed out."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest

REPO = Path(__file__).resolve().parents[1]


def install_stubs():
    """Just enough of python3-dbus and gi for the daemon to import."""
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
    dbus.Array = lambda items, signature=None: list(items)
    dbus.Byte = int
    dbus.String = str
    dbus.ObjectPath = str
    for name in ('', '.service', '.exceptions', '.mainloop', '.mainloop.glib'):
        sys.modules['dbus' + name] = eval('dbus' + name)

    glib = types.SimpleNamespace(idle_add=lambda *a: None, timeout_add=lambda *a: None)
    gi = types.ModuleType('gi')
    gi.repository = types.ModuleType('gi.repository')
    gi.repository.GLib = glib
    sys.modules['gi'] = gi
    sys.modules['gi.repository'] = gi.repository


install_stubs()
spec = importlib.util.spec_from_file_location('sentryusb_ble', REPO / 'server/ble/sentryusb-ble.py')
ble = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ble)
ble.log.disabled = True

SERVICE = types.SimpleNamespace(path='/test/service', get_path=lambda: '/test/service')
PHONE = '/org/bluez/hci0/dev_AA_AA_AA_AA_AA_AA'
OTHER = '/org/bluez/hci0/dev_BB_BB_BB_BB_BB_BB'


class FakeResponse:
    def __init__(self):
        self.sent = []

    def send_notification(self, value):
        self.sent.append(json.loads(bytes(value)))


class BleAuthTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.config = self.dir / 'sentryusb.conf'
        os.environ['SENTRYUSB_CONFIG_PATH'] = str(self.config)
        self.addCleanup(os.environ.pop, 'SENTRYUSB_CONFIG_PATH', None)
        ble.PIN_FILE = str(self.dir / 'ble-pin')
        ble.BOOT_PIN_FILE = str(self.dir / 'BLE_PIN')
        ble.PIN_BINDING_FILE = str(self.dir / 'ble-pin-web-auth')
        ble.remount_root_rw = lambda: None
        ble.authenticated_peers.clear()
        ble.auth_failures.clear()
        ble._web_creds_cache = (None, None)
        ble._legacy_binding_attempted = False

        self.proxied = []
        ble.proxy_api_request = lambda method, path, body=None: (
            self.proxied.append((method, path)) or {'status': 200, 'body': {}})
        ble.threading = types.SimpleNamespace(
            Thread=lambda target, daemon: types.SimpleNamespace(start=target))

        self.auth = ble.AuthCharacteristic(None, 0, SERVICE)
        self.auth.notifying = True
        self.response = FakeResponse()
        self.api = ble.APIRequestCharacteristic(None, 0, SERVICE, self.response)

    def write_config(self, text):
        # Atomic replace, like sentryusb_config::write_file.
        tmp = self.dir / 'sentryusb.conf.tmp'
        tmp.write_bytes(text if isinstance(text, bytes) else text.encode())
        os.replace(tmp, self.config)

    def login(self, user='admin', password='hunter2'):
        self.write_config(f"export WEB_USERNAME={user}\nexport WEB_PASSWORD='{password}'\n")

    def read(self, device=PHONE):
        return json.loads(bytes(self.auth.ReadValue({'device': device})))

    def send(self, msg, device=PHONE):
        self.auth.WriteValue(list(json.dumps(msg).encode()), {'device': device})
        return json.loads(bytes(self.auth.value))

    def set_pin(self, pin='1234', device=PHONE, **login):
        return self.send({'action': 'set_pin', 'pin': pin, **login}, device)

    def api_request(self, device=PHONE):
        self.response.sent.clear()
        before = len(self.proxied)
        self.api.WriteValue(list(json.dumps({'id': 7, 'method': 'GET', 'path': '/status'}).encode()),
                            {'device': device})
        if len(self.proxied) > before:
            return 'proxied'
        return self.response.sent[-1]

    def test_read_reports_whether_web_login_is_required(self):
        self.assertEqual(self.read(), {'claimed': False, 'authenticated': False,
                                       'web_auth_required': False})
        self.write_config('export WEB_USERNAME=admin\nexport WEB_PASSWORD=\n')
        self.assertFalse(self.read()['web_auth_required'])  # partial = off, as auth.rs
        self.login()
        self.assertTrue(self.read()['web_auth_required'])

    def test_claim_without_web_login_configured(self):
        self.assertEqual(self.set_pin(), {'success': True})
        self.assertEqual(self.read(), {'claimed': True, 'authenticated': True,
                                       'web_auth_required': False})
        self.assertFalse(self.read(OTHER)['authenticated'])
        self.assertEqual(self.api_request(), 'proxied')

    def test_claim_requires_web_login_when_enabled(self):
        self.login()
        self.assertEqual(self.set_pin(), {'success': False, 'error': 'web_login_required'})
        self.assertEqual(self.set_pin(username='admin', password='nope'),
                         {'success': False, 'error': 'wrong_web_login'})
        self.assertEqual(self.set_pin(username='admin ', password='hunter2'),
                         {'success': False, 'error': 'wrong_web_login'})
        self.assertFalse(self.read()['claimed'])
        self.assertFalse(Path(ble.PIN_FILE).exists())
        self.assertEqual(self.set_pin(username='admin', password='hunter2'), {'success': True})
        self.assertEqual(self.read(), {'claimed': True, 'authenticated': True,
                                       'web_auth_required': True})

    def test_wrong_web_logins_lock_out_the_peer(self):
        self.login()
        for _ in range(ble.AUTH_MAX_FAILS):
            self.assertEqual(self.set_pin(username='admin', password='guess')['error'],
                             'wrong_web_login')
        self.assertEqual(self.set_pin(username='admin', password='hunter2'),
                         {'success': False, 'error': 'too_many_attempts'})
        self.assertEqual(self.set_pin('5678', device=OTHER,
                                      username='admin', password='hunter2'), {'success': True})

    def test_unidentified_peer_cannot_claim_or_use_the_api(self):
        self.assertEqual(self.set_pin(device=''), {'success': False, 'error': 'unknown_peer'})
        self.assertFalse(Path(ble.PIN_FILE).exists())
        self.assertFalse(self.read('')['authenticated'])

    def test_unclaimed_pi_serves_no_api(self):
        for configure in (lambda: None, self.login):
            configure()
            for device in ('', PHONE):
                self.assertEqual(self.api_request(device),
                                 {'id': 7, 'status': 403, 'body': {'error': 'not_authenticated'}})
        self.assertEqual(self.proxied, [])

    def test_pin_alone_reauthenticates_after_claim(self):
        self.login()
        self.set_pin(username='admin', password='hunter2')
        self.assertEqual(self.api_request(OTHER)['status'], 403)
        self.assertEqual(self.send({'action': 'authenticate', 'pin': '0000'}, OTHER)['error'],
                         'wrong_pin')
        self.assertEqual(self.send({'action': 'authenticate', 'pin': '1234'}, OTHER),
                         {'success': True})
        self.assertEqual(self.api_request(OTHER), 'proxied')

    def test_changing_web_login_resets_claim(self):
        self.login()
        self.set_pin(username='admin', password='hunter2')
        self.assertEqual(self.api_request(), 'proxied')

        self.login(password='new-secret')
        self.assertEqual(self.api_request()['status'], 403)
        self.assertEqual(self.read(), {'claimed': False, 'authenticated': False,
                                       'web_auth_required': True})
        for path in (ble.PIN_FILE, ble.BOOT_PIN_FILE, ble.PIN_BINDING_FILE):
            self.assertFalse(Path(path).exists(), path)
        self.assertEqual(self.send({'action': 'authenticate', 'pin': '1234'})['error'],
                         'not_claimed')
        self.assertEqual(self.set_pin(username='admin', password='hunter2')['error'],
                         'wrong_web_login')
        self.assertEqual(self.set_pin(username='admin', password='new-secret'),
                         {'success': True})

    def test_unrelated_config_changes_keep_claim(self):
        self.login()
        self.set_pin(username='admin', password='hunter2')
        self.write_config(self.config.read_text() + 'export SNAPSHOT_INTERVAL=480\n')
        self.assertEqual(self.api_request(), 'proxied')

    def test_enabling_or_disabling_web_login_resets_claim(self):
        self.set_pin()
        self.login()
        self.assertFalse(self.read()['claimed'])

        self.set_pin(username='admin', password='hunter2')
        self.write_config('# web login removed\n')
        self.assertFalse(self.read()['claimed'])

    def test_claim_from_before_binding_adopts_current_login(self):
        self.login()
        Path(ble.PIN_FILE).write_text('1234')
        self.assertTrue(self.read()['claimed'])
        self.assertTrue(Path(ble.PIN_BINDING_FILE).exists())
        self.assertEqual(self.send({'action': 'authenticate', 'pin': '1234'}), {'success': True})
        self.login(password='changed')
        self.assertFalse(self.read()['claimed'])

    def test_corrupt_binding_resets_claim(self):
        self.set_pin()
        Path(ble.PIN_BINDING_FILE).write_text('{"salt": "zz"}')
        self.assertFalse(self.read()['claimed'])

    def test_unreadable_config_fails_closed_without_wiping_claim(self):
        self.login()
        self.set_pin(username='admin', password='hunter2')
        self.write_config(b'export WEB_PASSWORD=\xff\n')
        self.assertEqual(self.read(), {'claimed': True, 'authenticated': True,
                                       'web_auth_required': True})

        ble.reset_claim('test')
        self.assertEqual(self.set_pin(username='admin', password='hunter2'),
                         {'success': False, 'error': 'web_login_unavailable'})

    def test_binding_stores_no_plain_password(self):
        self.login(password='hunter2')
        self.set_pin(username='admin', password='hunter2')
        binding = Path(ble.PIN_BINDING_FILE)
        self.assertNotIn('hunter2', binding.read_text())
        self.assertEqual(binding.stat().st_mode & 0o777, 0o600)

    def test_config_parsing_matches_sentryusb_config(self):
        parsed = ble.parse_config_exports(
            "# export WEB_USERNAME=commented\n"
            "export WEB_USERNAME=admin # inline comment\r\n"
            "  export WEB_PASSWORD=\"p w#1\"\n"
            "export BAD-KEY=x\n"
            "export ANSI=$'x'\n"
            "export WEB_USERNAME='last wins'\n")
        self.assertEqual(parsed, {'WEB_USERNAME': 'last wins', 'WEB_PASSWORD': 'p w#1',
                                  'ANSI': 'x'})


if __name__ == '__main__':
    unittest.main()
