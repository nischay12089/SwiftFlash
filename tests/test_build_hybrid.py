"""Device-free checks for the hybrid ZIP builder (Python standard library)."""

import contextlib
import io
from pathlib import Path
import stat
import struct
import sys
import tempfile
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import build_hybrid

needs_zstd = unittest.skipIf(build_hybrid.zstd is None, "compression.zstd requires Python 3.14+")


def expand_sparse(data):
    """Independent Android sparse reader: RAW and FILL chunks only."""
    magic, major, _, header, chunk_header, block, blocks, chunks, _ = struct.unpack_from("<IHHHHIIII", data)
    assert (magic, major, header, chunk_header) == (build_hybrid.SPARSE_MAGIC, 1, 28, 12)
    output, offset = bytearray(), header
    for _ in range(chunks):
        kind, _, count, total = struct.unpack_from("<HHII", data, offset)
        payload = data[offset + chunk_header:offset + total]
        if kind == build_hybrid.RAW_CHUNK:
            assert len(payload) == count * block
            output += payload
        elif kind == build_hybrid.FILL_CHUNK:
            assert len(payload) == 4
            output += payload * (count * block // 4)
        else:
            raise AssertionError("unexpected chunk type {:#x}".format(kind))
        offset += total
    assert offset == len(data) and len(output) == blocks * block
    return bytes(output)


MANIFEST = """product onyx
set_active a
flash boot_ab "images/boot space.img"
flash_region modemfirmware_ab CN images/cn.img images/ww.img
flash vendor_boot_ab "images/boot space.img"
reboot fastboot
flash super images/super.img
reboot bootloader
wipe metadata
wipe userdata
reboot system
"""


class HybridBuilderTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="hybrid builder [spaces] ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.images = self.root / "images"
        self.images.mkdir()
        for name in ("boot space.img", "cn.img", "ww.img", "super.img"):
            (self.images / name).write_bytes((name + " fixture").encode())
        self.manifest = self.root / "manifest.txt"
        self.manifest.write_text(MANIFEST, encoding="utf-8", newline="\n")
        self.recovery = self.root / "update-binary"
        self.recovery.write_bytes(b"recovery executable fixture")
        self.host = self.root / "desktop-binary"
        self.host.write_bytes(b"desktop executable fixture")
        self.output = self.root / "hybrid.zip"

    def args(self, *extra, compression="none"):
        return build_hybrid.parser().parse_args([
            "--images", str(self.images), "--manifest", str(self.manifest),
            "--recovery-binary", str(self.recovery), "--linux-binary", str(self.host),
            "--output", str(self.output), "--compression", compression, *extra,
        ])

    def test_archive_root_shares_images_once_and_preserves_executable_permissions(self):
        (self.images / "unused.img").write_bytes(b"not referenced")
        build_hybrid.build(self.args())
        with zipfile.ZipFile(self.output) as archive:
            names = archive.namelist()
            self.assertEqual(len(names), len(set(names)))
            self.assertEqual(sum(name.startswith("images/") for name in names), 4)
            self.assertNotIn("images/unused.img", names)
            self.assertIn("META-INF/com/google/android/update-binary", names)
            self.assertIn("fastboot-manifest.txt", names)
            self.assertIn("bin/linux/swiftflash-fastboot", names)
            self.assertEqual(archive.read("fastboot-manifest.txt").decode(), MANIFEST)
            self.assertEqual(archive.read("META-INF/com/google/android/update-binary"), self.recovery.read_bytes())
            for name in ("META-INF/com/google/android/update-binary", "bin/linux/swiftflash-fastboot", "linux_install_upgrade.sh"):
                self.assertEqual(stat.S_IMODE(archive.getinfo(name).external_attr >> 16), 0o755)
            for name in names:
                self.assertEqual(archive.getinfo(name).compress_type, zipfile.ZIP_STORED)
                # Images need ZIP64; recovery-extracted installer files stay plain.
                if name.startswith("images/"):
                    self.assertGreaterEqual(archive.getinfo(name).extract_version, 45)
                else:
                    self.assertLess(archive.getinfo(name).extract_version, 45)
            images = [name.startswith("images/") for name in names]
            self.assertEqual(images, sorted(images), "images must follow installer entries")
            self.assertIsNone(archive.testzip())

    @needs_zstd
    def test_default_stores_each_image_once_as_checksummed_zstd(self):
        build_hybrid.main([
            "--images", str(self.images), "--manifest", str(self.manifest),
            "--recovery-binary", str(self.recovery), "--linux-binary", str(self.host),
            "--output", str(self.output),
        ])
        with zipfile.ZipFile(self.output) as archive:
            images = sorted(name for name in archive.namelist() if name.startswith("images/"))
            self.assertEqual(images, [
                "images/boot space.img.zst", "images/cn.img.zst",
                "images/super.img.zst", "images/ww.img.zst",
            ])
            for name in images:
                frame = archive.read(name)
                self.assertEqual(frame[:4], b"\x28\xb5\x2f\xfd")
                self.assertTrue(frame[4] & 0x04, "zstd content checksum flag")
                source = self.images / name[len("images/"):-len(".zst")]
                self.assertEqual(build_hybrid.zstd.decompress(frame), source.read_bytes())
            self.assertEqual(archive.read("fastboot-manifest.txt").decode(), MANIFEST)

    @needs_zstd
    def test_large_raw_images_become_lossless_sparse_before_compression(self):
        block = build_hybrid.SPARSE_BLOCK
        raw = b"".join((
            bytes(block * 3),
            b"\xab\xcd\xef\x01" * (block // 4) * 2,
            bytes(range(256)) * (block // 256) * 5,
            bytes(block),
            b"tail" * (block // 4),
        ))
        (self.images / "super.img").write_bytes(raw)
        build_hybrid.build(self.args("--sparse-min-mib", "0", compression="zstd"))
        with zipfile.ZipFile(self.output) as archive:
            self.assertEqual(build_hybrid.zstd.decompress(archive.read("images/super.img.zst")), raw)
        self.output.unlink()
        (self.images / "super.img").write_bytes(raw * 100)
        build_hybrid.build(self.args("--sparse-min-mib", "1", compression="zstd"))
        with zipfile.ZipFile(self.output) as archive:
            sparse = build_hybrid.zstd.decompress(archive.read("images/super.img.zst"))
            boot = build_hybrid.zstd.decompress(archive.read("images/boot space.img.zst"))
        self.assertEqual(struct.unpack_from("<I", sparse)[0], build_hybrid.SPARSE_MAGIC)
        self.assertLess(len(sparse), len(raw) * 100)
        self.assertEqual(expand_sparse(sparse), raw * 100)
        self.assertEqual(boot, (self.images / "boot space.img").read_bytes())

    def test_already_sparse_and_unaligned_images_are_stored_unchanged(self):
        sparse = struct.pack("<IHHHHIIII", build_hybrid.SPARSE_MAGIC, 1, 0, 28, 12, 4096, 1, 1, 0)
        sparse += struct.pack("<HHII", build_hybrid.FILL_CHUNK, 0, 1, 16) + bytes(4)
        (self.images / "super.img").write_bytes(sparse + bytes(2 * 1024 * 1024))
        (self.images / "cn.img").write_bytes(bytes(2 * 1024 * 1024 + 1))
        build_hybrid.build(self.args("--sparse-min-mib", "1"))
        with zipfile.ZipFile(self.output) as archive:
            for name in ("super.img", "cn.img"):
                self.assertEqual(archive.read("images/" + name), (self.images / name).read_bytes())

    def test_banner_lines_become_recovery_ui_print_lines(self):
        banner = self.root / "banner.txt"
        banner.write_text("Device  : onyx\n\nModder  : someone\n", encoding="utf-8")
        build_hybrid.build(self.args("--banner", str(banner)))
        with zipfile.ZipFile(self.output) as archive:
            script = archive.read("META-INF/com/google/android/updater-script").decode()
        self.assertTrue(script.startswith(build_hybrid.UPDATER_SCRIPT.decode()))
        self.assertTrue(script.endswith("ui_print Device  : onyx\nui_print\nui_print Modder  : someone\n"))
        banner.write_text("bad\x1b[31m\n", encoding="utf-8")
        with self.assertRaisesRegex(build_hybrid.BuildError, "control"):
            build_hybrid.build(self.args("--banner", str(banner), "--force"))

    def test_zstd_without_python_support_fails_before_creating_output(self):
        with mock.patch.object(build_hybrid, "zstd", None):
            with self.assertRaisesRegex(build_hybrid.BuildError, "3.14"):
                build_hybrid.build(self.args(compression="zstd"))
        self.assertFalse(self.output.exists())

    def test_missing_nonselected_regional_image_fails_before_creating_output(self):
        (self.images / "ww.img").unlink()
        with self.assertRaisesRegex(build_hybrid.BuildError, "ww.img"):
            build_hybrid.build(self.args())
        self.assertFalse(self.output.exists())

    def test_simple_flash_aliases_are_included_at_root_with_unix_execute_permission(self):
        build_hybrid.build(self.args())
        with zipfile.ZipFile(self.output) as archive:
            for name in ("flash.bat", "flash.sh"):
                self.assertEqual(archive.read(name), (build_hybrid.ASSETS / name).read_bytes())
            self.assertEqual(stat.S_IMODE(archive.getinfo("flash.sh").external_attr >> 16), 0o755)

    def test_empty_image_and_empty_recovery_binary_are_rejected(self):
        for path in (self.images / "super.img", self.recovery):
            original = path.read_bytes()
            path.write_bytes(b"")
            with self.assertRaisesRegex(build_hybrid.BuildError, "nonempty"):
                build_hybrid.build(self.args())
            self.assertFalse(self.output.exists())
            path.write_bytes(original)

    def test_output_must_remain_outside_image_tree(self):
        with self.assertRaisesRegex(build_hybrid.BuildError, "outside"):
            build_hybrid.build(self.args("--output", str(self.images / "nested" / "hybrid.zip")))
        self.assertFalse((self.images / "nested").exists())

    def test_manifest_rejects_traversal_commands_compression_and_data_flashing(self):
        invalid = (
            'flash boot_ab images/../outside.img',
            'flash boot_ab images/./boot.img',
            'flash boot_ab images//boot.img',
            'flash boot_ab images/boot.img/',
            'flash boot_ab "images\\outside.img"',
            'flash boot_ab images/\\outside.img',
            "flash boot_ab 'images/boot.img'",
            'flash boot_ab /absolute/image.img',
            'flash boot_ab images/boot.img.gz',
            'flash boot_ab images/boot.img.GZ',
            'flash userdata_ab images/data.img',
            'flash USERDATA_ab images/data.img',
            'flash . images/data.img',
            'flash _ab images/data.img',
            'shell rm -rf images',
            'flash_region modemfirmware_ab invalid images/cn.img images/ww.img',
        )
        for instruction in invalid:
            with self.subTest(instruction=instruction):
                with self.assertRaises(build_hybrid.BuildError):
                    build_hybrid.manifest_images("product onyx\n" + instruction)

    def test_manifest_rejects_operations_after_final_reboot(self):
        with self.assertRaisesRegex(build_hybrid.BuildError, "final"):
            build_hybrid.manifest_images(MANIFEST + "wipe userdata\n")

    def test_raw_firmware_extensions_and_extensionless_images_are_supported(self):
        for name in ("firmware.mbn", "firmware.elf", "firmware"):
            with self.subTest(name=name):
                self.manifest.write_text("product onyx\nflash abl_ab images/" + name, encoding="utf-8")
                (self.images / name).write_bytes(b"raw firmware fixture")
                build_hybrid.build(self.args("--force"))
                with zipfile.ZipFile(self.output) as archive:
                    self.assertEqual(archive.read("images/" + name), b"raw firmware fixture")

    def test_complete_rom_requires_at_least_one_desktop_executable(self):
        args = self.args()
        args.linux_binary = None
        with self.assertRaisesRegex(build_hybrid.BuildError, "at least one desktop"):
            build_hybrid.build(args)
        self.assertFalse(self.output.exists())

    def test_manifest_rejects_duplicate_expanded_recovery_partition_targets(self):
        with self.assertRaisesRegex(build_hybrid.BuildError, "more than once"):
            build_hybrid.manifest_images("product onyx\nflash boot_ab images/boot.img\nflash boot_a images/other.img\n")

    def test_oversized_manifest_is_rejected_for_recovery_compatibility(self):
        self.manifest.write_text(MANIFEST + "#" * (256 * 1024), encoding="utf-8")
        with self.assertRaisesRegex(build_hybrid.BuildError, "256 KiB"):
            build_hybrid.build(self.args())
        self.assertFalse(self.output.exists())

    def test_symlink_image_escape_is_rejected(self):
        outside = self.root / "outside.img"
        outside.write_bytes(b"outside")
        image = self.images / "super.img"
        image.unlink()
        try:
            image.symlink_to(outside)
        except OSError as error:
            self.skipTest("symlink creation unavailable: " + str(error))
        with self.assertRaisesRegex(build_hybrid.BuildError, "escapes"):
            build_hybrid.build(self.args())
        self.assertFalse(self.output.exists())

    def test_user_tools_are_included_with_dependencies(self):
        tools = self.root / "tools" / "windows"
        tools.mkdir(parents=True)
        (tools / "fastboot.exe").write_bytes(b"vendor executable fixture")
        (tools / "AdbWinApi.dll").write_bytes(b"vendor dependency fixture")
        build_hybrid.build(self.args("--tools", str(tools.parent)))
        with zipfile.ZipFile(self.output) as archive:
            self.assertEqual(archive.read("bin/windows/AdbWinApi.dll"), b"vendor dependency fixture")
            self.assertIn("bin/windows/fastboot.exe", archive.namelist())

    def test_all_supplied_host_binaries_share_one_recovery_zip(self):
        build_hybrid.build(self.args("--windows-binary", str(self.host), "--macos-binary", str(self.host)))
        with zipfile.ZipFile(self.output) as archive:
            for name in ("bin/windows/swiftflash-fastboot.exe", "bin/linux/swiftflash-fastboot", "bin/macos/swiftflash-fastboot"):
                self.assertEqual(archive.read(name), self.host.read_bytes())
            self.assertEqual(sum(name.startswith("images/") for name in archive.namelist()), 4)

    def test_empty_tool_dependency_is_rejected(self):
        tools = self.root / "tools" / "windows"
        tools.mkdir(parents=True)
        (tools / "fastboot.exe").write_bytes(b"vendor executable fixture")
        (tools / "empty.dll").write_bytes(b"")
        with self.assertRaisesRegex(build_hybrid.BuildError, "nonempty"):
            build_hybrid.build(self.args("--tools", str(tools.parent)))
        self.assertFalse(self.output.exists())

    def test_existing_archive_is_kept_unless_force_is_explicit(self):
        self.output.write_bytes(b"existing output")
        with self.assertRaisesRegex(build_hybrid.BuildError, "already exists"):
            build_hybrid.build(self.args())
        self.assertEqual(self.output.read_bytes(), b"existing output")
        build_hybrid.build(self.args("--force"))
        self.assertTrue(zipfile.is_zipfile(self.output))

    def test_template_is_explicitly_marked_and_contains_no_rom_images(self):
        args = build_hybrid.parser().parse_args([
            "--template", "--manifest", str(self.manifest),
            "--recovery-binary", str(self.recovery), "--output", str(self.output),
        ])
        build_hybrid.build(args)
        with zipfile.ZipFile(self.output) as archive:
            self.assertIn("TEMPLATE-NOT-FLASHABLE.txt", archive.namelist())
            self.assertEqual([name for name in archive.namelist() if name.startswith("images/")], ["images/.gitkeep"])
            self.assertIn(b"NOT A FLASHABLE ROM", archive.read("TEMPLATE-NOT-FLASHABLE.txt"))

    def test_empty_flasher_lists_required_images_without_template_marker(self):
        args = build_hybrid.parser().parse_args([
            "--empty", "--manifest", str(self.manifest), "--recovery-binary", str(self.recovery),
            "--linux-binary", str(self.host), "--output", str(self.output),
        ])
        build_hybrid.build(args)
        with zipfile.ZipFile(self.output) as archive:
            names = archive.namelist()
            self.assertNotIn("TEMPLATE-NOT-FLASHABLE.txt", names)
            self.assertEqual([name for name in names if name.startswith("images/")], ["images/PUT-YOUR-IMAGES-HERE.txt"])
            notice = archive.read("images/PUT-YOUR-IMAGES-HERE.txt").decode()
            for image in ("images/boot space.img", "images/cn.img", "images/ww.img", "images/super.img"):
                self.assertIn(image + "\n", notice)
            self.assertIn("bin/linux/swiftflash-fastboot", names)
        args.linux_binary = None
        with self.assertRaisesRegex(build_hybrid.BuildError, "at least one desktop"):
            build_hybrid.build(args)

    def test_image_copy_is_streamed_and_source_read_failure_leaves_no_output(self):
        original_copy = build_hybrid.shutil.copyfileobj
        calls = []

        def copy(source, target, length=0):
            calls.append(length)
            return original_copy(source, target, length)

        with mock.patch.object(build_hybrid.shutil, "copyfileobj", side_effect=copy):
            build_hybrid.build(self.args())
        self.assertTrue(calls)
        self.assertTrue(all(length == 1024 * 1024 for length in calls))
        self.output.unlink()
        with mock.patch.object(build_hybrid.shutil, "copyfileobj", side_effect=OSError("read failed")):
            with self.assertRaisesRegex(OSError, "read failed"):
                build_hybrid.build(self.args())
        self.assertFalse(self.output.exists())
        self.assertEqual(list(self.root.glob(".swiftflash-hybrid-*.zip")), [])

    def test_cli_reports_input_errors_without_a_partial_zip(self):
        (self.images / "cn.img").unlink()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status = build_hybrid.main([
                "--images", str(self.images), "--manifest", str(self.manifest),
                "--recovery-binary", str(self.recovery), "--linux-binary", str(self.host),
                "--output", str(self.output),
            ])
        self.assertEqual(status, 1)
        self.assertIn("Hybrid package error", stderr.getvalue())
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
