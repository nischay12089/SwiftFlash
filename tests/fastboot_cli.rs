//! Exercise the desktop CLI through real child processes, without USB access.
//! The only executable it can launch here is a tiny native fastboot test double.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// `printf hello | zstd -1 --check`: a libzstd frame with a content checksum.
const HELLO_ZST: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x48, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0xa3, 0x6d,
    0x9f, 0x88,
];

const MANIFEST: &str = "\
product onyx
set_active a
flash boot_ab \"images/boot [test] image.img\"
flash_region modemfirmware_ab CN images/cn.img images/ww.img
reboot fastboot
flash super images/super.img
reboot bootloader
wipe metadata
wipe userdata
reboot system
";

// Compile once instead of relying on shell scripts, which would miss Windows
// native process argument handling (and are deliberately rejected by the CLI).
const FAKE_FASTBOOT: &str = r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut log = OpenOptions::new().create(true).append(true)
        .open(env::var_os("FAKE_FASTBOOT_LOG").expect("log path")).unwrap();
    writeln!(log, "{}", args.join("\t")).unwrap();
    drop(log);
    let selected = args.first().map(String::as_str) == Some("-s");
    let command = if selected {
        assert!(args.get(1).is_some());
        &args[2..]
    } else {
        &args[..]
    };
    let state = env::var_os("FAKE_FASTBOOT_STATE").expect("state path");
    match command.first().map(String::as_str) {
        Some("getvar") => {
            assert_eq!(command.len(), 2);
            if env::var("FAKE_FASTBOOT_MISSING_VAR").ok().as_deref()
                == Some(command[1].as_str()) {
                // AOSP may report a failed getvar without failing the process.
                eprintln!("getvar:{} FAILED (remote: unknown variable)", command[1]);
                return;
            }
            match command[1].as_str() {
                "product" => eprintln!("product: {}", env::var("FAKE_FASTBOOT_PRODUCT")
                    .unwrap_or_else(|_| "onyx".into())),
                "is-userspace" => eprintln!("is-userspace: {}",
                    fs::read_to_string(&state).unwrap_or_else(|_| "no".into())),
                variable => panic!("unexpected getvar {variable}"),
            }
            eprintln!("Finished. Total time: 0.001s");
        }
        Some("oem") => {
            assert_eq!(command, ["oem", "hwid"]);
            eprintln!("(bootloader) hwcountry: {}", env::var("FAKE_FASTBOOT_COUNTRY")
                .unwrap_or_else(|_| "IN".into()));
        }
        Some("--set-active=a") => {
            assert_eq!(command.len(), 1);
            eprintln!("Setting current slot to 'a' OKAY");
        }
        Some("flash") => {
            assert_eq!(command.len(), 3);
            assert!(Path::new(&command[2]).is_file(), "image argument was split or corrupted");
            if env::var("FAKE_FASTBOOT_FAIL_PARTITION").ok().as_deref()
                == Some(command[1].as_str()) {
                eprintln!("FAILED (remote: simulated flash failure)");
                std::process::exit(1);
            }
            if let Some(copies) = env::var_os("FAKE_FASTBOOT_COPY_DIR") {
                fs::copy(&command[2], Path::new(&copies).join(&command[1])).unwrap();
            }
            eprintln!("Writing '{}' OKAY", command[1]);
        }
        Some("erase") => {
            assert_eq!(command.len(), 2);
            assert!(matches!(command[1].as_str(), "metadata" | "userdata"));
            eprintln!("Erasing '{}' OKAY", command[1]);
        }
        Some("reboot") => {
            assert!(command.len() <= 2);
            match command.get(1).map(String::as_str) {
                Some("fastboot") => {
                    if env::var_os("FAKE_FASTBOOT_STUCK_MODE").is_none() {
                        fs::write(&state, "yes").unwrap();
                    }
                }
                Some("bootloader") => fs::write(&state, "no").unwrap(),
                None => {}
                target => panic!("unexpected reboot target {target:?}"),
            }
            eprintln!("Rebooting OKAY");
        }
        unexpected => panic!("unexpected fastboot command {unexpected:?}"),
    }
}
"#;

fn fake_fastboot() -> &'static Path {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            // Cargo owns this directory and removes its contents with cargo clean.
            let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("fake fastboot [native]-{}", std::process::id()));
            fs::create_dir_all(&directory).unwrap();
            let source = directory.join("fake_fastboot.rs");
            fs::write(&source, FAKE_FASTBOOT).unwrap();
            let executable =
                directory.join(format!("fake fastboot [native]{}", env::consts::EXE_SUFFIX));
            let compiler = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let mut command = Command::new(compiler);
            command
                .arg("--edition=2021")
                .arg("--crate-name=swiftflash_fake_fastboot")
                .arg(&source)
                .arg("-o")
                .arg(&executable);
            let target = env::var_os("SWIFTFLASH_TEST_RUSTC_TARGET").or_else(|| {
                // Match a GNU-targeted test suite even when rustup's default host
                // toolchain is MSVC. Ordinary native CI needs no override.
                if cfg!(all(
                    target_os = "windows",
                    target_env = "gnu",
                    target_arch = "x86"
                )) {
                    Some("i686-pc-windows-gnu".into())
                } else if cfg!(all(
                    target_os = "windows",
                    target_env = "gnu",
                    target_arch = "x86_64"
                )) {
                    Some("x86_64-pc-windows-gnu".into())
                } else {
                    None
                }
            });
            if let Some(target) = target {
                command.arg("--target").arg(target);
            }
            if let Some(linker) = env::var_os("SWIFTFLASH_TEST_RUSTC_LINKER") {
                let mut option = std::ffi::OsString::from("linker=");
                option.push(linker);
                command.arg("-C").arg(option);
            }
            // Build the helper with the same linker/sysroot flags as Cargo,
            // including toolchains that need explicit native library paths.
            if let Ok(flags) = env::var("CARGO_ENCODED_RUSTFLAGS") {
                command.args(flags.split('\u{1f}').filter(|flag| !flag.is_empty()));
            }
            let output = command
                .output()
                .expect("compile the native fastboot test double with rustc");
            assert!(
                output.status.success(),
                "fake fastboot compilation failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            executable
        })
        .as_path()
}

struct Package {
    root: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

impl Package {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = env::temp_dir().join(format!(
            "swiftflash CLI [ROM package]-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("fastboot-manifest.txt"), MANIFEST).unwrap();
        for name in ["boot [test] image.img", "cn.img", "ww.img", "super.img"] {
            fs::write(root.join("images").join(name), b"tiny raw image fixture").unwrap();
        }
        Self {
            log: root.join("fastboot invocations.txt"),
            state: root.join("userspace state.txt"),
            root,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_swiftflash-fastboot"));
        command
            .arg("--package")
            .arg(&self.root)
            .arg("--fastboot")
            .arg(fake_fastboot());
        self.configure_command(command)
    }

    fn configure_command(&self, mut command: Command) -> Command {
        command
            .env("FAKE_FASTBOOT_LOG", &self.log)
            .env("FAKE_FASTBOOT_STATE", &self.state)
            .stdin(Stdio::null());
        // Each fixture sets its own child environment; parallel tests never
        // mutate the test process's global environment.
        for name in [
            "FAKE_FASTBOOT_PRODUCT",
            "FAKE_FASTBOOT_COUNTRY",
            "FAKE_FASTBOOT_FAIL_PARTITION",
            "FAKE_FASTBOOT_MISSING_VAR",
            "FAKE_FASTBOOT_STUCK_MODE",
            "FAKE_FASTBOOT_COPY_DIR",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn install_packaged_launchers(&self) {
        let platform = host_platform();
        let bin = self.root.join("bin").join(platform);
        fs::create_dir_all(&bin).unwrap();
        let cli = bin.join(format!("swiftflash-fastboot{}", env::consts::EXE_SUFFIX));
        let fastboot = bin.join(format!("fastboot{}", env::consts::EXE_SUFFIX));
        fs::copy(env!("CARGO_BIN_EXE_swiftflash-fastboot"), &cli).unwrap();
        fs::copy(fake_fastboot(), &fastboot).unwrap();
        make_executable(&cli);
        make_executable(&fastboot);

        let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("fastboot-template");
        let alias = if cfg!(windows) {
            "flash.bat"
        } else {
            "flash.sh"
        };
        let suffix = if cfg!(windows) { "bat" } else { "sh" };
        for name in [
            alias.to_owned(),
            format!("{platform}_install_upgrade.{suffix}"),
            format!("{platform}_install_and_format_data.{suffix}"),
        ] {
            let destination = self.root.join(&name);
            fs::copy(template.join(&name), &destination)
                .unwrap_or_else(|error| panic!("cannot copy packaged launcher {name}: {error}"));
            make_executable(&destination);
        }
    }

    fn launcher_command(&self, name: &str) -> Command {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/C", name]);
            command
        } else {
            let mut command = Command::new("sh");
            command.arg(self.root.join(name));
            command
        };
        command.current_dir(&self.root);
        self.configure_command(command)
    }

    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('\t').map(str::to_owned).collect())
            .collect()
    }

    fn operations(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter_map(|args| {
                let command = if args.first().map(String::as_str) == Some("-s") {
                    args[2..].to_vec()
                } else {
                    args
                };
                if matches!(command.first().map(String::as_str), Some("getvar" | "oem")) {
                    None
                } else {
                    Some(command)
                }
            })
            .collect()
    }

    fn image(&self, name: &str) -> String {
        fs::canonicalize(self.root.join("images").join(name))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn remove_image(&self, name: &str) {
        fs::remove_file(self.root.join("images").join(name)).unwrap();
    }
}

fn host_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        panic!("packaged launchers do not support this host operating system")
    }
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn output(command: &mut Command) -> Output {
    command.output().expect("run swiftflash-fastboot CLI")
}

fn diagnostics(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", diagnostics(output));
}

fn assert_failure(output: &Output) {
    assert!(!output.status.success(), "{}", diagnostics(output));
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn assert_no_confirmation_prompt(output: &Output) {
    let text = diagnostics(output);
    assert!(!text.contains("Flash this device?"), "{text}");
    assert!(!text.contains("Type WIPE"), "{text}");
    assert!(text.contains("Flashing complete"), "{text}");
}

#[test]
fn packaged_flash_alias_runs_upgrade_with_no_arguments_or_confirmation() {
    let package = Package::new();
    package.install_packaged_launchers();
    let name = if cfg!(windows) {
        "flash.bat"
    } else {
        "flash.sh"
    };
    let result = output(&mut package.launcher_command(name));
    assert_success(&result);
    assert_no_confirmation_prompt(&result);
    let operations = package.operations();
    assert_eq!(operations.len(), 7, "{operations:?}");
    assert_eq!(operations[0], words(&["--set-active=a"]));
    assert_eq!(operations[1][..2], ["flash", "boot_ab"]);
    assert_eq!(operations[2][..2], ["flash", "modemfirmware_ab"]);
    assert_eq!(operations[4][..2], ["flash", "super"]);
    assert!(operations.iter().all(|args| args[0] != "erase"));
    assert_eq!(operations.last().unwrap(), &words(&["reboot"]));
}

#[test]
fn packaged_wipe_launcher_runs_with_no_arguments_or_confirmation() {
    let package = Package::new();
    package.install_packaged_launchers();
    let suffix = if cfg!(windows) { "bat" } else { "sh" };
    let name = format!("{}_install_and_format_data.{suffix}", host_platform());
    let result = output(&mut package.launcher_command(&name));
    assert_success(&result);
    assert_no_confirmation_prompt(&result);
    let operations = package.operations();
    assert_eq!(operations.len(), 9, "{operations:?}");
    assert_eq!(
        &operations[5..],
        &[
            words(&["reboot", "bootloader"]),
            words(&["erase", "metadata"]),
            words(&["erase", "userdata"]),
            words(&["reboot"]),
        ]
    );
}

#[test]
fn upgrade_preserves_vendor_partitions_region_order_and_native_arguments() {
    let package = Package::new();
    let result = output(package.command().arg("--yes"));
    assert_success(&result);
    assert_eq!(
        package.operations(),
        vec![
            words(&["--set-active=a"]),
            vec![
                "flash".into(),
                "boot_ab".into(),
                package.image("boot [test] image.img")
            ],
            vec![
                "flash".into(),
                "modemfirmware_ab".into(),
                package.image("ww.img")
            ],
            words(&["reboot", "fastboot"]),
            vec!["flash".into(), "super".into(), package.image("super.img")],
            words(&["reboot", "bootloader"]),
            words(&["reboot"]),
        ]
    );
    let calls = package.calls();
    assert_eq!(calls[0], words(&["getvar", "product"]));
    assert!(calls
        .iter()
        .all(|args| args.first().map(String::as_str) != Some("-s")));
    assert!(calls
        .iter()
        .any(|args| args == &words(&["getvar", "is-userspace"])));
    assert!(!calls.iter().any(|args| args
        .iter()
        .any(|arg| matches!(arg.as_str(), "serialno" | "devices"))));
    assert!(diagnostics(&result).contains("Flashing complete"));
}

#[test]
fn wipe_erases_both_data_partitions_after_returning_to_bootloader() {
    let package = Package::new();
    let result = output(package.command().args(["--mode", "wipe", "--yes"]));
    assert_success(&result);
    let operations = package.operations();
    assert_eq!(
        &operations[5..],
        &[
            words(&["reboot", "bootloader"]),
            words(&["erase", "metadata"]),
            words(&["erase", "userdata"]),
            words(&["reboot"]),
        ]
    );
}

#[test]
fn china_uses_only_its_selected_regional_image() {
    let package = Package::new();
    package.remove_image("ww.img");
    let result = output(
        package
            .command()
            .arg("--yes")
            .env("FAKE_FASTBOOT_COUNTRY", "CN"),
    );
    assert_success(&result);
    assert_eq!(
        package.operations()[2],
        vec![
            "flash".into(),
            "modemfirmware_ab".into(),
            package.image("cn.img")
        ]
    );
}

#[test]
fn missing_late_image_blocks_every_mutation() {
    let package = Package::new();
    package.remove_image("super.img");
    let result = output(package.command().args(["--mode", "wipe", "--yes"]));
    assert_failure(&result);
    assert!(diagnostics(&result).contains("super.img"));
    assert!(package.operations().is_empty(), "{:?}", package.calls());
}

#[test]
fn non_china_device_requires_worldwide_image_before_mutating() {
    let package = Package::new();
    package.remove_image("ww.img");
    let result = output(
        package
            .command()
            .arg("--yes")
            .env("FAKE_FASTBOOT_COUNTRY", "INDIA"),
    );
    assert_failure(&result);
    assert!(diagnostics(&result).contains("ww.img"));
    assert!(package.operations().is_empty(), "{:?}", package.calls());
}

#[test]
fn product_mismatch_blocks_every_mutation() {
    let package = Package::new();
    let result = output(
        package
            .command()
            .arg("--yes")
            .env("FAKE_FASTBOOT_PRODUCT", "other-device"),
    );
    assert_failure(&result);
    assert!(diagnostics(&result).contains("other-device"));
    assert!(package.operations().is_empty(), "{:?}", package.calls());
}

#[test]
fn failed_getvar_with_successful_exit_is_rejected() {
    let package = Package::new();
    let result = output(
        package
            .command()
            .arg("--yes")
            .env("FAKE_FASTBOOT_MISSING_VAR", "product"),
    );
    assert_failure(&result);
    assert!(package.operations().is_empty(), "{:?}", package.calls());
}

#[test]
fn failed_flash_stops_before_all_subsequent_operations() {
    let package = Package::new();
    let result = output(
        package
            .command()
            .args(["--mode", "wipe", "--yes"])
            .env("FAKE_FASTBOOT_FAIL_PARTITION", "modemfirmware_ab"),
    );
    assert_failure(&result);
    assert!(diagnostics(&result).contains("simulated flash failure"));
    let operations = package.operations();
    assert_eq!(operations.len(), 3, "{operations:?}");
    assert_eq!(
        operations.last().unwrap()[..2],
        ["flash", "modemfirmware_ab"]
    );
}

#[test]
fn absent_confirmation_cancels_before_mutating() {
    let package = Package::new();
    let result = output(&mut package.command());
    assert_success(&result);
    assert!(diagnostics(&result).contains("Cancelled"));
    assert!(package.operations().is_empty(), "{:?}", package.calls());
}

#[test]
fn dry_run_validates_the_plan_without_starting_fastboot() {
    let package = Package::new();
    let result = output(package.command().args(["--dry-run", "--mode", "wipe"]));
    assert_success(&result);
    assert!(!package.log.exists(), "dry-run launched fastboot");
    let text = diagnostics(&result);
    assert!(
        text.contains("erase metadata") && text.contains("erase userdata"),
        "{text}"
    );
    assert!(text.contains("cn.img") && text.contains("ww.img"), "{text}");
}

#[test]
fn zstd_images_are_staged_flashed_expanded_and_removed() {
    let package = Package::new();
    package.remove_image("super.img");
    fs::write(package.root.join("images/super.img.zst"), HELLO_ZST).unwrap();
    let copies = package.root.join("flashed copies");
    fs::create_dir(&copies).unwrap();
    let result = output(
        package
            .command()
            .arg("--yes")
            .env("FAKE_FASTBOOT_COPY_DIR", &copies),
    );
    assert_success(&result);
    assert_eq!(fs::read(copies.join("super")).unwrap(), b"hello");
    let flash = package
        .operations()
        .into_iter()
        .find(|args| args.starts_with(&words(&["flash", "super"])))
        .unwrap();
    assert!(flash[2].contains(".swiftflash-staging"), "{flash:?}");
    assert!(!package.root.join(".swiftflash-staging").exists());
}

#[test]
fn corrupt_zstd_image_blocks_every_mutation_and_dry_run() {
    let package = Package::new();
    package.remove_image("super.img");
    fs::write(
        package.root.join("images/super.img.zst"),
        &HELLO_ZST[..HELLO_ZST.len() - 2],
    )
    .unwrap();
    assert_failure(&output(package.command().arg("--dry-run")));
    assert!(!package.log.exists(), "dry-run launched fastboot");
    let result = output(package.command().arg("--yes"));
    assert_failure(&result);
    assert!(diagnostics(&result).contains("super.img.zst"));
    assert!(package.operations().is_empty(), "{:?}", package.calls());
    assert!(!package.root.join(".swiftflash-staging").exists());
}

#[test]
fn optional_serial_is_forwarded_without_enumeration_or_serialno_checks() {
    let package = Package::new();
    let result = output(
        package
            .command()
            .args(["--yes", "--serial", "operator-selected"]),
    );
    assert_success(&result);
    assert_eq!(package.operations().len(), 7);
    let calls = package.calls();
    assert!(calls
        .iter()
        .all(|args| args.get(..2) == Some(&words(&["-s", "operator-selected"])[..])));
    assert!(!calls.iter().any(|args| args
        .iter()
        .any(|arg| matches!(arg.as_str(), "serialno" | "devices"))));
}

#[test]
fn unverified_fastbootd_transition_blocks_later_writes() {
    let package = Package::new();
    let result = output(
        package
            .command()
            .args(["--yes", "--timeout", "1"])
            .env("FAKE_FASTBOOT_STUCK_MODE", "1"),
    );
    assert_failure(&result);
    assert!(diagnostics(&result).contains("fastbootd"));
    let operations = package.operations();
    assert_eq!(operations.len(), 4, "{operations:?}");
    assert_eq!(operations.last().unwrap(), &words(&["reboot", "fastboot"]));
}

#[test]
fn no_reboot_keeps_required_mode_transitions() {
    let package = Package::new();
    let result = output(package.command().args(["--yes", "--no-reboot"]));
    assert_success(&result);
    let reboots: Vec<_> = package
        .operations()
        .into_iter()
        .filter(|args| args.first().map(AsRef::as_ref) == Some(OsStr::new("reboot")))
        .collect();
    assert_eq!(
        reboots,
        [
            words(&["reboot", "fastboot"]),
            words(&["reboot", "bootloader"])
        ]
    );
}
