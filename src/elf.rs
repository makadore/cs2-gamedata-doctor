//! Loading an x86-64 ELF image: the sections we care about, the dynamic relocations that give
//! every pointer in `.data.rel.ro` its value (the file itself stores zeros there), and the
//! function ranges recorded in `.eh_frame`. Section headers are validated against the file
//! size, so a truncated or hostile file is an error, never a panic.

use crate::image::Format;
pub use crate::image::{Image, Section, Target};
use anyhow::{anyhow, bail, Context, Result};
use gimli::{BaseAddresses, CieOrFde, EhFrame, LittleEndian, UnwindSection};
use goblin::elf::header::EM_X86_64;
use goblin::elf::reloc::{R_X86_64_64, R_X86_64_GLOB_DAT, R_X86_64_RELATIVE};
use goblin::elf::sym::{STT_FUNC, STT_GNU_IFUNC};
use goblin::elf::Elf;
use std::collections::{HashMap, HashSet};

pub fn parse(data: Vec<u8>, path: String, version: String) -> Result<Image> {
    let elf = Elf::parse(&data).with_context(|| format!("parsing {path} as ELF"))?;
    if !elf.is_64 || elf.header.e_machine != EM_X86_64 {
        bail!("{path}: only x86-64 ELF images are supported");
    }

    let mut text = None;
    let mut drr = None;
    let mut eh_frame = None;
    let mut rodata = Vec::new();
    for sh in &elf.section_headers {
        let name = elf.shdr_strtab.get_at(sh.sh_name).unwrap_or("");
        let wanted = name == ".text"
            || name == ".data.rel.ro"
            || name == ".eh_frame"
            || name.starts_with(".rodata");
        if !wanted {
            continue;
        }
        let end = sh
            .sh_offset
            .checked_add(sh.sh_size)
            .ok_or_else(|| anyhow!("{path}: section {name} overflows"))?;
        if end > data.len() as u64 {
            bail!(
                "{path}: section {name} extends past the end of the file ({end} > {})",
                data.len()
            );
        }
        let hi = sh
            .sh_addr
            .checked_add(sh.sh_size)
            .ok_or_else(|| anyhow!("{path}: section {name}: virtual address range overflows"))?;
        let sec = Section {
            lo: sh.sh_addr,
            hi,
            off: sh.sh_offset as usize,
        };
        match name {
            ".text" => text = Some(sec),
            ".data.rel.ro" => drr = Some(sec),
            ".eh_frame" => eh_frame = Some(sec),
            _ => rodata.push(sec),
        }
    }
    let text = text.ok_or_else(|| anyhow!("{path}: no .text section"))?;
    let drr =
        drr.ok_or_else(|| anyhow!("{path}: no .data.rel.ro section (not a relro PIC image?)"))?;

    let mut relocs = HashMap::with_capacity(elf.dynrelas.len());
    for r in elf.dynrelas.iter() {
        match r.r_type {
            R_X86_64_RELATIVE => {
                relocs.insert(r.r_offset, Target::Addr(r.r_addend.unwrap_or(0) as u64));
            }
            R_X86_64_64 | R_X86_64_GLOB_DAT => {
                let name = elf
                    .dynsyms
                    .get(r.r_sym)
                    .and_then(|s| elf.dynstrtab.get_at(s.st_name))
                    .unwrap_or("?")
                    .to_string();
                relocs.insert(r.r_offset, Target::Sym(name));
            }
            _ => {}
        }
    }
    if relocs.is_empty() {
        bail!("{path}: no dynamic relocations, cannot resolve pointers (RELR-packed relocations are not supported yet)");
    }

    let func_syms: HashSet<String> = elf
        .dynsyms
        .iter()
        .filter(|s| s.st_type() == STT_FUNC || s.st_type() == STT_GNU_IFUNC)
        .filter_map(|s| elf.dynstrtab.get_at(s.st_name).map(str::to_string))
        .collect();

    let img_lo = rodata
        .iter()
        .map(|s| s.lo)
        .chain([text.lo, drr.lo])
        .min()
        .unwrap_or(text.lo);
    let img_hi = rodata
        .iter()
        .map(|s| s.hi)
        .chain([text.hi, drr.hi])
        .max()
        .unwrap_or(drr.hi);
    let mut img = Image {
        path,
        version,
        format: Format::Elf,
        base: 0,
        data,
        text,
        drr,
        rodata,
        relocs,
        func_ranges: HashMap::new(),
        func_syms,
        imports: HashMap::new(),
        img_lo,
        img_hi,
    };
    if let Some(sec) = eh_frame {
        img.func_ranges = fde_ranges(&img, &sec);
    }
    Ok(img)
}

/// Exact function ranges recorded in `.eh_frame`; empty when the section is missing or broken.
fn fde_ranges(img: &Image, sec: &Section) -> HashMap<u64, u64> {
    let mut out = HashMap::new();
    let Some(bytes) = img.section_bytes(sec) else {
        return out;
    };
    let eh = EhFrame::new(bytes, LittleEndian);
    let bases = BaseAddresses::default()
        .set_eh_frame(sec.lo)
        .set_text(img.text.lo);
    let mut entries = eh.entries(&bases);
    loop {
        match entries.next() {
            Ok(Some(CieOrFde::Fde(partial))) => {
                if let Ok(fde) =
                    partial.parse(|section, bases, offset| section.cie_from_offset(bases, offset))
                {
                    if fde.len() > 0 {
                        out.insert(fde.initial_address(), fde.initial_address() + fde.len());
                    }
                }
            }
            Ok(Some(CieOrFde::Cie(_))) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}
