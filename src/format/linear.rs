//! The DivineMC Linear format, version 2 (`spec/SPEC.md`): an append-only log of chunk records and packs of small
//! chunks, with an index published through one of two slots. This module reads files of versions 1 and 2 with the
//! recovery rules of the spec and writes fresh, compacted files of version 2.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use xxhash_rust::xxh64::xxh64;

use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::format::dicts;
use crate::format::{
    EncodeProfile, ReadOutcome, normalize_timestamp_to_millis, parse_region_coords_from_path,
};
use crate::io::read_file_bytes;
use crate::model::{ChunkData, REGION_CHUNK_COUNT, Region};
use crate::writer::RegionWriteTarget;

pub const MAGIC: [u8; 8] = [0x89, b'L', b'I', b'N', 0x0D, 0x0A, 0x1A, 0x0A];
pub const MAJOR_VERSION: u16 = 2;
pub const OLDEST_READABLE_MAJOR: u16 = 1;
pub const MINOR_VERSION: u16 = 0;
pub const HEADER_BYTES: usize = 32;
pub const SLOT_BYTES: usize = 32;
pub const SLOT_A: usize = HEADER_BYTES;
pub const SLOT_B: usize = HEADER_BYTES + SLOT_BYTES;
pub const LOG_START: usize = HEADER_BYTES + 2 * SLOT_BYTES;
pub const RECORD_MAGIC: u32 = 0x4C4E_4352;
pub const RECORD_HEADER_BYTES: usize = 36;
pub const PACK_MAGIC: u32 = 0x4C4E_504B;
pub const PACK_MEMBER_BYTES: usize = 6;
// writer policy from the spec's "Recommendations for writers", not format limits
pub const PACK_MAX_PAYLOAD: usize = 1024;
pub const PACK_MAX_MEMBER_RAW: usize = 64 * 1024;
pub const PACK_MAX_MEMBERS: usize = 64;
pub const PACK_MAX_RAW: usize = 1024 * 1024;
pub const INDEX_MAGIC: u32 = 0x4C4E_4958;
pub const INDEX_HEADER_BYTES: usize = 28;
pub const INDEX_ENTRY_BYTES: usize = 20;
pub const INDEX_BODY_BYTES: usize = REGION_CHUNK_COUNT * INDEX_ENTRY_BYTES;
pub const MAX_RAW_LENGTH: usize = 512 * 1024 * 1024;
const INDEX_COMPRESSION_LEVEL: i32 = 3;

/// True when the bytes start like a Linear 1.x file, or are a file cut off before its header was complete.
pub fn looks_like(bytes: &[u8]) -> bool {
    if bytes.len() >= MAGIC.len() {
        bytes[..MAGIC.len()] == MAGIC
    } else {
        MAGIC.starts_with(bytes)
    }
}

fn be_u16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().unwrap())
}
fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be_u64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    offset: u64,
    length: u32,
    timestamp: i64,
}

/// One chunk as the file stores it, before decompression.
#[derive(Clone, Debug)]
pub struct StoredChunk {
    pub index: usize,
    pub timestamp: i64,
    pub dictionary_id: u32,
    pub raw_length: usize,
    pub record_length: usize,
    pub result: std::result::Result<Vec<u8>, String>,
}

/// The published state of a file: the current generation and every chunk, each decoded or with its error.
#[derive(Debug)]
pub struct FileState {
    pub generation: u64,
    pub chunks: Vec<StoredChunk>,
    pub warnings: Vec<String>,
    pub file_bytes: usize,
    pub live_bytes: usize,
}

impl FileState {
    pub fn garbage_bytes(&self) -> usize {
        self.file_bytes.saturating_sub(LOG_START + self.live_bytes)
    }
}

fn parse_index(bytes: &[u8], generation: u64, index_offset: usize) -> Option<Vec<Entry>> {
    if bytes.len() < INDEX_HEADER_BYTES || be_u32(bytes, 0) != INDEX_MAGIC {
        return None;
    }
    let compressed = be_u32(bytes, 16) as usize;
    if be_u64(bytes, 4) != generation
        || be_u32(bytes, 12) as usize != INDEX_BODY_BYTES
        || compressed != bytes.len() - INDEX_HEADER_BYTES
        || xxh64(&bytes[INDEX_HEADER_BYTES..], 0) != be_u64(bytes, 20)
    {
        return None;
    }
    let body = zstd::bulk::decompress(&bytes[INDEX_HEADER_BYTES..], INDEX_BODY_BYTES).ok()?;
    if body.len() != INDEX_BODY_BYTES {
        return None;
    }
    let mut entries = Vec::with_capacity(REGION_CHUNK_COUNT);
    let mut length_at: HashMap<u64, u32> = HashMap::new();
    let index_offset = index_offset as u64;
    for i in 0..REGION_CHUNK_COUNT {
        let at = i * INDEX_ENTRY_BYTES;
        let entry = Entry {
            offset: be_u64(&body, at),
            length: be_u32(&body, at + 8),
            timestamp: be_u64(&body, at + 12) as i64,
        };
        if entry.offset != 0 {
            if entry.offset < LOG_START as u64
                || entry.offset > index_offset
                || (entry.length as usize) < RECORD_HEADER_BYTES
                || entry.length as u64 > index_offset - entry.offset
            {
                return None;
            }
            // chunks of one pack point at the same record and must agree on its length
            if *length_at.entry(entry.offset).or_insert(entry.length) != entry.length {
                return None;
            }
        }
        entries.push(entry);
    }
    Some(entries)
}

/// `spec/SPEC.md`, "Salvage": the newest checksummed record of every chunk anywhere in the log.
fn salvage(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = vec![
        Entry {
            offset: 0,
            length: 0,
            timestamp: 0
        };
        REGION_CHUNK_COUNT
    ];
    let mut pos = LOG_START;
    while pos + RECORD_HEADER_BYTES <= bytes.len() {
        let magic = be_u32(bytes, pos);
        if magic == RECORD_MAGIC {
            let index = be_u16(bytes, pos + 4) as usize;
            let compressed = be_u32(bytes, pos + 12) as usize;
            let raw = be_u32(bytes, pos + 16) as usize;
            let end = pos + RECORD_HEADER_BYTES + compressed;
            if index < REGION_CHUNK_COUNT
                && raw <= MAX_RAW_LENGTH
                && compressed <= raw + raw / 128 + 1024
                && end <= bytes.len()
                && xxh64(&bytes[pos + RECORD_HEADER_BYTES..end], 0) == be_u64(bytes, pos + 28)
            {
                let timestamp = be_u64(bytes, pos + 20) as i64;
                if entries[index].offset == 0 || timestamp >= entries[index].timestamp {
                    entries[index] = Entry {
                        offset: pos as u64,
                        length: (end - pos) as u32,
                        timestamp,
                    };
                }
                pos = end;
                continue;
            }
        } else if magic == PACK_MAGIC {
            if let Some((length, pack)) = salvage_pack(bytes, pos) {
                for index in pack.members.iter().map(|m| m.0) {
                    let timestamp = pack.timestamp;
                    if entries[index].offset == 0 || timestamp >= entries[index].timestamp {
                        entries[index] = Entry {
                            offset: pos as u64,
                            length: length as u32,
                            timestamp,
                        };
                    }
                }
                pos += length;
                continue;
            }
        } else if magic == INDEX_MAGIC && pos + INDEX_HEADER_BYTES <= bytes.len() {
            let end = pos + INDEX_HEADER_BYTES + be_u32(bytes, pos + 16) as usize;
            if end <= bytes.len()
                && xxh64(&bytes[pos + INDEX_HEADER_BYTES..end], 0) == be_u64(bytes, pos + 20)
            {
                pos = end;
                continue;
            }
        }
        pos += 1;
    }
    entries
}

/// The pack at `pos` and its length, when all of it checks out; salvage accepts nothing less.
fn salvage_pack(bytes: &[u8], pos: usize) -> Option<(usize, Pack)> {
    if pos + RECORD_HEADER_BYTES > bytes.len() {
        return None;
    }
    let end = pos + RECORD_HEADER_BYTES + be_u32(bytes, pos + 12) as usize;
    if end > bytes.len() {
        return None;
    }
    decode_pack(&bytes[pos..end])
        .ok()
        .map(|pack| (end - pos, pack))
}

/// The decompressed chunks of one pack.
#[derive(Debug)]
pub struct Pack {
    pub dictionary_id: u32,
    pub timestamp: i64,
    /// (local index, start, length) of each member's NBT in `raw`
    members: Vec<(usize, usize, usize)>,
    raw: Vec<u8>,
}

impl Pack {
    pub fn member(&self, index: usize) -> std::result::Result<Vec<u8>, String> {
        let at = self
            .members
            .binary_search_by_key(&index, |m| m.0)
            .map_err(|_| format!("index points at a pack that does not hold chunk {index}"))?;
        let (_, start, length) = self.members[at];
        Ok(self.raw[start..start + length].to_vec())
    }
}

/// Checks a pack record and decompresses all of its chunks.
pub fn decode_pack(record: &[u8]) -> std::result::Result<Pack, String> {
    if record.len() < RECORD_HEADER_BYTES || be_u32(record, 0) != PACK_MAGIC {
        return Err("not a pack record".to_string());
    }
    let count = be_u16(record, 4) as usize;
    let dict_id = be_u32(record, 8);
    let compressed = be_u32(record, 12) as usize;
    let raw = be_u32(record, 16) as usize;
    let timestamp = be_u64(record, 20) as i64;
    if count == 0
        || count > REGION_CHUNK_COUNT
        || raw < count * PACK_MEMBER_BYTES
        || raw > MAX_RAW_LENGTH
        || compressed > raw + raw / 128 + 1024
    {
        return Err("pack record claims an impossible size".to_string());
    }
    if RECORD_HEADER_BYTES + compressed != record.len() {
        return Err("pack record has a length that disagrees with the index".to_string());
    }
    let payload = &record[RECORD_HEADER_BYTES..];
    if xxh64(payload, 0) != be_u64(record, 28) {
        return Err("checksum mismatch in pack record".to_string());
    }
    let out = if dict_id == 0 {
        zstd::bulk::decompress(payload, raw)
    } else {
        let dict = dicts::get(dict_id).ok_or_else(|| {
            format!("pack needs zstd dictionary {dict_id}, which this build does not have")
        })?;
        zstd::bulk::Decompressor::with_dictionary(dict).and_then(|mut d| d.decompress(payload, raw))
    }
    .map_err(|e| format!("pack does not decompress: {e}"))?;
    if out.len() != raw {
        return Err(format!(
            "pack decompressed to {} bytes, expected {raw}",
            out.len()
        ));
    }
    let mut members: Vec<(usize, usize, usize)> = Vec::with_capacity(count);
    let mut at = count * PACK_MEMBER_BYTES;
    for i in 0..count {
        let index = be_u16(&out, i * PACK_MEMBER_BYTES) as usize;
        let length = be_u32(&out, i * PACK_MEMBER_BYTES + 2) as usize;
        if index >= REGION_CHUNK_COUNT || members.last().is_some_and(|m| index <= m.0) {
            return Err("pack record has a corrupt member table".to_string());
        }
        members.push((index, at, length));
        at = at.saturating_add(length);
    }
    if at != raw {
        return Err(format!(
            "pack members add up to {at} bytes, header says {raw}"
        ));
    }
    Ok(Pack {
        dictionary_id: dict_id,
        timestamp,
        members,
        raw: out,
    })
}

/// Decodes one record (header and payload) of chunk `index`.
pub fn decode_record(
    record: &[u8],
    index: usize,
) -> std::result::Result<(u32, usize, Vec<u8>), String> {
    if record.len() < RECORD_HEADER_BYTES {
        return Err(format!("record of chunk {index} is truncated"));
    }
    if be_u32(record, 0) != RECORD_MAGIC || be_u16(record, 4) as usize != index {
        return Err(format!(
            "index points at a record that is not chunk {index}"
        ));
    }
    let dict_id = be_u32(record, 8);
    let compressed = be_u32(record, 12) as usize;
    let raw = be_u32(record, 16) as usize;
    if compressed != record.len() - RECORD_HEADER_BYTES {
        return Err(format!(
            "record of chunk {index} has a length that disagrees with the index"
        ));
    }
    if raw > MAX_RAW_LENGTH || compressed > raw + raw / 128 + 1024 {
        return Err(format!(
            "record of chunk {index} claims an impossible size {raw}"
        ));
    }
    let payload = &record[RECORD_HEADER_BYTES..];
    if xxh64(payload, 0) != be_u64(record, 28) {
        return Err(format!("checksum mismatch in chunk {index}"));
    }
    let out = if dict_id == 0 {
        zstd::bulk::decompress(payload, raw)
    } else {
        let dict = dicts::get(dict_id).ok_or_else(|| {
            format!("chunk {index} needs zstd dictionary {dict_id}, which this build does not have")
        })?;
        zstd::bulk::Decompressor::with_dictionary(dict).and_then(|mut d| d.decompress(payload, raw))
    }
    .map_err(|e| format!("chunk {index} does not decompress: {e}"))?;
    if out.len() != raw {
        return Err(format!(
            "chunk {index} decompressed to {} bytes, expected {raw}",
            out.len()
        ));
    }
    Ok((dict_id, raw, out))
}

/// Reads a whole Linear file following the spec's opening rules. Errors only for files that must be refused.
pub fn read_state(path: &Path, bytes: &[u8]) -> Result<FileState> {
    let mut warnings = Vec::new();
    if bytes.len() < LOG_START {
        if bytes.len() >= 8 && be_u64(bytes, 0) == crate::format::LINEAR_SUPERBLOCK {
            bail!("{} is in a legacy Linear layout", path.display());
        }
        // a cut-off creation is a prefix of the magic; anything else is not ours and must not read as empty
        ensure!(
            MAGIC.starts_with(&bytes[..bytes.len().min(MAGIC.len())]),
            "{} is not a Linear file",
            path.display()
        );
        return Ok(FileState {
            generation: 0,
            chunks: Vec::new(),
            warnings,
            file_bytes: bytes.len(),
            live_bytes: 0,
        });
    }
    if be_u64(bytes, 0) == crate::format::LINEAR_SUPERBLOCK {
        bail!("{} is in a legacy Linear layout", path.display());
    }
    ensure!(
        bytes[..8] == MAGIC,
        "{} is not a Linear file",
        path.display()
    );
    ensure!(
        xxh64(&bytes[..24], 0) == be_u64(bytes, 24),
        "{} has a corrupt header",
        path.display()
    );
    let major = be_u16(bytes, 8);
    ensure!(
        (OLDEST_READABLE_MAJOR..=MAJOR_VERSION).contains(&major),
        "{} is Linear format {major}, this build reads {OLDEST_READABLE_MAJOR} to {MAJOR_VERSION}",
        path.display()
    );
    if let Ok((x, z)) = parse_region_coords_from_path(path) {
        let (hx, hz) = (be_u32(bytes, 16) as i32, be_u32(bytes, 20) as i32);
        if (hx, hz) != (x, z) {
            warnings.push(format!("header says region {hx},{hz}"));
        }
    }

    let mut best: Option<(u64, usize, usize, Vec<Entry>)> = None;
    let mut generations = Vec::new();
    for (name, at) in [("A", SLOT_A), ("B", SLOT_B)] {
        let generation = be_u64(bytes, at);
        if generation == 0 {
            continue;
        }
        let offset = be_u64(bytes, at + 8) as usize;
        let length = be_u32(bytes, at + 16) as usize;
        let checksum_ok = xxh64(&bytes[at..at + 24], 0) == be_u64(bytes, at + 24);
        let in_file = offset >= LOG_START
            && length >= INDEX_HEADER_BYTES
            && offset
                .checked_add(length)
                .is_some_and(|end| end <= bytes.len());
        let entries = if checksum_ok && in_file {
            parse_index(&bytes[offset..offset + length], generation, offset)
        } else {
            None
        };
        match entries {
            None => warnings.push(format!(
                "slot {name} (generation {generation}) is not valid"
            )),
            Some(entries) => {
                generations.push(generation);
                if best.as_ref().is_none_or(|b| generation > b.0) {
                    best = Some((generation, offset, length, entries));
                }
            }
        }
    }
    ensure!(
        !(generations.len() == 2 && generations[0] == generations[1]),
        "{} has two slots with generation {}",
        path.display(),
        generations[0]
    );
    let (generation, index_length, entries) = match best {
        Some((generation, _, index_length, entries)) => (generation, index_length, entries),
        // a file as it is created, before its first flush: empty, nothing to salvage
        None if bytes.len() == LOG_START => (
            0,
            0,
            vec![
                Entry {
                    offset: 0,
                    length: 0,
                    timestamp: 0
                };
                REGION_CHUNK_COUNT
            ],
        ),
        None => {
            let damaged = be_u64(bytes, SLOT_A) != 0 || be_u64(bytes, SLOT_B) != 0;
            let entries = salvage(bytes);
            warnings.push(format!(
                "no valid slot; salvaged {} chunks by scanning the log ({})",
                entries.iter().filter(|e| e.offset != 0).count(),
                if damaged {
                    "the slots are damaged"
                } else {
                    "cut off during its first flush"
                }
            ));
            (0, 0, entries)
        }
    };

    let mut chunks = Vec::new();
    let mut live = index_length;
    let mut counted = HashSet::new();
    let mut packs: HashMap<u64, std::result::Result<Pack, String>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.offset == 0 {
            continue;
        }
        if counted.insert(entry.offset) {
            live += entry.length as usize;
        }
        let record = &bytes[entry.offset as usize..entry.offset as usize + entry.length as usize];
        let decoded = if record.len() >= 4 && be_u32(record, 0) == PACK_MAGIC {
            match packs
                .entry(entry.offset)
                .or_insert_with(|| decode_pack(record))
            {
                Ok(pack) => pack
                    .member(index)
                    .map(|raw| (pack.dictionary_id, raw.len(), raw)),
                Err(error) => Err(format!("chunk {index}: {error}")),
            }
        } else {
            decode_record(record, index)
        };
        let (dictionary_id, raw_length) = match &decoded {
            Ok((d, r, _)) => (*d, *r),
            Err(_) => (
                if record.len() >= 12 {
                    be_u32(record, 8)
                } else {
                    0
                },
                0,
            ),
        };
        chunks.push(StoredChunk {
            index,
            timestamp: entry.timestamp,
            dictionary_id,
            raw_length,
            record_length: entry.length as usize,
            result: decoded.map(|(_, _, raw)| raw),
        });
    }
    Ok(FileState {
        generation,
        chunks,
        warnings,
        file_bytes: bytes.len(),
        live_bytes: live,
    })
}

pub fn read_region(path: &Path) -> Result<ReadOutcome> {
    let bytes = read_file_bytes(path)?;
    read_region_bytes(path, &bytes)
}

pub fn read_region_bytes(path: &Path, bytes: &[u8]) -> Result<ReadOutcome> {
    let (region_x, region_z) = parse_region_coords_from_path(path)?;
    let state = read_state(path, bytes)?;
    let mut region = Region::new(region_x, region_z);
    let mut diagnostics = Vec::new();
    let mut discarded_chunks = 0;
    for warning in state.warnings {
        diagnostics.push(
            Diagnostic::warning(DiagnosticCode::InvalidMetadata, warning)
                .with_path(path)
                .with_region_coords(region_x, region_z),
        );
    }
    for chunk in state.chunks {
        match chunk.result {
            Ok(raw_nbt) => region.set_chunk(
                chunk.index,
                ChunkData {
                    timestamp: chunk.timestamp,
                    raw_nbt,
                },
            )?,
            Err(error) => {
                diagnostics.push(
                    Diagnostic::warning(
                        DiagnosticCode::CorruptChunk,
                        format!("chunk is corrupted and was skipped: {error}"),
                    )
                    .with_path(path)
                    .with_region_coords(region_x, region_z)
                    .with_chunk_index(chunk.index),
                );
                discarded_chunks += 1;
            }
        }
    }
    Ok(ReadOutcome {
        region,
        diagnostics,
        discarded_chunks,
    })
}

pub fn header(region_x: i32, region_z: i32) -> [u8; HEADER_BYTES] {
    header_of_version(MAJOR_VERSION, region_x, region_z)
}

pub fn header_of_version(major: u16, region_x: i32, region_z: i32) -> [u8; HEADER_BYTES] {
    let mut out = [0_u8; HEADER_BYTES];
    out[..8].copy_from_slice(&MAGIC);
    out[8..10].copy_from_slice(&major.to_be_bytes());
    out[10..12].copy_from_slice(&MINOR_VERSION.to_be_bytes());
    out[16..20].copy_from_slice(&region_x.to_be_bytes());
    out[20..24].copy_from_slice(&region_z.to_be_bytes());
    let hash = xxh64(&out[..24], 0);
    out[24..32].copy_from_slice(&hash.to_be_bytes());
    out
}

pub fn slot(generation: u64, index_offset: u64, index_length: u32) -> [u8; SLOT_BYTES] {
    let mut out = [0_u8; SLOT_BYTES];
    out[..8].copy_from_slice(&generation.to_be_bytes());
    out[8..16].copy_from_slice(&index_offset.to_be_bytes());
    out[16..20].copy_from_slice(&index_length.to_be_bytes());
    let hash = xxh64(&out[..24], 0);
    out[24..32].copy_from_slice(&hash.to_be_bytes());
    out
}

/// Builds a complete record for chunk `index`.
pub fn encode_record(
    index: usize,
    raw: &[u8],
    timestamp: i64,
    dict_id: u32,
    compressor: &mut zstd::bulk::Compressor<'_>,
) -> Result<Vec<u8>> {
    ensure!(
        raw.len() <= MAX_RAW_LENGTH,
        "chunk {index} is {} bytes, over the format limit",
        raw.len()
    );
    let payload = compressor
        .compress(raw)
        .context("zstd compression failed")?;
    let mut out = Vec::with_capacity(RECORD_HEADER_BYTES + payload.len());
    out.extend_from_slice(&RECORD_MAGIC.to_be_bytes());
    out.extend_from_slice(&(index as u16).to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(&dict_id.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&xxh64(&payload, 0).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Builds a complete pack of `members` (local index, NBT, timestamp), which must be in ascending index order.
pub fn encode_pack(
    members: &[(usize, Vec<u8>, i64)],
    dict_id: u32,
    compressor: &mut zstd::bulk::Compressor<'_>,
) -> Result<Vec<u8>> {
    ensure!(
        !members.is_empty() && members.len() <= REGION_CHUNK_COUNT,
        "a pack holds 1 to {REGION_CHUNK_COUNT} chunks"
    );
    let raw_total: usize =
        members.len() * PACK_MEMBER_BYTES + members.iter().map(|m| m.1.len()).sum::<usize>();
    ensure!(
        raw_total <= MAX_RAW_LENGTH,
        "pack of {raw_total} bytes is over the format limit"
    );
    let mut raw = Vec::with_capacity(raw_total);
    for (i, (index, nbt, _)) in members.iter().enumerate() {
        ensure!(
            i == 0 || *index > members[i - 1].0,
            "pack members must be in ascending order"
        );
        raw.extend_from_slice(&(*index as u16).to_be_bytes());
        raw.extend_from_slice(&(nbt.len() as u32).to_be_bytes());
    }
    for (_, nbt, _) in members {
        raw.extend_from_slice(nbt);
    }
    let newest = members.iter().map(|m| m.2).max().unwrap_or(0);
    let payload = compressor
        .compress(&raw)
        .context("zstd compression failed")?;
    let mut out = Vec::with_capacity(RECORD_HEADER_BYTES + payload.len());
    out.extend_from_slice(&PACK_MAGIC.to_be_bytes());
    out.extend_from_slice(&(members.len() as u16).to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(&dict_id.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    out.extend_from_slice(&newest.to_be_bytes());
    out.extend_from_slice(&xxh64(&payload, 0).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn encode_index(generation: u64, entries: &[(u64, u32, i64)]) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(INDEX_BODY_BYTES);
    for &(offset, length, timestamp) in entries {
        body.extend_from_slice(&offset.to_be_bytes());
        body.extend_from_slice(&(if offset == 0 { 0 } else { length }).to_be_bytes());
        body.extend_from_slice(&(if offset == 0 { 0 } else { timestamp }).to_be_bytes());
    }
    let compressed = zstd::bulk::compress(&body, INDEX_COMPRESSION_LEVEL)?;
    let mut out = Vec::with_capacity(INDEX_HEADER_BYTES + compressed.len());
    out.extend_from_slice(&INDEX_MAGIC.to_be_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&(INDEX_BODY_BYTES as u32).to_be_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(&xxh64(&compressed, 0).to_be_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

pub fn encode_region_to_writer(
    region: &Region,
    compression_level: i32,
    data_type: Option<&str>,
    target: &mut dyn RegionWriteTarget,
    mut profile: Option<&mut EncodeProfile>,
) -> Result<Vec<Diagnostic>> {
    let dict_id = data_type.map(dicts::newest_for).unwrap_or(0);
    let mut compressor = match dicts::get(dict_id) {
        Some(dict) => zstd::bulk::Compressor::with_dictionary(compression_level, dict)?,
        None => zstd::bulk::Compressor::new(compression_level)?,
    };
    let mut out = Output {
        body: Vec::new(),
        entries: vec![(0_u64, 0_u32, 0_i64); REGION_CHUNK_COUNT],
        position: LOG_START as u64,
    };
    // small chunks wait here for a pack; (index, NBT, timestamp, the chunk's own record)
    let mut group: Vec<(usize, Vec<u8>, i64, Vec<u8>)> = Vec::new();
    let mut group_raw = 0_usize;
    for (index, chunk) in region.iter_chunks() {
        let started = Instant::now();
        let timestamp = normalize_timestamp_to_millis(chunk.timestamp);
        let record = encode_record(index, &chunk.raw_nbt, timestamp, dict_id, &mut compressor)?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.record_compress(started.elapsed());
            profile.record_unit(chunk.raw_nbt.len(), record.len());
        }
        if record.len() - RECORD_HEADER_BYTES > PACK_MAX_PAYLOAD
            || chunk.raw_nbt.len() > PACK_MAX_MEMBER_RAW
        {
            out.push(&record, &[(index, timestamp)]);
            continue;
        }
        if group.len() == PACK_MAX_MEMBERS
            || (!group.is_empty() && group_raw + chunk.raw_nbt.len() > PACK_MAX_RAW)
        {
            out.push_group(&mut group, dict_id, &mut compressor)?;
            group_raw = 0;
        }
        group_raw += chunk.raw_nbt.len();
        group.push((index, chunk.raw_nbt.clone(), timestamp, record));
    }
    out.push_group(&mut group, dict_id, &mut compressor)?;
    let Output {
        body,
        entries,
        position,
    } = out;
    let index = encode_index(1, &entries)?;
    let started = Instant::now();
    let file = target.main_file();
    file.write_all(&header(region.region_x, region.region_z))?;
    file.write_all(&slot(1, position, index.len() as u32))?;
    file.write_all(&[0_u8; SLOT_BYTES])?;
    file.write_all(&body)?;
    file.write_all(&index)?;
    if let Some(profile) = profile {
        profile.record_file_write(started.elapsed());
    }
    Ok(Vec::new())
}

struct Output {
    body: Vec<u8>,
    entries: Vec<(u64, u32, i64)>,
    position: u64,
}

impl Output {
    fn push(&mut self, record: &[u8], members: &[(usize, i64)]) {
        for &(index, timestamp) in members {
            self.entries[index] = (self.position, record.len() as u32, timestamp);
        }
        self.position += record.len() as u64;
        self.body.extend_from_slice(record);
    }

    /// Writes the waiting small chunks as one pack, or as its own record when only one waits.
    fn push_group(
        &mut self,
        group: &mut Vec<(usize, Vec<u8>, i64, Vec<u8>)>,
        dict_id: u32,
        compressor: &mut zstd::bulk::Compressor<'_>,
    ) -> Result<()> {
        match group.len() {
            0 => {}
            1 => {
                let (index, _, timestamp, record) = &group[0];
                self.push(record, &[(*index, *timestamp)]);
            }
            _ => {
                let members: Vec<(usize, Vec<u8>, i64)> = group
                    .drain(..)
                    .map(|(index, raw, timestamp, _)| (index, raw, timestamp))
                    .collect();
                let pack = encode_pack(&members, dict_id, compressor)?;
                let pointers: Vec<(usize, i64)> = members.iter().map(|m| (m.0, m.2)).collect();
                self.push(&pack, &pointers);
            }
        }
        group.clear();
        Ok(())
    }
}
