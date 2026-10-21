use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::manifest::Operation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryFlash {
    /// The relative ZIP entry, shared with the desktop fastboot manifest.
    pub image: PathBuf,
    /// The concrete partition name, after expanding an `_ab` suffix.
    pub partition: String,
    /// An absolute block-device path supplied by the recovery resolver.
    pub target: PathBuf,
}

/// Expand the manifest's literal `_ab` shorthand into both physical slots.
/// Recovery does not rely on a synthetic `boot_ab` block-device alias.
pub fn partition_targets(partition: &str) -> Result<Vec<String>, String> {
    if partition.is_empty()
        || matches!(partition, "." | "..")
        || partition.starts_with('-')
        || !partition
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
    {
        return Err(format!("Invalid recovery partition name: {partition:?}"));
    }

    let base = partition
        .strip_suffix("_ab")
        .or_else(|| partition.strip_suffix("_a"))
        .or_else(|| partition.strip_suffix("_b"))
        .unwrap_or(partition);
    if base.is_empty() {
        return Err(format!(
            "Recovery partition has no base name: {partition:?}"
        ));
    }
    if base.eq_ignore_ascii_case("userdata") || base.eq_ignore_ascii_case("metadata") {
        return Err(format!(
            "Recovery flashing of {partition} is forbidden; format data manually in recovery if needed."
        ));
    }

    match partition.strip_suffix("_ab") {
        Some(base) => Ok(vec![format!("{base}_a"), format!("{base}_b")]),
        None => Ok(vec![partition.to_string()]),
    }
}

/// Build a complete recovery plan without reading images or writing targets.
/// The resolver must check target availability and return canonical absolute
/// paths, so different partition aliases resolving to one block are rejected.
pub fn build_plan(
    operations: &[Operation],
    mut resolver: impl FnMut(&str) -> Result<PathBuf, String>,
) -> Result<Vec<RecoveryFlash>, String> {
    let mut plan = Vec::new();
    let mut partitions = HashSet::new();
    let mut targets = HashSet::new();

    for operation in operations {
        let Operation::Flash { partition, image } = operation else {
            return Err("The recovery flash plan accepts only image flash operations.".into());
        };
        validate_zip_image(image)?;
        for partition in partition_targets(partition)? {
            if !partitions.insert(partition.clone()) {
                return Err(format!(
                    "Recovery partition {partition} is targeted more than once."
                ));
            }
            let target = resolver(&partition)
                .map_err(|error| format!("Recovery target {partition} is unavailable: {error}"))?;
            if !target.is_absolute()
                || target
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(format!(
                    "Recovery target {partition} must resolve to an absolute path without traversal: {}",
                    target.display()
                ));
            }
            if !targets.insert(target.clone()) {
                return Err(format!(
                    "Recovery target {} is targeted more than once (partition {partition}).",
                    target.display()
                ));
            }
            plan.push(RecoveryFlash {
                image: image.clone(),
                partition,
                target,
            });
        }
    }
    Ok(plan)
}

fn validate_zip_image(image: &Path) -> Result<(), String> {
    let Some(entry) = image.to_str() else {
        return Err("Recovery images must use UTF-8 ZIP entry names.".into());
    };
    if !entry.starts_with("images/")
        || entry.contains(['\\', ':'])
        || entry
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || image
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "Recovery image must be a relative images/ ZIP entry without traversal: {entry}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RebootTarget;

    fn flash(partition: &str, image: &str) -> Operation {
        Operation::Flash {
            partition: partition.into(),
            image: image.into(),
        }
    }

    fn target(partition: &str) -> PathBuf {
        std::env::temp_dir()
            .join("swiftflash-recovery-plan-tests")
            .join(partition)
    }

    #[test]
    fn expands_ab_partitions_in_a_then_b_order() {
        assert_eq!(partition_targets("boot_ab").unwrap(), ["boot_a", "boot_b"]);
        assert_eq!(
            partition_targets("vendor-boot_ab").unwrap(),
            ["vendor-boot_a", "vendor-boot_b"]
        );
    }

    #[test]
    fn preserves_unsuffixed_and_explicit_slot_names() {
        for partition in ["super", "vendor_boot_a", "dtbo_b", "multi-image-oem"] {
            assert_eq!(partition_targets(partition).unwrap(), [partition]);
        }
    }

    #[test]
    fn rejects_data_partitions_including_slot_variants() {
        for partition in [
            "userdata",
            "userdata_ab",
            "userdata_a",
            "userdata_b",
            "metadata",
            "metadata_ab",
            "metadata_a",
            "metadata_b",
            "USERDATA",
            "METADATA_ab",
        ] {
            assert!(partition_targets(partition).is_err(), "{partition}");
        }
    }

    #[test]
    fn rejects_unsafe_or_empty_partition_names() {
        for partition in [
            "", ".", "..", "_ab", "_a", "_b", "../boot", "boot/a", "boot\\a", "-boot", "boot a",
        ] {
            assert!(partition_targets(partition).is_err(), "{partition:?}");
        }
    }

    #[test]
    fn retains_selected_regional_image_and_manifest_order() {
        let operations = [
            flash("xbl_ab", "images/xbl.img"),
            flash("cust", "images/cust_cn.img"),
            flash("super", "images/super.img"),
        ];
        let mut resolved = Vec::new();
        let plan = build_plan(&operations, |partition| {
            resolved.push(partition.to_string());
            Ok(target(partition))
        })
        .unwrap();
        assert_eq!(resolved, ["xbl_a", "xbl_b", "cust", "super"]);
        assert_eq!(plan.len(), 4);
        assert_eq!(plan[0].image, Path::new("images/xbl.img"));
        assert_eq!(plan[1].image, Path::new("images/xbl.img"));
        assert_eq!(plan[2].image, Path::new("images/cust_cn.img"));
        assert_eq!(plan[2].partition, "cust");
        assert_eq!(plan[3].target, target("super"));
    }

    #[test]
    fn requires_both_physical_slots_before_returning_any_plan() {
        let operations = [flash("boot_ab", "images/boot.img")];
        let mut resolved = Vec::new();
        let result = build_plan(&operations, |partition| {
            resolved.push(partition.to_string());
            if partition == "boot_b" {
                Err("missing block device".into())
            } else {
                Ok(target(partition))
            }
        });
        assert!(result.unwrap_err().contains("boot_b"));
        assert_eq!(resolved, ["boot_a", "boot_b"]);
    }

    #[test]
    fn rejects_duplicate_partitions_even_with_the_same_image() {
        let operations = [
            flash("boot_ab", "images/boot.img"),
            flash("boot_a", "images/boot.img"),
        ];
        assert!(build_plan(&operations, |partition| Ok(target(partition)))
            .unwrap_err()
            .contains("more than once"));
    }

    #[test]
    fn rejects_distinct_partition_aliases_resolving_to_one_target() {
        let operations = [
            flash("boot", "images/boot.img"),
            flash("kernel", "images/kernel.img"),
        ];
        assert!(build_plan(&operations, |_| Ok(target("same-block")))
            .unwrap_err()
            .contains("more than once"));
    }

    #[test]
    fn rejects_nonflash_operations_without_resolving_any_target() {
        for operation in [
            Operation::SetActive("a".into()),
            Operation::Reboot(RebootTarget::Fastboot),
            Operation::Reboot(RebootTarget::System),
            Operation::Erase("userdata".into()),
        ] {
            let mut resolved = false;
            assert!(build_plan(&[operation], |_| {
                resolved = true;
                Ok(target("unused"))
            })
            .is_err());
            assert!(!resolved);
        }
    }

    #[test]
    fn rejects_images_outside_relative_zip_image_entries() {
        for image in [
            "boot.img",
            "../images/boot.img",
            "images/../boot.img",
            "images/./boot.img",
            "images//boot.img",
            "images/",
            "/images/boot.img",
            "C:/images/boot.img",
            "images\\boot.img",
        ] {
            assert!(
                build_plan(&[flash("boot", image)], |partition| Ok(target(partition))).is_err(),
                "{image}"
            );
        }
    }

    #[test]
    fn rejects_nonabsolute_or_traversing_resolved_targets() {
        let operations = [flash("boot", "images/boot.img")];
        assert!(build_plan(&operations, |_| Ok("relative/boot".into())).is_err());
        assert!(build_plan(&operations, |_| Ok(target("boot").join("..").join("other"))).is_err());
    }
}
