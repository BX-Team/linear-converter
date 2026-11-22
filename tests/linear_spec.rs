//! Cross-checks this implementation of the Linear format against the reference files in `spec/` (copied from
//! DivineMC's `linear-format/` by `scripts/sync-spec.sh`).

use std::path::{Path, PathBuf};

use linear_converter::format::dicts;
use linear_converter::format::linear;
use sha2::{Digest, Sha256};

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn shipped_dictionaries_match_the_spec_checksums() {
    let sums = std::fs::read_to_string(spec_dir().join("dicts/SHA256SUMS")).unwrap();
    let mut checked = 0;
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let sum = parts.next().unwrap();
        let name = parts.next().unwrap();
        let id: u32 = name.split('-').next().unwrap().parse().unwrap();
        let dict =
            dicts::get(id).unwrap_or_else(|| panic!("dictionary {id} ({name}) is not compiled in"));
        assert_eq!(hex(&Sha256::digest(dict)), sum, "{name}");
        assert_eq!(
            u32::from_le_bytes(dict[4..8].try_into().unwrap()),
            id,
            "{name} carries a different dictionary ID"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        dicts::DICTIONARIES.len(),
        "SHA256SUMS and the compiled-in list differ"
    );
}

#[test]
fn golden_files_read_as_the_manifest_says() {
    let dir = spec_dir().join("golden");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    for spec in manifest["files"].as_array().unwrap() {
        let name = spec["file"].as_str().unwrap();
        let path = dir.join(name);
        let bytes = std::fs::read(&path).unwrap();
        let result = linear::read_state(&path, &bytes);
        match spec["open"].as_str().unwrap() {
            "legacy" => {
                let error = result.expect_err(name);
                assert!(format!("{error:#}").contains("legacy"), "{name}: {error:#}");
            }
            "error" => {
                result.expect_err(name);
            }
            "ok" => {
                let state = result.unwrap_or_else(|e| panic!("{name}: {e:#}"));
                let expected = spec["chunks"].as_array().unwrap();
                assert_eq!(state.chunks.len(), expected.len(), "{name}: chunk count");
                for (chunk, want) in state.chunks.iter().zip(expected) {
                    assert_eq!(
                        chunk.index as u64,
                        want["index"].as_u64().unwrap(),
                        "{name}"
                    );
                    assert_eq!(
                        chunk.timestamp,
                        want["timestamp"].as_i64().unwrap(),
                        "{name} chunk {}",
                        chunk.index
                    );
                    if want.get("error").is_some() {
                        assert!(
                            chunk.result.is_err(),
                            "{name} chunk {} must fail to read",
                            chunk.index
                        );
                        continue;
                    }
                    let raw = chunk
                        .result
                        .as_ref()
                        .unwrap_or_else(|e| panic!("{name} chunk {}: {e}", chunk.index));
                    assert_eq!(
                        raw.len() as u64,
                        want["length"].as_u64().unwrap(),
                        "{name} chunk {}",
                        chunk.index
                    );
                    assert_eq!(
                        hex(&Sha256::digest(raw)),
                        want["sha256"].as_str().unwrap(),
                        "{name} chunk {}",
                        chunk.index
                    );
                }
            }
            other => panic!("unknown expectation {other}"),
        }
    }
}

#[test]
fn written_files_read_back_and_carry_the_dictionary() {
    use linear_converter::model::{ChunkData, Region};
    let dir = tempfile::tempdir().unwrap();
    let mut region = Region::new(3, -4);
    for index in [0_usize, 17, 1023] {
        let raw: Vec<u8> = (0..5000).map(|i| ((i * 31 + index) % 97) as u8).collect();
        region
            .set_chunk(
                index,
                ChunkData {
                    timestamp: 1_700_000_000_000 + index as i64,
                    raw_nbt: raw,
                },
            )
            .unwrap();
    }
    let out = dir.path().join("region").join("r.3.-4.linear");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    linear_converter::writer::write_region_with_transaction(
        linear_converter::format::RegionFormat::Linear,
        &out,
        |target| {
            linear_converter::format::encode_region_to_writer_profiled(
                &region,
                linear_converter::format::RegionFormat::Linear,
                6,
                Some("region"),
                target,
                None,
            )?;
            Ok(())
        },
    )
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let state = linear::read_state(&out, &bytes).unwrap();
    assert_eq!(state.generation, 1);
    assert_eq!(state.garbage_bytes(), 0);
    for chunk in &state.chunks {
        assert_eq!(chunk.dictionary_id, dicts::newest_for("region"));
        assert_eq!(
            chunk.result.as_ref().unwrap(),
            &region.chunk(chunk.index).unwrap().raw_nbt
        );
        assert_eq!(
            chunk.timestamp,
            region.chunk(chunk.index).unwrap().timestamp
        );
    }
}

#[test]
fn small_chunks_are_written_as_packs() {
    use linear_converter::model::{ChunkData, Region};
    let dir = tempfile::tempdir().unwrap();
    let mut region = Region::new(0, 0);
    // void-world chunks: kilobytes of NBT that differ only in their coordinates
    for index in 0..300_usize {
        let mut raw = vec![0_u8; 4000];
        raw[..4].copy_from_slice(&(index as u32).to_be_bytes());
        region
            .set_chunk(
                index * 3,
                ChunkData {
                    timestamp: 1_700_000_000_000 + index as i64,
                    raw_nbt: raw,
                },
            )
            .unwrap();
    }
    let big: Vec<u8> = (0..200_000_u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    region
        .set_chunk(
            1000,
            ChunkData {
                timestamp: 1_700_000_000_500,
                raw_nbt: big.clone(),
            },
        )
        .unwrap();
    let out = dir.path().join("region").join("r.0.0.linear");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    linear_converter::writer::write_region_with_transaction(
        linear_converter::format::RegionFormat::Linear,
        &out,
        |target| {
            linear_converter::format::encode_region_to_writer_profiled(
                &region,
                linear_converter::format::RegionFormat::Linear,
                6,
                Some("region"),
                target,
                None,
            )?;
            Ok(())
        },
    )
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[8..10], &linear::MAJOR_VERSION.to_be_bytes());
    let packs = (linear::LOG_START..bytes.len() - 4)
        .filter(|&i| bytes[i..i + 4] == linear::PACK_MAGIC.to_be_bytes())
        .count();
    assert!(packs >= 300 / linear::PACK_MAX_MEMBERS, "{packs} packs");
    let state = linear::read_state(&out, &bytes).unwrap();
    assert_eq!(state.garbage_bytes(), 0);
    assert_eq!(state.chunks.len(), 301);
    for chunk in &state.chunks {
        let original = region.chunk(chunk.index).unwrap();
        assert_eq!(
            chunk.result.as_ref().unwrap(),
            &original.raw_nbt,
            "chunk {}",
            chunk.index
        );
        assert_eq!(chunk.timestamp, original.timestamp, "chunk {}", chunk.index);
    }
    assert!(
        bytes.len() < 300 * 40 + big.len() + 4096,
        "{} bytes",
        bytes.len()
    );
}

#[test]
fn a_freshly_created_file_is_empty_without_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.0.0.linear");
    let mut bytes = linear::header(0, 0).to_vec();
    bytes.resize(linear::LOG_START, 0);
    let state = linear::read_state(&path, &bytes).unwrap();
    assert!(state.chunks.is_empty());
    assert!(state.warnings.is_empty(), "{:?}", state.warnings);
}
