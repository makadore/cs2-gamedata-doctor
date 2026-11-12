//! Hostile or truncated PE files are errors, never panics.

use gamedata_doctor::image::Image;
use std::path::PathBuf;

fn dll() -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/old.dll")).unwrap()
}

fn parse(d: Vec<u8>) -> anyhow::Result<Image> {
    Image::parse(d, "x.dll".into(), "?".into())
}

#[test]
fn truncated_files_are_errors() {
    let full = dll();
    for cut in [2usize, 64, 200, 600, 1024, 2048, 4096, 9000, full.len() - 1] {
        let d = full[..cut.min(full.len())].to_vec();
        assert!(parse(d).is_err(), "cut at {cut} must not parse");
    }
}

#[test]
fn a_pe_for_another_architecture_is_refused() {
    // COFF header Machine field: AMD64 is 0x8664; stamp ARM64 (0xAA64) on the fixture
    let mut d = dll();
    let pe = u32::from_le_bytes(d[0x3c..0x40].try_into().unwrap()) as usize;
    d[pe + 4..pe + 6].copy_from_slice(&0xAA64u16.to_le_bytes());
    let err = match parse(d) {
        Ok(_) => panic!("an ARM64 image must not be decoded as x86-64"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("x86-64"), "{err}");
}

#[test]
fn garbage_after_the_mz_magic_is_an_error() {
    assert!(parse(b"MZ this is not a PE file at all".to_vec()).is_err());
}

#[test]
fn a_section_past_the_end_of_the_file_is_an_error() {
    let mut d = dll();
    let pe = u32::from_le_bytes(d[0x3c..0x40].try_into().unwrap()) as usize;
    let opt_size = u16::from_le_bytes(d[pe + 20..pe + 22].try_into().unwrap()) as usize;
    let sh = pe + 24 + opt_size; // first section header; SizeOfRawData at +16
    d[sh + 16..sh + 20].copy_from_slice(&0x7fff_fff0u32.to_le_bytes());
    let err = match parse(d) {
        Ok(_) => panic!("a section past the end of the file must not parse"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("past the end") || err.contains("parsing"),
        "{err}"
    );
}
