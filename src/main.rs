#[allow(dead_code)]
#[path = "fastboot/manifest.rs"]
mod manifest;
mod recovery;

use std::env;
use std::fs::OpenOptions;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("SwiftFlasher: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().collect();
    if args.len() != 4 {
        return Err("Recovery usage: update-binary <api-version> <output-fd> <rom.zip>".into());
    }
    let fd: u32 = args[2]
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or("Invalid recovery output descriptor")?;
    let mut pipe = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{fd}"))
        .map_err(|error| format!("Cannot open recovery command pipe: {error}"))?;
    recovery::ui_print(&mut pipe, "==============================================");
    recovery::ui_print(&mut pipe, "=               Swift-fLASHER                =");
    recovery::ui_print(&mut pipe, "=     By Instantnoodlep & Nischay_12089      =");
    recovery::ui_print(&mut pipe, "==============================================");
    let result = recovery::install(Path::new(&args[3]), &mut pipe);
    if let Err(error) = &result {
        recovery::ui_print(&mut pipe, &format!("ERROR: {error}"));
    }
    result
}
