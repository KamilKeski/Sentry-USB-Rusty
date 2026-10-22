#!/usr/bin/env python3
"""Every writer of /etc/avahi/services/sentryusb.service renders the same file,
and the avahi daemon config drops the workstation record.

The Rust setup embeds setup/pi/avahi-sentryusb.service with include_str! and
has its own unit tests; this covers the BLE daemon, the update migration
script and the legacy bash setup against that same template.
"""
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import types
import unittest

REPO = Path(__file__).resolve().parents[1]
TEMPLATE = (REPO / 'setup/pi/avahi-sentryusb.service').read_text()
MACHINE_ID = '0123456789abcdef0123456789abec92\n'
EXPECTED = TEMPLATE.replace('@DEVICE_SUFFIX@', 'EC92')
# Trimmed Debian avahi-daemon.conf.
STOCK_CONF = """[server]
use-ipv4=yes
#use-ipv6=yes
ratelimit-burst=1000

[publish]
#publish-addresses=yes
#publish-hinfo=no
#publish-workstation=no
#publish-aaaa-on-ipv4=yes

[reflector]
#enable-reflector=no
"""


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
    sys.modules.update({'dbus': dbus, 'dbus.service': dbus.service,
                        'dbus.exceptions': dbus.exceptions, 'dbus.mainloop': dbus.mainloop,
                        'dbus.mainloop.glib': dbus.mainloop.glib})
    gi = types.ModuleType('gi')
    gi.repository = types.ModuleType('gi.repository')
    gi.repository.GLib = types.SimpleNamespace()
    sys.modules.update({'gi': gi, 'gi.repository': gi.repository})

    spec = importlib.util.spec_from_file_location('sentryusb_ble', REPO / 'server/ble/sentryusb-ble.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.log.disabled = True
    return module


ble = load_ble_daemon()

# Commands the shell snippets call besides the ones under test.
SHELL_STUBS = '''
dpkg() { return 0; }
apt-get() { return 0; }
systemctl() { echo "systemctl $*" >> "$WORK/calls"; }
command() { return 0; }
copy_script() { cp "$REPO/$1" "$2/"; }
setup_progress() { :; }
'''


class AvahiServiceTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.work = Path(tmp.name)
        (self.work / 'machine-id').write_text(MACHINE_ID)
        (self.work / 'services').mkdir()
        self.service = self.work / 'services/sentryusb.service'
        self.conf = self.work / 'avahi-daemon.conf'
        self.conf.write_text(STOCK_CONF)

    def isolate(self, script):
        # /tmp first: the work dir itself may live under /tmp.
        return (script.replace('/tmp', str(self.work))
                      .replace('/etc/avahi/services', str(self.work / 'services'))
                      .replace('/etc/machine-id', str(self.work / 'machine-id')))

    def run_shell(self, script):
        result = subprocess.run(['bash', '-c', 'set -e\n' + SHELL_STUBS + script],
                                env={**os.environ, 'WORK': str(self.work), 'REPO': str(REPO),
                                     'AVAHI_CONF': str(self.conf)},
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def run_migration(self):
        source = (REPO / 'crates/sentryusb/src/migrate.rs').read_text()
        block = re.search(r'^# ── Install/update Avahi mDNS service ──\n.*?^fi\n', source, re.M | re.S)
        self.assertIsNotNone(block)
        # The script lives in a format! string: undo the brace escaping.
        script = block[0].replace('{{', '{').replace('}}', '}')
        payload = self.work / 'setup/pi'
        payload.mkdir(parents=True, exist_ok=True)
        (payload / 'avahi-sentryusb.service').write_text(TEMPLATE)
        (payload / 'avahi-ipv4-only.sh').write_text((REPO / 'setup/pi/avahi-ipv4-only.sh').read_text())
        calls = self.work / 'calls'
        calls.unlink(missing_ok=True)
        self.run_shell(f'TMPDIR={self.work}\n' + self.isolate(script))
        return calls.read_text() if calls.exists() else ''

    def assert_daemon_config(self, conf):
        sections = re.split(r'^(?=\[)', conf, flags=re.M)
        publish = next(s for s in sections if s.startswith('[publish]'))
        self.assertIn('\npublish-workstation=no\n', publish)
        self.assertIn('\npublish-aaaa-on-ipv4=no\n', publish)
        self.assertEqual(conf.count('publish-workstation'), 1)
        for kept in ('ratelimit-burst=1000', '#publish-addresses=yes', '#enable-reflector=no'):
            self.assertIn(kept, conf)

    def test_ble_daemon_template_matches_setup_template(self):
        self.assertEqual(ble.AVAHI_SERVICE_TEMPLATE, TEMPLATE)
        self.assertEqual(ble.avahi_service_xml('EC92'), EXPECTED)

    def test_template_advertises_sentryusb_and_http(self):
        self.assertIn('<type>_sentryusb._tcp</type>', EXPECTED)
        self.assertIn('<txt-record>suffix=EC92</txt-record>', EXPECTED)
        self.assertIn('<type>_http._tcp</type>', EXPECTED)
        self.assertEqual(TEMPLATE.count('@DEVICE_SUFFIX@'), 1)

    def test_migration_script_renders_template(self):
        self.run_migration()
        self.assertEqual(self.service.read_text(), EXPECTED)
        self.assert_daemon_config(self.conf.read_text())

    def test_migration_falls_back_without_machine_id(self):
        (self.work / 'machine-id').unlink()
        self.run_migration()
        self.assertEqual(self.service.read_text(), TEMPLATE.replace('@DEVICE_SUFFIX@', '0000'))

    def test_migration_restarts_avahi_only_on_change(self):
        self.assertIn('systemctl restart avahi-daemon', self.run_migration())
        self.assertNotIn('restart', self.run_migration())
        self.conf.write_text(STOCK_CONF)
        self.assertIn('systemctl restart avahi-daemon', self.run_migration())

    def test_daemon_config_script_is_idempotent(self):
        script = ['bash', str(REPO / 'setup/pi/avahi-ipv4-only.sh')]
        env = {**os.environ, 'AVAHI_CONF': str(self.conf)}
        first = subprocess.run(script, env=env, capture_output=True, text=True, check=True)
        once = self.conf.read_text()
        second = subprocess.run(script, env=env, capture_output=True, text=True, check=True)
        self.assertEqual((first.stdout, second.stdout), ('changed\n', 'unchanged\n'))
        self.assertEqual(self.conf.read_text(), once)
        self.assert_daemon_config(once)

    def test_legacy_setup_renders_template(self):
        source = (REPO / 'setup/pi/setup-sentryusb').read_text()
        function = re.search(r'^function install_avahi_service \(\) \{.*?^}\n', source, re.M | re.S)
        self.assertIsNotNone(function)
        self.run_shell(self.isolate(function[0]) + 'install_avahi_service\n')
        self.assertEqual(self.service.read_text(), EXPECTED)
        self.assert_daemon_config(self.conf.read_text())

    def test_ble_daemon_repairs_a_file_with_the_right_suffix(self):
        # What a re-run of the old setup left behind, plus a stray suffix.
        self.service.write_text('<service-group><service><type>_http._tcp</type>'
                                '<txt-record>suffix=EC92</txt-record></service></service-group>\n')
        calls = []
        ble.AVAHI_SERVICE_PATH = str(self.service)
        ble.get_device_suffix = lambda: 'EC92'
        ble.subprocess = types.SimpleNamespace(run=lambda args, **kw: calls.append(args))

        ble.update_avahi_service_name('SentryUSB-EC92')
        self.assertEqual(self.service.read_text(), EXPECTED)
        self.assertIn(['systemctl', 'restart', 'avahi-daemon'], calls)

        calls.clear()
        ble.update_avahi_service_name('SentryUSB-EC92')
        self.assertEqual(calls, [], 'a current file must not restart avahi')


if __name__ == '__main__':
    unittest.main()
