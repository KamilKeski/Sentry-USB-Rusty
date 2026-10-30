#!/usr/bin/env python3
"""Architecture selection tests: no actual disks, network, or repairs."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import hashlib
import io
import os
import json
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("repair_tool", Path(__file__).resolve().parents[1] / "run/xfs-repair-tool.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

def elf(bits, machine):
    b = bytearray(64)
    b[:4] = b"\x7fELF"
    b[4] = 2 if bits == 64 else 1
    b[5] = 1
    b[18:20] = machine.to_bytes(2, "little")
    return bytes(b)

class RepairToolTests(unittest.TestCase):
    def test_aarch64_kernel_rejects_armhf_even_with_32bit_userspace(self):
        self.assertFalse(tool.compatible("aarch64", elf(32, 40)))
        self.assertTrue(tool.compatible("aarch64", elf(64, 183)))

    def test_native_32bit_and_x86_are_supported(self):
        self.assertTrue(tool.compatible("armv7l", elf(32, 40)))
        self.assertFalse(tool.compatible("armv7l", elf(64, 183)))
        self.assertTrue(tool.compatible("x86_64", elf(64, 62)))
        self.assertTrue(tool.compatible("i686", elf(32, 3)))

    def test_scripts_truncation_and_wrong_machine_fail_closed(self):
        for data in (b"#!/bin/sh", b"\x7fELF", elf(64, 62)):
            self.assertFalse(tool.compatible("aarch64", data))
        self.assertFalse(tool.compatible("unknown", elf(64, 183)))

    def test_native_selection_does_not_download(self):
        with tempfile.TemporaryDirectory() as d:
            binary = Path(d) / "xfs_repair"
            with patch.object(tool, "prepare_bundle", side_effect=AssertionError("network")):
                for kernel, bits, machine in [('armv6l', 32, 40), ('armv7l', 32, 40),
                                               ('aarch64', 64, 183), ('x86_64', 64, 62),
                                               ('i686', 32, 3)]:
                    with self.subTest(kernel=kernel):
                        binary.write_bytes(elf(bits, machine))
                        self.assertEqual(tool.select_tool(kernel, binary, Path(d)), [str(binary)])

    def test_missing_native_tool_on_32bit_fails_without_downloading_arm64(self):
        with patch.object(tool, 'prepare_bundle', side_effect=AssertionError('wrong architecture')):
            with self.assertRaisesRegex(RuntimeError, 'install native xfsprogs'):
                tool.select_tool('armv7l', None, Path('/unused'))

    def test_mixed_arm_uses_bundle_and_never_falls_back(self):
        with tempfile.TemporaryDirectory() as d:
            binary = Path(d) / "xfs_repair"
            binary.write_bytes(elf(32, 40))
            with patch.object(tool, "prepare_bundle", return_value=["loader", "--library-path", "libs", "repair"]) as prepare:
                self.assertEqual(tool.select_tool("aarch64", binary, Path(d)), ["loader", "--library-path", "libs", "repair"])
                prepare.assert_called_once()
            with patch.object(tool, "prepare_bundle", side_effect=RuntimeError("offline")):
                with self.assertRaisesRegex(RuntimeError, "offline"):
                    tool.select_tool("aarch64", binary, Path(d))

    def test_download_checksum_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "checksum"):
            tool.verify_package(b"bad package", "0" * 64)

    @unittest.skipIf(os.name == 'nt', 'Linux cache locking')
    def test_bundle_is_verified_cached_offline_and_tampering_rejected(self):
        data = b'verified deb fixture'
        packages = [['pool/fixture.deb', hashlib.sha256(data).hexdigest()]]
        def run(args, **kwargs):
            if args[0] == 'dpkg-deb':
                root = Path(args[-1])
                binary = root / 'usr/sbin/xfs_repair'
                loader = root / 'usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1'
                for f in (binary, loader):
                    f.parent.mkdir(parents=True, exist_ok=True)
                    f.write_bytes(elf(64, 183))
            else:
                self.assertEqual(args[-1], '-V')
        with tempfile.TemporaryDirectory() as d, patch.object(tool, 'PACKAGES', packages):
            with patch.object(tool.urllib.request, 'urlopen', return_value=io.BytesIO(data)), patch.object(tool.subprocess, 'run', side_effect=run):
                command = tool.prepare_bundle(Path(d))
            with patch.object(tool.urllib.request, 'urlopen', side_effect=AssertionError('offline')):
                self.assertEqual(tool.prepare_bundle(Path(d)), command)
                # Existing installs boot read-only. A valid persistent cache
                # must work without opening its write lock or using /run.
                volatile = Path(d) / 'unused-volatile'
                with patch.object(tool, 'PERSISTENT', Path(d)), patch.object(tool, 'VOLATILE', volatile), \
                     patch.object(tool.platform, 'machine', return_value='aarch64'), \
                     patch.object(tool.shutil, 'which', return_value=None), \
                     patch.object(tool.os, 'access', return_value=False), \
                     patch.object(tool.sys, 'stdout', new_callable=io.StringIO) as stdout:
                    tool.main()
                    self.assertEqual(json.loads(stdout.getvalue()), command)
                    self.assertFalse(volatile.exists())
                Path(command[-1]).write_bytes(elf(32, 40))
                with self.assertRaisesRegex(RuntimeError, 'integrity'):
                    tool.prepare_bundle(Path(d))

    @unittest.skipIf(os.name == 'nt', 'Linux cache locking')
    def test_bad_download_never_extracts_or_publishes(self):
        with tempfile.TemporaryDirectory() as d, patch.object(tool, 'PACKAGES', [['pool/bad.deb', '0' * 64]]):
            with patch.object(tool.urllib.request, 'urlopen', return_value=io.BytesIO(b'bad')), patch.object(tool.subprocess, 'run') as run:
                with self.assertRaisesRegex(RuntimeError, 'checksum'):
                    tool.prepare_bundle(Path(d))
                run.assert_not_called()
                self.assertEqual([f.name for f in Path(d).iterdir()], ['prepare.lock'])

if __name__ == "__main__":
    unittest.main()
