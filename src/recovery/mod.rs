pub mod image;
mod plan;

use crate::manifest::{Manifest, ZSTD_SUFFIX};
use image::{expand_image, validate_image, ImageFormat};
use plan::{build_plan, RecoveryFlash};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use zip::ZipArchive;

const MANIFEST: &str = "fastboot-manifest.txt";
const UPDATER_SCRIPT: &str = "META-INF/com/google/android/updater-script";
const BOOTCTL: &str = "/system/bin/bootctl";

pub fn ui_print(pipe: &mut impl Write, message: &str) {
    let _ = writeln!(pipe, "ui_print {message}\nui_print");
    let _ = pipe.flush();
}

fn ui_command(pipe: &mut impl Write, command: &str) {
    let _ = writeln!(pipe, "{command}");
    let _ = pipe.flush();
}

pub fn install(zip_path: &Path, pipe: &mut impl Write) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| format!("Cannot open ROM ZIP: {e}"))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("Invalid ROM ZIP: {e}"))?;
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("Cannot inspect ZIP entry: {e}"))?;
        if !names.insert(entry.name().to_owned()) {
            return Err(format!("Duplicate ZIP entry: {}", entry.name()));
        }
    }
    if names.contains(MANIFEST) {
        let source = read_text(&mut archive, MANIFEST)?;
        let manifest = Manifest::parse(&source)?;
        if names.contains(UPDATER_SCRIPT) {
            print_banner(&read_text(&mut archive, UPDATER_SCRIPT)?, pipe);
        }
        verify_product(&manifest.products)?;
        let country = if manifest.needs_country() {
            Some(hardware_country()?)
        } else {
            None
        };
        let operations = manifest.recovery_operations(country.as_deref())?;
        let mut plan = build_plan(&operations, resolve_partition)?;
        let formats = resolve_entries(&mut plan, &names)?;
        let slot = manifest.active_slot();
        if slot.is_some() {
            // Fail before the first write if the final slot switch cannot happen.
            check_bootctl_with(run_bootctl)?;
        }
        ui_print(pipe, "Hybrid package: recovery installation");
        ui_print(
            pipe,
            &match slot {
                Some(slot) => format!(
                    "Keeping user data. Both slots will be flashed, then slot {slot} activated."
                ),
                None => "Keeping user data and the active slot. Both slots will be flashed.".into(),
            },
        );
        ui_print(
            pipe,
            "Checking every selected image and partition before flashing...",
        );
        let prepared = prepare(&mut archive, plan, Some(&formats))?;
        flash_prepared(&mut archive, prepared, pipe)?;
        if let Some(slot) = slot {
            ui_print(pipe, &format!("Activating slot {slot}..."));
            activate_slot_with(slot, run_bootctl).map_err(|error| {
                format!(
                    "All images were written, but {error}. Select slot {} in your recovery's Reboot menu before rebooting.",
                    slot.to_ascii_uppercase()
                )
            })?;
        }
    } else {
        install_legacy(&mut archive, pipe)?;
    }
    ui_print(
        pipe,
        "Flashing complete. Reboot using your recovery's controls.",
    );
    Ok(())
}

fn read_text<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Result<String, String> {
    let mut entry = archive
        .by_name(name)
        .map_err(|e| format!("Cannot read {name}: {e}"))?;
    if entry.size() > 256 * 1024 {
        return Err(format!("Installer script {name} exceeds 256 KiB"));
    }
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|e| format!("Cannot decode {name}: {e}"))?;
    Ok(text)
}

/// Show the package's `ui_print` banner lines. Other updater-script lines are
/// ignored: in hybrid packages the manifest alone defines what is flashed.
fn print_banner(script: &str, pipe: &mut impl Write) {
    for line in script.lines() {
        let line = line.trim_end();
        if line == "ui_print" {
            ui_print(pipe, "");
        } else if let Some(text) = line.strip_prefix("ui_print ") {
            ui_print(pipe, text);
        }
    }
}

/// Point each planned image at its stored ZIP entry: the manifest's image
/// itself, or the zstd-compressed `<image>.zst`. Storing both is ambiguous.
fn resolve_entries(
    plan: &mut [RecoveryFlash],
    names: &HashSet<String>,
) -> Result<HashMap<PathBuf, ImageFormat>, String> {
    let mut formats = HashMap::new();
    for flash in plan {
        let image = flash
            .image
            .to_str()
            .ok_or("Image path is not valid UTF-8")?;
        let compressed = format!("{image}{ZSTD_SUFFIX}");
        let (entry, format) = match (names.contains(image), names.contains(&compressed)) {
            (true, true) => {
                return Err(format!(
                    "Both {image} and {compressed} exist; the package must store one form"
                ))
            }
            (false, true) => (PathBuf::from(compressed), ImageFormat::Zstd),
            _ => (flash.image.clone(), ImageFormat::Raw),
        };
        formats.insert(entry.clone(), format);
        flash.image = entry;
    }
    Ok(formats)
}

fn run_bootctl(args: &[&str]) -> Result<String, String> {
    let program = if Path::new(BOOTCTL).exists() {
        BOOTCTL
    } else {
        "bootctl"
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run bootctl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "bootctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn check_bootctl_with(
    mut run: impl FnMut(&[&str]) -> Result<String, String>,
) -> Result<(), String> {
    let slots = run(&["get-number-slots"]).map_err(|error| {
        format!("This package activates a slot after flashing, but {error}. Use the fastboot launchers or a recovery that provides bootctl.")
    })?;
    if slots.parse::<u32>().map_or(true, |count| count < 2) {
        return Err(format!(
            "This package requires an A/B device, but bootctl reports {slots:?} slots"
        ));
    }
    Ok(())
}

fn activate_slot_with(
    slot: &str,
    mut run: impl FnMut(&[&str]) -> Result<String, String>,
) -> Result<(), String> {
    let index = match slot {
        "a" => "0",
        "b" => "1",
        _ => return Err(format!("slot {slot:?} is not a or b")),
    };
    run(&["set-active-boot-slot", index]).map(|_| ())
}

fn property(name: &str) -> Result<String, String> {
    let output = Command::new("getprop")
        .arg(name)
        .output()
        .map_err(|e| format!("Cannot read Android property {name}: {e}"))?;
    if !output.status.success() {
        return Err(format!("getprop {name} failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn verify_product(products: &[String]) -> Result<(), String> {
    verify_product_with(products, property)
}

fn verify_product_with(
    products: &[String],
    mut read: impl FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    let mut observed = Vec::new();
    for key in ["ro.product.device", "ro.product.vendor.device"] {
        let value = read(key)?;
        if products.contains(&value) {
            return Ok(());
        }
        if !value.is_empty() {
            observed.push(value);
        }
    }
    Err(format!(
        "Device product mismatch: found [{}], expected [{}]",
        observed.join(", "),
        products.join(", ")
    ))
}

fn hardware_country() -> Result<String, String> {
    hardware_country_with(property)
}

fn hardware_country_with(
    mut read: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    for key in ["ro.boot.hwc", "ro.boot.hwcountry"] {
        let value = read(key)?;
        if !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Ok(value.to_ascii_uppercase());
        }
    }
    Err("Hardware country is unavailable; cannot safely choose regional modem firmware".into())
}

fn resolve_partition(name: &str) -> Result<PathBuf, String> {
    for base in ["/dev/block/by-name", "/dev/block/bootdevice/by-name"] {
        let path = Path::new(base).join(name);
        if path.exists() {
            return validate_block_path(&path);
        }
    }
    Err(format!("Required recovery partition {name} is unavailable"))
}

fn validate_block_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || !path.starts_with("/dev/block")
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!(
            "Invalid recovery block-device path: {}",
            path.display()
        ));
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("Cannot resolve {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let metadata = fs::metadata(&canonical)
            .map_err(|e| format!("Cannot inspect {}: {e}", path.display()))?;
        if !metadata.file_type().is_block_device() {
            return Err(format!(
                "Recovery target is not a block device: {}",
                path.display()
            ));
        }
        Ok(canonical)
    }
    #[cfg(not(unix))]
    {
        let _ = canonical;
        Err("Recovery installation requires Android/Linux block devices".into())
    }
}

struct PreparedFlash {
    specification: RecoveryFlash,
    output: File,
    format: ImageFormat,
    size: u64,
}

fn format_for_path(path: &Path) -> Result<ImageFormat, String> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("gz") => Ok(ImageFormat::Gzip),
        Some("zst") => Ok(ImageFormat::Zstd),
        Some("xz" | "zip" | "7z") => Err(format!(
            "Unsupported recovery image format: {}",
            path.display()
        )),
        _ => Ok(ImageFormat::Raw),
    }
}

fn prepare<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    plan: Vec<RecoveryFlash>,
    formats: Option<&HashMap<PathBuf, ImageFormat>>,
) -> Result<Vec<PreparedFlash>, String> {
    let mut sizes = HashMap::new();
    let mut prepared = Vec::new();
    for specification in plan {
        let image = specification
            .image
            .to_str()
            .ok_or("Image path is not valid UTF-8")?;
        let format = match formats.and_then(|formats| formats.get(&specification.image)) {
            Some(format) => *format,
            None => format_for_path(&specification.image)?,
        };
        let size = if let Some(size) = sizes.get(image) {
            *size
        } else {
            let entry = archive
                .by_name(image)
                .map_err(|e| format!("Required image {image} is unavailable: {e}"))?;
            if entry.is_dir() || entry.size() == 0 {
                return Err(format!("Image {image} is empty or not a file"));
            }
            let size =
                validate_image(entry, format).map_err(|e| format!("Invalid image {image}: {e}"))?;
            if size == 0 {
                return Err(format!("Image {image} expands to zero bytes"));
            }
            sizes.insert(image.to_owned(), size);
            size
        };
        let mut output = OpenOptions::new()
            .write(true)
            .open(&specification.target)
            .map_err(|e| format!("Cannot open partition {}: {e}", specification.partition))?;
        let capacity = output.seek(SeekFrom::End(0)).map_err(|e| {
            format!(
                "Cannot determine capacity of {}: {e}",
                specification.partition
            )
        })?;
        if capacity < size {
            return Err(format!(
                "Image {image} expands to {size} bytes, exceeding {} capacity ({capacity})",
                specification.partition
            ));
        }
        output
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("Cannot seek {}: {e}", specification.partition))?;
        prepared.push(PreparedFlash {
            specification,
            output,
            format,
            size,
        });
    }
    if prepared.is_empty() {
        return Err("No recovery partitions to flash".into());
    }
    Ok(prepared)
}

/// Reports overall progress while an image streams from the ZIP. Images are
/// weighted by expanded size; within one, progress follows bytes read.
struct ProgressReader<'a, R, W: Write> {
    inner: R,
    pipe: &'a mut W,
    read: u64,
    length: u64,
    start: f64,
    span: f64,
    reported: u32,
}

impl<R: Read, W: Write> Read for ProgressReader<'_, R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.read += count as u64;
        let fraction =
            self.start + self.span * (self.read as f64 / self.length.max(1) as f64).min(1.0);
        let permille = (fraction * 1000.0) as u32;
        if permille > self.reported {
            self.reported = permille;
            ui_command(self.pipe, &format!("set_progress {fraction:.3}"));
        }
        Ok(count)
    }
}

fn flash_prepared<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    mut plan: Vec<PreparedFlash>,
    pipe: &mut impl Write,
) -> Result<(), String> {
    let total = plan.len();
    let total_bytes: u64 = plan.iter().map(|flash| flash.size).sum();
    let mut done_bytes = 0_u64;
    for (index, flash) in plan.iter_mut().enumerate() {
        let name = flash
            .specification
            .image
            .to_str()
            .ok_or("Image path is not valid UTF-8")?;
        ui_print(
            pipe,
            &format!(
                "[{}/{}] Flashing {}...",
                index + 1,
                total,
                flash.specification.partition
            ),
        );
        let entry = archive
            .by_name(name)
            .map_err(|e| format!("Cannot reopen image {name}: {e}"))?;
        let length = entry.size();
        let source = ProgressReader {
            inner: entry,
            pipe: &mut *pipe,
            read: 0,
            length,
            start: done_bytes as f64 / total_bytes as f64,
            span: flash.size as f64 / total_bytes as f64,
            reported: 0,
        };
        let written = expand_image(source, &mut flash.output, flash.format)
            .map_err(|e| format!("Flashing {} failed: {e}", flash.specification.partition))?;
        if written != flash.size {
            return Err(format!("Image {name} changed size after preflight"));
        }
        flash
            .output
            .sync_all()
            .map_err(|e| format!("Cannot sync {}: {e}", flash.specification.partition))?;
        done_bytes += flash.size;
        ui_command(
            pipe,
            &format!("set_progress {:.6}", done_bytes as f64 / total_bytes as f64),
        );
    }
    Ok(())
}

fn install_legacy<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    pipe: &mut impl Write,
) -> Result<(), String> {
    let source = read_text(archive, UPDATER_SCRIPT)?;
    let mut plan = Vec::new();
    let mut formats = HashMap::new();
    let mut targets = HashSet::new();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        let error = || format!("Invalid updater-script instruction at line {}", index + 1);
        match words[0] {
            "ui_print" => ui_print(pipe, line.strip_prefix("ui_print ").unwrap_or("")),
            "verify_device" if words.len() >= 2 => verify_product(
                &words[1..]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>(),
            )?,
            "show_progress" if words.len() == 3 => {}
            "set_progress" if words.len() == 2 => {}
            "package_flash_partition" if words.len() == 4 => {
                let format = match words[1] {
                    "raw" => ImageFormat::Raw,
                    "gzip" => ImageFormat::Gzip,
                    "zstd" => ImageFormat::Zstd,
                    _ => return Err(error()),
                };
                let image = PathBuf::from(words[2]);
                if !image.starts_with("images")
                    || image
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)))
                {
                    return Err(error());
                }
                let target = validate_block_path(Path::new(words[3]))?;
                if !targets.insert(target.clone()) {
                    return Err(format!("Duplicate recovery target: {}", target.display()));
                }
                if let Some(previous) = formats.insert(image.clone(), format) {
                    if previous != format {
                        return Err(format!("Conflicting formats for {}", image.display()));
                    }
                }
                plan.push(RecoveryFlash {
                    image,
                    partition: words[3].to_owned(),
                    target,
                });
            }
            _ => return Err(error()),
        }
    }
    let prepared = prepare(archive, plan, Some(&formats))?;
    flash_prepared(archive, prepared, pipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zip::write::FileOptions;

    struct Targets(PathBuf);
    impl Targets {
        fn new(capacity: usize) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "swiftflash-recovery-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            for name in ["boot_a", "boot_b", "super"] {
                fs::write(root.join(name), vec![b'X'; capacity]).unwrap();
            }
            Self(root)
        }
        fn plan(&self) -> Vec<RecoveryFlash> {
            let manifest = Manifest::parse("product onyx\nset_active a\nflash boot_ab images/boot.img\nreboot fastboot\nflash super images/super.img\nreboot bootloader\nwipe userdata\nreboot system").unwrap();
            let operations = manifest.recovery_operations(None).unwrap();
            build_plan(&operations, |partition| {
                fs::canonicalize(self.0.join(partition)).map_err(|e| e.to_string())
            })
            .unwrap()
        }
        fn assert_untouched(&self, capacity: usize) {
            for name in ["boot_a", "boot_b", "super"] {
                assert_eq!(fs::read(self.0.join(name)).unwrap(), vec![b'X'; capacity]);
            }
        }
    }
    impl Drop for Targets {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in entries {
            zip.start_file(
                *name,
                FileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(contents).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn shared_manifest_flashes_both_slots_without_data_or_slot_mutation() {
        let targets = Targets::new(32);
        let mut zip = ZipArchive::new(Cursor::new(archive(&[
            ("images/boot.img", b"BOOT"),
            ("images/super.img", b"SUPER"),
        ])))
        .unwrap();
        let prepared = prepare(&mut zip, targets.plan(), None).unwrap();
        targets.assert_untouched(32);
        flash_prepared(&mut zip, prepared, &mut Vec::new()).unwrap();
        for name in ["boot_a", "boot_b"] {
            assert_eq!(&fs::read(targets.0.join(name)).unwrap()[..4], b"BOOT");
        }
        assert_eq!(&fs::read(targets.0.join("super")).unwrap()[..5], b"SUPER");
    }

    #[test]
    fn missing_late_zip_image_prevents_all_writes() {
        let targets = Targets::new(32);
        let mut zip =
            ZipArchive::new(Cursor::new(archive(&[("images/boot.img", b"BOOT")]))).unwrap();
        assert!(prepare(&mut zip, targets.plan(), None).is_err());
        targets.assert_untouched(32);
    }

    #[test]
    fn oversized_image_prevents_all_writes() {
        let targets = Targets::new(4);
        let mut zip = ZipArchive::new(Cursor::new(archive(&[
            ("images/boot.img", b"BOOT"),
            ("images/super.img", b"TOO LARGE"),
        ])))
        .unwrap();
        assert!(prepare(&mut zip, targets.plan(), None)
            .err()
            .unwrap()
            .contains("capacity"));
        targets.assert_untouched(4);
    }

    #[test]
    fn bad_zip_checksum_prevents_all_writes() {
        let targets = Targets::new(32);
        let mut bytes = archive(&[
            ("images/boot.img", b"BOOT"),
            ("images/super.img", b"UNIQUE_SUPER_CONTENT"),
        ]);
        let start = bytes
            .windows(20)
            .position(|window| window == b"UNIQUE_SUPER_CONTENT")
            .unwrap();
        bytes[start] ^= 0x40;
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert!(prepare(&mut zip, targets.plan(), None).is_err());
        targets.assert_untouched(32);
    }

    #[test]
    fn zstd_entries_are_resolved_verified_and_flashed_expanded() {
        let targets = Targets::new(32);
        let compressed = zstd::stream::encode_all(&b"SUPER"[..], 3).unwrap();
        let bytes = archive(&[
            ("images/boot.img", b"BOOT"),
            ("images/super.img.zst", &compressed),
        ]);
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names: HashSet<String> = zip.file_names().map(str::to_owned).collect();
        let mut plan = targets.plan();
        let formats = resolve_entries(&mut plan, &names).unwrap();
        assert_eq!(plan[2].image, Path::new("images/super.img.zst"));
        assert_eq!(
            formats[Path::new("images/super.img.zst")],
            ImageFormat::Zstd
        );
        assert_eq!(formats[Path::new("images/boot.img")], ImageFormat::Raw);
        let prepared = prepare(&mut zip, plan, Some(&formats)).unwrap();
        let mut pipe = Vec::new();
        flash_prepared(&mut zip, prepared, &mut pipe).unwrap();
        assert_eq!(&fs::read(targets.0.join("super")).unwrap()[..5], b"SUPER");
        let output = String::from_utf8(pipe).unwrap();
        assert!(output.trim_end().ends_with("set_progress 1.000000"));
    }

    #[test]
    fn storing_both_image_forms_is_rejected() {
        let targets = Targets::new(32);
        let names: HashSet<String> = [
            "images/boot.img",
            "images/super.img",
            "images/super.img.zst",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(resolve_entries(&mut targets.plan(), &names)
            .unwrap_err()
            .contains("Both"));
    }

    #[test]
    fn slot_activation_checks_bootctl_first_and_uses_slot_indexes() {
        let mut calls = Vec::new();
        check_bootctl_with(|args| {
            calls.push(args.join(" "));
            Ok("2".into())
        })
        .unwrap();
        activate_slot_with("a", |args| {
            calls.push(args.join(" "));
            Ok(String::new())
        })
        .unwrap();
        activate_slot_with("b", |args| {
            calls.push(args.join(" "));
            Ok(String::new())
        })
        .unwrap();
        assert_eq!(
            calls,
            [
                "get-number-slots",
                "set-active-boot-slot 0",
                "set-active-boot-slot 1"
            ]
        );
        assert!(
            check_bootctl_with(|_| Err("cannot run bootctl: missing".into()))
                .unwrap_err()
                .contains("bootctl")
        );
        assert!(check_bootctl_with(|_| Ok("1".into())).is_err());
        assert!(activate_slot_with("c", |_| Ok(String::new())).is_err());
    }

    #[test]
    fn banner_prints_only_ui_print_lines() {
        let mut pipe = Vec::new();
        print_banner(
            "# comment\nui_print Device : onyx\nui_print\nverify_device onyx\nui_print(\"edify\");\n",
            &mut pipe,
        );
        assert_eq!(
            String::from_utf8(pipe).unwrap(),
            "ui_print Device : onyx\nui_print\nui_print \nui_print\n"
        );
    }

    #[test]
    fn property_checks_never_request_serial_numbers() {
        let mut requested = Vec::new();
        verify_product_with(&["onyx".into()], |key| {
            requested.push(key.to_owned());
            Ok(if key == "ro.product.vendor.device" {
                "onyx"
            } else {
                ""
            }
            .into())
        })
        .unwrap();
        assert_eq!(requested, ["ro.product.device", "ro.product.vendor.device"]);
        requested.clear();
        assert_eq!(
            hardware_country_with(|key| {
                requested.push(key.to_owned());
                Ok("CN".into())
            })
            .unwrap(),
            "CN"
        );
        assert_eq!(requested, ["ro.boot.hwc"]);
        assert!(verify_product_with(&["onyx".into()], |_| Ok("other".into())).is_err());
        assert!(hardware_country_with(|_| Ok(String::new())).is_err());
    }
}
