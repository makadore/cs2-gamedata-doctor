//! Loading an x86-64 PE image (`server.dll`): sections, the base relocation table that marks
//! every absolute pointer (vtable slots, RTTI pointers), and the `.pdata` exception table that
//! records every function's exact range. Headers are validated against the file size.

use crate::image::{Format, Image, Section, Target};
use anyhow::{anyhow, bail, Context, Result};
use goblin::pe::PE;
use std::collections::{HashMap, HashSet};

const IMAGE_REL_BASED_DIR64: u16 = 10;

pub fn parse(data: Vec<u8>, path: String, version: String) -> Result<Image> {
    let pe = PE::parse(&data).with_context(|| format!("parsing {path} as PE"))?;
    const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
    if !pe.is_64 || pe.header.coff_header.machine != IMAGE_FILE_MACHINE_AMD64 {
        bail!(
            "{path}: only x86-64 PE images are supported (machine {:#06x})",
            pe.header.coff_header.machine
        );
    }
    let base = pe.image_base as u64;

    let mut text = None;
    let mut rdata = None;
    let mut pdata = None;
    let mut reloc = None;
    let mut names = Vec::new();
    for s in &pe.sections {
        let name = s.name().unwrap_or("");
        let size = s
            .size_of_raw_data
            .min(s.virtual_size.max(s.size_of_raw_data)) as u64;
        let raw = s.size_of_raw_data as u64;
        let end = (s.pointer_to_raw_data as u64)
            .checked_add(raw)
            .ok_or_else(|| anyhow!("{path}: section {name} overflows"))?;
        if end > data.len() as u64 {
            bail!(
                "{path}: section {name} extends past the end of the file ({end} > {})",
                data.len()
            );
        }
        let lo = base
            .checked_add(s.virtual_address as u64)
            .ok_or_else(|| anyhow!("{path}: section {name}: address overflows"))?;
        // bytes beyond the raw data are zero in memory; we only read what the file holds
        let sec = Section {
            lo,
            hi: lo + size.min(raw),
            off: s.pointer_to_raw_data as usize,
        };
        match name {
            ".text" => text = Some(sec),
            ".rdata" => rdata = Some(sec),
            ".pdata" => pdata = Some(sec),
            ".reloc" => reloc = Some(sec),
            ".data" => names.push(sec),
            _ => {}
        }
    }
    let text = text.ok_or_else(|| anyhow!("{path}: no .text section"))?;
    let rdata = rdata.ok_or_else(|| anyhow!("{path}: no .rdata section"))?;
    let reloc = reloc.ok_or_else(|| {
        anyhow!("{path}: no .reloc section, cannot find the image's absolute pointers")
    })?;
    names.push(rdata);

    let img_lo = pe
        .sections
        .iter()
        .map(|s| base + s.virtual_address as u64)
        .min()
        .unwrap_or(base);
    let img_hi = pe
        .sections
        .iter()
        .map(|s| base + s.virtual_address as u64 + s.virtual_size.max(s.size_of_raw_data) as u64)
        .max()
        .unwrap_or(base);

    // import address table entries by address, so a thunk `jmp [rip+x]` can be named: the k-th
    // name of a descriptor's lookup table belongs to the k-th qword of its address table
    let mut imports: HashMap<u64, String> = HashMap::new();
    if let Some(id) = &pe.import_data {
        for d in &id.import_data {
            let iat = base + d.import_directory_entry.import_address_table_rva as u64;
            let Some(lut) = &d.import_lookup_table else {
                continue;
            };
            for (k, e) in lut.iter().enumerate() {
                if let goblin::pe::import::SyntheticImportLookupTableEntry::HintNameTableRVA((
                    _,
                    hn,
                )) = e
                {
                    imports.insert(iat + 8 * k as u64, hn.name.to_string());
                }
            }
        }
    }
    let func_syms: HashSet<String> = imports.values().cloned().collect();

    let mut img = Image {
        path,
        version,
        format: Format::Pe,
        base,
        data,
        text,
        drr: rdata,
        rodata: names,
        relocs: HashMap::new(),
        func_ranges: HashMap::new(),
        func_syms,
        imports,
        img_lo,
        img_hi,
    };

    // base relocations: blocks of { page_rva u32, block_size u32, entries u16[] }
    let mut relocs = HashMap::new();
    if let Some(bytes) = img.section_bytes(&reloc) {
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            let page = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as u64;
            let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if size < 8 || pos + size > bytes.len() {
                break;
            }
            for e in bytes[pos + 8..pos + size].as_chunks::<2>().0 {
                let entry = u16::from_le_bytes(*e);
                if entry >> 12 != IMAGE_REL_BASED_DIR64 {
                    continue;
                }
                let at = base + page + (entry & 0xfff) as u64;
                let Some(sec) = img.data_section(at) else {
                    continue;
                };
                if let Some(v) = img.raw_u64(sec, at) {
                    if v >= img_lo && v < img_hi {
                        relocs.insert(at, Target::Addr(v));
                    }
                }
            }
            pos += size;
        }
    }
    if relocs.is_empty() {
        bail!("{}: the base relocation table is empty", img.path);
    }
    img.relocs = relocs;

    // .pdata: RUNTIME_FUNCTION { begin rva, end rva, unwind rva }. A function the compiler split
    // into several unwind regions has one primary entry and chained ones (UNW_FLAG_CHAININFO in
    // the unwind info's first byte); a chained region that follows its predecessor without a gap
    // extends that function, any other chained region is a detached part and starts nothing.
    const UNW_FLAG_CHAININFO: u8 = 0x4;
    let mut entries: Vec<(u64, u64, bool)> = Vec::new();
    if let Some(sec) = pdata {
        if let Some(bytes) = img.section_bytes(&sec) {
            for e in bytes.as_chunks::<12>().0 {
                let b = u32::from_le_bytes(e[0..4].try_into().unwrap()) as u64;
                let n = u32::from_le_bytes(e[4..8].try_into().unwrap()) as u64;
                let u = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
                if b == 0 || n <= b || !img.in_text(base + b) {
                    continue;
                }
                let chained = img
                    .data_section(base + u)
                    .and_then(|s| img.slice(s, base + u, 1))
                    .and_then(|x| x.first().copied())
                    .map(|first| (first >> 3) & UNW_FLAG_CHAININFO != 0)
                    .unwrap_or(false);
                entries.push((base + b, base + n, chained));
            }
        }
    }
    entries.sort_unstable();
    let mut ranges: HashMap<u64, u64> = HashMap::new();
    let mut current: Option<(u64, u64)> = None;
    for (s, e, chained) in entries {
        match (&mut current, chained) {
            (Some((_, end)), true) if s == *end => *end = e,
            (_, true) => {}
            (cur, false) => {
                if let Some((cs, ce)) = cur.take() {
                    ranges.entry(cs).or_insert(ce);
                }
                *cur = Some((s, e));
            }
        }
    }
    if let Some((cs, ce)) = current {
        ranges.entry(cs).or_insert(ce);
    }
    img.func_ranges = ranges;
    Ok(img)
}
