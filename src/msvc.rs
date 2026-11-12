//! MSVC RTTI walk for x86-64 PE images. Every vftable is preceded by a pointer to its
//! `RTTICompleteObjectLocator`, which names the class (`TypeDescriptor`, whose decorated name
//! `.?AVFoo@@` sits in `.data`) and records the whole hierarchy with subobject offsets
//! (`ClassHierarchyDescriptor` -> array of `BaseClassDescriptor`). The result is the same `Rtti`
//! the Itanium walk produces; `offset_to_top` keeps the Itanium sign convention (minus the
//! subobject offset the locator stores).
//!
//! Decorated type names are demangled by a small parser that covers what class names need:
//! plain and nested names, templates with class and integer arguments, pointers and the basic
//! types. Anything else keeps its decorated form, which is still a stable key across builds.

use crate::image::{Image, Target};
use crate::rtti::{finish, BaseRef, Rtti, VTable};
use std::collections::{HashMap, HashSet};

/// `.?AVFoo@Bar@@` -> `Bar::Foo`; `.?AV?$CTier2AppSystem@VIVEngineServer2@@$0A@@@` ->
/// `CTier2AppSystem<IVEngineServer2,0>`. `None` for forms the parser does not cover.
pub fn demangle_type(dec: &str) -> Option<String> {
    let rest = dec.as_bytes().strip_prefix(b".?A")?;
    let mut p = Parser {
        s: rest,
        i: 0,
        names: Vec::new(),
    };
    let name = p.class_name()?;
    if p.i != p.s.len() {
        return None;
    }
    Some(name)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// Back-reference table: identifiers (and template names) in order of first appearance.
    names: Vec<String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    /// `V`/`U`/`T` (class/struct/union) or `W4` (enum), then a qualified name.
    fn class_name(&mut self) -> Option<String> {
        match self.bump()? {
            b'V' | b'U' | b'T' => self.qualified(),
            b'W' => {
                self.bump()?;
                self.qualified()
            }
            _ => None,
        }
    }

    /// Characters up to the next `@`, which is consumed.
    fn ident(&mut self) -> Option<String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == b'@' {
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'<' || c == b'>') {
                return None;
            }
            self.i += 1;
        }
        if self.i == start || self.peek() != Some(b'@') {
            return None;
        }
        let id = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        Some(id)
    }

    /// Segments (innermost first) each ended by `@`, the list ended by one more `@`.
    fn qualified(&mut self) -> Option<String> {
        let mut segs = Vec::new();
        loop {
            match self.peek()? {
                b'@' => {
                    self.i += 1;
                    break;
                }
                b'0'..=b'9' => {
                    let d = (self.bump()? - b'0') as usize;
                    segs.push(self.names.get(d)?.clone());
                }
                b'?' => {
                    if !self.s[self.i..].starts_with(b"?$") {
                        return None; // anonymous namespaces and other special scopes
                    }
                    self.i += 2;
                    let name = self.ident()?;
                    // the argument list has its own back-reference table, starting with the
                    // template's own name; the whole instantiation is one entry of the outer table
                    let outer = std::mem::replace(&mut self.names, vec![name.clone()]);
                    let args = self.template_args();
                    self.names = outer;
                    let full = format!("{name}<{}>", args?.join(","));
                    self.names.push(full.clone());
                    segs.push(full);
                }
                _ => {
                    let id = self.ident()?;
                    self.names.push(id.clone());
                    segs.push(id);
                }
            }
        }
        segs.reverse();
        Some(segs.join("::"))
    }

    fn template_args(&mut self) -> Option<Vec<String>> {
        let mut out = Vec::new();
        loop {
            if self.peek()? == b'@' {
                self.i += 1;
                return Some(out);
            }
            out.push(self.ty()?);
        }
    }

    fn ty(&mut self) -> Option<String> {
        let c = self.bump()?;
        let basic = |s: &str| Some(s.to_string());
        match c {
            b'V' | b'U' | b'T' => self.qualified(),
            b'W' => {
                self.bump()?;
                self.qualified()
            }
            b'P' | b'Q' | b'A' => {
                let suffix = if c == b'A' { "&" } else { "*" };
                if self.peek() == Some(b'E') {
                    self.i += 1; // __ptr64
                }
                let cv = match self.bump()? {
                    b'A' => "",
                    b'B' => "const ",
                    b'C' => "volatile ",
                    b'D' => "const volatile ",
                    _ => return None,
                };
                let t = self.ty()?;
                Some(format!("{cv}{t}{suffix}"))
            }
            b'$' => match self.bump()? {
                b'0' => self.number().map(|n| n.to_string()),
                b'$' => match self.bump()? {
                    b'T' => basic("nullptr"),
                    b'C' => {
                        self.bump()?;
                        self.ty()
                    }
                    b'Q' => self.ty().map(|t| format!("{t}&&")),
                    _ => None,
                },
                _ => None,
            },
            b'X' => basic("void"),
            b'D' => basic("char"),
            b'C' => basic("signed char"),
            b'E' => basic("unsigned char"),
            b'F' => basic("short"),
            b'G' => basic("unsigned short"),
            b'H' => basic("int"),
            b'I' => basic("unsigned int"),
            b'J' => basic("long"),
            b'K' => basic("unsigned long"),
            b'M' => basic("float"),
            b'N' => basic("double"),
            b'O' => basic("long double"),
            b'Z' => basic("..."),
            b'_' => match self.bump()? {
                b'N' => basic("bool"),
                b'J' => basic("__int64"),
                b'K' => basic("unsigned __int64"),
                b'W' => basic("wchar_t"),
                b'Q' => basic("char8_t"),
                b'S' => basic("char16_t"),
                b'U' => basic("char32_t"),
                _ => None,
            },
            _ => None,
        }
    }

    /// A non-type integer argument: `?` negates; a digit stands for value + 1; otherwise hex
    /// digits written `A`..`P`, ended by `@`.
    fn number(&mut self) -> Option<i64> {
        let neg = if self.peek() == Some(b'?') {
            self.i += 1;
            true
        } else {
            false
        };
        let c = self.bump()?;
        let v = if c.is_ascii_digit() {
            (c - b'0') as i64 + 1
        } else {
            let mut v = 0i64;
            let mut ch = c;
            loop {
                if ch == b'@' {
                    break;
                }
                if !(b'A'..=b'P').contains(&ch) {
                    return None;
                }
                v = v.checked_mul(16)? + (ch - b'A') as i64;
                ch = self.bump()?;
            }
            v
        };
        Some(if neg { -v } else { v })
    }
}

/// The imported name behind a thunk `jmp qword ptr [rip+disp]` (FF 25 disp32) whose target is an
/// import address table entry: a pure virtual slot points at such a thunk for `_purecall`, the
/// MSVC counterpart of `__cxa_pure_virtual`.
fn import_thunk(img: &Image, a: u64) -> Option<String> {
    let b = img.slice(&img.text, a, 6)?;
    if b.len() < 6 || b[0] != 0xFF || b[1] != 0x25 {
        return None;
    }
    let disp = i32::from_le_bytes(b[2..6].try_into().unwrap()) as i64;
    let iat = (a as i64 + 6 + disp) as u64;
    img.imports.get(&iat).cloned()
}

pub fn analyze(img: &Image) -> Rtti {
    let base = img.base;

    // 1. type descriptors: { vptr | spare | ".?AV...@@" }, the name 16 bytes after the object
    let mut td_name: HashMap<u64, String> = HashMap::new();
    for sec in &img.rodata {
        let Some(b) = img.section_bytes(sec) else {
            continue;
        };
        let mut pos = 0usize;
        while pos + 3 <= b.len() {
            let Some(rel) = b[pos..].windows(3).position(|w| w == b".?A") else {
                break;
            };
            let at = pos + rel;
            let Some(len) = b[at..].iter().position(|&c| c == 0) else {
                break;
            };
            let tok = &b[at..at + len];
            if len <= 512 && at >= 16 {
                let td = sec.lo + at as u64 - 16;
                if img.relocs.contains_key(&td) {
                    let raw = String::from_utf8_lossy(tok).into_owned();
                    let name = demangle_type(&raw).unwrap_or(raw);
                    td_name.insert(td, name);
                }
            }
            pos = at + len + 1;
        }
    }

    // 2. complete object locators: { signature 1 | offset | cdOffset | td rva | chd rva | self rva }
    let col = |t: u64| -> Option<(i64, u64, u64)> {
        let sec = img.data_section(t)?;
        if t + 24 > sec.hi || img.raw_u32(sec, t)? != 1 {
            return None;
        }
        let offset = img.raw_u32(sec, t + 4)? as i64;
        let td = base + img.raw_u32(sec, t + 12)? as u64;
        let chd = base + img.raw_u32(sec, t + 16)? as u64;
        let self_rva = img.raw_u32(sec, t + 20)? as u64;
        if base + self_rva != t || !td_name.contains_key(&td) {
            return None;
        }
        Some((offset, td, chd))
    };
    let mut heads: Vec<(u64, i64, u64, u64)> = img
        .relocs
        .iter()
        .filter_map(|(&loc, t)| match t {
            Target::Addr(a) if img.drr.contains(loc) => {
                col(*a).map(|(o, td, chd)| (loc, o, td, chd))
            }
            _ => None,
        })
        .collect();
    heads.sort_unstable();
    let head_set: HashSet<u64> = heads.iter().map(|h| h.0).collect();

    // 3. vtables: the slots follow the locator pointer until the next locator pointer or a
    //    pointer that does not lead into .text
    let mut vtables = Vec::new();
    let mut class_chd: HashMap<u64, u64> = HashMap::new();
    for &(loc, offset, td, chd) in &heads {
        let mut slots = Vec::new();
        let mut q = loc + 8;
        while q + 8 <= img.drr.hi && !head_set.contains(&q) {
            match img.relocs.get(&q) {
                Some(Target::Addr(a)) if img.in_text(*a) => {
                    slots.push(match import_thunk(img, *a) {
                        Some(name) => Target::Sym(name),
                        None => Target::Addr(*a),
                    })
                }
                _ => break,
            }
            q += 8;
        }
        if slots.is_empty() {
            continue;
        }
        vtables.push(VTable {
            class: td_name[&td].clone(),
            offset_to_top: -offset,
            addr: loc + 8,
            slots,
        });
        class_chd.entry(td).or_insert(chd);
    }

    // 4. hierarchy: CHD { signature | attributes | numBaseClasses | base class array rva };
    //    the array lists the class itself, then its bases in pre-order, each BCD
    //    { td rva | numContainedBases | mdisp | pdisp | vdisp | attributes | chd rva }
    let mut bases: HashMap<u64, Vec<BaseRef>> = HashMap::new();
    let mut nested: HashMap<u64, Vec<BaseRef>> = HashMap::new();
    let mut damaged: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (&td, &chd) in &class_chd {
        let Some(sec) = img.data_section(chd) else {
            continue;
        };
        let (Some(n), Some(bca_rva)) = (img.raw_u32(sec, chd + 8), img.raw_u32(sec, chd + 12))
        else {
            continue;
        };
        let bca = base + bca_rva as u64;
        let Some(bsec) = img.data_section(bca) else {
            continue;
        };
        // the whole array: (type descriptor, bases contained below it, mdisp, pdisp)
        let declared = n as u64;
        let n = declared.min(512);
        let mut flat: Vec<(u64, usize, i64, bool)> = Vec::new();
        for i in 0..n {
            let Some(bcd_rva) = img.raw_u32(bsec, bca + 4 * i) else {
                break;
            };
            let bcd = base + bcd_rva as u64;
            let Some(dsec) = img.data_section(bcd) else {
                break;
            };
            let (Some(btd), Some(contained), Some(mdisp), Some(pdisp)) = (
                img.raw_u32(dsec, bcd),
                img.raw_u32(dsec, bcd + 4),
                img.raw_u32(dsec, bcd + 8),
                img.raw_u32(dsec, bcd + 12),
            ) else {
                break;
            };
            flat.push((
                base + btd as u64,
                contained as usize,
                mdisp as i32 as i64,
                pdisp as i32 != -1,
            ));
        }
        // a record that claims more than it holds is not read in part: a shortened list of
        // bases would look as sound as a whole one
        let whole = declared <= 512
            && flat.len() as u64 == declared
            && flat
                .iter()
                .enumerate()
                .all(|(i, e)| i + e.1 < flat.len() && td_name.contains_key(&e.0))
            && flat.first().is_some_and(|e| e.1 + 1 == flat.len());
        if !whole {
            damaged.insert(td_name[&td].clone());
            continue;
        }
        for (i, (owner, list)) in direct_bases(&flat).into_iter().enumerate() {
            if i == 0 {
                bases.insert(td, list);
            } else {
                // a base with no table of its own in this image (an inlined template layer) has
                // no hierarchy record that would be read above: what this class says about it
                // is all there is
                nested.entry(owner).or_insert(list);
            }
        }
    }
    for (td, list) in nested {
        bases.entry(td).or_insert(list);
    }

    let mut class_ti: HashMap<String, u64> = HashMap::new();
    let mut sorted: Vec<(&u64, &String)> = td_name.iter().collect();
    sorted.sort();
    for (&td, name) in sorted {
        class_ti.entry(name.clone()).or_insert(td);
    }
    let n_names = td_name.len();
    let n_typeinfo = class_chd.len();
    let mut r = finish(vtables, class_ti, td_name, bases, n_names, n_typeinfo);
    r.damaged = damaged;
    r
}

#[cfg(test)]
mod tests {
    use super::demangle_type;

    #[test]
    fn plain_and_nested_names() {
        assert_eq!(
            demangle_type(".?AVCCSPlayerController@@").as_deref(),
            Some("CCSPlayerController")
        );
        assert_eq!(
            demangle_type(".?AUInner@Outer@@").as_deref(),
            Some("Outer::Inner")
        );
        assert_eq!(demangle_type(".?AW4Color@@").as_deref(), Some("Color"));
    }

    #[test]
    fn templates_with_class_and_integer_arguments() {
        assert_eq!(
            demangle_type(".?AV?$CTier2AppSystem@VIVEngineServer2@@$0A@@@").as_deref(),
            Some("CTier2AppSystem<IVEngineServer2,0>")
        );
        assert_eq!(
            demangle_type(".?AV?$CUtlVector@PEAVCBaseEntity@@V?$CUtlMemory@PEAVCBaseEntity@@H@@@@")
                .as_deref(),
            Some("CUtlVector<CBaseEntity*,CUtlMemory<CBaseEntity*,int>>")
        );
        assert_eq!(demangle_type(".?AV?$Box@$00@@").as_deref(), Some("Box<1>"));
        assert_eq!(
            demangle_type(".?AV?$Box@$0?A@@@").as_deref(),
            Some("Box<0>")
        );
        assert_eq!(
            demangle_type(".?AV?$Box@$0BA@@@").as_deref(),
            Some("Box<16>")
        );
    }

    #[test]
    fn back_references_inside_template_arguments() {
        // a real name from engine2.dll: inside the argument list the table starts with the
        // template's own name (0), then CHLTVServerAsync (1), which `V1@` repeats
        assert_eq!(
            demangle_type(
                ".?AV?$CDelayedCall2@VCHLTVServerAsync@@V1@UStartRecordingCall_t@ITvStreamSource@@Uempty_t@@@@"
            )
            .as_deref(),
            Some("CDelayedCall2<CHLTVServerAsync,CHLTVServerAsync,ITvStreamSource::StartRecordingCall_t,empty_t>")
        );
        // and the outer table continues after the instantiation: `1` here is the instantiation
        assert_eq!(
            demangle_type(".?AVInner@?$Outer@H@@1@@").as_deref(),
            None,
            "a back-reference to a template instantiation as a scope is not a form we claim"
        );
    }

    #[test]
    fn unsupported_forms_stay_decorated() {
        assert_eq!(
            demangle_type(".?AV?A0x1234@Foo@@"),
            None,
            "anonymous namespace"
        );
        assert_eq!(demangle_type("hello"), None);
        assert_eq!(demangle_type(".?AVFoo@@junk"), None, "trailing bytes");
    }
}

/// The direct bases of every entry of a base class array, given flat in pre-order as
/// (type descriptor, bases contained below it, mdisp, reached through a virtual base): the
/// entries right below an entry, each followed by its own subtree. mdisp is counted from the
/// complete class, so a base's place inside its parent is the difference of the two.
fn direct_bases(flat: &[(u64, usize, i64, bool)]) -> Vec<(u64, Vec<BaseRef>)> {
    let mut out = Vec::new();
    for (i, &(owner, contained, at, _)) in flat.iter().enumerate() {
        let end = (i + contained).min(flat.len().saturating_sub(1));
        let mut list = Vec::new();
        let mut j = i + 1;
        while j <= end {
            let (btd, sub, mdisp, is_virtual) = flat[j];
            list.push(BaseRef {
                ti: btd,
                offset: mdisp - at,
                is_virtual,
            });
            j += 1 + sub;
        }
        out.push((owner, list));
    }
    out
}

#[cfg(test)]
mod base_tests {
    use super::*;

    #[test]
    fn a_layer_without_a_table_of_its_own_still_passes_its_bases_on() {
        // CEngineServer : CTier<IEngine> : IEngine : IApp, and a second base at +16 with its own
        // base. Only CEngineServer has a hierarchy record; the layers are known from its array.
        let flat = [
            (1, 5, 0, false),  // CEngineServer
            (2, 2, 0, false),  // CTier<IEngine>
            (3, 1, 0, false),  // IEngine
            (4, 0, 0, false),  // IApp
            (5, 1, 16, false), // Second
            (6, 0, 24, false), // SecondBase, 8 bytes into Second
        ];
        let d = direct_bases(&flat);
        let of = |td: u64| -> Vec<(u64, i64)> {
            d.iter()
                .find(|x| x.0 == td)
                .unwrap()
                .1
                .iter()
                .map(|b| (b.ti, b.offset))
                .collect()
        };
        assert_eq!(of(1), vec![(2, 0), (5, 16)]);
        assert_eq!(of(2), vec![(3, 0)]);
        assert_eq!(of(3), vec![(4, 0)]);
        assert_eq!(of(5), vec![(6, 8)]);
        assert!(of(4).is_empty());
    }

    #[test]
    fn a_count_that_runs_past_the_array_is_cut_not_followed() {
        let d = direct_bases(&[(1, 900, 0, false), (2, 7, 0, false)]);
        assert_eq!(d[0].1.len(), 1);
        assert!(d[1].1.is_empty());
    }
}
