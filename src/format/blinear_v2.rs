use std::io::{Cursor, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::format::{
    BLINEAR_HASH_SEED, BLINEAR_SUPERBLOCK, EncodeProfile, ReadOutcome, decode_chunk_section,
    encode_chunk_section, newest_timestamp_millis, parse_region_coords_from_path,
};
use crate::io::read_file_bytes;
use crate::model::{REGION_CHUNK_COUNT, Region};
use crate::writer::RegionWriteTarget;

const HEADER_SIZE: usize = 18;

pub fn read_region(path: &Path) -> Result<ReadOutcome> {
    let bytes = read_file_bytes(path)?;
    read_region_bytes(path, &bytes)
}

const SWAP_SUPERBLOCK: i64 = 0x1145141919810;
const SWAP_VERSION: u8 = 0x02;
const SWAP_SECTOR_BYTES: usize = 17;
const SWAP_HEADER_BYTES: usize = 8 + 1 + 4 + 8 + REGION_CHUNK_COUNT * SWAP_SECTOR_BYTES;

/// DivineMC's Buffered region keeps its newest chunks in `<file>.swp` and syncs the master file only every few
/// seconds; after a crash the swap holds data the master lacks. Each swap chunk that decodes cleanly and is not older
/// than the master's copy replaces it.
fn merge_swap_file(path: &Path, outcome: &mut ReadOutcome) -> Result<()> {
    let swap_path = path.with_file_name(format!(
        "{}.swp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    ));
    if !swap_path.is_file() {
        return Ok(());
    }
    let bytes = read_file_bytes(&swap_path)?;
    let (region_x, region_z) = (outcome.region.region_x, outcome.region.region_z);
    let warn = |message: String| {
        Diagnostic::warning(DiagnosticCode::CorruptChunk, message)
            .with_path(&swap_path)
            .with_region_coords(region_x, region_z)
    };
    if bytes.len() < SWAP_HEADER_BYTES
        || i64::from_be_bytes(bytes[0..8].try_into().unwrap()) != SWAP_SUPERBLOCK
        || bytes[8] != SWAP_VERSION
    {
        outcome
            .diagnostics
            .push(warn("swap file has no valid header and was ignored".into()));
        return Ok(());
    }
    let seed = u32::from_be_bytes(bytes[9..13].try_into().unwrap());
    for index in 0..REGION_CHUNK_COUNT {
        let at = 21 + index * SWAP_SECTOR_BYTES;
        let offset = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        let length = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
        if bytes[at + 16] != 1 {
            continue;
        }
        let decoded = (|| -> Result<crate::model::ChunkData> {
            ensure!(
                length >= 4
                    && offset
                        .checked_add(length)
                        .is_some_and(|end| end <= bytes.len()),
                "sector out of bounds"
            );
            let sector = &bytes[offset..offset + length];
            let original = i32::from_be_bytes(sector[0..4].try_into().unwrap());
            ensure!(original >= 16, "sector too short");
            let section = lz4_flex::block::decompress(&sector[4..], original as usize)?;
            decode_chunk_section(&section, seed)
        })();
        match decoded {
            Ok(chunk) => {
                let newer = outcome
                    .region
                    .chunk(index)
                    .is_none_or(|master| chunk.timestamp >= master.timestamp);
                if newer {
                    outcome.region.set_chunk(index, chunk)?;
                }
            }
            Err(error) => outcome.diagnostics.push(
                warn(format!(
                    "swap copy of the chunk is unreadable, the master copy is kept: {error:#}"
                ))
                .with_chunk_index(index),
            ),
        }
    }
    Ok(())
}

pub(crate) fn read_region_bytes(path: &Path, bytes: &[u8]) -> Result<ReadOutcome> {
    ensure!(
        bytes.len() >= HEADER_SIZE,
        "blinear_v2 region {} is too small",
        path.display()
    );

    let (region_x, region_z) = parse_region_coords_from_path(path)?;
    let mut cursor = Cursor::new(&bytes);

    let superblock = cursor.read_i64::<BigEndian>()?;
    ensure!(
        superblock == BLINEAR_SUPERBLOCK,
        "invalid blinear_v2 superblock"
    );

    let version = cursor.read_u8()?;
    ensure!(version == 0x02, "invalid blinear_v2 version {version}");

    let _master_timestamp = cursor.read_i64::<BigEndian>()?;
    let _compression_level = cursor.read_u8()?;

    let compressed = &bytes[HEADER_SIZE..];
    let decompressed = zstd::stream::decode_all(Cursor::new(compressed))
        .with_context(|| format!("failed to decompress {}", path.display()))?;

    let mut region = Region::new(region_x, region_z);
    let mut diagnostics = Vec::new();
    let mut discarded_chunks = 0;
    let mut offset = 0_usize;

    for index in 0..REGION_CHUNK_COUNT {
        ensure!(
            offset + 4 <= decompressed.len(),
            "blinear_v2 region {} ended before slot {index}",
            path.display()
        );

        let section_len = i32::from_be_bytes(decompressed[offset..offset + 4].try_into().unwrap());
        offset += 4;
        ensure!(
            section_len >= 0,
            "blinear_v2 region {} has a negative section length at slot {index}",
            path.display()
        );

        if section_len == 0 {
            continue;
        }

        let section_len = section_len as usize;
        if offset + section_len > decompressed.len() {
            diagnostics.push(
                Diagnostic::warning(
                    DiagnosticCode::SkippedData,
                    "chunk points past the decompressed buffer and was skipped",
                )
                .with_path(path)
                .with_region_coords(region_x, region_z)
                .with_chunk_index(index),
            );
            discarded_chunks += 1;
            break;
        }

        let section = &decompressed[offset..offset + section_len];
        offset += section_len;

        match decode_chunk_section(section, BLINEAR_HASH_SEED) {
            Ok(chunk) => region.set_chunk(index, chunk)?,
            Err(error) => {
                diagnostics.push(
                    Diagnostic::warning(
                        DiagnosticCode::CorruptChunk,
                        format!("chunk is corrupted and was skipped: {error:#}"),
                    )
                    .with_path(path)
                    .with_region_coords(region_x, region_z)
                    .with_chunk_index(index),
                );
                discarded_chunks += 1;
            }
        }
    }

    ensure!(
        offset == decompressed.len(),
        "blinear_v2 region {} has trailing bytes after slot parsing",
        path.display()
    );

    let mut outcome = ReadOutcome {
        region,
        diagnostics,
        discarded_chunks,
    };
    merge_swap_file(path, &mut outcome)?;
    Ok(outcome)
}

pub fn encode_region_to_writer(
    region: &Region,
    compression_level: i32,
    target: &mut dyn RegionWriteTarget,
) -> Result<Vec<Diagnostic>> {
    encode_region_to_writer_profiled(region, compression_level, target, None)
}

pub fn encode_region_to_writer_profiled(
    region: &Region,
    compression_level: i32,
    target: &mut dyn RegionWriteTarget,
    mut profile: Option<&mut EncodeProfile>,
) -> Result<Vec<Diagnostic>> {
    let main = target.main_file();
    let write_started_at = Instant::now();
    main.write_i64::<BigEndian>(BLINEAR_SUPERBLOCK)?;
    main.write_u8(0x02)?;
    main.write_i64::<BigEndian>(newest_timestamp_millis(region))?;
    main.write_u8(compression_level as u8)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.record_file_write(write_started_at.elapsed());
    }

    if profile.is_some() {
        let mut raw = Vec::new();

        for index in 0..REGION_CHUNK_COUNT {
            if let Some(chunk) = region.chunk(index) {
                let section = encode_chunk_section(chunk, BLINEAR_HASH_SEED)?;
                let section_len =
                    i32::try_from(section.len()).context("blinear_v2 section is too large")?;
                raw.write_i32::<BigEndian>(section_len)?;
                raw.extend_from_slice(&section);
            } else {
                raw.write_i32::<BigEndian>(0)?;
            }
        }

        let compress_started_at = Instant::now();
        let compressed = zstd::bulk::compress(&raw, compression_level)
            .context("failed to zstd-compress the blinear_v2 payload")?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.record_compress(compress_started_at.elapsed());
            profile.record_unit(raw.len(), compressed.len());
        }

        let write_started_at = Instant::now();
        main.write_all(&compressed)?;
        if let Some(profile) = profile {
            profile.record_file_write(write_started_at.elapsed());
        }

        return Ok(Vec::new());
    }

    {
        let mut encoder = zstd::stream::write::Encoder::new(main, compression_level)
            .context("failed to create the blinear_v2 zstd encoder")?;

        for index in 0..REGION_CHUNK_COUNT {
            if let Some(chunk) = region.chunk(index) {
                let section = encode_chunk_section(chunk, BLINEAR_HASH_SEED)?;
                let section_len =
                    i32::try_from(section.len()).context("blinear_v2 section is too large")?;
                encoder.write_i32::<BigEndian>(section_len)?;
                encoder.write_all(&section)?;
            } else {
                encoder.write_i32::<BigEndian>(0)?;
            }
        }

        encoder
            .finish()
            .context("failed to finish the blinear_v2 zstd stream")?;
    }

    Ok(Vec::new())
}

#[cfg(test)]
mod swap_tests {
    use super::*;
    use crate::format::RegionFormat;
    use crate::format::{BLINEAR_HASH_SEED, encode_chunk_section};
    use crate::model::ChunkData;
    use crate::writer::write_region_with_transaction;

    fn chunk(byte: u8, timestamp: i64) -> ChunkData {
        ChunkData {
            timestamp,
            raw_nbt: vec![byte; 300],
        }
    }

    #[test]
    fn swap_file_left_by_a_crash_wins_over_the_older_master() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let master = dir.path().join("r.0.0.b_linear");
        let mut region = Region::new(0, 0);
        region.set_chunk(0, chunk(1, 1_000))?;
        region.set_chunk(1, chunk(2, 5_000))?;
        write_region_with_transaction(RegionFormat::BlinearV2, &master, |target| {
            encode_region_to_writer(&region, 3, target)?;
            Ok(())
        })?;

        // swap: chunk 0 newer, chunk 1 older than the master, chunk 2 only in the swap, chunk 3 corrupt
        let mut header = Vec::new();
        header.extend_from_slice(&SWAP_SUPERBLOCK.to_be_bytes());
        header.push(SWAP_VERSION);
        header.extend_from_slice(&BLINEAR_HASH_SEED.to_be_bytes());
        header.extend_from_slice(&0_i64.to_be_bytes());
        let mut sectors = vec![(0_u64, 0_u64, 0_u8); REGION_CHUNK_COUNT];
        let mut data = Vec::new();
        for (index, chunk) in [
            (0, chunk(9, 2_000)),
            (1, chunk(8, 4_000)),
            (2, chunk(7, 3_000)),
            (3, chunk(6, 3_000)),
        ] {
            let section = encode_chunk_section(&chunk, BLINEAR_HASH_SEED)?;
            let mut stored = (section.len() as i32).to_be_bytes().to_vec();
            stored.extend_from_slice(&lz4_flex::block::compress(&section));
            if index == 3 {
                let last = stored.len() - 1;
                stored[last] ^= 0xFF;
            }
            sectors[index] = (
                (SWAP_HEADER_BYTES + data.len()) as u64,
                stored.len() as u64,
                1,
            );
            data.extend_from_slice(&stored);
        }
        for (offset, length, has) in sectors {
            header.extend_from_slice(&offset.to_be_bytes());
            header.extend_from_slice(&length.to_be_bytes());
            header.push(has);
        }
        header.extend_from_slice(&data);
        std::fs::write(dir.path().join("r.0.0.b_linear.swp"), header)?;

        let outcome = read_region(&master)?;
        assert_eq!(outcome.region.chunk(0).unwrap().raw_nbt, vec![9; 300]);
        assert_eq!(outcome.region.chunk(1).unwrap().raw_nbt, vec![2; 300]);
        assert_eq!(outcome.region.chunk(2).unwrap().raw_nbt, vec![7; 300]);
        assert!(outcome.region.chunk(3).is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.location.chunk_index == Some(3))
        );
        Ok(())
    }
}
