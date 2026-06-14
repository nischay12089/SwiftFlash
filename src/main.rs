use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Write};
use std::process::Command;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 { std::process::exit(1); }

    let out_fd = &args[2];
    let zip_path = &args[3];
    let script_path = "/tmp/updater-script"; 

    // 1. Open the recovery command pipe ONCE and keep it open
    let fd_path = format!("/proc/self/fd/{}", out_fd);
    let mut pipe = OpenOptions::new().write(true).open(&fd_path)
        .expect("Failed to open recovery command pipe");

    // 2. HARDCODED BRANDING (Cannot be removed by changing the updater-script)
    ui_print(&mut pipe, "==============================================");
    ui_print(&mut pipe, "=                SwiftFlasher                =");
    ui_print(&mut pipe, "==============================================");
    ui_print(&mut pipe, " ");

    // Extract updater-script from zip
    let _ = Command::new("unzip").args(&["-p", zip_path, "META-INF/com/google/android/updater-script"]).output()
        .and_then(|out| fs::write(script_path, out.stdout));

    if let Ok(script) = fs::read_to_string(script_path) {
        for line in script.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }

            let parts: Vec<&str> = line.split_whitespace().collect();
            let command = parts[0];

            match command {
                "ui_print" => {
                    // Grab everything after "ui_print", preserving all extra spaces!
                    let msg = if line.len() > 8 {
                        let text = &line[8..];
                        if text.starts_with(' ') { &text[1..] } else { text }
                    } else {
                        ""
                    };
                    ui_print(&mut pipe, msg);
                }
                "verify_device" => {
                    ui_print(&mut pipe, "Verifying device...");
                }
                "show_progress" => {
                    if parts.len() >= 3 {
                        ui_print_cmd(&mut pipe, &format!("progress {} {}", parts[1], parts[2]));
                    }
                }
                "set_progress" => {
                    if parts.len() >= 2 {
                        ui_print_cmd(&mut pipe, &format!("set_progress {}", parts[1]));
                    }
                }
                "package_flash_partition" => {
                    if parts.len() < 4 { continue; }
                    let format = parts[1];
                    let src = parts[2];
                    let dest = parts[3];
                    
                    ui_print(&mut pipe, &format!("Flashing {}...", src));
                    
                    if let Err(e) = flash_partition(format, src, dest, zip_path) {
                        ui_print(&mut pipe, &format!("ERROR: Failed to flash {}: {}", src, e));
                        std::process::exit(1); 
                    }
                }
                _ => {}
            }
        }
    }
}

// Write to the pipe and instantly flush it to the UI
fn ui_print(pipe: &mut File, message: &str) {
    let _ = writeln!(pipe, "ui_print {}", message);
    let _ = writeln!(pipe, "ui_print");
    let _ = pipe.flush();
}

// Send hidden progress commands to the pipe and flush
fn ui_print_cmd(pipe: &mut File, cmd: &str) {
    let _ = writeln!(pipe, "{}", cmd);
    let _ = pipe.flush();
}

fn flash_partition(format: &str, src: &str, dest: &str, zip_path: &str) -> Result<(), String> {
    let zip_file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| e.to_string())?;
    let mut source_stream = archive.by_name(src).map_err(|e| format!("File {} not in zip: {}", src, e))?;

    let mut out_file = OpenOptions::new().write(true).open(dest)
        .map_err(|e| format!("Cannot open block device {}: {}", dest, e))?;

    match format {
        "raw" => {
            std::io::copy(&mut source_stream, &mut out_file).map_err(|e| e.to_string())?;
        }
        "zstd" => {
            let mut decoder = zstd::stream::Decoder::new(source_stream).map_err(|e| e.to_string())?;
            std::io::copy(&mut decoder, &mut out_file).map_err(|e| e.to_string())?;
        }
        "gzip" => {
            let mut decoder = flate2::read::GzDecoder::new(source_stream);
            std::io::copy(&mut decoder, &mut out_file).map_err(|e| e.to_string())?;
        }
        _ => return Err(format!("Unsupported format: {}", format)),
    }

    Ok(())
}
