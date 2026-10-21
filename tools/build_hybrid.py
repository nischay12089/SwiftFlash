#!/usr/bin/env python3
"""Build a recovery-installable ROM ZIP that also extracts into a fastboot package.

Uses only Python's standard library. Images are streamed into ZIP_STORED, ZIP64
entries; by default each is zstd-compressed as NAME.zst (Python 3.14+), and
large raw images are first converted to Android sparse form so empty space
costs nothing on either flashing path. No ROM data or third-party executables
are downloaded.
"""

import argparse
from dataclasses import dataclass
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import stat
import struct
import sys
import tempfile
import zipfile

try:
    from compression import zstd
except ImportError:  # Python < 3.14 has no standard-library zstd.
    zstd = None


REPOSITORY = Path(__file__).resolve().parents[1]
ASSETS = REPOSITORY / "fastboot-template"
PLATFORMS = ("windows", "linux", "macos")
LAUNCHERS = (
    "flash.bat",
    "flash.sh",
    "windows_install_upgrade.bat",
    "windows_install_and_format_data.bat",
    "linux_install_upgrade.sh",
    "linux_install_and_format_data.sh",
    "macos_install_upgrade.sh",
    "macos_install_and_format_data.sh",
)
COMPRESSED_SUFFIXES = {".gz", ".zst", ".7z", ".zip", ".xz"}
UPDATER_SCRIPT = b"""# SwiftFlash hybrid installer: the recovery binary reads fastboot-manifest.txt.
# Recovery preserves user data and ignores desktop-only wipe and reboot steps.
ui_print SwiftFlash hybrid installer
"""
MIB = 1024 * 1024
SPARSE_MAGIC = 0xED26FF3A
SPARSE_BLOCK = 4096
RAW_CHUNK = 0xCAC1
FILL_CHUNK = 0xCAC2
# Bounded RAW chunks keep fastboot's resparsing of large images cheap.
MAX_RAW_BLOCKS = 4096
ZERO_BLOCK = bytes(SPARSE_BLOCK)
TEMPLATE_NOTICE = b"""SWIFTFLASH HYBRID TEMPLATE - NOT A FLASHABLE ROM

This archive intentionally contains no ROM images or third-party fastboot tool.
Build a complete hybrid ZIP with tools/build_hybrid.py --images DIR and fresh
recovery/desktop executables. Both regional image branches must be supplied.
Do not install this template on a phone or treat it as a ready-to-flash ROM.
"""
EMPTY_NOTICE = """SWIFTFLASH EMPTY FLASHER - PUT YOUR IMAGES IN THIS images/ FOLDER

Every file listed below is named in fastboot-manifest.txt. Add each one here
either uncompressed (NAME.img, raw or Android sparse) or zstd-compressed
(NAME.img.zst, for example: zstd -19 --check NAME.img), never both forms.
Edit fastboot-manifest.txt if your ROM ships different images.

Recovery: add the files to the images/ folder inside this ZIP (7-Zip or WinRAR,
compression "Store") and flash the ZIP. Keep META-INF/ at the ZIP root.
Fastboot: extract the ZIP, copy the files into images/, run flash.bat (Windows)
or flash.sh (Linux).

Until every listed file is present, both installers stop before writing.

"""


class BuildError(ValueError):
    """An input cannot form a valid hybrid package."""


@dataclass(frozen=True)
class Entry:
    name: str
    source: Path = None
    data: bytes = None
    mode: int = 0o644
    image: bool = False
    sparse: bool = False
    compress: bool = False


def _valid_name(value):
    return re.fullmatch(r"[A-Za-z0-9_.][A-Za-z0-9_.-]*", value) is not None


def _image_name(value):
    path = PurePosixPath(value)
    if (
        not value.startswith("images/")
        or "\\" in value
        or ":" in value
        or ".." in path.parts
        or any(part in ("", ".", "..") for part in value.split("/"))
        or len(path.parts) < 2
        or path.is_absolute()
    ):
        raise BuildError("image paths must stay inside images/: " + value)
    if path.suffix.lower() in COMPRESSED_SUFFIXES:
        raise BuildError("extract compressed images before hybrid packaging: " + value)
    return path.as_posix()


def manifest_images(source):
    """Validate the shared manifest DSL and return unique image names in order."""
    images = {}
    products = None
    operations = 0
    final_reboot = False
    recovery_partitions = set()
    for number, line in enumerate(source.lstrip("\ufeff").splitlines(), 1):
        try:
            # Match the Rust DSL: only double quotes quote text, and backslashes
            # are literal (image path validation rejects them).
            lexer = shlex.shlex(line, posix=True)
            lexer.whitespace_split = True
            lexer.quotes = '"'
            lexer.escape = ""
            lexer.commenters = "#"
            words = list(lexer)
            if not words:
                continue
            if final_reboot:
                raise BuildError("reboot system must be the final instruction")
            command, args = words[0], words[1:]
            if command == "product":
                if products is not None or operations or not args:
                    raise BuildError("declare product once, before all operations")
                if not all(_valid_name(product) for product in args):
                    raise BuildError("invalid product name")
                products = args
                continue
            if products is None:
                raise BuildError("product must be the first declaration")
            operations += 1
            if command == "set_active" and args in (["a"], ["b"]):
                continue
            if command in ("flash", "flash_region") and len(args) == (2 if command == "flash" else 4):
                partition = args[0]
                if not _valid_name(partition):
                    raise BuildError("invalid partition name")
                base = re.sub(r"_(?:ab|a|b)$", "", partition)
                if not base or partition in (".", ".."):
                    raise BuildError("invalid recovery partition name")
                if base.lower() in ("metadata", "userdata"):
                    raise BuildError("use wipe for metadata/userdata; flashing data partitions is forbidden")
                targets = (base + "_a", base + "_b") if partition.endswith("_ab") else (partition,)
                for target in targets:
                    if target in recovery_partitions:
                        raise BuildError("recovery partition is targeted more than once: " + target)
                    recovery_partitions.add(target)
                if command == "flash_region":
                    if re.fullmatch(r"[A-Z]{2}", args[1]) is None:
                        raise BuildError("flash_region requires a two-letter uppercase country")
                    names = args[2:]
                else:
                    names = args[1:]
                for name in names:
                    images[_image_name(name)] = None
                continue
            if command == "wipe" and args in (["metadata"], ["userdata"]):
                continue
            if command == "reboot" and args in (["fastboot"], ["bootloader"], ["system"]):
                final_reboot = args == ["system"]
                continue
            raise BuildError("unknown instruction or invalid arguments")
        except (ValueError, BuildError) as error:
            raise BuildError("Manifest line {}: {}".format(number, error)) from error
    if products is None:
        raise BuildError("manifest must declare a compatible product")
    if not images:
        raise BuildError("manifest must contain a flash instruction")
    return tuple(images)


def _file(path, label):
    path = Path(path).expanduser()
    if not path.is_file() or path.stat().st_size == 0:
        raise BuildError("{} must be a nonempty file: {}".format(label, path))
    # Readability is checked before the archive or any output directory is made.
    with path.open("rb"):
        pass
    return path.resolve()


def _is_sparse(path):
    with path.open("rb") as source:
        header = source.read(4)
    return len(header) == 4 and struct.unpack("<I", header)[0] == SPARSE_MAGIC


def _sparse_runs(path):
    """Classify each 4 KiB block once as a uniform 32-bit fill or raw data.

    Returns merged [kind, blocks, fill] runs. Only RAW and FILL chunks are
    produced, never DONT_CARE, so every block is written exactly as in the
    raw image on both flashing paths.
    """
    runs = []
    with path.open("rb") as source:
        while True:
            data = source.read(SPARSE_BLOCK * 256)
            if not data:
                break
            for offset in range(0, len(data), SPARSE_BLOCK):
                block = data[offset:offset + SPARSE_BLOCK]
                if block == ZERO_BLOCK:
                    kind, fill = FILL_CHUNK, ZERO_BLOCK[:4]
                elif block == block[:4] * (SPARSE_BLOCK // 4):
                    kind, fill = FILL_CHUNK, block[:4]
                else:
                    kind, fill = RAW_CHUNK, None
                last = runs[-1] if runs else None
                if last and last[0] == kind and last[2] == fill and (kind == FILL_CHUNK or last[1] < MAX_RAW_BLOCKS):
                    last[1] += 1
                else:
                    runs.append([kind, 1, fill])
    return runs


def _payload(entry):
    """Return (length, chunks) of the bytes stored for an image entry."""
    if not entry.sparse:
        size = entry.source.stat().st_size

        def raw():
            with entry.source.open("rb") as source:
                while True:
                    data = source.read(MIB)
                    if not data:
                        return
                    yield data

        return size, raw()
    runs = _sparse_runs(entry.source)
    total_blocks = sum(blocks for _, blocks, _ in runs)
    size = 28 + sum(12 + (blocks * SPARSE_BLOCK if kind == RAW_CHUNK else 4) for kind, blocks, _ in runs)

    def sparse():
        yield struct.pack("<IHHHHIIII", SPARSE_MAGIC, 1, 0, 28, 12, SPARSE_BLOCK, total_blocks, len(runs), 0)
        with entry.source.open("rb") as source:
            for kind, blocks, fill in runs:
                length = blocks * SPARSE_BLOCK
                if kind == FILL_CHUNK:
                    source.seek(length, os.SEEK_CUR)
                    yield struct.pack("<HHII", FILL_CHUNK, 0, blocks, 16) + fill
                    continue
                yield struct.pack("<HHII", RAW_CHUNK, 0, blocks, 12 + length)
                while length:
                    data = source.read(min(length, MIB))
                    if not data:
                        raise BuildError("image changed while packaging: " + str(entry.source))
                    length -= len(data)
                    yield data
            if source.read(1):
                raise BuildError("image changed while packaging: " + str(entry.source))

    return size, sparse()


def _zstd_level(size, requested):
    """Choose the zstd level (1-22) for one image whose stored payload is `size` bytes.

    `requested` is --zstd-level. On the POCO F7 ROM, super.img's 7.2 GiB of
    erofs data took about 5 minutes at level 19 yet came out only ~7% smaller
    than at level 3; the other 37 images total ~0.8 GiB and finish in seconds
    at any level. Decoding speed on the phone barely changes between levels.
    """
    # TODO(human): return the level to use for this image.
    return requested


def _write_image(destination, entry, args, progress):
    size, chunks = _payload(entry)
    compressor = None
    if entry.compress:
        compressor = zstd.ZstdCompressor(options={
            zstd.CompressionParameter.compression_level: _zstd_level(size, args.zstd_level),
            zstd.CompressionParameter.checksum_flag: 1,
            zstd.CompressionParameter.nb_workers: args.zstd_threads or os.cpu_count() or 1,
        })
        compressor.set_pledged_input_size(size)
    done = 0
    reported = -1
    for chunk in chunks:
        destination.write(compressor.compress(chunk) if compressor else chunk)
        done += len(chunk)
        if progress and size >= 64 * MIB and done * 10 // size > reported:
            reported = done * 10 // size
            progress("  {}: {}%".format(entry.name, reported * 10))
    if done != size:
        raise BuildError("image changed while packaging: " + str(entry.source))
    if compressor:
        destination.write(compressor.flush())


def _updater_script(banner):
    if banner is None:
        return UPDATER_SCRIPT
    path = _file(banner, "Banner")
    if path.stat().st_size > 16 * 1024:
        raise BuildError("banner exceeds 16 KiB")
    lines = []
    for line in path.read_text(encoding="utf-8-sig").splitlines():
        line = line.rstrip()
        if any(ord(character) < 32 for character in line):
            raise BuildError("banner lines must not contain control characters")
        lines.append("ui_print " + line if line else "ui_print")
    return UPDATER_SCRIPT + ("\n".join(lines) + "\n").encode("utf-8")


def _inside(path, root):
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def build(args, progress=None):
    """Validate inputs, then atomically create the archive. Return its path."""
    if not args.template and not any(getattr(args, platform + "_binary") is not None for platform in PLATFORMS):
        raise BuildError("a hybrid ROM ZIP requires at least one desktop --windows-binary, --linux-binary, or --macos-binary")
    compress = args.compression == "zstd" and args.images is not None
    if compress and zstd is None:
        raise BuildError("zstd compression requires Python 3.14 or newer (compression.zstd); use --compression none")
    if not 1 <= args.zstd_level <= 22:
        raise BuildError("--zstd-level must be from 1 to 22")
    if args.zstd_threads < 0 or args.sparse_min_mib < 0:
        raise BuildError("--zstd-threads and --sparse-min-mib must not be negative")
    manifest = _file(args.manifest, "Manifest")
    if manifest.stat().st_size > 256 * 1024:
        raise BuildError("manifest exceeds the recovery installer's 256 KiB limit")
    image_names = manifest_images(manifest.read_text(encoding="utf-8-sig"))
    output = Path(args.output).expanduser().absolute()
    output_resolved = output.resolve()
    if output.exists() and not args.force:
        raise BuildError("output already exists; choose another path or use --force: " + str(output))
    if output.exists() and output.is_dir():
        raise BuildError("output must be a ZIP file path")

    entries = {}

    def add(name, source=None, data=None, mode=0o644, **image):
        if name in entries:
            raise BuildError("duplicate archive entry: " + name)
        if source is not None:
            source = _file(source, name)
            if source == output_resolved:
                raise BuildError("output would overwrite a source file: " + str(source))
        entries[name] = Entry(name, source, data, mode, **image)

    add("META-INF/com/google/android/update-binary", args.recovery_binary, mode=0o755)
    add("META-INF/com/google/android/updater-script", data=_updater_script(args.banner))
    add("fastboot-manifest.txt", manifest)
    add("README.md", REPOSITORY / "README.md")
    for launcher in LAUNCHERS:
        add(launcher, ASSETS / launcher, mode=0o755 if launcher.endswith(".sh") else 0o644)
    for platform in PLATFORMS:
        add("bin/{}/README.md".format(platform), ASSETS / "bin" / platform / "README.md")
        binary = getattr(args, platform + "_binary")
        if binary is not None:
            name = "swiftflash-fastboot.exe" if platform == "windows" else "swiftflash-fastboot"
            add("bin/{}/{}".format(platform, name), binary, mode=0o755)

    if args.template:
        add("images/.gitkeep", data=b"")
        add("TEMPLATE-NOT-FLASHABLE.txt", data=TEMPLATE_NOTICE)
    elif args.empty:
        # Users append images later; installers stop until every one exists.
        listing = "".join(name + "\n" for name in image_names)
        add("images/PUT-YOUR-IMAGES-HERE.txt", data=(EMPTY_NOTICE + listing).encode("utf-8"))
    else:
        images = Path(args.images).expanduser().resolve()
        if not images.is_dir():
            raise BuildError("--images must be a directory")
        if _inside(output_resolved, images):
            raise BuildError("output must be outside the image source tree")
        image_entries = []
        for name in image_names:
            relative = PurePosixPath(name).relative_to("images")
            image = images.joinpath(*relative.parts).resolve()
            if not _inside(image, images):
                raise BuildError("image escapes the source directory: " + name)
            image_entries.append((name, _file(image, name)))
        # Images go last so every installer file stays at a 32-bit offset.
        for name, image in image_entries:
            size = image.stat().st_size
            sparse = (
                args.sparse_min_mib > 0
                and size >= args.sparse_min_mib * MIB
                and size % SPARSE_BLOCK == 0
                and not _is_sparse(image)
            )
            add(name + ".zst" if compress else name, image, image=True, sparse=sparse, compress=compress)

    if args.tools is not None:
        tools = Path(args.tools).expanduser().resolve()
        if not tools.is_dir():
            raise BuildError("--tools must contain windows/, linux/, or macos/ tool directories")
        found = False
        for platform in PLATFORMS:
            directory = tools / platform
            if not directory.exists():
                continue
            if directory.is_symlink() or not directory.is_dir():
                raise BuildError("tool platform directories must be real directories: " + str(directory))
            found = True
            executable = "fastboot.exe" if platform == "windows" else "fastboot"
            _file(directory / executable, "{} fastboot tool".format(platform))
            for path in sorted(directory.rglob("*")):
                if path.is_symlink():
                    raise BuildError("tool symlinks are not supported: " + str(path))
                if path.is_dir():
                    continue
                if not path.is_file():
                    raise BuildError("tool entry must be a regular file: " + str(path))
                relative = path.relative_to(directory).as_posix()
                if relative in ("swiftflash-fastboot", "swiftflash-fastboot.exe", "README.md"):
                    raise BuildError("reserved tool filename: " + relative)
                mode = 0o755 if path.name == executable or path.stat().st_mode & stat.S_IXUSR else 0o644
                add("bin/{}/{}".format(platform, relative), path, mode=mode)
        if not found:
            raise BuildError("--tools contains no supported platform directories")
    # Keep the large images after every installer and tool entry.
    entries = dict(sorted(entries.items(), key=lambda item: item[1].image))

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(prefix=".swiftflash-hybrid-", suffix=".zip", dir=output.parent, delete=False) as handle:
            temporary = Path(handle.name)
        with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_STORED, allowZip64=True) as archive:
            for entry in entries.values():
                info = zipfile.ZipInfo(entry.name)
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | entry.mode) << 16
                info.compress_type = zipfile.ZIP_STORED
                if entry.source is not None:
                    info.file_size = entry.source.stat().st_size
                else:
                    info.file_size = len(entry.data)
                # Images always get ZIP64 local headers because their stored
                # size is unknown until written; small installer entries keep
                # plain headers for recovery ZIP readers. Nothing is buffered.
                with archive.open(info, "w", force_zip64=entry.image) as destination:
                    if entry.sparse or entry.compress:
                        _write_image(destination, entry, args, progress)
                    elif entry.source is not None:
                        with entry.source.open("rb") as source:
                            shutil.copyfileobj(source, destination, length=MIB)
                    else:
                        destination.write(entry.data)
                if progress and entry.image:
                    progress("{}: {} -> {}{}".format(
                        entry.name, _size(entry.source.stat().st_size), _size(info.compress_size),
                        " (sparse)" if entry.sparse else "",
                    ))
        temporary.replace(output)
    finally:
        if temporary is not None and temporary.exists():
            temporary.unlink()
    return output


def _size(length):
    return "{:.2f} GiB".format(length / 1024 ** 3) if length >= 1024 ** 3 else "{:.1f} MiB".format(length / MIB)


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    image_source = result.add_mutually_exclusive_group(required=True)
    image_source.add_argument("--images", type=Path, help="directory containing extracted ROM images")
    image_source.add_argument("--template", action="store_true", help="build an explicitly non-flashable template without images")
    image_source.add_argument("--empty", action="store_true", help="build a fillable flasher whose images/ folder you complete later")
    result.add_argument("--recovery-binary", type=Path, required=True, help="freshly compiled Android ARM64 recovery executable")
    for platform in PLATFORMS:
        result.add_argument("--" + platform + "-binary", type=Path, help="compiled desktop flasher for " + platform)
    result.add_argument("--manifest", type=Path, default=ASSETS / "fastboot-manifest.txt")
    result.add_argument("--tools", type=Path, help="optional tool tree with windows/, linux/, and/or macos/ subdirectories")
    result.add_argument("--output", type=Path, required=True)
    result.add_argument("--force", action="store_true", help="replace an existing output archive")
    result.add_argument("--compression", choices=("zstd", "none"), default="zstd", help="store images as NAME.zst (default) or unchanged")
    result.add_argument("--zstd-level", type=int, default=9, help="zstd level 1-22 (default: 9)")
    result.add_argument("--zstd-threads", type=int, default=0, help="zstd worker threads (default: all CPUs)")
    result.add_argument("--sparse-min-mib", type=int, default=512, help="convert raw images at least this large to Android sparse form; 0 disables (default: 512)")
    result.add_argument("--banner", type=Path, help="text file whose lines recovery shows before installing")
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        output = build(args, progress=lambda message: print(message, file=sys.stderr, flush=True))
    except (ValueError, OSError, UnicodeError, zipfile.LargeZipFile) as error:
        print("Hybrid package error: " + str(error), file=sys.stderr)
        return 1
    label = "NON-FLASHABLE TEMPLATE" if args.template else "Empty flasher ZIP" if args.empty else "Hybrid ROM ZIP"
    print("{}: {}".format(label, output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
