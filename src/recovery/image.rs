//! Bounded-memory image decoding for recovery block-device writes.
//!
//! Sparse layout and CRC semantics follow AOSP libsparse/sparse_format.h.
//! Call `validate_image` on a separate source pass before opening a block
//! device for writing: decompressor trailers and late chunks can fail at EOF.

use crc32fast::Hasher;
use flate2::read::MultiGzDecoder;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

const SPARSE_MAGIC: u32 = 0xed26_ff3a;
const FILE_HEADER_SIZE: usize = 28;
const CHUNK_HEADER_SIZE: usize = 12;
const BUFFER_SIZE: usize = 64 * 1024;
const RAW_CHUNK: u16 = 0xcac1;
const FILL_CHUNK: u16 = 0xcac2;
const DONT_CARE_CHUNK: u16 = 0xcac3;
const CRC32_CHUNK: u16 = 0xcac4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Raw,
    Gzip,
    Zstd,
}

/// Decode compression, detect Android sparse data, and write the expanded
/// image from the destination's current position. DONT_CARE ranges advance the
/// destination without overwriting existing bytes; they count as zeros in CRCs.
/// Returns the logical expanded length, including skipped ranges.
pub fn expand_image<R: Read, W: Write + Seek>(
    source: R,
    destination: &mut W,
    format: ImageFormat,
) -> Result<u64, String> {
    let length = match format {
        ImageFormat::Raw => expand_decoded(source, destination),
        ImageFormat::Gzip => expand_decoded(MultiGzDecoder::new(source), destination),
        ImageFormat::Zstd => {
            let decoder = zstd::stream::read::Decoder::new(source)
                .map_err(|error| format!("Cannot open zstd image: {error}"))?;
            expand_decoded(decoder, destination)
        }
    }?;
    destination
        .flush()
        .map_err(|error| format!("Cannot flush expanded image: {error}"))?;
    Ok(length)
}

/// Fully decode and validate an image without allocating its expanded contents
/// or writing to a device. This consumes the source, including compressed-frame
/// trailers and sparse checksums, and returns its expanded byte count.
pub fn validate_image<R: Read>(source: R, format: ImageFormat) -> Result<u64, String> {
    expand_image(source, &mut CountingSink::default(), format)
}

fn expand_decoded<R: Read, W: Write + Seek>(
    mut source: R,
    destination: &mut W,
) -> Result<u64, String> {
    let mut prefix = [0_u8; 4];
    let mut length = 0;
    while length < prefix.len() {
        match source.read(&mut prefix[length..]) {
            Ok(0) => break,
            Ok(count) => length += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Cannot read decoded image: {error}")),
        }
    }
    if length == 0 {
        return Err("Image is empty after decompression".into());
    }
    if length == prefix.len() && u32::from_le_bytes(prefix) == SPARSE_MAGIC {
        expand_sparse(&mut source, destination, prefix)
    } else {
        let mut raw = Cursor::new(&prefix[..length]).chain(source);
        io::copy(&mut raw, destination).map_err(|error| format!("Cannot copy raw image: {error}"))
    }
}

fn expand_sparse<R: Read, W: Write + Seek>(
    source: &mut R,
    destination: &mut W,
    magic: [u8; 4],
) -> Result<u64, String> {
    let mut header = [0_u8; FILE_HEADER_SIZE];
    header[..4].copy_from_slice(&magic);
    read_exact(source, &mut header[4..], "sparse file header")?;
    let major = le16(&header[4..6]);
    let file_header_size = usize::from(le16(&header[8..10]));
    let chunk_header_size = usize::from(le16(&header[10..12]));
    let block_size = u64::from(le32(&header[12..16]));
    let total_blocks = u64::from(le32(&header[16..20]));
    let total_chunks = le32(&header[20..24]);
    let image_checksum = le32(&header[24..28]);
    if major != 1 {
        return Err(format!("Unsupported Android sparse major version {major}"));
    }
    if file_header_size < FILE_HEADER_SIZE || chunk_header_size < CHUNK_HEADER_SIZE {
        return Err("Android sparse header sizes are too small".into());
    }
    if block_size == 0 || block_size % 4 != 0 {
        return Err("Android sparse block size must be a nonzero multiple of four".into());
    }
    if total_blocks == 0 || total_chunks == 0 {
        return Err("Android sparse image must contain blocks and chunks".into());
    }
    let expanded_length = total_blocks
        .checked_mul(block_size)
        .filter(|length| *length <= i64::MAX as u64)
        .ok_or("Android sparse expanded length exceeds supported seek range")?;
    let start = destination
        .stream_position()
        .map_err(|error| format!("Cannot locate image destination: {error}"))?;
    start
        .checked_add(expanded_length)
        .filter(|end| *end <= i64::MAX as u64)
        .ok_or("Android sparse destination position would overflow")?;
    discard(
        source,
        file_header_size - FILE_HEADER_SIZE,
        "extended sparse file header",
    )?;

    let mut written_blocks = 0_u64;
    let mut checksum = Hasher::new();
    let mut buffer = [0_u8; BUFFER_SIZE];
    for index in 0..total_chunks {
        let mut header = [0_u8; CHUNK_HEADER_SIZE];
        read_exact(source, &mut header, &format!("sparse chunk {index} header"))?;
        let chunk_type = le16(&header[..2]);
        let blocks = u64::from(le32(&header[4..8]));
        let total_size = u64::from(le32(&header[8..12]));
        let payload_size = total_size
            .checked_sub(chunk_header_size as u64)
            .ok_or_else(|| format!("Sparse chunk {index} is smaller than its header"))?;
        let next_blocks = written_blocks
            .checked_add(blocks)
            .filter(|blocks| *blocks <= total_blocks)
            .ok_or_else(|| format!("Sparse chunk {index} exceeds declared output block count"))?;
        let bytes = blocks
            .checked_mul(block_size)
            .ok_or_else(|| format!("Sparse chunk {index} size overflows"))?;
        // Check each chunk's complete layout before touching its destination.
        match chunk_type {
            RAW_CHUNK if blocks > 0 && payload_size == bytes => {}
            FILL_CHUNK if blocks > 0 && payload_size == 4 => {}
            DONT_CARE_CHUNK if blocks > 0 && payload_size == 0 => {}
            CRC32_CHUNK if blocks == 0 && payload_size == 4 => {}
            RAW_CHUNK | FILL_CHUNK | DONT_CARE_CHUNK | CRC32_CHUNK => {
                return Err(format!(
                    "Sparse chunk {index} has an invalid payload size or block count"
                ));
            }
            _ => {
                return Err(format!(
                    "Unknown Android sparse chunk type 0x{chunk_type:04x}"
                ))
            }
        }
        discard(
            source,
            chunk_header_size - CHUNK_HEADER_SIZE,
            "extended sparse chunk header",
        )?;
        match chunk_type {
            RAW_CHUNK => {
                let mut remaining = bytes;
                while remaining > 0 {
                    let count = remaining.min(BUFFER_SIZE as u64) as usize;
                    read_exact(source, &mut buffer[..count], "sparse RAW chunk data")?;
                    checksum.update(&buffer[..count]);
                    write_all(destination, &buffer[..count])?;
                    remaining -= count as u64;
                }
            }
            FILL_CHUNK => {
                let mut pattern = [0_u8; 4];
                read_exact(source, &mut pattern, "sparse FILL chunk pattern")?;
                for word in buffer.chunks_exact_mut(4) {
                    word.copy_from_slice(&pattern);
                }
                let mut remaining = bytes;
                while remaining > 0 {
                    let count = remaining.min(BUFFER_SIZE as u64) as usize;
                    checksum.update(&buffer[..count]);
                    write_all(destination, &buffer[..count])?;
                    remaining -= count as u64;
                }
            }
            DONT_CARE_CHUNK => {
                destination
                    .seek(SeekFrom::Current(bytes as i64))
                    .map_err(|error| format!("Cannot seek over sparse DONT_CARE range: {error}"))?;
                checksum_zeros(&mut checksum, bytes);
            }
            CRC32_CHUNK => {
                let mut expected = [0_u8; 4];
                read_exact(source, &mut expected, "sparse CRC32 chunk checksum")?;
                if le32(&expected) != checksum.clone().finalize() {
                    return Err(format!(
                        "Android sparse CRC32 chunk {index} checksum mismatch"
                    ));
                }
            }
            _ => unreachable!(),
        }
        written_blocks = next_blocks;
    }
    if written_blocks != total_blocks {
        return Err(format!("Android sparse block count mismatch: decoded {written_blocks}, expected {total_blocks}"));
    }
    if image_checksum != 0 && image_checksum != checksum.finalize() {
        return Err("Android sparse image checksum mismatch".into());
    }
    // This also forces gzip/zstd to verify their final frame and exposes ZIP
    // entry read errors instead of silently stopping at the sparse boundary.
    let mut trailing = [0_u8; 1];
    loop {
        match source.read(&mut trailing) {
            Ok(0) => break,
            Ok(_) => return Err("Unexpected trailing data after Android sparse image".into()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Cannot finish decoded sparse image: {error}")),
        }
    }
    Ok(expanded_length)
}

fn checksum_zeros(checksum: &mut Hasher, mut bytes: u64) {
    // Combining doubled runs hashes a huge skipped range in logarithmic time
    // while preserving the CRC convention that DONT_CARE bytes are zero.
    let mut run = Hasher::new();
    run.update(&[0]);
    while bytes != 0 {
        if bytes & 1 != 0 {
            checksum.combine(&run);
        }
        bytes >>= 1;
        if bytes != 0 {
            let previous = run.clone();
            run.combine(&previous);
        }
    }
}

fn read_exact(reader: &mut impl Read, bytes: &mut [u8], part: &str) -> Result<(), String> {
    reader
        .read_exact(bytes)
        .map_err(|error| format!("Cannot read {part}: {error}"))
}

fn write_all(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    writer
        .write_all(bytes)
        .map_err(|error| format!("Cannot write expanded image: {error}"))
}

fn discard(reader: &mut impl Read, mut bytes: usize, part: &str) -> Result<(), String> {
    let mut buffer = [0_u8; 1024];
    while bytes > 0 {
        let count = bytes.min(buffer.len());
        read_exact(reader, &mut buffer[..count], part)?;
        bytes -= count;
    }
    Ok(())
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("two-byte sparse field"))
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte sparse field"))
}

#[derive(Default)]
struct CountingSink {
    position: u64,
}

impl Write for CountingSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "expanded length overflow")
            })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for CountingSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => {
                let next = i128::from(self.position) + i128::from(offset);
                u64::try_from(next).ok()
            }
            SeekFrom::End(_) => None,
        };
        self.position =
            next.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image seek"))?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn chunk(kind: u16, blocks: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(kind.to_le_bytes());
        bytes.extend(0_u16.to_le_bytes());
        bytes.extend(blocks.to_le_bytes());
        bytes.extend((CHUNK_HEADER_SIZE as u32 + payload.len() as u32).to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    fn sparse(blocks: u32, chunks: &[Vec<u8>], checksum: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(SPARSE_MAGIC.to_le_bytes());
        bytes.extend(1_u16.to_le_bytes());
        bytes.extend(0_u16.to_le_bytes());
        bytes.extend((FILE_HEADER_SIZE as u16).to_le_bytes());
        bytes.extend((CHUNK_HEADER_SIZE as u16).to_le_bytes());
        bytes.extend(4_u32.to_le_bytes());
        bytes.extend(blocks.to_le_bytes());
        bytes.extend((chunks.len() as u32).to_le_bytes());
        bytes.extend(checksum.to_le_bytes());
        for chunk in chunks {
            bytes.extend(chunk);
        }
        bytes
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn expand(bytes: &[u8], format: ImageFormat) -> Result<Vec<u8>, String> {
        let mut output = Cursor::new(Vec::new());
        expand_image(bytes, &mut output, format)?;
        Ok(output.into_inner())
    }

    #[test]
    fn raw_images_and_short_reads_preserve_every_byte() {
        struct ShortReads<'a>(&'a [u8]);
        impl Read for ShortReads<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(self.0.len()).min(1);
                output[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }
        for bytes in [&b"a"[..], &b"abc"[..], &b"raw boot image bytes"[..]] {
            let mut output = Cursor::new(Vec::new());
            assert_eq!(
                expand_image(ShortReads(bytes), &mut output, ImageFormat::Raw).unwrap(),
                bytes.len() as u64
            );
            assert_eq!(output.into_inner(), bytes);
        }
        assert!(validate_image(&b""[..], ImageFormat::Raw).is_err());
    }

    #[test]
    fn compressed_raw_images_are_decoded_and_trailers_checked() {
        let raw = b"ANDROID! tiny raw boot fixture";
        let gz = gzip(raw);
        let zst = zstd::stream::encode_all(&raw[..], 1).unwrap();
        for (bytes, format) in [(&gz, ImageFormat::Gzip), (&zst, ImageFormat::Zstd)] {
            assert_eq!(expand(bytes, format).unwrap(), raw);
            assert_eq!(
                validate_image(&bytes[..], format).unwrap(),
                raw.len() as u64
            );
            assert!(validate_image(&bytes[..bytes.len() - 2], format).is_err());
        }
        let mut corrupt = gz;
        let last = corrupt.len() - 5;
        corrupt[last] ^= 1;
        assert!(validate_image(&corrupt[..], ImageFormat::Gzip).is_err());
    }

    #[test]
    fn concatenated_compression_frames_are_not_silently_truncated() {
        let mut gz = gzip(b"first");
        gz.extend(gzip(b"second"));
        assert_eq!(expand(&gz, ImageFormat::Gzip).unwrap(), b"firstsecond");
        let mut zst = zstd::stream::encode_all(&b"first"[..], 1).unwrap();
        zst.extend(zstd::stream::encode_all(&b"second"[..], 1).unwrap());
        assert_eq!(expand(&zst, ImageFormat::Zstd).unwrap(), b"firstsecond");
    }

    #[test]
    fn sparse_raw_fill_skip_and_crc_match_aosp_semantics() {
        let logical = [
            b"ABCD".as_slice(),
            &[0x12, 0x34, 0x56, 0x78].repeat(2),
            &[0; 4],
            b"EFGH",
        ]
        .concat();
        let crc = crc32fast::hash(&logical);
        let image = sparse(
            5,
            &[
                chunk(RAW_CHUNK, 1, b"ABCD"),
                chunk(FILL_CHUNK, 2, &[0x12, 0x34, 0x56, 0x78]),
                chunk(DONT_CARE_CHUNK, 1, &[]),
                chunk(RAW_CHUNK, 1, b"EFGH"),
                chunk(CRC32_CHUNK, 0, &crc.to_le_bytes()),
            ],
            crc,
        );
        let mut output = Cursor::new(vec![0x99; 20]);
        assert_eq!(
            expand_image(&image[..], &mut output, ImageFormat::Raw).unwrap(),
            20
        );
        let mut expected = logical;
        expected[12..16].fill(0x99);
        assert_eq!(output.into_inner(), expected);
        assert_eq!(validate_image(&image[..], ImageFormat::Raw).unwrap(), 20);
        for (compressed, format) in [
            (gzip(&image), ImageFormat::Gzip),
            (
                zstd::stream::encode_all(&image[..], 1).unwrap(),
                ImageFormat::Zstd,
            ),
        ] {
            assert_eq!(validate_image(&compressed[..], format).unwrap(), 20);
        }
    }

    #[test]
    fn trailing_skip_preserves_existing_bytes_and_reports_logical_length() {
        let image = sparse(
            3,
            &[chunk(RAW_CHUNK, 1, b"ABCD"), chunk(DONT_CARE_CHUNK, 2, &[])],
            0,
        );
        let mut output = Cursor::new(vec![0x77; 12]);
        assert_eq!(
            expand_image(&image[..], &mut output, ImageFormat::Raw).unwrap(),
            12
        );
        assert_eq!(output.position(), 12);
        assert_eq!(
            output.into_inner(),
            [b"ABCD".as_slice(), &[0x77; 8]].concat()
        );
    }

    #[test]
    fn supports_extended_headers_and_new_minor_versions() {
        let mut image = sparse(1, &[chunk(RAW_CHUNK, 1, b"ABCD")], 0);
        image[6..8].copy_from_slice(&9_u16.to_le_bytes());
        image[8..10].copy_from_slice(&32_u16.to_le_bytes());
        image[10..12].copy_from_slice(&16_u16.to_le_bytes());
        image.splice(28..28, [0xab; 4]);
        image[40..44].copy_from_slice(&20_u32.to_le_bytes());
        image.splice(44..44, [0xcd; 4]);
        assert_eq!(expand(&image, ImageFormat::Raw).unwrap(), b"ABCD");
    }

    #[test]
    fn malformed_headers_are_rejected_before_destination_writes() {
        let valid = sparse(1, &[chunk(RAW_CHUNK, 1, b"ABCD")], 0);
        for (range, replacement) in [
            (4..6, 2_u16.to_le_bytes().to_vec()),
            (8..10, 27_u16.to_le_bytes().to_vec()),
            (10..12, 11_u16.to_le_bytes().to_vec()),
            (12..16, 3_u32.to_le_bytes().to_vec()),
            (12..16, 0_u32.to_le_bytes().to_vec()),
            (16..20, 0_u32.to_le_bytes().to_vec()),
        ] {
            let mut image = valid.clone();
            image[range].copy_from_slice(&replacement);
            let mut output = Cursor::new(Vec::new());
            assert!(expand_image(&image[..], &mut output, ImageFormat::Raw).is_err());
            assert!(output.into_inner().is_empty());
        }
        let mut oversized = valid;
        oversized[12..16].copy_from_slice(&(u32::MAX - 3).to_le_bytes());
        oversized[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_image(&oversized[..], ImageFormat::Raw).is_err());
    }

    #[test]
    fn malformed_chunks_truncation_counts_and_trailing_data_are_rejected() {
        let invalid = [
            sparse(1, &[chunk(RAW_CHUNK, 1, b"ABC")], 0),
            sparse(1, &[chunk(FILL_CHUNK, 1, b"ABC")], 0),
            sparse(1, &[chunk(DONT_CARE_CHUNK, 1, b"x")], 0),
            sparse(1, &[chunk(CRC32_CHUNK, 1, &[0; 4])], 0),
            sparse(1, &[chunk(0xcaff, 1, b"ABCD")], 0),
            sparse(1, &[chunk(RAW_CHUNK, 2, b"ABCDEFGH")], 0),
            sparse(2, &[chunk(RAW_CHUNK, 1, b"ABCD")], 0),
        ];
        for image in invalid {
            assert!(validate_image(&image[..], ImageFormat::Raw).is_err());
        }
        let valid = sparse(1, &[chunk(RAW_CHUNK, 1, b"ABCD")], 0);
        for length in 4..valid.len() {
            assert!(
                validate_image(&valid[..length], ImageFormat::Raw).is_err(),
                "accepted prefix of {length} bytes"
            );
        }
        let mut underflow = valid.clone();
        underflow[36..40].copy_from_slice(&1_u32.to_le_bytes());
        assert!(validate_image(&underflow[..], ImageFormat::Raw).is_err());
        let mut trailing = valid;
        trailing.push(0);
        assert!(validate_image(&trailing[..], ImageFormat::Raw).is_err());
    }

    #[test]
    fn sparse_checksums_reject_bad_content_and_count_skips_as_zero() {
        let actual = crc32fast::hash(b"ABCD");
        let bad_chunk = sparse(
            1,
            &[
                chunk(RAW_CHUNK, 1, b"ABCD"),
                chunk(CRC32_CHUNK, 0, &(actual ^ 1).to_le_bytes()),
            ],
            0,
        );
        assert!(validate_image(&bad_chunk[..], ImageFormat::Raw)
            .unwrap_err()
            .contains("checksum"));
        let bad_header = sparse(1, &[chunk(RAW_CHUNK, 1, b"ABCD")], actual ^ 1);
        assert!(validate_image(&bad_header[..], ImageFormat::Raw)
            .unwrap_err()
            .contains("checksum"));
        for length in [0_usize, 1, 3, 64, 65_537] {
            let mut checksum = Hasher::new();
            checksum.update(b"prefix");
            checksum_zeros(&mut checksum, length as u64);
            let expected = [b"prefix".as_slice(), &vec![0; length]].concat();
            assert_eq!(checksum.finalize(), crc32fast::hash(&expected));
        }
    }

    #[test]
    fn huge_skip_is_validated_without_allocating_or_writing_its_contents() {
        let image = sparse(u32::MAX, &[chunk(DONT_CARE_CHUNK, u32::MAX, &[])], 0);
        assert_eq!(
            validate_image(&image[..], ImageFormat::Raw).unwrap(),
            u64::from(u32::MAX) * 4
        );
    }

    #[test]
    fn destination_errors_are_propagated() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write denied"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Seek for Broken {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Err(io::Error::other("seek denied"))
            }
        }
        assert!(expand_image(&b"raw"[..], &mut Broken, ImageFormat::Raw)
            .unwrap_err()
            .contains("write denied"));
        let sparse = sparse(1, &[chunk(DONT_CARE_CHUNK, 1, &[])], 0);
        assert!(expand_image(&sparse[..], &mut Broken, ImageFormat::Raw)
            .unwrap_err()
            .contains("seek denied"));
    }
}
