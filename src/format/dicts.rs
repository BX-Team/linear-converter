//! The zstd dictionaries of the Linear format (`spec/SPEC.md`, "Dictionaries"), compiled in. A new Minecraft version
//! adds a line to `DICTIONARIES`; lines are never removed.

/// (ID, data type, Minecraft version, bytes)
pub const DICTIONARIES: &[(u32, &str, &str, &[u8])] = &[
    (
        1,
        "region",
        "26.3",
        include_bytes!("../../spec/dicts/1-region-26.3.zdict"),
    ),
    (
        2,
        "entities",
        "26.3",
        include_bytes!("../../spec/dicts/2-entities-26.3.zdict"),
    ),
    (
        3,
        "poi",
        "26.3",
        include_bytes!("../../spec/dicts/3-poi-26.3.zdict"),
    ),
];

pub fn get(id: u32) -> Option<&'static [u8]> {
    if id == 0 {
        return None;
    }
    DICTIONARIES.iter().find(|d| d.0 == id).map(|d| d.3)
}

/// The dictionary new chunks of a data type (the region folder name) are written with, or 0.
pub fn newest_for(data_type: &str) -> u32 {
    DICTIONARIES
        .iter()
        .filter(|d| d.1 == data_type)
        .map(|d| d.0)
        .max()
        .unwrap_or(0)
}
