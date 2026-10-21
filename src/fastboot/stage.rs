//! Decompress zstd-stored package images before the first device mutation.
//!
//! Fastboot only ever receives raw or Android sparse files. Each distinct
//! `.zst` image is decoded once, with every frame checksum verified, into a
//! reserved staging directory that is removed when flashing ends.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ruzstd::decoding::StreamingDecoder;

use super::manifest::{is_compressed, Operation};

/// Reserved name inside the chosen parent; a stale copy is replaced.
const STAGING: &str = ".swiftflash-staging";
const BUFFER_SIZE: usize = 1024 * 1024;

pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// Decompress every `.zst` flash image under `parent` and point the plan at
    /// the verified copies. Returns `None` when the plan has no compressed image.
    pub fn prepare(parent: &Path, operations: &mut [Operation]) -> Result<Option<Self>, String> {
        let mut compressed: Vec<PathBuf> = Vec::new();
        for operation in operations.iter() {
            if let Operation::Flash { image, .. } = operation {
                if is_compressed(image) && !compressed.contains(image) {
                    compressed.push(image.clone());
                }
            }
        }
        if compressed.is_empty() {
            return Ok(None);
        }
        let parent = fs::canonicalize(parent)
            .map_err(|e| format!("Cannot open staging directory {}: {e}", parent.display()))?;
        let dir = parent.join(STAGING);
        if fs::symlink_metadata(&dir).is_ok() {
            fs::remove_dir_all(&dir).map_err(|e| {
                format!(
                    "Cannot remove stale staging directory {}: {e}",
                    dir.display()
                )
            })?;
        }
        fs::create_dir(&dir)
            .map_err(|e| format!("Cannot create staging directory {}: {e}", dir.display()))?;
        // From here on, Drop removes the directory on success and on any error.
        let staging = Self { dir };
        println!(
            "Decompressing {} zstd image(s) into {} before flashing...",
            compressed.len(),
            staging.dir.display()
        );
        let mut staged = HashMap::new();
        for (index, image) in compressed.iter().enumerate() {
            let name = image
                .file_stem()
                .ok_or_else(|| format!("Invalid image name: {}", image.display()))?;
            let mut output = staging.dir.join(format!("{index:02}-"));
            output.as_mut_os_string().push(name);
            decompress_file(image, Some(&output))?;
            staged.insert(image.clone(), output);
        }
        for operation in operations.iter_mut() {
            if let Operation::Flash { image, .. } = operation {
                if let Some(output) = staged.get(image) {
                    *image = output.clone();
                }
            }
        }
        Ok(Some(staging))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Fully decode a stored `.zst` image and verify its frame checksums. With an
/// output path the decoded bytes are written there; otherwise they are counted.
pub fn decompress_file(image: &Path, output: Option<&Path>) -> Result<u64, String> {
    let name = image
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source = File::open(image).map_err(|e| format!("Cannot read {}: {e}", image.display()))?;
    let length = source
        .metadata()
        .map_err(|e| format!("Cannot inspect {}: {e}", image.display()))?
        .len();
    let mut reported = None;
    let mut progress = |read: u64| {
        let percent = read.saturating_mul(100) / length.max(1);
        if reported != Some(percent) {
            reported = Some(percent);
            print!("\r  {name}: {percent:>3}%");
            let _ = io::stdout().flush();
        }
    };
    let result = match output {
        Some(path) => {
            let file =
                File::create(path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
            let mut writer = BufWriter::with_capacity(BUFFER_SIZE, file);
            decompress(source, &mut writer, &mut progress).and_then(|size| {
                writer
                    .flush()
                    .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
                Ok(size)
            })
        }
        None => decompress(source, &mut io::sink(), &mut progress),
    };
    println!();
    let size =
        result.map_err(|e| format!("Image {} is not valid zstd data: {e}", image.display()))?;
    println!("  {name}: verified, {} expanded", human_size(size));
    Ok(size)
}

struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.read += count as u64;
        Ok(count)
    }
}

/// Decode one or more concatenated zstd frames. Reports compressed bytes read.
fn decompress(
    source: impl Read,
    output: &mut impl Write,
    progress: &mut impl FnMut(u64),
) -> Result<u64, String> {
    let mut source = Counted {
        inner: BufReader::with_capacity(BUFFER_SIZE, source),
        read: 0,
    };
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut total = 0_u64;
    let mut frames = 0_u64;
    loop {
        if frames > 0
            && source
                .inner
                .fill_buf()
                .map_err(|e| format!("cannot read compressed data: {e}"))?
                .is_empty()
        {
            break;
        }
        let mut decoder = StreamingDecoder::new(&mut source)
            .map_err(|e| format!("frame {frames} header: {e}"))?;
        loop {
            let count = match decoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("frame {frames}: {error}")),
            };
            output
                .write_all(&buffer[..count])
                .map_err(|e| format!("cannot write decompressed data: {e}"))?;
            total += count as u64;
            progress(decoder.get_ref().read);
        }
        let frame = decoder.into_frame_decoder();
        if let Some(expected) = frame.get_checksum_from_data() {
            if frame.get_calculated_checksum() != Some(expected) {
                return Err(format!("frame {frames} checksum mismatch"));
            }
        }
        frames += 1;
    }
    if total == 0 {
        return Err("decompresses to zero bytes".into());
    }
    Ok(total)
}

pub fn human_size(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    if bytes as f64 >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Level-1 zstd frames with content checksums, produced by the reference
    // `zstd` CLI so the pure-Rust decoder is checked against libzstd output.
    const HELLO: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x48, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0xa3,
        0x6d, 0x9f, 0x88,
    ];
    const WORLD: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x48, 0x29, 0x00, 0x00, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0xef,
        0x51, 0xee, 0x66,
    ];

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "swiftflash-stage-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        decompress(bytes, &mut output, &mut |_| {})?;
        Ok(output)
    }

    #[test]
    fn decodes_single_and_concatenated_frames() {
        assert_eq!(decode(HELLO).unwrap(), b"hello");
        assert_eq!(decode(&[HELLO, WORLD].concat()).unwrap(), b"helloworld");
    }

    #[test]
    fn rejects_truncation_corruption_and_empty_input() {
        assert!(decode(&HELLO[..HELLO.len() - 1]).is_err());
        let mut corrupt = HELLO.to_vec();
        corrupt[10] ^= 1;
        assert!(decode(&corrupt).unwrap_err().contains("checksum"));
        assert!(decode(b"").is_err());
        assert!(decode(b"not zstd").is_err());
        assert!(decode(&[HELLO, b"x"].concat()).is_err());
    }

    #[test]
    fn stages_each_compressed_image_once_and_cleans_up() {
        let package = Directory::new();
        let compressed = package.0.join("boot.img.zst");
        let raw = package.0.join("vbmeta.img");
        fs::write(&compressed, HELLO).unwrap();
        fs::write(&raw, b"raw").unwrap();
        let mut operations = vec![
            Operation::Flash {
                partition: "boot_ab".into(),
                image: compressed.clone(),
            },
            Operation::Flash {
                partition: "vbmeta_ab".into(),
                image: raw.clone(),
            },
            Operation::Flash {
                partition: "recovery_ab".into(),
                image: compressed,
            },
        ];
        let staging = Staging::prepare(&package.0, &mut operations)
            .unwrap()
            .unwrap();
        let Operation::Flash { image: staged, .. } = &operations[0] else {
            panic!()
        };
        assert!(staged.starts_with(&staging.dir));
        assert_eq!(fs::read(staged).unwrap(), b"hello");
        assert_eq!(
            operations[1],
            Operation::Flash {
                partition: "vbmeta_ab".into(),
                image: raw
            }
        );
        assert!(matches!(&operations[2], Operation::Flash { image, .. } if image == staged));
        let dir = staging.dir.clone();
        drop(staging);
        assert!(!dir.exists());
    }

    #[test]
    fn corrupt_image_fails_staging_and_leaves_no_directory() {
        let package = Directory::new();
        let image = package.0.join("super.img.zst");
        fs::write(&image, &HELLO[..HELLO.len() - 2]).unwrap();
        let mut operations = vec![Operation::Flash {
            partition: "super".into(),
            image,
        }];
        assert!(Staging::prepare(&package.0, &mut operations).is_err());
        assert!(!package.0.join(STAGING).exists());
    }
}
