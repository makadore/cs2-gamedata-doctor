//! A loaded x86-64 image in either of the two formats the tool reads: a Linux ELF shared object
//! (`libserver.so`) or a Windows PE DLL (`server.dll`). The format-specific loaders in `elf.rs`
//! and `pe.rs` fill the same structure: the code section, the section that holds the vtables
//! (relocated pointers), the sections where type names live, every relocated pointer with its
//! target, and the exact function ranges the binary records (`.eh_frame` or `.pdata`).

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// What a vtable slot (a relocated pointer, or the lack of one) resolves to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// An address inside this image.
    Addr(u64),
    /// An imported symbol (for vtables typically `__cxa_pure_virtual`).
    Sym(String),
    /// A null entry: GCC zeroes the destructor slots of abstract classes.
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// ELF with Itanium C++ ABI RTTI (GCC/Clang).
    Elf,
    /// PE with MSVC RTTI.
    Pe,
}

impl Format {
    /// The gamedata key this format's indices live under.
    pub fn os(self) -> &'static str {
        match self {
            Format::Elf => "linux",
            Format::Pe => "windows",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub lo: u64,
    pub hi: u64,
    pub off: usize,
}

impl Section {
    pub fn contains(&self, v: u64) -> bool {
        v >= self.lo && v < self.hi
    }

    pub fn offset_of(&self, v: u64) -> usize {
        self.off + (v - self.lo) as usize
    }
}

pub struct Image {
    pub path: String,
    pub version: String,
    pub format: Format,
    /// Preferred load address: the PE image base, 0 for a position-independent ELF.
    pub base: u64,
    pub data: Vec<u8>,
    pub text: Section,
    /// The section holding the vtables: `.data.rel.ro` in ELF, `.rdata` in PE.
    pub drr: Section,
    /// Sections scanned for type names: `.rodata*` in ELF, `.data` and `.rdata` in PE.
    pub rodata: Vec<Section>,
    /// Every relocated pointer location -> what it points at.
    pub relocs: HashMap<u64, Target>,
    /// Exact function ranges the binary records (start -> end): `.eh_frame` FDEs in ELF, the
    /// `.pdata` exception table in PE. Functions missing here get a heuristic end.
    pub func_ranges: HashMap<u64, u64>,
    /// Imported or exported symbols that are functions (a vtable slot may point at one).
    pub func_syms: HashSet<String>,
    /// PE import address table: entry address -> imported name, so that an import thunk
    /// (`jmp [rip+x]`) a slot points at can be named (`_purecall` above all).
    pub imports: HashMap<u64, String>,
    pub img_lo: u64,
    pub img_hi: u64,
}

impl Image {
    pub fn load(path: &Path) -> Result<Image> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(data, path.display().to_string(), read_version(path))
    }

    /// Dispatch on the file's magic: `\x7fELF` or `MZ`.
    pub fn parse(data: Vec<u8>, path: String, version: String) -> Result<Image> {
        if data.starts_with(b"\x7fELF") {
            crate::elf::parse(data, path, version)
        } else if data.starts_with(b"MZ") {
            crate::pe::parse(data, path, version)
        } else {
            bail!("{path}: neither an ELF nor a PE image");
        }
    }

    pub fn in_text(&self, v: u64) -> bool {
        self.text.contains(v)
    }

    /// True for a symbol a vtable slot may legitimately point at (`__cxa_pure_virtual` and
    /// friends), false for data symbols such as libstdc++'s typeinfo vtables.
    pub fn is_func_sym(&self, name: &str) -> bool {
        self.func_syms.contains(name)
    }

    /// True when a constant falls inside the image: such immediates are addresses, not values.
    pub fn addr_like(&self, v: u64) -> bool {
        v >= self.img_lo && v < self.img_hi
    }

    /// The whole section as stored in the file.
    pub fn section_bytes(&self, sec: &Section) -> Option<&[u8]> {
        self.data.get(sec.off..sec.off + (sec.hi - sec.lo) as usize)
    }

    /// `len` bytes of a section starting at a virtual address, clipped to the section.
    pub fn slice(&self, sec: &Section, v: u64, len: usize) -> Option<&[u8]> {
        if !sec.contains(v) {
            return None;
        }
        let off = sec.offset_of(v);
        let end = off + len.min((sec.hi - v) as usize);
        self.data.get(off..end)
    }

    /// Raw little-endian qword as stored in the file (no relocation applied).
    pub fn raw_u64(&self, sec: &Section, v: u64) -> Option<u64> {
        if !sec.contains(v) || v + 8 > sec.hi {
            return None;
        }
        let o = sec.offset_of(v);
        self.data
            .get(o..o + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// Raw little-endian dword as stored in the file.
    pub fn raw_u32(&self, sec: &Section, v: u64) -> Option<u32> {
        if !sec.contains(v) || v + 4 > sec.hi {
            return None;
        }
        let o = sec.offset_of(v);
        self.data
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    /// The section among `rodata` and `drr` that holds `v`, if any.
    pub fn data_section(&self, v: u64) -> Option<&Section> {
        if self.drr.contains(v) {
            return Some(&self.drr);
        }
        self.rodata.iter().find(|s| s.contains(v))
    }
}

/// PatchVersion from a steam.inf next to the binary or in the usual game/csgo locations
/// (`game/csgo/bin/linuxsteamrt64`, `game/bin/linuxsteamrt64`, `game/csgo/bin/win64`, `game/bin/win64`).
pub fn read_version(path: &Path) -> String {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        dir.join("steam.inf"),
        dir.join("../../steam.inf"),
        dir.join("../../csgo/steam.inf"),
        dir.join("../../../csgo/steam.inf"),
    ];
    for cand in candidates {
        if let Ok(s) = std::fs::read_to_string(&cand) {
            for line in s.lines() {
                if let Some(v) = line.trim().strip_prefix("PatchVersion=") {
                    return v.to_string();
                }
            }
        }
    }
    "?".to_string()
}
