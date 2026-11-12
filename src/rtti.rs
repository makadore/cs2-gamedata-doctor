//! Itanium C++ ABI RTTI walk: mangled type names in .rodata -> typeinfo objects -> vtables.
//!
//! A vtable group in `.data.rel.ro` looks like `offset_to_top | typeinfo* | slot0 | slot1 ...`.
//! `offset_to_top` is plain data (no relocation), the typeinfo pointer and every slot are
//! relocated pointers, so the layout can be recognised without any symbol.
//!
//! The typeinfo objects also record the class hierarchy (`__si_class_type_info` for a single
//! base, `__vmi_class_type_info` with per-base offsets and flags), which is what places an
//! interface's table inside an implementation.

use crate::image::{Format, Image, Target};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug)]
pub struct VTable {
    pub class: String,
    pub offset_to_top: i64,
    pub addr: u64,
    pub slots: Vec<Target>,
}

/// One direct base recorded in a typeinfo object.
#[derive(Clone, Copy, Debug)]
pub struct BaseRef {
    pub ti: u64,
    /// Subobject offset for a non-virtual base; for a virtual base the ABI stores a vtable
    /// offset instead, which this tool does not follow.
    pub offset: i64,
    pub is_virtual: bool,
}

/// Where a base subobject lives inside a class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BaseLookup {
    NotABase,
    /// Reached only through a virtual base: the offset is not known statically.
    Virtual,
    /// The class's hierarchy record is damaged: what its bases are is not known.
    Damaged,
    NonVirtual(i64),
}

pub struct Rtti {
    pub vtables: Vec<VTable>,
    /// Class name -> index into `vtables` of its primary (offset_to_top == 0) table.
    pub primary: BTreeMap<String, usize>,
    /// Class names that own more than one primary table (several classes demangle to the same
    /// name, or construction vtables): their index cannot be trusted.
    pub ambiguous: BTreeSet<String>,
    /// Sorted, deduplicated addresses of every function a vtable slot points at.
    pub func_starts: Vec<u64>,
    /// Class name -> typeinfo address (the first one when names collide).
    pub class_ti: HashMap<String, u64>,
    /// typeinfo address -> class name.
    pub ti_name: HashMap<u64, String>,
    /// typeinfo address -> direct bases.
    pub bases: HashMap<u64, Vec<BaseRef>>,
    /// Classes whose hierarchy record could not be read whole (a damaged image): any question
    /// about their bases is answered `Damaged`, never from a partial list.
    pub damaged: BTreeSet<String>,
    pub n_names: usize,
    pub n_typeinfo: usize,
}

fn looks_mangled(t: &[u8]) -> bool {
    if t.len() < 3 || t.len() > 256 || !t.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_') {
        return false;
    }
    (b'1'..=b'9').contains(&t[0]) || (t[0] == b'N' && *t.last().unwrap() == b'E')
}

fn demangle(t: &str) -> String {
    if let Ok(sym) = cpp_demangle::Symbol::new(format!("_ZTS{t}").into_bytes()) {
        let s = sym.to_string();
        if let Some(rest) = s.strip_prefix("typeinfo name for ") {
            return rest.to_string();
        }
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if let Ok(n) = digits.parse::<usize>() {
        let rest = &t[digits.len()..];
        if rest.len() == n {
            return rest.to_string();
        }
    }
    t.to_string()
}

/// Choose each class's primary table; a class with several of them is ambiguous.
pub(crate) fn pick_primary(vtables: &[VTable]) -> (BTreeMap<String, usize>, BTreeSet<String>) {
    let mut primary = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for (i, vt) in vtables.iter().enumerate() {
        if vt.offset_to_top != 0 {
            continue;
        }
        if primary.insert(vt.class.clone(), i).is_some() {
            ambiguous.insert(vt.class.clone());
        }
    }
    // keep the first table for ambiguous names so reports can still describe it
    for (i, vt) in vtables.iter().enumerate() {
        if vt.offset_to_top == 0 && ambiguous.contains(&vt.class) {
            primary.insert(vt.class.clone(), i);
            break;
        }
    }
    (primary, ambiguous)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TiKind {
    Plain,
    Single,
    Multi,
    Unknown,
}

/// What kind of typeinfo object sits at `p`, from the libstdc++ vtable its vptr points at:
/// an imported symbol when libstdc++ is dynamic, a local vtable when it is linked statically.
fn ti_kind(img: &Image, p: u64, vtable_names: &HashMap<u64, &str>) -> TiKind {
    let name: &str = match img.relocs.get(&p) {
        Some(Target::Sym(s)) => s,
        Some(Target::Addr(a)) => vtable_names.get(a).copied().unwrap_or(""),
        _ => "",
    };
    if name.contains("__si_class_type_info") {
        TiKind::Single
    } else if name.contains("__vmi_class_type_info") {
        TiKind::Multi
    } else if name.contains("__class_type_info") {
        TiKind::Plain
    } else {
        TiKind::Unknown
    }
}

/// The RTTI of an image in whichever ABI it was compiled for.
pub fn analyze(img: &Image) -> Rtti {
    match img.format {
        Format::Elf => analyze_itanium(img),
        Format::Pe => crate::msvc::analyze(img),
    }
}

/// Build the vtable set from the sorted tables, picking primaries and function starts.
pub(crate) fn finish(
    vtables: Vec<VTable>,
    class_ti: HashMap<String, u64>,
    ti_name: HashMap<u64, String>,
    bases: HashMap<u64, Vec<BaseRef>>,
    n_names: usize,
    n_typeinfo: usize,
) -> Rtti {
    let (primary, ambiguous) = pick_primary(&vtables);
    let mut func_starts: Vec<u64> = vtables
        .iter()
        .flat_map(|v| v.slots.iter())
        .filter_map(|t| {
            if let Target::Addr(a) = t {
                Some(*a)
            } else {
                None
            }
        })
        .collect();
    func_starts.sort_unstable();
    func_starts.dedup();
    Rtti {
        vtables,
        primary,
        ambiguous,
        func_starts,
        class_ti,
        ti_name,
        bases,
        damaged: BTreeSet::new(),
        n_names,
        n_typeinfo,
    }
}

fn analyze_itanium(img: &Image) -> Rtti {
    // 1. candidate type names
    let mut names: HashMap<u64, String> = HashMap::new();
    for sec in &img.rodata {
        let Some(b) = img.section_bytes(sec) else {
            continue;
        };
        let mut pos = 0usize;
        while pos < b.len() {
            let end = match b[pos..].iter().position(|&c| c == 0) {
                Some(e) => pos + e,
                None => break,
            };
            let tok = &b[pos..end];
            if looks_mangled(tok) {
                names.insert(
                    sec.lo + pos as u64,
                    demangle(std::str::from_utf8(tok).unwrap()),
                );
            }
            pos = end + 1;
        }
    }

    // 2. typeinfo objects: { vptr (reloc), name pointer (reloc to a name), ... }
    let mut typeinfo: HashMap<u64, String> = HashMap::new();
    let mut p = img.drr.lo;
    while p + 16 <= img.drr.hi {
        if let Some(Target::Addr(v)) = img.relocs.get(&(p + 8)) {
            if let Some(n) = names.get(v) {
                if img.relocs.contains_key(&p) {
                    typeinfo.insert(p, n.clone());
                }
            }
        }
        p += 8;
    }

    // 3. vtables
    let mut vtables = Vec::new();
    let mut p = img.drr.lo + 8;
    while p + 8 <= img.drr.hi {
        if let Some(Target::Addr(v)) = img.relocs.get(&p) {
            if let Some(class) = typeinfo.get(v) {
                if !img.relocs.contains_key(&(p - 8)) {
                    let ott = img.raw_u64(&img.drr, p - 8).unwrap_or(0) as i64;
                    let mut slots = Vec::new();
                    let mut q = p + 8;
                    while q + 8 <= img.drr.hi {
                        match img.relocs.get(&q) {
                            Some(Target::Addr(a)) if img.in_text(*a) => {
                                slots.push(Target::Addr(*a))
                            }
                            Some(Target::Sym(s)) if img.is_func_sym(s) => {
                                slots.push(Target::Sym(s.clone()))
                            }
                            None => {
                                // A zero without relocation is a null slot (GCC zeroes the destructor
                                // entries of abstract classes), unless it is the offset_to_top of the
                                // next vtable, which is followed by a typeinfo pointer.
                                let raw = img.raw_u64(&img.drr, q).unwrap_or(1);
                                let header_next = matches!(img.relocs.get(&(q + 8)), Some(Target::Addr(t)) if typeinfo.contains_key(t));
                                if raw == 0 && !header_next {
                                    slots.push(Target::Null);
                                } else {
                                    break;
                                }
                            }
                            _ => break,
                        }
                        q += 8;
                    }
                    // zero padding after a table is not part of it
                    while slots.last() == Some(&Target::Null) {
                        slots.pop();
                    }
                    if !slots.is_empty() {
                        vtables.push(VTable {
                            class: class.clone(),
                            offset_to_top: ott,
                            addr: p + 8,
                            slots,
                        });
                    }
                }
            }
        }
        p += 8;
    }

    // 4. the hierarchy recorded in the typeinfo objects
    let vtable_names: HashMap<u64, &str> =
        vtables.iter().map(|v| (v.addr, v.class.as_str())).collect();
    let mut bases: HashMap<u64, Vec<BaseRef>> = HashMap::new();
    for &p in typeinfo.keys() {
        match ti_kind(img, p, &vtable_names) {
            TiKind::Single => {
                if let Some(Target::Addr(b)) = img.relocs.get(&(p + 16)) {
                    bases.insert(
                        p,
                        vec![BaseRef {
                            ti: *b,
                            offset: 0,
                            is_virtual: false,
                        }],
                    );
                }
            }
            TiKind::Multi => {
                let Some(word) = img.raw_u64(&img.drr, p + 16) else {
                    continue;
                };
                let count = (word >> 32) as usize;
                let mut list = Vec::with_capacity(count.min(64));
                for k in 0..count.min(64) {
                    let at = p + 24 + (k as u64) * 16;
                    let (Some(Target::Addr(b)), Some(flags)) =
                        (img.relocs.get(&at), img.raw_u64(&img.drr, at + 8))
                    else {
                        break;
                    };
                    let flags = flags as i64;
                    list.push(BaseRef {
                        ti: *b,
                        offset: flags >> 8,
                        is_virtual: flags & 1 != 0,
                    });
                }
                bases.insert(p, list);
            }
            TiKind::Plain | TiKind::Unknown => {}
        }
    }
    let mut class_ti: HashMap<String, u64> = HashMap::new();
    let mut ti_sorted: Vec<(&u64, &String)> = typeinfo.iter().collect();
    ti_sorted.sort();
    for (&p, name) in ti_sorted {
        class_ti.entry(name.clone()).or_insert(p);
    }
    let ti_name: HashMap<u64, String> = typeinfo.iter().map(|(p, n)| (*p, n.clone())).collect();
    finish(
        vtables,
        class_ti,
        ti_name,
        bases,
        names.len(),
        typeinfo.len(),
    )
}

impl Rtti {
    /// Where the `base` subobject of `class` lives, following non-virtual bases only.
    pub fn subobject_offset(&self, class: &str, base: &str) -> BaseLookup {
        let (Some(&c), Some(&b)) = (self.class_ti.get(class), self.class_ti.get(base)) else {
            return BaseLookup::NotABase;
        };
        if c == b {
            return BaseLookup::NonVirtual(0);
        }
        if self.damaged.contains(class) {
            return BaseLookup::Damaged;
        }
        let mut via_virtual = false;
        if let Some(off) = self.walk(c, b, 0, 0, &mut via_virtual) {
            return BaseLookup::NonVirtual(off);
        }
        if via_virtual {
            BaseLookup::Virtual
        } else {
            BaseLookup::NotABase
        }
    }

    fn walk(
        &self,
        from: u64,
        target: u64,
        acc: i64,
        depth: u32,
        via_virtual: &mut bool,
    ) -> Option<i64> {
        if depth > 16 {
            return None;
        }
        for b in self.bases.get(&from).map(Vec::as_slice).unwrap_or(&[]) {
            if b.is_virtual {
                if b.ti == target || self.walk(b.ti, target, 0, depth + 1, via_virtual).is_some() {
                    *via_virtual = true;
                }
                continue;
            }
            if b.ti == target {
                return Some(acc + b.offset);
            }
            if let Some(off) = self.walk(b.ti, target, acc + b.offset, depth + 1, via_virtual) {
                return Some(off);
            }
        }
        None
    }

    /// The table of `class` that serves its `base` subobject: the primary table for the primary
    /// base, otherwise the secondary table whose offset_to_top is minus the subobject offset.
    pub fn table_for_base(&self, class: &str, base: &str) -> Result<&VTable, BaseLookup> {
        match self.subobject_offset(class, base) {
            BaseLookup::NonVirtual(0) => self
                .primary
                .get(class)
                .map(|&i| &self.vtables[i])
                .ok_or(BaseLookup::NotABase),
            BaseLookup::NonVirtual(off) => self
                .vtables
                .iter()
                .find(|v| v.class == class && v.offset_to_top == -off)
                .ok_or(BaseLookup::NonVirtual(off)),
            other => Err(other),
        }
    }

    /// Every class with a primary table that has `base` anywhere in its ancestry, with how the
    /// base is reached; `base` itself excluded, sorted by name. This is the full set an interface
    /// verdict must account for: a class reached through a virtual base cannot be checked and
    /// therefore blocks the verdict rather than silently dropping out of it.
    pub fn candidates(&self, base: &str) -> Vec<(String, BaseLookup)> {
        // every class with a table of its own, not only those with a primary table: under MSVC a
        // class whose only table serves a virtual base has no primary, yet it implements the base;
        // a class known by its typeinfo alone has no instances and implements nothing
        let with_tables: BTreeSet<&String> = self.vtables.iter().map(|v| &v.class).collect();
        let mut out: Vec<(String, BaseLookup)> = with_tables
            .into_iter()
            .filter(|c| c.as_str() != base)
            .map(|c| (c.clone(), self.subobject_offset(c, base)))
            .filter(|(_, l)| *l != BaseLookup::NotABase)
            .collect();
        out.sort();
        out
    }

    /// Classes (with a primary table) that derive from `base` through non-virtual bases only,
    /// `base` itself excluded, sorted by name.
    pub fn implementers(&self, base: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .primary
            .keys()
            .filter(|c| c.as_str() != base)
            .filter(|c| matches!(self.subobject_offset(c, base), BaseLookup::NonVirtual(_)))
            .cloned()
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangled_type_names() {
        assert!(looks_mangled(b"22CCSPlayer_ItemServices"));
        assert!(looks_mangled(b"N3foo3BarE"));
        assert!(!looks_mangled(b"hello"), "no length prefix");
        assert!(!looks_mangled(b"0abc"), "a length never starts with 0");
        assert!(
            !looks_mangled(b"11CBase-Entity"),
            "only identifier characters"
        );
        assert!(!looks_mangled(b"1A"), "too short to be worth a look");
    }

    #[test]
    fn demangles_plain_and_nested_names() {
        assert_eq!(demangle("11CBaseEntity"), "CBaseEntity");
        assert_eq!(
            demangle("22CCSPlayer_ItemServices"),
            "CCSPlayer_ItemServices"
        );
        assert_eq!(demangle("N3foo3BarE"), "foo::Bar");
        assert_eq!(demangle("12CCSGameRules"), "CCSGameRules");
    }

    #[test]
    fn template_names_come_out_readable() {
        // typeinfo name of CTier2AppSystem<IVEngineServer2, 0>
        let d = demangle("15CTier2AppSystemI15IVEngineServer2Lx0EE");
        assert!(d.starts_with("CTier2AppSystem<IVEngineServer2"), "{d}");
    }

    fn vt(class: &str, ott: i64, addr: u64) -> VTable {
        VTable {
            class: class.into(),
            offset_to_top: ott,
            addr,
            slots: vec![Target::Null],
        }
    }

    #[test]
    fn two_primary_tables_with_one_name_make_the_class_ambiguous() {
        let tables = vec![
            vt("A", 0, 0x100),
            vt("B", 0, 0x200),
            vt("A", 0, 0x300),
            vt("B", -16, 0x400),
        ];
        let (primary, ambiguous) = pick_primary(&tables);
        assert_eq!(ambiguous.iter().collect::<Vec<_>>(), vec!["A"]);
        assert_eq!(primary["A"], 0, "the first table is kept for reporting");
        assert_eq!(primary["B"], 1, "a secondary table does not count");
    }

    fn hierarchy() -> Rtti {
        // Base(1) ; Mixin(2) ; Both(3): Base at 0, Mixin at 16 ; Deep(4): Both at 0 ; V(5): virtual Base
        let vtables = vec![
            vt("Base", 0, 0x10),
            vt("Mixin", 0, 0x20),
            vt("Both", 0, 0x30),
            vt("Both", -16, 0x40),
            vt("Deep", 0, 0x50),
            vt("Deep", -16, 0x60),
            vt("V", 0, 0x70),
        ];
        let (primary, ambiguous) = pick_primary(&vtables);
        let mut bases = HashMap::new();
        bases.insert(
            3,
            vec![
                BaseRef {
                    ti: 1,
                    offset: 0,
                    is_virtual: false,
                },
                BaseRef {
                    ti: 2,
                    offset: 16,
                    is_virtual: false,
                },
            ],
        );
        bases.insert(
            4,
            vec![BaseRef {
                ti: 3,
                offset: 0,
                is_virtual: false,
            }],
        );
        bases.insert(
            5,
            vec![BaseRef {
                ti: 1,
                offset: 24,
                is_virtual: true,
            }],
        );
        let names = [
            ("Base", 1u64),
            ("Mixin", 2),
            ("Both", 3),
            ("Deep", 4),
            ("V", 5),
        ];
        Rtti {
            vtables,
            primary,
            ambiguous,
            func_starts: vec![],
            class_ti: names.iter().map(|(n, t)| (n.to_string(), *t)).collect(),
            ti_name: names.iter().map(|(n, t)| (*t, n.to_string())).collect(),
            bases,
            damaged: BTreeSet::new(),
            n_names: 5,
            n_typeinfo: 5,
        }
    }

    #[test]
    fn subobject_offsets_follow_non_virtual_chains() {
        let r = hierarchy();
        assert_eq!(
            r.subobject_offset("Both", "Base"),
            BaseLookup::NonVirtual(0)
        );
        assert_eq!(
            r.subobject_offset("Both", "Mixin"),
            BaseLookup::NonVirtual(16)
        );
        assert_eq!(
            r.subobject_offset("Deep", "Mixin"),
            BaseLookup::NonVirtual(16),
            "two levels down"
        );
        assert_eq!(
            r.subobject_offset("V", "Base"),
            BaseLookup::Virtual,
            "a virtual base has no static offset"
        );
        assert_eq!(r.subobject_offset("Mixin", "Base"), BaseLookup::NotABase);
        assert_eq!(
            r.subobject_offset("Base", "Base"),
            BaseLookup::NonVirtual(0)
        );
    }

    #[test]
    fn the_table_serving_a_base_is_found_by_its_offset_to_top() {
        let r = hierarchy();
        assert_eq!(r.table_for_base("Both", "Base").unwrap().addr, 0x30);
        assert_eq!(r.table_for_base("Both", "Mixin").unwrap().addr, 0x40);
        assert_eq!(r.table_for_base("Deep", "Mixin").unwrap().addr, 0x60);
        assert_eq!(
            r.table_for_base("V", "Base").unwrap_err(),
            BaseLookup::Virtual
        );
        assert_eq!(r.implementers("Mixin"), vec!["Both", "Deep"]);
        assert_eq!(
            r.implementers("Base"),
            vec!["Both", "Deep"],
            "V reaches Base only virtually"
        );
    }
}
