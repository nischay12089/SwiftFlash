#[path = "../fastboot/manifest.rs"]
mod manifest;
#[path = "../fastboot/runner.rs"]
mod runner;
#[path = "../fastboot/stage.rs"]
mod stage;

use manifest::{is_compressed, Manifest, Mode};
use runner::{resolve_fastboot, Fastboot};
use stage::Staging;
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "SwiftFlasher — desktop fastboot flasher

Usage: swiftflash-fastboot [OPTIONS]

  --package PATH    Extracted ROM package (default: current directory)
  --manifest PATH   Flash plan, relative to package or absolute
                    (default: fastboot-manifest.txt)
  --mode MODE       upgrade (default) or wipe (erases userdata and metadata)
  --fastboot PATH   Explicit fastboot executable (default: bin/<platform>/fastboot)
  --serial SERIAL   Optional fastboot selector (no serial-number checking)
  --dry-run         Validate images and print plan without invoking fastboot
                    Both regional branches must exist for this device-free check
  --yes             Confirm flashing noninteractively, including erasure in wipe mode
  --no-reboot       Skip the final system reboot; retain fastbootd/bootloader transitions
  --timeout SECONDS Command and reconnect timeout (default: 120; range: 1–3600)
  --flash-timeout SECONDS
                    Per-image flash timeout (default: 1800; range: 1–7200)
  --staging-dir PATH
                    Where .zst images are decompressed before flashing
                    (default: the package directory; removed afterwards)
  -h, --help        Show help
  -V, --version     Show version

Uses the package's exact partition names. Images may be raw, Android sparse, or
zstd-compressed as NAME.zst; compressed images are verified and decompressed
before the device is changed. An unlocked bootloader is required.
";

#[derive(Debug)]
struct Options {
    package: PathBuf,
    manifest: PathBuf,
    fastboot: Option<PathBuf>,
    serial: Option<String>,
    mode: Mode,
    dry_run: bool,
    yes: bool,
    no_reboot: bool,
    timeout: Duration,
    flash_timeout: Duration,
    staging_dir: Option<PathBuf>,
}

enum Action {
    Run(Options),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, String> {
    let mut options = Options {
        package: env::current_dir()
            .map_err(|e| format!("Cannot determine current directory: {e}"))?,
        manifest: PathBuf::from("fastboot-manifest.txt"),
        fastboot: None,
        serial: None,
        mode: Mode::Upgrade,
        dry_run: false,
        yes: false,
        no_reboot: false,
        timeout: Duration::from_secs(120),
        flash_timeout: Duration::from_secs(1800),
        staging_dir: None,
    };
    let mut seen = HashSet::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let flag = arg.to_str().ok_or("Option names must be valid text")?;
        // Launchers supply --yes; accept callers repeating that switch.
        if !seen.insert(flag.to_owned()) && flag != "--yes" {
            return Err(format!("Repeated option: {flag}"));
        }
        match flag {
            "--help" | "-h" => return Ok(Action::Help),
            "--version" | "-V" => return Ok(Action::Version),
            "--dry-run" => options.dry_run = true,
            "--yes" => options.yes = true,
            "--no-reboot" => options.no_reboot = true,
            "--package" | "--manifest" | "--fastboot" | "--serial" | "--mode" | "--timeout"
            | "--flash-timeout" | "--staging-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("Missing value for {flag}"))?;
                if value.is_empty() || value.to_string_lossy().starts_with("--") {
                    return Err(format!("Missing value for {flag}"));
                }
                match flag {
                    "--package" => options.package = value.into(),
                    "--manifest" => options.manifest = value.into(),
                    "--fastboot" => options.fastboot = Some(value.into()),
                    "--staging-dir" => options.staging_dir = Some(value.into()),
                    "--serial" => {
                        let serial = value
                            .into_string()
                            .map_err(|_| "Serial must be valid text")?;
                        options.serial = Some(serial);
                    }
                    "--mode" => {
                        options.mode = match value.to_str() {
                            Some("upgrade") => Mode::Upgrade,
                            Some("wipe") => Mode::Wipe,
                            _ => return Err("--mode must be upgrade or wipe".into()),
                        }
                    }
                    "--timeout" => {
                        let seconds: u64 = value
                            .to_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("--timeout must be an integer from 1 to 3600")?;
                        if !(1..=3600).contains(&seconds) {
                            return Err("--timeout must be an integer from 1 to 3600".into());
                        }
                        options.timeout = Duration::from_secs(seconds);
                    }
                    "--flash-timeout" => {
                        let seconds: u64 = value
                            .to_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("--flash-timeout must be an integer from 1 to 7200")?;
                        if !(1..=7200).contains(&seconds) {
                            return Err("--flash-timeout must be an integer from 1 to 7200".into());
                        }
                        options.flash_timeout = Duration::from_secs(seconds);
                    }
                    _ => unreachable!(),
                }
            }
            _ => return Err(format!("Unknown option: {flag}. Use --help for usage.")),
        }
    }
    Ok(Action::Run(options))
}

fn main() {
    let result = match parse_args(env::args_os().skip(1)) {
        Ok(Action::Help) => {
            print!("{HELP}");
            Ok(())
        }
        Ok(Action::Version) => {
            println!("swiftflash-fastboot {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Ok(Action::Run(options)) => run(options),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("SwiftFlasher error: {error}");
        std::process::exit(1);
    }
}

fn run(options: Options) -> Result<(), String> {
    let root = fs::canonicalize(&options.package)
        .map_err(|e| format!("Cannot open package {}: {e}", options.package.display()))?;
    if !root.is_dir() {
        return Err("--package must be a directory".into());
    }
    let path = root.join(&options.manifest);
    let source = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read manifest {}: {e}", path.display()))?;
    let manifest = Manifest::parse(&source)?;
    println!("==============================================");
    println!("                SwiftFlasher");
    println!("==============================================");
    println!("Package: {}", root.display());
    println!("Compatible products: {}", manifest.products.join(", "));
    match options.mode {
        Mode::Upgrade => println!("Mode: upgrade — data erasure instructions are disabled."),
        Mode::Wipe => println!("Mode: wipe — erases apps, settings, and internal storage."),
    }
    if options.dry_run {
        let preview = manifest.preview(&root, options.mode, options.no_reboot)?;
        println!("Dry run: no fastboot process or device access.");
        for (index, line) in preview.iter().enumerate() {
            println!("{:>2}. {line}", index + 1);
        }
        for image in manifest.package_images(&root)? {
            if is_compressed(&image) {
                stage::decompress_file(&image, None)?;
            }
        }
        println!("Package preflight passed. Device compatibility and mode will be checked during a real run.");
        return Ok(());
    }

    let executable = resolve_fastboot(&root, options.fastboot.as_deref())?;
    println!("Fastboot: {}", executable.display());
    let mut fastboot = Fastboot::new(executable, options.serial, options.timeout)
        .with_flash_timeout(options.flash_timeout);
    let country = fastboot.connect(&manifest.products, manifest.needs_country())?;
    let mut operations =
        manifest.plan(&root, options.mode, country.as_deref(), options.no_reboot)?;
    // Removed when this function returns, including after a cancellation or error.
    let _staging = Staging::prepare(
        options.staging_dir.as_deref().unwrap_or(&root),
        &mut operations,
    )?;
    println!("Preflight passed. {} operations:", operations.len());
    for (index, operation) in operations.iter().enumerate() {
        println!("{:>2}. {}", index + 1, manifest::describe(operation));
    }
    if !options.yes && !confirm(options.mode)? {
        println!("Cancelled before flashing.");
        return Ok(());
    }
    fastboot.execute(&operations)?;
    println!("Flashing complete.");
    Ok(())
}

fn confirm(mode: Mode) -> Result<bool, String> {
    match mode {
        Mode::Upgrade => print!("Flash this device? [y/N] "),
        Mode::Wipe => print!("Erase all user data and flash this device? Type WIPE to continue: "),
    }
    io::stdout()
        .flush()
        .map_err(|e| format!("Cannot display confirmation: {e}"))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("Cannot read confirmation: {e}"))?;
    Ok(confirmation_matches(mode, &answer))
}

fn confirmation_matches(mode: Mode, answer: &str) -> bool {
    match mode {
        Mode::Upgrade => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Mode::Wipe => answer.trim() == "WIPE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Action, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn defaults_to_interactive_upgrade() {
        let Action::Run(options) = parse(&[]).unwrap() else {
            panic!()
        };
        assert_eq!(options.mode, Mode::Upgrade);
        assert!(!options.yes);
        assert!(!options.dry_run);
        assert_eq!(options.timeout, Duration::from_secs(120));
    }

    #[test]
    fn accepts_explicit_automation_and_paths_with_spaces() {
        let Action::Run(options) = parse(&[
            "--package",
            "C:/ROM [onyx]/Package",
            "--mode",
            "wipe",
            "--yes",
            "--no-reboot",
            "--timeout",
            "30",
            "--staging-dir",
            "D:/Scratch Space",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(options.mode, Mode::Wipe);
        assert!(options.yes && options.no_reboot);
        assert_eq!(options.package, PathBuf::from("C:/ROM [onyx]/Package"));
        assert_eq!(options.staging_dir, Some(PathBuf::from("D:/Scratch Space")));
    }

    #[test]
    fn rejects_typos_missing_values_and_ambiguous_modes() {
        for args in [
            &["--typo"][..],
            &["--package"],
            &["--serial", "--yes"],
            &["--mode", "wipe", "--mode", "upgrade"],
            &["--mode", "format"],
            &["--timeout", "0"],
            &["--timeout", "3601"],
            &["--timeout", "one"],
        ] {
            assert!(parse(args).is_err(), "Accepted {args:?}");
        }
    }

    #[test]
    fn wiping_requires_specific_confirmation() {
        for answer in ["", "y", "Y", "yes", "wipe", "no"] {
            assert!(!confirmation_matches(Mode::Wipe, answer));
        }
        assert!(confirmation_matches(Mode::Wipe, "WIPE\n"));
        assert!(!confirmation_matches(Mode::Upgrade, ""));
        assert!(confirmation_matches(Mode::Upgrade, "Y\n"));
    }
}
