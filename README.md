# SwiftFlash

One ROM ZIP for Android recovery and desktop fastboot, using one shared set of
zstd-compressed images.

## Flash with fastboot

Use the complete ROM package for your unlocked device. It already contains the
images, desktop flasher, and compatible fastboot tool.

1. Extract the ZIP completely and connect your phone in bootloader fastboot mode.
2. **Windows:** double-click `flash.bat`. **Linux or macOS:** run `./flash.sh`.

Flashing starts after the automatic package and device checks, with no questions
or command-line options. The standard launcher skips data erasure.

Images are stored as `images/NAME.img.zst`. Before the phone is changed, the
flasher verifies and decompresses them into `.swiftflash-staging` inside the
package folder and removes that folder afterwards. Keep free disk space roughly
equal to the expanded images (about 8 GB for the POCO F7 ROM), or pass
`--staging-dir PATH` to use another drive.

For a clean installation, run `windows_install_and_format_data.bat`,
`linux_install_and_format_data.sh`, or `macos_install_and_format_data.sh`.
**These launchers erase `metadata` and `userdata`, including user data, without asking for confirmation.**

## Flash in recovery

Select the same complete ZIP in your compatible Android recovery and install it.
Recovery streams each `.zst` image straight to its partition, flashes both slots
for `_ab` partitions, and preserves user data. When the manifest selects a slot
(`set_active a`), recovery activates it after flashing with `bootctl`, because a
fastboot `super.img` usually populates only that slot; a recovery without
`bootctl` is rejected before anything is written. Rebooting is left to
recovery's controls.

Archives labeled **hybrid template** contain no ROM images and are not flashable
ROMs. Their `TEMPLATE-NOT-FLASHABLE.txt` marker identifies them.

## For ROM maintainers: build the package

The rest of this document describes package creation and advanced controls.
End users of a complete ROM package do not need to edit a manifest or copy tools.

Build a fresh Android ARM64 recovery executable and the desktop executable for
each supported host. Python 3.14 or newer packages them into one ZIP (its
standard-library `compression.zstd` writes the images; Python 3.9+ works with
`--compression none`). For Linux:

```sh
cross build --locked --release --target aarch64-unknown-linux-musl --bin swiftflash
cargo build --locked --release --no-default-features --bin swiftflash-fastboot
python3 tools/build_hybrid.py \
  --images /path/to/extracted-ROM/images \
  --recovery-binary target/aarch64-unknown-linux-musl/release/swiftflash \
  --linux-binary target/release/swiftflash-fastboot \
  --tools /path/to/compatible-host-tools \
  --banner examples/onyx-hyperos-3.0.3-in/banner.txt \
  --output dist/ROM-hybrid.zip
```

| Option | Effect |
| --- | --- |
| `--compression zstd\|none` | Store images as `NAME.zst` with frame checksums (default) or unchanged. |
| `--zstd-level N` | 1-22 (default 9). Level 19 is a few percent smaller and much slower to build; decoding speed barely changes. |
| `--zstd-threads N` | Compression workers (default: every CPU). |
| `--sparse-min-mib N` | Convert raw images at least this large (default 512) to Android sparse form using only RAW and FILL chunks, so empty space is never stored, transferred, or staged. `0` disables it. |
| `--banner FILE` | Lines recovery shows before installing. |

The desktop executable decodes zstd in pure Rust, so it needs no C compiler on
any host. For a Linux build that runs on any distribution, target
`x86_64-unknown-linux-musl` for a static executable.

Add `--windows-binary PATH` and/or `--macos-binary PATH` for additional hosts.
A full hybrid ZIP requires at least one desktop executable. The macOS option
accepts one architecture-specific or universal executable. The recovery binary
always targets Android ARM64, independently of the build computer.

Distribute a compatible fastboot tool for every included desktop host using
`--tools DIR`. Its source layout is:

```text
DIR/
  windows/fastboot.exe        # Include its required DLLs here
  linux/fastboot
  macos/fastboot
```

Supply only the platform directories needed. Tool and dependency files must be
nonempty and must not be symlinks. The reference uses literal `_ab` partition
names; use its vendor-compatible fastboot when those names require vendor
support. Follow the tools' licenses. SwiftFlash does not download ROM data or
third-party tools. The builder permits omitting `--tools` for development, but a
package distributed for direct flashing must include the appropriate tools.
Fastboot is never selected implicitly from `PATH`.

The builder defaults to `fastboot-template/fastboot-manifest.txt`; use
`--manifest PATH` for another reviewed device plan. `--images` points directly
to the image directory. Each unique referenced image is included once, including
both regional branches. The builder rejects missing or empty inputs, compressed
image names in the manifest, traversal and symlink escapes, repeated expanded
recovery targets, and output inside the image source tree. Uncompressed `.img`,
`.bin`, `.mbn`, `.elf`, and extensionless firmware files are supported.

Images use streaming stored ZIP entries with ZIP64 headers and are written after
every installer file, which keeps plain ZIP headers for the files recovery
extracts itself. Output is written atomically; replacing an existing ZIP
requires `--force`.

`examples/onyx-hyperos-3.0.3-in/` holds the manifest and banner used for the
POCO F7 HyperOS 3.0.3 IN package.

## For ROM maintainers: ZIP layout and installation behavior

```text
ROM-hybrid.zip
  META-INF/com/google/android/
    update-binary
    updater-script
  fastboot-manifest.txt
  images/                     # Shared images, stored once as NAME.img.zst
  bin/windows/                # Desktop executable, fastboot, and required DLLs
  bin/linux/                  # Desktop executable and fastboot
  bin/macos/                  # Desktop executable and fastboot
  flash.bat
  flash.sh
  windows_install_upgrade.bat
  windows_install_and_format_data.bat
  linux_install_upgrade.sh
  linux_install_and_format_data.sh
  macos_install_upgrade.sh
  macos_install_and_format_data.sh
  README.md
```

There is no enclosing ROM directory inside the ZIP. `update-binary` reads the
root manifest directly; recovery shows the `ui_print` lines of `updater-script`
as a banner and ignores everything else in it.

The manifest always names the uncompressed image (`images/super.img`). Both
installers accept either that file or `images/super.img.zst`; a package holding
both forms of one image is rejected as ambiguous.

| Installation path | Behavior |
| --- | --- |
| Recovery ZIP | Validate product, regional selection, every image (fully decoded, checksums included), target partitions, and `bootctl` before writing. Expand `_ab` into `_a` and `_b`; preserve data; activate the manifest's slot last. Ignore wipe and reboot instructions. |
| `flash.bat` / `flash.sh` / upgrade launchers | Verify and decompress `.zst` images, use literal partition names, select the manifest's slot, follow bootloader/fastbootd transitions, and skip data-erasure instructions. Start without an interactive confirmation. |
| Format-data launchers | Run the desktop flash plan and erase `metadata` and `userdata` without an interactive confirmation. |

Both installation paths read raw, Android sparse, and zstd-compressed images.
Recovery-only `updater-script` packages additionally retain Gzip support. The
existing recovery API remains
`update-binary <api-version> <output-fd> <rom.zip>`. Neither installation path
queries or compares device serial numbers.

The supplied manifest follows the POCO F7 (`onyx`) reference package's partition
names, slot `a`, firmware order, regional modem images, `ofox.img` recovery, and
`super.img` fastbootd transition. Review and adapt it for each ROM and device;
the template does not establish compatibility with other devices or verify image
authenticity.

## For ROM maintainers: manifest

The manifest is an ordered list. Blank lines and `#` comments are allowed;
double quotes support paths containing spaces. Image paths use `/`, start with
`images/`, and stay inside the package. Shell commands are not supported.

| Instruction | Behavior |
| --- | --- |
| `product onyx` | Required first declaration; list compatible device codenames on this line. |
| `set_active a` | Select slot `a` or `b`: first during desktop flashing, after all writes in recovery. |
| `flash abl_ab images/abl.img` | Flash the literal target on desktop; expand `_ab` into both slots in recovery. |
| `flash_region modemfirmware_ab CN images/modemfirmware.img images/modemfirmware_ww.img` | Select the first image for `HwCountry=CN`, otherwise the second. |
| `reboot fastboot` | Enter fastbootd and wait for reconnect on desktop. |
| `reboot bootloader` | Return to bootloader fastboot and wait for reconnect on desktop. |
| `wipe metadata` / `wipe userdata` | Erase only in desktop wipe mode. |
| `reboot system` | Final desktop reboot; must be the last instruction. |

Desktop flashing reads `HwCountry` using `oem hwid`. A missing selected regional
image stops execution before flashing; the CN image is never substituted for a
missing WW image. Package creation and device-free previews require both images.
Product, image, and mode checks remain enabled with the direct launchers, and a
failing fastboot command stops the sequence.

## Advanced: preview and command line

A device-free preview is optional:

```powershell
.\flash.bat --dry-run
```

```sh
./flash.sh --dry-run
```

Dry-run validates all referenced files, including both regional branches and a
full decode of every `.zst` image, without starting fastboot or accessing a phone.

The desktop executable itself remains interactive when invoked directly.
The supplied launchers add `--yes` automatically, resolve their own package
directory, forward advanced arguments, and preserve the executable's exit status.

```text
swiftflash-fastboot [--package PATH] [options]

--package PATH          Package root (default: current directory)
--manifest PATH         Manifest relative to the package root or absolute
                        (default: fastboot-manifest.txt)
--fastboot PATH         Explicit fastboot executable file
--serial SERIAL         Optional selector forwarded directly to fastboot
--mode upgrade|wipe     Skip or include wipe instructions (default: upgrade)
--dry-run               Validate and print the plan without invoking fastboot
--yes                   Skip confirmation when invoking the executable directly
--no-reboot             Skip only the final system reboot
--timeout SECONDS       Command/reconnect timeout (default: 120, maximum: 3600)
--flash-timeout SECONDS Per-image timeout (default: 1800, maximum: 7200)
--staging-dir PATH      Where .zst images are decompressed (default: package)
--help                  Show usage
```

`--no-reboot` keeps intermediate fastbootd/bootloader transitions. The optional
`--serial` selector is passed to fastboot without device enumeration or a serial
comparison.

## For ROM maintainers: tests and CI artifacts

```sh
cargo test --locked --no-default-features
cargo test --locked --bin swiftflash
python3 -m unittest discover -s tests -p test_build_hybrid.py -v  # zstd tests need 3.14
```

Tests use temporary files and fake fastboot processes; no phone is flashed.

CI builds recovery ARM64 plus Windows x86_64, Linux x86_64, macOS ARM64, and
macOS x86_64 desktop executables. It combines fresh binaries into
`swiftflash-hybrid-template-<platform>` archives. These intentionally contain
**no ROM images or third-party fastboot tool** and include
`TEMPLATE-NOT-FLASHABLE.txt`. Use the builder with real images and compatible
host tools to produce a complete distribution. `--template` creates the same
explicit template form locally. `--empty` instead builds a fillable flasher:
the same installers and tools, no `TEMPLATE-NOT-FLASHABLE.txt` marker, and an
`images/PUT-YOUR-IMAGES-HERE.txt` listing every image the manifest expects. The `fastboot-template/` source can also serve
as a standalone extracted desktop package, and CI retains the
`update-binary-aarch64` artifact for the existing recovery ZIP workflow.
