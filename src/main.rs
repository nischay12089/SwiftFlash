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
                    let msg = parts[1..].join(" ");
                    ui_print(out_fd, &msg);
                }
                "verify_device" => {
                    ui_print(out_fd, "Verifying device...");
                }
                "show_progress" => {
                    if parts.len() >= 3 {
                        ui_print_cmd(out_fd, &format!("progress {} {}", parts[1], parts[2]));
                    }
                }
                "set_progress" => {
                    if parts.len() >= 2 {
                        ui_print_cmd(out_fd, &format!("set_progress {}", parts[1]));
                    }
                }
                "package_flash_partition" => {
                    if parts.len() < 4 { continue; }
                    let format = parts[1];
                    let src = parts[2];
                    let dest = parts[3];
                    
                    ui_print(out_fd, &format!("Flashing {}...", src));
                    
                    // THIS IS THE REAL FLASHING ENGINE
                    if let Err(e) = flash_partition(format, src, dest, zip_path) {
                        ui_print(out_fd, &format!("ERROR: Failed to flash {}: {}", src, e));
                        std::process::exit(1); // Abort installation on failure
                    }
                }
                _ => {}
            }
        }
    }
}

// Function to print text to the screen
fn ui_print(fd: &str, message: &str) {
    let fd_path = format!("/proc/self/fd/{}", fd);
    if let Ok(mut file) = OpenOptions::new().append(true).open(fd_path) {
        let _ = writeln!(file, "ui_print {}", message);
        let _ = writeln!(file, "ui_print");
    }
}

// Function to send invisible commands (like progress bars) to recovery
fn ui_print_cmd(fd: &str, cmd: &str) {
    let fd_path = format!("/proc/self/fd/{}", fd);
    if let Ok(mut file) = OpenOptions::new().append(true).open(fd_path) {
        let _ = writeln!(file, "{}", cmd);
    }
}

// The core function that actually reads the zip, decompresses, and writes to the block
fn flash_partition(format: &str, src: &str, dest: &str, zip_path: &str) -> Result<(), String> {
    // 1. Open the zip file natively in Rust
    let zip_file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| e.to_string())?;
    
    // 2. Find the requested image (boot.img, super.bin.zst, etc.)
    let mut source_stream = archive.by_name(src).map_err(|e| format!("File {} not in zip: {}", src, e))?;

    // 3. Open the block device for writing (DO NOT TRUNCATE block devices)
    let mut out_file = OpenOptions::new().write(true).open(dest)
        .map_err(|e| format!("Cannot open block device {}: {}", dest, e))?;

    // 4. Route to the correct decompressor and copy the bytes
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
