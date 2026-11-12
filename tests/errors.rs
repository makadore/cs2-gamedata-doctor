//! Hostile or broken input must come back as an error, never as a panic.

use gamedata_doctor::elf::Image;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn parse(data: Vec<u8>) -> anyhow::Result<Image> {
    Image::parse(data, "input".into(), "?".into())
}

#[test]
fn not_an_elf_file() {
    let err = parse(b"hello, this is not an ELF image at all".to_vec())
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("ELF"), "{err}");
}

#[test]
fn truncated_image_is_an_error_not_a_panic() {
    let whole = fixture("old.so");
    for cut in [64usize, 1000, 3000, whole.len() / 2, whole.len() - 100] {
        let r = parse(whole[..cut].to_vec());
        assert!(
            r.is_err(),
            "a file cut at {cut} bytes parsed as a valid image"
        );
    }
}

/// Find a section header by name and return the file offset of its sh_size field.
fn section_size_field(data: &[u8], wanted: &str) -> usize {
    let u16_at = |o: usize| u16::from_le_bytes(data[o..o + 2].try_into().unwrap()) as usize;
    let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as usize;
    let u64_at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap()) as usize;
    let shoff = u64_at(0x28);
    let (shentsize, shnum, shstrndx) = (u16_at(0x3a), u16_at(0x3c), u16_at(0x3e));
    let strtab_off = u64_at(shoff + shstrndx * shentsize + 0x18);
    for i in 0..shnum {
        let h = shoff + i * shentsize;
        let name_off = strtab_off + u32_at(h);
        let end = data[name_off..].iter().position(|&c| c == 0).unwrap() + name_off;
        if &data[name_off..end] == wanted.as_bytes() {
            return h + 0x20;
        }
    }
    panic!("section {wanted} not found");
}

#[test]
fn a_virtual_address_range_that_overflows_is_rejected() {
    // file range stays valid, only sh_addr is pushed next to u64::MAX
    let mut data = fixture("old.so");
    let addr_field = section_size_field(&data, ".text") - 0x10;
    data[addr_field..addr_field + 8].copy_from_slice(&(u64::MAX - 16).to_le_bytes());
    let err = parse(data)
        .err()
        .expect("overflowing sh_addr + sh_size must be rejected");
    assert!(err.to_string().contains("overflows"), "{err}");
}

#[test]
fn a_section_declared_past_the_end_of_the_file_is_rejected() {
    for sec in [".text", ".data.rel.ro", ".rodata"] {
        let mut data = fixture("old.so");
        let size_field = section_size_field(&data, sec);
        data[size_field..size_field + 8].copy_from_slice(&0x7fff_ffff_u64.to_le_bytes());
        let err = parse(data)
            .err()
            .unwrap_or_else(|| panic!("{sec} past EOF must be rejected"));
        assert!(err.to_string().contains("past the end"), "{sec}: {err}");
    }
}

#[test]
fn a_section_size_that_overflows_is_rejected() {
    let mut data = fixture("old.so");
    let size_field = section_size_field(&data, ".text");
    data[size_field..size_field + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(parse(data).is_err());
}

#[test]
fn the_fixture_itself_still_loads() {
    let img = parse(fixture("old.so")).expect("old.so is a valid image");
    assert!(img.relocs.len() > 50);
}
