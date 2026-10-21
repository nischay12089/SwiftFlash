use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Upgrade,
    Wipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebootTarget {
    Fastboot,
    Bootloader,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    SetActive(String),
    Flash { partition: String, image: PathBuf },
    Reboot(RebootTarget),
    Erase(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Operation(Operation),
    FlashRegion {
        partition: String,
        country: String,
        matching: PathBuf,
        other: PathBuf,
    },
}

#[derive(Debug)]
pub struct Manifest {
    pub products: Vec<String>,
    steps: Vec<Step>,
}

/// Packages may store any manifest image zstd-compressed as `<image>.zst`.
/// The manifest always names the uncompressed image; exactly one form exists.
pub const ZSTD_SUFFIX: &str = ".zst";

pub fn compressed_name(image: &Path) -> PathBuf {
    let mut name = image.as_os_str().to_os_string();
    name.push(ZSTD_SUFFIX);
    PathBuf::from(name)
}

pub fn is_compressed(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

impl Manifest {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut products = Vec::new();
        let mut steps = Vec::new();
        let mut final_reboot = false;
        for (index, line) in source.trim_start_matches('\u{feff}').lines().enumerate() {
            let fail = |message: &str| format!("Manifest line {}: {message}", index + 1);
            let words = tokenize(line).map_err(|e| fail(&e))?;
            if words.is_empty() {
                continue;
            }
            if final_reboot {
                return Err(fail("reboot system must be the final instruction"));
            }
            let args = &words[1..];
            let operation = match words[0].as_str() {
                "product" => {
                    if !products.is_empty() || !steps.is_empty() || args.is_empty() {
                        return Err(fail(
                            "declare product <codename> [codename ...] once, before all operations",
                        ));
                    }
                    for product in args {
                        validate_name(product).map_err(|e| fail(&e))?;
                    }
                    products = args.to_vec();
                    continue;
                }
                "set_active" if args.len() == 1 && matches!(args[0].as_str(), "a" | "b") => {
                    Operation::SetActive(args[0].clone())
                }
                "flash" if args.len() == 2 => {
                    validate_flash_partition(&args[0]).map_err(|e| fail(&e))?;
                    Operation::Flash {
                        partition: args[0].clone(),
                        image: image_path(&args[1]).map_err(|e| fail(&e))?,
                    }
                }
                "flash_region" if args.len() == 4 => {
                    validate_flash_partition(&args[0]).map_err(|e| fail(&e))?;
                    if args[1].len() != 2 || !args[1].bytes().all(|b| b.is_ascii_uppercase()) {
                        return Err(fail("flash_region country must be two uppercase letters"));
                    }
                    steps.push(Step::FlashRegion {
                        partition: args[0].clone(),
                        country: args[1].clone(),
                        matching: image_path(&args[2]).map_err(|e| fail(&e))?,
                        other: image_path(&args[3]).map_err(|e| fail(&e))?,
                    });
                    continue;
                }
                "reboot" if args.len() == 1 => Operation::Reboot(match args[0].as_str() {
                    "fastboot" => RebootTarget::Fastboot,
                    "bootloader" => RebootTarget::Bootloader,
                    "system" => {
                        final_reboot = true;
                        RebootTarget::System
                    }
                    _ => {
                        return Err(fail(
                            "reboot target must be fastboot, bootloader, or system",
                        ))
                    }
                }),
                "wipe"
                    if args.len() == 1 && matches!(args[0].as_str(), "metadata" | "userdata") =>
                {
                    Operation::Erase(args[0].clone())
                }
                _ => return Err(fail("unknown instruction or invalid arguments")),
            };
            steps.push(Step::Operation(operation));
        }
        if products.is_empty() {
            return Err("Manifest must declare at least one compatible product".into());
        }
        if !steps.iter().any(|s| {
            matches!(
                s,
                Step::FlashRegion { .. } | Step::Operation(Operation::Flash { .. })
            )
        }) {
            return Err("Manifest must contain at least one flash instruction".into());
        }
        Ok(Self { products, steps })
    }

    /// The slot the desktop flow leaves active; recovery activates it too,
    /// because a fastboot super image usually populates only that slot.
    #[allow(dead_code)] // Used by the recovery executable, not the desktop target.
    pub fn active_slot(&self) -> Option<&str> {
        self.steps.iter().rev().find_map(|step| match step {
            Step::Operation(Operation::SetActive(slot)) => Some(slot.as_str()),
            _ => None,
        })
    }

    /// Every stored image either regional branch could use, in manifest order.
    pub fn package_images(&self, root: &Path) -> Result<Vec<PathBuf>, String> {
        let root = fs::canonicalize(root).map_err(|e| format!("Cannot open package: {e}"))?;
        let mut images = Vec::new();
        for step in &self.steps {
            let names = match step {
                Step::FlashRegion {
                    matching, other, ..
                } => vec![matching, other],
                Step::Operation(Operation::Flash { image, .. }) => vec![image],
                Step::Operation(_) => continue,
            };
            for name in names {
                let image = resolve_image(&root, name)?;
                if !images.contains(&image) {
                    images.push(image);
                }
            }
        }
        Ok(images)
    }

    pub fn needs_country(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::FlashRegion { .. }))
    }

    /// Select shared images for direct recovery writes, retaining their order.
    #[allow(dead_code)] // Used by the recovery executable, not the desktop target.
    pub fn recovery_operations(&self, country: Option<&str>) -> Result<Vec<Operation>, String> {
        let mut operations = Vec::new();
        for step in &self.steps {
            match step {
                Step::Operation(operation @ Operation::Flash { .. }) => {
                    operations.push(operation.clone())
                }
                Step::FlashRegion {
                    partition,
                    country: expected,
                    matching,
                    other,
                } => {
                    let country = country.ok_or(
                        "Cannot select regional firmware: hardware country is unavailable",
                    )?;
                    operations.push(Operation::Flash {
                        partition: partition.clone(),
                        image: if country == expected {
                            matching.clone()
                        } else {
                            other.clone()
                        },
                    });
                }
                _ => {}
            }
        }
        Ok(operations)
    }

    /// Resolve every selected image before executing even the first command.
    pub fn plan(
        &self,
        root: &Path,
        mode: Mode,
        country: Option<&str>,
        no_reboot: bool,
    ) -> Result<Vec<Operation>, String> {
        let root = fs::canonicalize(root).map_err(|e| format!("Cannot open package: {e}"))?;
        let mut plan = Vec::new();
        for step in &self.steps {
            let operation = match step {
                Step::Operation(Operation::Flash { partition, image }) => Operation::Flash {
                    partition: partition.clone(),
                    image: resolve_image(&root, image)?,
                },
                Step::FlashRegion {
                    partition,
                    country: matching_country,
                    matching,
                    other,
                } => {
                    let country = country
                        .ok_or("Cannot select regional firmware: device country is unavailable")?;
                    Operation::Flash {
                        partition: partition.clone(),
                        image: resolve_image(
                            &root,
                            if country == matching_country {
                                matching
                            } else {
                                other
                            },
                        )?,
                    }
                }
                Step::Operation(Operation::Erase(_)) if mode == Mode::Upgrade => continue,
                Step::Operation(Operation::Reboot(RebootTarget::System)) if no_reboot => continue,
                Step::Operation(operation) => operation.clone(),
            };
            plan.push(operation);
        }
        Ok(plan)
    }

    /// Device-free preview checks both regional branches; it never calls fastboot.
    pub fn preview(&self, root: &Path, mode: Mode, no_reboot: bool) -> Result<Vec<String>, String> {
        let root = fs::canonicalize(root).map_err(|e| format!("Cannot open package: {e}"))?;
        let mut lines = Vec::new();
        for step in &self.steps {
            match step {
                Step::FlashRegion {
                    partition,
                    country,
                    matching,
                    other,
                } => {
                    let matching = resolve_image(&root, matching)?;
                    let other = resolve_image(&root, other)?;
                    lines.push(format!("if HwCountry={country}: flash {partition} \"{}\"; otherwise: flash {partition} \"{}\"", matching.display(), other.display()));
                }
                Step::Operation(Operation::Flash { partition, image }) => {
                    let image = resolve_image(&root, image)?;
                    lines.push(format!("flash {partition} \"{}\"", image.display()));
                }
                Step::Operation(Operation::Erase(_)) if mode == Mode::Upgrade => {}
                Step::Operation(Operation::Reboot(RebootTarget::System)) if no_reboot => {}
                Step::Operation(op) => lines.push(describe(op)),
            }
        }
        Ok(lines)
    }
}

pub fn describe(operation: &Operation) -> String {
    match operation {
        Operation::SetActive(slot) => format!("set_active {slot}"),
        Operation::Flash { partition, image } => {
            format!("flash {partition} \"{}\"", image.display())
        }
        Operation::Erase(partition) => format!("erase {partition}"),
        Operation::Reboot(target) => match target {
            RebootTarget::Fastboot => "reboot fastboot",
            RebootTarget::Bootloader => "reboot bootloader",
            RebootTarget::System => "reboot",
        }
        .into(),
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
    {
        Err(format!("Invalid product or partition name: {name}"))
    } else {
        Ok(())
    }
}

fn validate_flash_partition(name: &str) -> Result<(), String> {
    validate_name(name)?;
    let base = name
        .strip_suffix("_ab")
        .or_else(|| name.strip_suffix("_a"))
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name);
    if base.eq_ignore_ascii_case("userdata") || base.eq_ignore_ascii_case("metadata") {
        return Err("Use wipe userdata / wipe metadata for data erasure; flashing these partitions is forbidden".into());
    }
    Ok(())
}

fn image_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.contains(['\\', ':'])
        || !value.starts_with("images/")
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "Image must be a relative images/ path without traversal: {value}"
        ));
    }
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "gz" | "zst" | "7z" | "zip" | "xz") {
        return Err(format!(
            "Name the uncompressed image in the manifest (a package may store it as NAME.zst): {value}"
        ));
    }
    Ok(path.to_path_buf())
}

/// Find the stored form of a manifest image: the image itself or `<image>.zst`.
fn resolve_image(root: &Path, image: &Path) -> Result<PathBuf, String> {
    let compressed = compressed_name(image);
    let present = |name: &Path| fs::symlink_metadata(root.join(name)).is_ok();
    match (present(image), present(&compressed)) {
        (true, true) => Err(format!(
            "Both {} and {} exist; keep only one form of each image",
            image.display(),
            compressed.display()
        )),
        (false, true) => resolve_stored(root, &compressed),
        _ => resolve_stored(root, image),
    }
}

fn resolve_stored(root: &Path, image: &Path) -> Result<PathBuf, String> {
    let path = fs::canonicalize(root.join(image))
        .map_err(|e| format!("Required image {} is unavailable: {e}", image.display()))?;
    if !path.starts_with(root) {
        return Err(format!(
            "Image escapes the package directory: {}",
            image.display()
        ));
    }
    let metadata =
        fs::metadata(&path).map_err(|e| format!("Cannot inspect {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "Image must be a nonempty file: {}",
            image.display()
        ));
    }
    // Open now so unreadable inputs fail before changing the active slot or flashing.
    fs::File::open(&path).map_err(|e| format!("Cannot read {}: {e}", image.display()))?;
    Ok(path)
}

fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '#' if !quoted => break,
            c if c.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut token));
                    started = false;
                }
            }
            c => {
                token.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("Unclosed double quote".into());
    }
    if started {
        tokens.push(token);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Package(PathBuf);
    impl Package {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "swiftflash-plan-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("images")).unwrap();
            for image in [
                "boot.img",
                "super.img",
                "cn.img",
                "ww.img",
                "boot space.img",
            ] {
                fs::write(path.join("images").join(image), b"fixture").unwrap();
            }
            Self(path)
        }
    }
    impl Drop for Package {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const SOURCE: &str = "product onyx\nset_active a\nflash boot_ab images/boot.img\nflash_region modemfirmware_ab CN images/cn.img images/ww.img\nreboot fastboot\nflash super images/super.img\nreboot bootloader\nwipe metadata\nwipe userdata\nreboot system";

    #[test]
    fn preserves_order_region_and_modes() {
        let package = Package::new();
        let manifest = Manifest::parse(SOURCE).unwrap();
        let upgrade = manifest
            .plan(&package.0, Mode::Upgrade, Some("CN"), false)
            .unwrap();
        assert_eq!(upgrade.len(), 7);
        assert!(!upgrade.iter().any(|s| matches!(s, Operation::Erase(_))));
        assert!(matches!(&upgrade[2], Operation::Flash{ image, .. } if image.ends_with("cn.img")));
        assert_eq!(upgrade[3], Operation::Reboot(RebootTarget::Fastboot));
        assert!(matches!(&upgrade[4], Operation::Flash{ partition, .. } if partition == "super"));
        let wipe = manifest
            .plan(&package.0, Mode::Wipe, Some("IN"), true)
            .unwrap();
        assert!(matches!(&wipe[2], Operation::Flash{ image, .. } if image.ends_with("ww.img")));
        assert_eq!(
            wipe[6..],
            [
                Operation::Erase("metadata".into()),
                Operation::Erase("userdata".into())
            ]
        );
    }

    #[test]
    fn missing_late_image_blocks_entire_plan() {
        let package = Package::new();
        fs::remove_file(package.0.join("images/super.img")).unwrap();
        let manifest = Manifest::parse(SOURCE).unwrap();
        assert!(manifest
            .plan(&package.0, Mode::Wipe, Some("CN"), false)
            .unwrap_err()
            .contains("super.img"));
    }

    #[test]
    fn dry_run_checks_both_branches_actual_plan_only_selected() {
        let package = Package::new();
        fs::remove_file(package.0.join("images/ww.img")).unwrap();
        let manifest = Manifest::parse(SOURCE).unwrap();
        assert!(manifest
            .preview(&package.0, Mode::Upgrade, false)
            .unwrap_err()
            .contains("ww.img"));
        assert!(manifest
            .plan(&package.0, Mode::Upgrade, Some("CN"), false)
            .is_ok());
        assert!(manifest
            .plan(&package.0, Mode::Upgrade, None, false)
            .is_err());
    }

    #[test]
    fn rejects_unsafe_and_invalid_instructions() {
        for source in [
            "flash boot images/boot.img",
            "product onyx\nunknown boot",
            "product onyx\nflash --slot images/boot.img",
            "product onyx\nflash boot ../boot.img",
            "product onyx\nflash boot images/../boot.img",
            "product onyx\nflash userdata images/boot.img",
            "product onyx\nflash metadata_ab images/boot.img",
            "product onyx\nflash boot images/boot.img.gz",
            "product onyx\nset_active c",
            "product onyx\nflash boot images/boot.img\nreboot system\nwipe userdata",
            "product onyx\nflash boot images/boot.img\nproduct other",
            "product onyx\nflash boot \"images/boot.img",
        ] {
            assert!(Manifest::parse(source).is_err(), "Accepted {source}");
        }
    }

    #[test]
    fn accepts_bom_comments_quotes_and_multiple_products() {
        let package = Package::new();
        let manifest = Manifest::parse("\u{feff}# package\r\nproduct onyx other\r\nflash boot_ab \"images/boot space.img\" # comment\r\n").unwrap();
        assert_eq!(manifest.products, ["onyx", "other"]);
        assert!(manifest
            .plan(&package.0, Mode::Upgrade, None, false)
            .is_ok());
    }

    #[test]
    fn resolves_zstd_stored_images_and_rejects_both_forms() {
        let package = Package::new();
        fs::rename(
            package.0.join("images/super.img"),
            package.0.join("images/super.img.zst"),
        )
        .unwrap();
        let manifest = Manifest::parse(SOURCE).unwrap();
        let plan = manifest
            .plan(&package.0, Mode::Upgrade, Some("CN"), false)
            .unwrap();
        assert!(
            matches!(&plan[4], Operation::Flash { image, .. } if image.ends_with("super.img.zst"))
        );
        let images = manifest.package_images(&package.0).unwrap();
        assert_eq!(images.len(), 4);
        assert!(images.iter().any(|image| is_compressed(image)));
        fs::write(package.0.join("images/super.img"), b"fixture").unwrap();
        assert!(manifest
            .plan(&package.0, Mode::Upgrade, Some("CN"), false)
            .unwrap_err()
            .contains("Both"));
    }

    #[test]
    fn reports_the_last_requested_active_slot() {
        assert_eq!(Manifest::parse(SOURCE).unwrap().active_slot(), Some("a"));
        let manifest = Manifest::parse("product onyx\nflash boot images/boot.img").unwrap();
        assert_eq!(manifest.active_slot(), None);
        let manifest =
            Manifest::parse("product onyx\nset_active a\nflash boot images/boot.img\nset_active b")
                .unwrap();
        assert_eq!(manifest.active_slot(), Some("b"));
    }

    #[test]
    fn rejects_empty_image() {
        let package = Package::new();
        fs::write(package.0.join("images/boot.img"), []).unwrap();
        assert!(Manifest::parse(SOURCE)
            .unwrap()
            .plan(&package.0, Mode::Upgrade, Some("CN"), false)
            .unwrap_err()
            .contains("nonempty"));
    }
}
