//! Byte signatures: where a gamedata signature pointed in the old build, which function of the new
//! build is the same one, and a fresh signature for it when the old bytes no longer match.
//!
//! A signature is a byte pattern with wildcards that must match exactly once in `.text`. After a
//! game update the function is usually still there, but its bytes moved. The steps:
//!   1. find the old signature's single match in the old build and the function that holds it;
//!   2. find the same function in the new build (`match_functions`): by an exact fingerprint that
//!      is unique in both builds, by string literals only that function refers to, by a unique
//!      opcode shape, and last by the call graph around already matched functions;
//!   3. if the old signature still matches once in the new build, and at that function, nothing
//!      needs to change; otherwise cut a new pattern from the new function's first bytes, with
//!      everything a linker moves wildcarded, long enough to be unique.
//!
//! Every answer carries the level of evidence it rests on; levels are measured against history
//! the same way the vtable indices are.

use crate::fp::{Fingerprinter, Fp};
use crate::image::{Image, Target};
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use xxhash_rust::xxh3::xxh3_64;

// ------------------------------------------------------------------------------------------
// patterns

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// `48 89 5C 24 ? 57` (CounterStrikeSharp, CS2Fixes jsonc)
    Spaced,
    /// `\x48\x89\x5C\x24\x2A\x57`, 0x2A standing for any byte (SourceMod-style KeyValues)
    Escaped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub bytes: Vec<Option<u8>>,
    pub style: Style,
}

impl Pattern {
    /// Parse either notation; `None` for symbol references (`@Msg`) and anything unparseable.
    pub fn parse(s: &str) -> Option<Pattern> {
        let s = s.trim();
        if s.is_empty() || s.starts_with('@') {
            return None;
        }
        let escaped = s.contains('x') && !s.contains(' ');
        let mut bytes = Vec::new();
        if escaped {
            for piece in s.replace('\\', "").split('x').filter(|p| !p.is_empty()) {
                let b = u8::from_str_radix(piece, 16).ok()?;
                bytes.push(if b == 0x2A { None } else { Some(b) });
            }
        } else {
            for tok in s.split_whitespace() {
                if tok.chars().all(|c| c == '?') {
                    bytes.push(None);
                } else {
                    bytes.push(Some(u8::from_str_radix(tok, 16).ok()?));
                }
            }
        }
        if bytes.iter().filter(|b| b.is_some()).count() < 2 {
            return None;
        }
        Some(Pattern {
            bytes,
            style: if escaped {
                Style::Escaped
            } else {
                Style::Spaced
            },
        })
    }

    pub fn render(&self) -> String {
        match self.style {
            Style::Spaced => self
                .bytes
                .iter()
                .map(|b| match b {
                    Some(v) => format!("{v:02X}"),
                    None => "?".to_string(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            Style::Escaped => self
                .bytes
                .iter()
                .map(|b| match b {
                    Some(v) => format!("\\x{v:02X}"),
                    None => "\\x2A".to_string(),
                })
                .collect(),
        }
    }

    /// Offsets of the matches in `hay`, at most `limit` of them.
    pub fn find(&self, hay: &[u8], limit: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let n = self.bytes.len();
        if n == 0 || hay.len() < n {
            return out;
        }
        let Some(anchor) = self.bytes.iter().position(|b| b.is_some()) else {
            return out;
        };
        let first = self.bytes[anchor].unwrap();
        let last = hay.len() - n;
        let mut i = 0usize;
        while i <= last {
            // jump to the next occurrence of the anchor byte
            match hay[i + anchor..=last + anchor]
                .iter()
                .position(|&c| c == first)
            {
                Some(p) => i += p,
                None => break,
            }
            if self
                .bytes
                .iter()
                .zip(&hay[i..i + n])
                .all(|(p, &h)| p.map(|v| v == h).unwrap_or(true))
            {
                out.push(i);
                if out.len() >= limit {
                    break;
                }
            }
            i += 1;
        }
        out
    }
}

// ------------------------------------------------------------------------------------------
// function inventory

#[derive(Debug)]
pub struct Func {
    pub start: u64,
    pub end: u64,
    pub fp: Fp,
    /// Hashes of the string literals the function refers to, sorted, deduplicated.
    pub strs: Vec<u64>,
    /// Direct call targets (and tail jumps to other functions), in code order.
    pub calls: Vec<u64>,
    /// Characteristic constants: immediates of 0x100 and above that are not addresses, and the
    /// 4-byte constants the code loads from read-only data (floats); sorted, deduplicated.
    pub consts: Vec<u64>,
    /// How many times each mnemonic occurs, sorted by mnemonic: the body's composition.
    pub ops: Vec<(u16, u16)>,
    /// Displacements off a pointer register (0x40..0x10000): the object fields the code touches.
    pub disps: Vec<u32>,
}

pub struct Db {
    pub funcs: Vec<Func>,
    by_start: HashMap<u64, usize>,
    /// Per function: the distinct functions it calls and the distinct functions calling it.
    pub callees: Vec<Vec<usize>>,
    pub callers: Vec<Vec<usize>>,
}

fn c_string_hash(img: &Image, addr: u64) -> Option<u64> {
    let sec = img.data_section(addr)?;
    let bytes = img.slice(sec, addr, 257)?;
    let len = bytes.iter().position(|&c| c == 0)?;
    if !(5..=256).contains(&len) {
        return None;
    }
    let s = &bytes[..len];
    if !s
        .iter()
        .all(|&c| (0x20..0x7f).contains(&c) || c == b'\n' || c == b'\t')
    {
        return None;
    }
    Some(xxh3_64(s))
}

impl Db {
    /// Every function the image lets us see: the ranges it records, vtable slots, direct call
    /// targets found by a sweep of `.text`, and `extra` (addresses signatures point at).
    pub fn build(img: &Image, extra: &[u64]) -> Db {
        let text = img.text;
        let code = img.section_bytes(&text).unwrap_or(&[]);
        let mut known: BTreeSet<u64> = img.func_ranges.keys().copied().collect();
        known.extend(crate::rtti::analyze(img).func_starts);
        known.extend(extra.iter().copied());
        known.retain(|a| text.contains(*a));
        let known_vec: Vec<u64> = known.iter().copied().collect();

        // pass 1: linear sweep for call targets, resynchronised at every known start
        let mut targets: HashSet<u64> = HashSet::new();
        let mut dec = Decoder::with_ip(64, code, text.lo, DecoderOptions::NONE);
        let mut ins = Instruction::default();
        let mut k = 0usize;
        // a function nobody calls directly still shows where it starts: at a 16-byte boundary,
        // after a ret or jmp and the padding behind it (or right behind it with a prologue)
        // ... unless a conditional or short jump lands there: that is a loop head or a block of
        // the same function, aligned the same way
        let mut candidates: HashSet<u64> = HashSet::new();
        let mut local_targets: HashSet<u64> = HashSet::new();
        let (mut after_terminal, mut saw_pad) = (false, false);
        while dec.can_decode() {
            let ip = dec.ip();
            while k < known_vec.len() && known_vec[k] <= ip {
                k += 1;
            }
            dec.decode_out(&mut ins);
            if k < known_vec.len() && ins.next_ip() > known_vec[k] {
                let s = known_vec[k];
                let _ = dec.set_position((s - text.lo) as usize);
                dec.set_ip(s);
                (after_terminal, saw_pad) = (false, false);
                continue;
            }
            if matches!(ins.mnemonic(), Mnemonic::Nop | Mnemonic::Int3) {
                saw_pad = true;
                continue;
            }
            if after_terminal && ip.is_multiple_of(16) {
                let b = &code[(ip - text.lo) as usize..];
                let prologue = b.len() >= 3
                    && (b[0] == 0x55
                        || b[0] == 0x53
                        || (b[0] == 0x41 && b[1] & 0xF8 == 0x50)
                        || (b[0] == 0x48 && (b[1] == 0x83 || b[1] == 0x81) && b[2] == 0xEC));
                if saw_pad || prologue {
                    candidates.insert(ip);
                }
            }
            if !ins.is_invalid()
                && (ins.flow_control() == FlowControl::ConditionalBranch
                    || (ins.flow_control() == FlowControl::UnconditionalBranch && ins.len() == 2))
                && matches!(ins.op0_kind(), OpKind::NearBranch64 | OpKind::NearBranch32)
            {
                local_targets.insert(ins.near_branch_target());
            }
            after_terminal = !ins.is_invalid()
                && matches!(
                    ins.flow_control(),
                    FlowControl::Return
                        | FlowControl::UnconditionalBranch
                        | FlowControl::IndirectBranch
                );
            saw_pad = false;
            if !ins.is_invalid()
                && ins.flow_control() == FlowControl::Call
                && matches!(ins.op0_kind(), OpKind::NearBranch64 | OpKind::NearBranch32)
            {
                let t = ins.near_branch_target();
                if text.contains(t) {
                    targets.insert(t);
                }
            }
        }
        known.extend(targets);
        known.extend(candidates.difference(&local_targets));
        let starts: Vec<u64> = known.into_iter().collect();
        let start_set: HashSet<u64> = starts.iter().copied().collect();

        // fingerprints with the full start list as boundaries
        let mut fper = Fingerprinter::with_trusted_starts(img, &starts);
        let fps: Vec<Fp> = starts.iter().map(|s| fper.of(&Target::Addr(*s))).collect();

        // pass 2: per function, the strings it refers to and the functions it calls
        let mut funcs = Vec::with_capacity(starts.len());
        for (i, &start) in starts.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(text.hi);
            let end = img
                .func_ranges
                .get(&start)
                .copied()
                .unwrap_or(next)
                .min(next)
                .max(start);
            let body = img
                .slice(&text, start, (end - start) as usize)
                .unwrap_or(&[]);
            let mut dec = Decoder::with_ip(64, body, start, DecoderOptions::NONE);
            let mut strs = Vec::new();
            let mut calls = Vec::new();
            let mut consts: Vec<u64> = Vec::new();
            let mut ops: HashMap<u16, u16> = HashMap::new();
            let mut disps: Vec<u32> = Vec::new();
            while dec.can_decode() {
                dec.decode_out(&mut ins);
                if ins.is_invalid() {
                    break;
                }
                let fc = ins.flow_control();
                if (fc == FlowControl::Call || fc == FlowControl::UnconditionalBranch)
                    && matches!(ins.op0_kind(), OpKind::NearBranch64 | OpKind::NearBranch32)
                {
                    let t = ins.near_branch_target();
                    let outside = t < start || t >= end;
                    if text.contains(t)
                        && (fc == FlowControl::Call || (outside && start_set.contains(&t)))
                    {
                        calls.push(t);
                    }
                }
                for k in 0..ins.op_count() {
                    if matches!(
                        ins.op_kind(k),
                        OpKind::Immediate32 | OpKind::Immediate64 | OpKind::Immediate32to64
                    ) {
                        let v = ins.immediate(k);
                        if (0x100..0xFFFF_FF00).contains(&v) && !img.addr_like(v) {
                            consts.push(v);
                        }
                    }
                }
                *ops.entry(ins.mnemonic() as u16).or_insert(0) += 1;
                if !ins.is_ip_rel_memory_operand()
                    && (0..ins.op_count()).any(|k| ins.op_kind(k) == OpKind::Memory)
                    && !matches!(
                        ins.memory_base(),
                        Register::None | Register::RSP | Register::RBP
                    )
                {
                    let d = ins.memory_displacement64();
                    if (0x40..0x10000).contains(&d) {
                        disps.push(d as u32);
                    }
                }
                if ins.is_ip_rel_memory_operand() {
                    let at = ins.ip_rel_memory_address();
                    if let Some(h) = c_string_hash(img, at) {
                        strs.push(h);
                    } else if fc == FlowControl::Next {
                        if let Some(v) = img.data_section(at).and_then(|s| img.raw_u32(s, at)) {
                            if v != 0 && !img.relocs.contains_key(&at) {
                                consts.push(0x1_0000_0000 | v as u64);
                            }
                        }
                    }
                }
            }
            disps.sort_unstable();
            disps.dedup();
            let mut ops: Vec<(u16, u16)> = ops.into_iter().collect();
            ops.sort_unstable();
            consts.sort_unstable();
            consts.dedup();
            strs.sort_unstable();
            strs.dedup();
            funcs.push(Func {
                start,
                end,
                fp: fps[i],
                strs,
                calls,
                consts,
                ops,
                disps,
            });
        }
        let by_start: HashMap<u64, usize> = funcs
            .iter()
            .enumerate()
            .map(|(i, f)| (f.start, i))
            .collect();
        let mut callees: Vec<Vec<usize>> = vec![Vec::new(); funcs.len()];
        let mut callers: Vec<Vec<usize>> = vec![Vec::new(); funcs.len()];
        for (i, f) in funcs.iter().enumerate() {
            let mut c: Vec<usize> = f
                .calls
                .iter()
                .filter_map(|a| by_start.get(a).copied())
                .collect();
            c.sort_unstable();
            c.dedup();
            for &j in &c {
                callers[j].push(i);
            }
            callees[i] = c;
        }
        Db {
            funcs,
            by_start,
            callees,
            callers,
        }
    }

    pub fn index_of(&self, start: u64) -> Option<usize> {
        self.by_start.get(&start).copied()
    }

    /// The function whose range holds `addr`.
    pub fn containing(&self, addr: u64) -> Option<usize> {
        let i = self.funcs.partition_point(|f| f.start <= addr);
        if i == 0 {
            return None;
        }
        let f = &self.funcs[i - 1];
        (addr < f.end.max(f.start + 1)).then_some(i - 1)
    }
}

// ------------------------------------------------------------------------------------------
// matching functions across builds

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Level {
    /// Exact fingerprint, unique in both builds.
    Exact,
    /// String literals only this function refers to, in both builds.
    Strings,
    /// The same slot of the same class's virtual table (the vtable alignment's verdict).
    VTable,
    /// Opcode shape unique in both builds.
    Shape,
    /// Position among the calls of already matched functions, with a similar body.
    CallGraph,
    /// Position in `.text` between two matched neighbours, with a similar body.
    Neighbours,
    /// The one clearly most similar function among those the matched callers, callees and
    /// neighbours lead to.
    Context,
    /// Each is the other's most similar candidate, by what survives a rewrite: the mix of
    /// instructions, the object fields touched, strings, matched callers and callees.
    Mutual,
}

fn unique_by<K: std::hash::Hash + Eq + Copy>(
    funcs: &[Func],
    key: impl Fn(&Func) -> Option<K>,
) -> HashMap<K, Option<usize>> {
    let mut m: HashMap<K, Option<usize>> = HashMap::new();
    for (i, f) in funcs.iter().enumerate() {
        if let Some(k) = key(f) {
            m.entry(k).and_modify(|v| *v = None).or_insert(Some(i));
        }
    }
    m
}

/// For every old function, the new function it is and why. Injective by construction.
pub fn match_functions(old: &Db, new: &Db, seeds: &[(u64, u64)]) -> Vec<Option<(usize, Level)>> {
    let mut out: Vec<Option<(usize, Level)>> = vec![None; old.funcs.len()];
    let mut taken: Vec<bool> = vec![false; new.funcs.len()];
    let set = |out: &mut Vec<Option<(usize, Level)>>,
               taken: &mut Vec<bool>,
               i: usize,
               j: usize,
               l: Level| {
        if out[i].is_none() && !taken[j] {
            out[i] = Some((j, l));
            taken[j] = true;
        }
    };

    // exact fingerprints
    let key = |f: &Func| (f.fp.instructions >= 3).then_some(f.fp.exact);
    let (uo, un) = (unique_by(&old.funcs, key), unique_by(&new.funcs, key));
    for (k, i) in &uo {
        if let (Some(i), Some(Some(j))) = (i, un.get(k)) {
            set(&mut out, &mut taken, *i, *j, Level::Exact);
        }
    }

    // strings referred to by exactly one function in each build; all of a function's must agree
    let refs = |db: &Db| {
        let mut m: HashMap<u64, Option<usize>> = HashMap::new();
        for (i, f) in db.funcs.iter().enumerate() {
            for s in &f.strs {
                m.entry(*s).and_modify(|v| *v = None).or_insert(Some(i));
            }
        }
        m
    };
    let (ro, rn) = (refs(old), refs(new));
    for (i, f) in old.funcs.iter().enumerate() {
        if out[i].is_some() {
            continue;
        }
        let mut votes: Vec<usize> = Vec::new();
        for s in &f.strs {
            if ro.get(s) == Some(&Some(i)) {
                if let Some(Some(j)) = rn.get(s) {
                    votes.push(*j);
                }
            }
        }
        if let Some(&j) = votes.first() {
            let (x, y) = (
                f.fp.instructions as f64,
                new.funcs[j].fp.instructions as f64,
            );
            let size_close = x.max(y) > 0.0 && (x - y).abs() / x.max(y) <= 0.3;
            if votes.iter().all(|&v| v == j) && (votes.len() >= 2 || size_close) {
                set(&mut out, &mut taken, i, j, Level::Strings);
            }
        }
    }

    // opcode shape
    let key = |f: &Func| (f.fp.instructions >= 8).then_some(f.fp.coarse);
    let (uo, un) = (unique_by(&old.funcs, key), unique_by(&new.funcs, key));
    for (k, i) in &uo {
        if let (Some(i), Some(Some(j))) = (i, un.get(k)) {
            set(&mut out, &mut taken, *i, *j, Level::Shape);
        }
    }

    // roots handed in by the caller: functions paired by their place in a virtual table
    for (a, b) in seeds {
        if let (Some(i), Some(j)) = (old.index_of(*a), new.index_of(*b)) {
            set(&mut out, &mut taken, i, j, Level::VTable);
        }
    }

    for _round in 0..6 {
        let mut progressed = false;

        // call graph: the k-th call of a matched pair with equally many calls is the same callee,
        // when the two callees also look alike
        let mut votes: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, f) in old.funcs.iter().enumerate() {
            let Some((j, _)) = out[i] else { continue };
            let g = &new.funcs[j];
            if f.calls.is_empty() || f.calls.len() != g.calls.len() {
                continue;
            }
            for (a, b) in f.calls.iter().zip(&g.calls) {
                if let (Some(ci), Some(cj)) = (old.index_of(*a), new.index_of(*b)) {
                    if out[ci].is_none() && !taken[cj] {
                        votes.entry(ci).or_default().push(cj);
                    }
                }
            }
        }
        let mut claims: HashMap<usize, Vec<usize>> = HashMap::new();
        for (ci, v) in &votes {
            if v.iter().all(|&x| x == v[0])
                && old.funcs[*ci].fp.instructions >= WEAK_MIN_INSTRUCTIONS
                && similarity(old, new, &out, &taken, *ci, v[0]) >= CALLGRAPH_MIN
            {
                claims.entry(v[0]).or_default().push(*ci);
            }
        }
        for (cj, cis) in claims {
            if cis.len() == 1 {
                set(&mut out, &mut taken, cis[0], cj, Level::CallGraph);
                progressed = true;
            }
        }

        // neighbours: functions keep their order in `.text`, so the unmatched ones between two
        // matched neighbours are aligned among themselves by similarity
        progressed |= match_between_anchors(old, new, &mut out, &mut taken);
        progressed |= match_sandwiched(old, new, &mut out, &mut taken);
        progressed |= match_by_context(old, new, &mut out, &mut taken);
        if !progressed {
            break;
        }
    }
    out
}

const CALLGRAPH_MIN: f64 = 0.55;
const NEIGHBOUR_MIN: f64 = 0.6;
const NEIGHBOUR_SAME_COUNT_MIN: f64 = 0.4;
const GAP_LIMIT: usize = 48;

/// How alike two functions look, 0..1: identical bodies 1, same opcode shape 0.9, otherwise a
/// blend of what survives a rewrite of the body: size, the string literals referred to, and the
/// already matched functions it calls and is called by.
/// Below this many instructions a function has too many look-alikes for the weak levels.
const WEAK_MIN_INSTRUCTIONS: u32 = 16;

pub fn similarity(
    old: &Db,
    new: &Db,
    out: &[Option<(usize, Level)>],
    taken: &[bool],
    i: usize,
    j: usize,
) -> f64 {
    let (a, b) = (&old.funcs[i], &new.funcs[j]);
    if a.fp.exact == b.fp.exact {
        return 1.0;
    }
    if a.fp.coarse == b.fp.coarse {
        return 0.9;
    }
    let (x, y) = (a.fp.instructions as f64, b.fp.instructions as f64);
    let size = if x.max(y) == 0.0 {
        1.0
    } else {
        1.0 - (x - y).abs() / x.max(y)
    };
    let mut parts: Vec<(f64, f64)> = vec![(0.3, size)];
    if !(a.strs.is_empty() && b.strs.is_empty()) {
        let shared = a
            .strs
            .iter()
            .filter(|s| b.strs.binary_search(s).is_ok())
            .count() as f64;
        let union = (a.strs.len() + b.strs.len()) as f64 - shared;
        parts.push((0.2, shared / union.max(1.0)));
    }
    // neighbours in the call graph, compared through the matches made so far; functions not
    // matched on either side count against the pair
    let through = |xs: &[usize], ys: &[usize]| -> Option<f64> {
        if xs.is_empty() && ys.is_empty() {
            return None;
        }
        let mapped: Vec<usize> = xs.iter().filter_map(|&k| out[k].map(|m| m.0)).collect();
        let inter = mapped
            .iter()
            .filter(|m| ys.binary_search(m).is_ok())
            .count() as f64;
        let pending = ys
            .iter()
            .filter(|&&k| !taken[k])
            .count()
            .min(xs.len() - mapped.len());
        let union = (xs.len() + ys.len()) as f64 - inter - pending as f64;
        Some((inter + 0.5 * pending as f64) / union.max(1.0))
    };
    if let Some(v) = through(&old.callees[i], &new.callees[j]) {
        parts.push((0.3, v));
    }
    if let Some(v) = through(&old.callers[i], &new.callers[j]) {
        parts.push((0.2, v));
    }
    let w: f64 = parts.iter().map(|p| p.0).sum();
    0.85 * parts.iter().map(|p| p.0 * p.1).sum::<f64>() / w
}

/// An unmatched function whose nearest matched neighbours on both sides (at most three steps
/// away) map to two close functions of the new build in the same order: the unmatched functions
/// between those two are its only candidates. One candidate for one function is accepted at a low
/// similarity; among several the best one must stand clear of the second. A function too small
/// for the weak levels is taken only in the tightest case: both direct neighbours matched, to
/// functions with exactly one unmatched function between them.
fn match_sandwiched(
    old: &Db,
    new: &Db,
    out: &mut [Option<(usize, Level)>],
    taken: &mut [bool],
) -> bool {
    let mut progressed = false;
    for i in 0..old.funcs.len() {
        if out[i].is_some() {
            continue;
        }
        let small = old.funcs[i].fp.instructions < WEAK_MIN_INSTRUCTIONS;
        let reach = if small { 1 } else { 3 };
        let prev =
            (1..=reach).find_map(|d| i.checked_sub(d).and_then(|p| out[p].map(|m| (p, m.0))));
        let next =
            (1..=reach).find_map(|d| out.get(i + d).copied().flatten().map(|m| (i + d, m.0)));
        let (Some((p, a)), Some((q, b))) = (prev, next) else {
            continue;
        };
        if b <= a || b - a > 8 {
            continue;
        }
        let pending = (p + 1..q).filter(|&k| out[k].is_none()).count();
        let cands: Vec<usize> = (a + 1..b).filter(|&k| !taken[k]).collect();
        if cands.is_empty() {
            continue;
        }
        let mut scored: Vec<(f64, usize)> = cands
            .iter()
            .map(|&k| (similarity(old, new, out, taken, i, k), k))
            .collect();
        scored.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap());
        let (best, j) = scored[0];
        let ok = if small {
            pending == 1 && cands.len() == 1 && b - a == 2 && best >= 0.5
        } else if pending == 1 && cands.len() == 1 {
            best >= 0.25
        } else {
            best >= 0.4 && scored.get(1).map(|s| best - s.0 >= 0.1).unwrap_or(true)
        };
        if ok {
            out[i] = Some((j, Level::Neighbours));
            taken[j] = true;
            progressed = true;
        }
    }
    progressed
}

const CONTEXT_MIN: f64 = 0.62;
const CONTEXT_MARGIN: f64 = 0.12;

/// Candidates for an unmatched function are the unmatched functions its surroundings lead to:
/// what the counterparts of its callers call, what calls the counterparts of its callees, and
/// the stretch of `.text` its matched neighbours map to. The most similar one is taken when it
/// is similar enough, clearly ahead of the runner-up, and no other old function wants it.
fn match_by_context(
    old: &Db,
    new: &Db,
    out: &mut [Option<(usize, Level)>],
    taken: &mut [bool],
) -> bool {
    let mut proposals: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    for i in 0..old.funcs.len() {
        if out[i].is_some() || old.funcs[i].fp.instructions < WEAK_MIN_INSTRUCTIONS {
            continue;
        }
        let mut cands: BTreeSet<usize> = BTreeSet::new();
        for &c in &old.callers[i] {
            if let Some((c2, _)) = out[c] {
                cands.extend(new.callees[c2].iter().filter(|&&k| !taken[k]));
            }
        }
        for &d in &old.callees[i] {
            if let Some((d2, _)) = out[d] {
                cands.extend(new.callers[d2].iter().filter(|&&k| !taken[k]));
            }
        }
        let before = (1..=6).find_map(|d| i.checked_sub(d).and_then(|p| out[p].map(|m| m.0)));
        let after = (1..=6).find_map(|d| out.get(i + d).copied().flatten().map(|m| m.0));
        for near in [before, after].into_iter().flatten() {
            let lo = near.saturating_sub(10);
            let hi = (near + 10).min(new.funcs.len() - 1);
            cands.extend((lo..=hi).filter(|&k| !taken[k]));
        }
        if cands.is_empty() || cands.len() > 400 {
            continue;
        }
        let mut scored: Vec<(f64, usize)> = cands
            .iter()
            .map(|&k| (similarity(old, new, out, taken, i, k), k))
            .collect();
        scored.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap());
        let (best, j) = scored[0];
        let clear = scored
            .get(1)
            .map(|s| best - s.0 >= CONTEXT_MARGIN)
            .unwrap_or(true);
        if best >= CONTEXT_MIN && clear {
            proposals.entry(j).or_default().push((i, best));
        }
    }
    let mut progressed = false;
    for (j, wants) in proposals {
        if wants.len() == 1 && !taken[j] {
            out[wants[0].0] = Some((j, Level::Context));
            taken[j] = true;
            progressed = true;
        }
    }
    progressed
}

const MUTUAL_MIN: f64 = 0.5;

fn jaccard<T: Ord>(a: &[T], b: &[T]) -> Option<f64> {
    if a.is_empty() && b.is_empty() {
        return None;
    }
    let shared = a.iter().filter(|x| b.binary_search(x).is_ok()).count() as f64;
    Some(shared / ((a.len() + b.len()) as f64 - shared).max(1.0))
}

/// Similarity for bodies that were rewritten: cosine of the mnemonic counts, shared field
/// displacements, shared strings, size, and the matched callers and callees.
pub fn rich_similarity(
    old: &Db,
    new: &Db,
    out: &[Option<(usize, Level)>],
    i: usize,
    j: usize,
) -> f64 {
    let (a, b) = (&old.funcs[i], &new.funcs[j]);
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    let (mut x, mut y) = (0usize, 0usize);
    while x < a.ops.len() || y < b.ops.len() {
        let ka = a.ops.get(x).map(|p| p.0).unwrap_or(u16::MAX);
        let kb = b.ops.get(y).map(|p| p.0).unwrap_or(u16::MAX);
        if ka == kb {
            let (va, vb) = (a.ops[x].1 as f64, b.ops[y].1 as f64);
            dot += va * vb;
            na += va * va;
            nb += vb * vb;
            x += 1;
            y += 1;
        } else if ka < kb {
            na += (a.ops[x].1 as f64).powi(2);
            x += 1;
        } else {
            nb += (b.ops[y].1 as f64).powi(2);
            y += 1;
        }
    }
    let cosine = if na > 0.0 && nb > 0.0 {
        dot / (na.sqrt() * nb.sqrt())
    } else {
        0.0
    };
    let (p, r) = (a.fp.instructions as f64, b.fp.instructions as f64);
    let size = if p.max(r) == 0.0 {
        1.0
    } else {
        1.0 - (p - r).abs() / p.max(r)
    };
    let mut parts: Vec<(f64, f64)> = vec![(0.25, cosine), (0.15, size)];
    if let Some(v) = jaccard(&a.disps, &b.disps) {
        parts.push((0.2, v));
    }
    if let Some(v) = jaccard(&a.strs, &b.strs) {
        parts.push((0.15, v));
    }
    let through = |xs: &[usize], ys: &[usize]| -> Option<f64> {
        let mapped: Vec<usize> = xs.iter().filter_map(|&k| out[k].map(|m| m.0)).collect();
        if mapped.is_empty() && ys.is_empty() {
            return None;
        }
        let inter = mapped.iter().filter(|m| ys.contains(m)).count() as f64;
        Some(inter / ((xs.len() + ys.len()) as f64 - inter).max(1.0))
    };
    if let Some(v) = through(&old.callees[i], &new.callees[j]) {
        parts.push((0.15, v));
    }
    if let Some(v) = through(&old.callers[i], &new.callers[j]) {
        parts.push((0.1, v));
    }
    let w: f64 = parts.iter().map(|p| p.0).sum();
    parts.iter().map(|p| p.0 * p.1).sum::<f64>() / w
}

/// Mutual best match over the whole image, for one function the passes above left unmatched:
/// its most similar unmatched new function, when that function's most similar unmatched old
/// function is this same one. No margin is asked for; the reciprocity is the evidence. Too
/// costly to run for every function, cheap for the few a gamedata file points at.
pub fn mutual_best(
    old: &Db,
    new: &Db,
    out: &[Option<(usize, Level)>],
    taken: &[bool],
    i: usize,
) -> Option<(usize, f64)> {
    if old.funcs[i].fp.instructions < WEAK_MIN_INSTRUCTIONS {
        return None;
    }
    let (j, s) = (0..new.funcs.len())
        .filter(|&k| !taken[k])
        .map(|k| (k, rich_similarity(old, new, out, i, k)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
    if s < MUTUAL_MIN {
        return None;
    }
    let back = (0..old.funcs.len())
        .filter(|&k| out[k].is_none())
        .map(|k| (k, rich_similarity(old, new, out, k, j)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
    (back.0 == i).then_some((j, s))
}

/// How much the matched callers and callees of two functions agree, 0..1; `None` when neither
/// side has at least two matched ones to judge by.
pub fn context_agreement(
    old: &Db,
    new: &Db,
    out: &[Option<(usize, Level)>],
    i: usize,
    j: usize,
) -> Option<f64> {
    let through = |xs: &[usize], ys: &[usize]| -> Option<f64> {
        let mapped: Vec<usize> = xs.iter().filter_map(|&k| out[k].map(|m| m.0)).collect();
        if mapped.len() < 2 {
            return None;
        }
        let inter = mapped.iter().filter(|m| ys.contains(m)).count() as f64;
        Some(inter / mapped.len() as f64)
    };
    let parts: Vec<f64> = [
        through(&old.callees[i], &new.callees[j]),
        through(&old.callers[i], &new.callers[j]),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.iter().sum::<f64>() / parts.len() as f64)
}

/// Where the code `delta` bytes into the old function is in the new one, when the instructions
/// up to that point are of the same kind in both: the same number of instructions further in.
fn follow_delta(
    old: &Image,
    new: &Image,
    old_start: u64,
    new_start: u64,
    delta: u64,
) -> Option<u64> {
    let ob = old.slice(&old.text, old_start, delta as usize + 16)?;
    let mut dec = Decoder::with_ip(64, ob, old_start, DecoderOptions::NONE);
    let mut ins = Instruction::default();
    let mut codes = Vec::new();
    while dec.ip() < old_start + delta {
        if !dec.can_decode() || codes.len() > 64 {
            return None;
        }
        dec.decode_out(&mut ins);
        if ins.is_invalid() {
            return None;
        }
        codes.push(ins.code());
    }
    if dec.ip() != old_start + delta {
        return None;
    }
    let nb = new.slice(&new.text, new_start, codes.len() * 15 + 16)?;
    let mut dec = Decoder::with_ip(64, nb, new_start, DecoderOptions::NONE);
    for c in &codes {
        if !dec.can_decode() {
            return None;
        }
        dec.decode_out(&mut ins);
        if ins.is_invalid() || ins.code() != *c {
            return None;
        }
    }
    Some(dec.ip())
}

/// Between every two consecutive anchors (matched pairs that keep their order in both builds)
/// align the unmatched functions by similarity; a pair is accepted at `NEIGHBOUR_MIN`.
fn match_between_anchors(
    old: &Db,
    new: &Db,
    out: &mut [Option<(usize, Level)>],
    taken: &mut [bool],
) -> bool {
    // anchors: the longest run of matches increasing in both builds
    let pairs: Vec<(usize, usize)> = out
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.map(|(j, _)| (i, j)))
        .collect();
    let mut tails: Vec<usize> = Vec::new(); // index into pairs of the smallest tail per length
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    for (k, &(_, j)) in pairs.iter().enumerate() {
        let pos = tails.partition_point(|&t| pairs[t].1 < j);
        prev[k] = if pos > 0 { Some(tails[pos - 1]) } else { None };
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut anchors = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(k) = cur {
        anchors.push(pairs[k]);
        cur = prev[k];
    }
    anchors.reverse();

    let mut progressed = false;
    for w in anchors.windows(2) {
        let ((i1, j1), (i2, j2)) = (w[0], w[1]);
        let a: Vec<usize> = (i1 + 1..i2)
            .filter(|&i| out[i].is_none() && old.funcs[i].fp.instructions >= WEAK_MIN_INSTRUCTIONS)
            .collect();
        let b: Vec<usize> = (j1 + 1..j2).filter(|&j| !taken[j]).collect();
        if a.is_empty() || b.is_empty() || a.len() > GAP_LIMIT || b.len() > GAP_LIMIT {
            continue;
        }
        // Needleman-Wunsch over the two short runs
        let (n, m) = (a.len(), b.len());
        let sim: Vec<Vec<f64>> = a
            .iter()
            .map(|&i| {
                b.iter()
                    .map(|&j| similarity(old, new, out, taken, i, j))
                    .collect()
            })
            .collect();
        const GAP: f64 = -0.2;
        let mut dp = vec![vec![0.0f64; m + 1]; n + 1];
        for (x, row) in dp.iter_mut().enumerate() {
            row[0] = x as f64 * GAP;
        }
        for (y, cell) in dp[0].iter_mut().enumerate() {
            *cell = y as f64 * GAP;
        }
        for x in 1..=n {
            for y in 1..=m {
                let pair = dp[x - 1][y - 1] + sim[x - 1][y - 1] - 0.5;
                dp[x][y] = pair.max(dp[x - 1][y] + GAP).max(dp[x][y - 1] + GAP);
            }
        }
        let (mut x, mut y) = (n, m);
        while x > 0 && y > 0 {
            let pair = dp[x - 1][y - 1] + sim[x - 1][y - 1] - 0.5;
            if dp[x][y] == pair {
                // the same number of unmatched functions on both sides is evidence by itself
                let need = if n == m {
                    NEIGHBOUR_SAME_COUNT_MIN
                } else {
                    NEIGHBOUR_MIN
                };
                if sim[x - 1][y - 1] >= need {
                    let (i, j) = (a[x - 1], b[y - 1]);
                    out[i] = Some((j, Level::Neighbours));
                    taken[j] = true;
                    progressed = true;
                }
                x -= 1;
                y -= 1;
            } else if dp[x][y] == dp[x - 1][y] + GAP {
                x -= 1;
            } else {
                y -= 1;
            }
        }
    }
    progressed
}

// ------------------------------------------------------------------------------------------
// generating a signature

/// A pattern for the code at `addr`: instruction bytes with every 4-byte displacement, every
/// near branch target and every address-like immediate wildcarded, extended instruction by
/// instruction until it matches exactly once in `.text`. `None` when `MAX_SIGNATURE` bytes are
/// not enough (identical functions folded apart, for example). A function with a twin needs a
/// long one: CS2Fixes' own `CS_Script_SetModel` signature is some 300 bytes.
pub const MAX_SIGNATURE: usize = 400;

pub fn generate(img: &Image, addr: u64, style: Style) -> Option<Pattern> {
    let text = img.text;
    let code = img.section_bytes(&text)?;
    let body = img.slice(&text, addr, MAX_SIGNATURE + 32)?;
    let mut dec = Decoder::with_ip(64, body, addr, DecoderOptions::NONE);
    let mut ins = Instruction::default();
    let mut bytes: Vec<Option<u8>> = Vec::new();
    while dec.can_decode() && bytes.len() < MAX_SIGNATURE {
        let at = (dec.ip() - addr) as usize;
        dec.decode_out(&mut ins);
        if ins.is_invalid() {
            return None;
        }
        let len = ins.len();
        let offs = dec.get_constant_offsets(&ins);
        let mut chunk: Vec<Option<u8>> = body[at..at + len].iter().map(|b| Some(*b)).collect();
        let mut mask = |o: usize, n: usize| {
            for b in chunk.iter_mut().skip(o).take(n) {
                *b = None;
            }
        };
        if offs.has_displacement() && offs.displacement_size() == 4 {
            mask(offs.displacement_offset(), 4);
        }
        if offs.has_immediate() {
            let is_branch = matches!(
                ins.op0_kind(),
                OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16
            );
            let size = offs.immediate_size();
            let addr_like = size >= 4
                && (0..ins.op_count()).any(|k| {
                    matches!(
                        ins.op_kind(k),
                        OpKind::Immediate32 | OpKind::Immediate64 | OpKind::Immediate32to64
                    ) && img.addr_like(ins.immediate(k))
                });
            if (is_branch && size == 4) || addr_like {
                mask(offs.immediate_offset(), size);
            }
        }
        bytes.extend(chunk);
        if bytes.len() >= 14 && bytes.iter().filter(|b| b.is_some()).count() >= 8 {
            let mut trimmed = bytes.clone();
            while trimmed.last() == Some(&None) {
                trimmed.pop();
            }
            let p = Pattern {
                bytes: trimmed,
                style,
            };
            let hits = p.find(code, 2);
            if hits.len() == 1 && text.lo + hits[0] as u64 == addr {
                return Some(p);
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------------------
// the report

#[derive(Clone, Debug)]
pub struct SigEntry {
    pub name: String,
    pub library: String,
    pub linux: Option<String>,
    pub windows: Option<String>,
    /// The file says the signature may match more than one function (`allow_multi_match`).
    pub multi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    /// The old signature still matches once in the new build (and at the matched function).
    StillValid,
    /// The function was found and a new unique signature was cut for it.
    Regenerated,
    /// The function was found but no unique signature could be cut, or the signature starts
    /// inside a function whose body changed.
    Located,
    /// The old signature still matches once, but not where the function matcher points.
    Conflict,
    /// The function was not found in the new build.
    NotFound,
    /// The old signature does not match exactly once in the old build.
    OldBroken,
    /// A symbol reference, another library, or no signature for this OS.
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
pub struct SigResult {
    pub entry: String,
    pub status: Status,
    pub level: Option<Level>,
    pub old_sig: Option<String>,
    pub new_sig: Option<String>,
    pub old_addr: Option<u64>,
    pub new_addr: Option<u64>,
    pub verdict: String,
    /// How much the old function and the one found look alike, 0..1 (`rich_similarity`): a
    /// measure of likeness, not a probability.
    pub similarity: Option<f64>,
    /// For a signature whose function was not found: the most similar unmatched functions of
    /// the new build, best first, for a person to choose from.
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub addr: u64,
    pub similarity: f64,
    /// A unique signature for that function, when one could be cut.
    pub signature: Option<String>,
}

/// How often each level was right on the gamedata history the matcher was tuned on (Linux and
/// Windows together, signatures the maintainers had to change): `(right, judged)`.
pub fn history(level: Level) -> (u32, u32) {
    match level {
        Level::Exact => (3, 3),
        Level::Strings => (63, 63),
        Level::VTable => (0, 0),
        Level::Shape => (10, 11),
        Level::CallGraph => (59, 59),
        Level::Neighbours => (67, 67),
        Level::Context => (8, 8),
        Level::Mutual => (10, 10),
    }
}

/// Which regenerated signatures may be written into a file without a person looking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// An exact fingerprint or the function's own strings.
    Strong,
    /// Also the levels that were judged at least 50 times in the history and never wrong.
    /// The history is the one the matcher was tuned on: a record of the past, not a proof
    /// for the next update.
    Historical,
    /// Everything the matcher found.
    Likely,
}

impl Trust {
    pub fn parse(s: &str) -> Option<Trust> {
        match s {
            "strong" => Some(Trust::Strong),
            "historical" => Some(Trust::Historical),
            "likely" => Some(Trust::Likely),
            _ => None,
        }
    }

    pub fn allows(self, level: Level) -> bool {
        let strong = level <= Level::Strings;
        match self {
            Trust::Strong => strong,
            Trust::Historical => {
                let (right, judged) = history(level);
                strong || (judged >= 50 && right == judged)
            }
            Trust::Likely => true,
        }
    }
}

/// A signature match is taken for a function start when it sits on a 16-byte boundary or right
/// behind a `ret`, an `int3` or a `nop`: a function nothing calls directly is in no other list.
fn starts_a_function(img: &Image, code: &[u8], at: usize) -> bool {
    (img.text.lo + at as u64).is_multiple_of(16)
        || (at > 0 && matches!(code[at - 1], 0xC3 | 0xCC | 0x90))
}

/// Function pairs the vtable alignment is sure about: the same slot of the same class in both
/// builds, at any level but a guess, and never two different answers for one function.
pub fn vtable_seeds(old: &Image, new: &Image) -> Vec<(u64, u64)> {
    use crate::diff::{self, Confidence};
    let (ro, rn) = (crate::rtti::analyze(old), crate::rtti::analyze(new));
    let mut fo = Fingerprinter::new(old, &ro.func_starts);
    let mut fnew = Fingerprinter::new(new, &rn.func_starts);
    let mut votes: HashMap<u64, Option<u64>> = HashMap::new();
    for (class, &io) in &ro.primary {
        let Some(&inew) = rn.primary.get(class) else {
            continue;
        };
        let (a, b) = (&ro.vtables[io].slots, &rn.vtables[inew].slots);
        let fa: Vec<Fp> = a.iter().map(|t| fo.of(t)).collect();
        let fb: Vec<Fp> = b.iter().map(|t| fnew.of(t)).collect();
        let al = diff::align(&fa, &fb);
        for (i, m) in al.map.iter().enumerate() {
            let (Some(j), Target::Addr(x), Some(Target::Addr(y))) =
                (m, &a[i], m.and_then(|j| b.get(j)))
            else {
                continue;
            };
            let _ = j;
            if al.confidence[i] == Confidence::Guess {
                continue;
            }
            votes
                .entry(*x)
                .and_modify(|v| {
                    if *v != Some(*y) {
                        *v = None;
                    }
                })
                .or_insert(Some(*y));
        }
    }
    // one new function must not be claimed by two old ones either
    let mut back: HashMap<u64, Option<u64>> = HashMap::new();
    for (x, y) in votes.iter().filter_map(|(x, y)| y.map(|y| (*x, y))) {
        back.entry(y).and_modify(|v| *v = None).or_insert(Some(x));
    }
    back.into_iter()
        .filter_map(|(y, x)| x.map(|x| (x, y)))
        .collect()
}

/// Which gamedata `library` a binary is, by its file name.
pub fn library_of(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let name = name.split('.').next().unwrap_or(&name);
    let name = name.strip_prefix("lib").unwrap_or(name);
    // the engine library is `engine2`, gamedata files call it `engine`
    if name == "engine2" {
        "engine".to_string()
    } else {
        name.to_string()
    }
}

pub struct Analysis {
    pub results: Vec<SigResult>,
    pub old_db: Db,
    pub new_db: Db,
    pub matches: Vec<Option<(usize, Level)>>,
}

pub fn analyze(old: &Image, new: &Image, entries: &[SigEntry]) -> Analysis {
    let os = old.format.os();
    let lib = library_of(&old.path);
    let old_code = old.section_bytes(&old.text).unwrap_or(&[]);
    let new_code = new.section_bytes(&new.text).unwrap_or(&[]);

    // first locate every signature in the old build: those addresses are function starts too
    struct Located {
        pat: Pattern,
        old_hits: Vec<usize>,
        new_hits: Vec<usize>,
    }
    let mut located: Vec<Option<Located>> = Vec::new();
    let mut extra_old = Vec::new();
    let mut extra_new = Vec::new();
    for e in entries {
        let raw = if os == "windows" {
            &e.windows
        } else {
            &e.linux
        };
        let pat = raw.as_deref().and_then(Pattern::parse);
        let right_lib = e.library == lib || (e.library.is_empty() && lib == "server");
        match pat {
            Some(pat) if right_lib => {
                let old_hits = pat.find(old_code, 2);
                let new_hits = pat.find(new_code, 2);
                // a function-aligned match is a function start even when nothing calls it directly
                if old_hits.len() == 1 && starts_a_function(old, old_code, old_hits[0]) {
                    extra_old.push(old.text.lo + old_hits[0] as u64);
                }
                if new_hits.len() == 1 && starts_a_function(new, new_code, new_hits[0]) {
                    extra_new.push(new.text.lo + new_hits[0] as u64);
                }
                located.push(Some(Located {
                    pat,
                    old_hits,
                    new_hits,
                }));
            }
            _ => located.push(None),
        }
    }
    let old_db = Db::build(old, &extra_old);
    let new_db = Db::build(new, &extra_new);
    // vtable slot pairs as extra roots (`vtable_seeds`) and characteristic constants in the
    // similarity were both measured against the history and gained nothing (12.11.2026), so
    // neither is switched on
    let mut matches = match_functions(&old_db, &new_db, &[]);
    let mut taken = vec![false; new_db.funcs.len()];
    for m in matches.iter().flatten() {
        taken[m.0] = true;
    }

    let mut results = Vec::new();
    for (e, loc) in entries.iter().zip(located) {
        let mut r = SigResult {
            entry: e.name.clone(),
            status: Status::Skipped,
            level: None,
            old_sig: None,
            new_sig: None,
            old_addr: None,
            new_addr: None,
            verdict: String::new(),
            similarity: None,
            candidates: Vec::new(),
        };
        let Some(loc) = loc else {
            r.verdict = "not a byte signature for this binary and OS".to_string();
            results.push(r);
            continue;
        };
        r.old_sig = Some(loc.pat.render());
        if e.multi && !loc.old_hits.is_empty() {
            // the file itself says several functions may match: nothing to follow, only whether
            // it still matches at all
            if loc.new_hits.is_empty() {
                r.status = Status::NotFound;
                r.verdict =
                    "BROKEN: a multi-match signature that matches nowhere in the new build; by hand"
                        .to_string();
            } else {
                r.status = Status::StillValid;
                r.verdict = "a multi-match signature, still matches in the new build".to_string();
            }
            results.push(r);
            continue;
        }
        if loc.old_hits.len() != 1 {
            r.status = Status::OldBroken;
            r.verdict = format!(
                "the old signature matches {} in the old build, nothing to follow",
                if loc.old_hits.is_empty() {
                    "nowhere"
                } else {
                    "more than once"
                }
            );
            // it may still be fine in the new build: say so
            if loc.new_hits.len() == 1 {
                r.new_addr = Some(new.text.lo + loc.new_hits[0] as u64);
                r.verdict.push_str("; it matches once in the new build");
            }
            results.push(r);
            continue;
        }
        let m_old = old.text.lo + loc.old_hits[0] as u64;
        r.old_addr = Some(m_old);
        let still = (loc.new_hits.len() == 1).then(|| new.text.lo + loc.new_hits[0] as u64);
        let fi = old_db.containing(m_old);
        // last resort for this one function: the mutual best match over the whole image
        if let Some(i) = fi {
            if matches[i].is_none() {
                if let Some((j, _)) = mutual_best(&old_db, &new_db, &matches, &taken, i) {
                    matches[i] = Some((j, Level::Mutual));
                    taken[j] = true;
                }
            }
        }
        // a shape match whose matched callers and callees point elsewhere is not trusted
        let matched = fi
            .and_then(|i| matches[i].map(|(j, l)| (i, j, l)))
            .filter(|&(i, j, l)| {
                l != Level::Shape
                    || context_agreement(&old_db, &new_db, &matches, i, j)
                        .map(|v| v >= 0.25)
                        .unwrap_or(true)
            });
        r.similarity = matched.map(|(i, j, _)| rich_similarity(&old_db, &new_db, &matches, i, j));
        let mut predicted = matched.map(|(i, j, l)| {
            let delta = m_old - old_db.funcs[i].start;
            (new_db.funcs[j].start, delta, l)
        });
        // the old bytes still match once, but in another function than the one the matcher
        // found: compare how much each of the two looks like the old function
        if let (Some(x), Some((i, j, l))) = (still, matched) {
            let hit = new_db.containing(x);
            if hit != Some(j) {
                let s_match = similarity(&old_db, &new_db, &matches, &taken, i, j);
                let s_hit = hit
                    .map(|k| similarity(&old_db, &new_db, &matches, &taken, i, k))
                    .unwrap_or(0.0);
                if l == Level::Exact || (s_match >= 0.65 && s_hit <= 0.35) {
                    // never resolved automatically: either the bytes drifted onto another
                    // function, or the matcher is wrong; a person has to look
                    r.status = Status::Conflict;
                    r.level = Some(l);
                    r.verdict = format!(
                        "CONFLICT: the old bytes still match once, at {x:#x}, but that function does not look like the old one (similarity {s_hit:.2}); the matcher points at {:#x} ({l:?}, similarity {s_match:.2}); check by hand",
                        new_db.funcs[j].start
                    );
                    results.push(r);
                    continue;
                } else if s_hit >= s_match + 0.15 {
                    predicted = None;
                }
            }
        }

        match (still, predicted) {
            (Some(x), Some((fstart, _, l))) => {
                let same = new_db.containing(x).map(|k| new_db.funcs[k].start) == Some(fstart);
                r.new_addr = Some(x);
                r.level = Some(l);
                if same {
                    r.status = Status::StillValid;
                    r.verdict =
                        format!("unchanged: still matches once, at the same function ({l:?})");
                } else {
                    r.status = Status::Conflict;
                    r.verdict = format!(
                        "CONFLICT: still matches once at {x:#x}, but the function matcher points at {fstart:#x} ({l:?}); check by hand"
                    );
                }
            }
            (Some(x), None) => {
                r.status = Status::StillValid;
                r.new_addr = Some(x);
                r.verdict = "unchanged: still matches once (the function itself was not matched)"
                    .to_string();
            }
            (None, Some((fstart, delta, l))) => {
                r.level = Some(l);
                let target = if delta == 0 || l == Level::Exact {
                    Some(fstart + delta)
                } else {
                    follow_delta(old, new, m_old - delta, fstart, delta)
                };
                match target {
                    None => {
                        r.status = Status::Located;
                        r.verdict = format!(
                            "BROKEN: the function is at {fstart:#x} ({l:?}), but the signature started {delta} bytes into it and the code before that point changed; cut it by hand"
                        );
                    }
                    Some(target) => {
                        r.new_addr = Some(target);
                        match generate(new, target, loc.pat.style) {
                            Some(p) => {
                                r.status = Status::Regenerated;
                                r.new_sig = Some(p.render());
                                r.verdict =
                                    format!("BROKEN: new signature for {target:#x} ({l:?})");
                            }
                            None => {
                                r.status = Status::Located;
                                r.verdict = format!(
                                    "BROKEN: the function is at {target:#x} ({l:?}), but no unique signature fits in {MAX_SIGNATURE} bytes"
                                );
                            }
                        }
                    }
                }
            }
            (None, None) => {
                r.status = Status::NotFound;
                // nothing passed the bar: leave a person the nearest unmatched functions
                if let Some(i) = fi.filter(|&i| old_db.funcs[i].start == m_old) {
                    let mut near: Vec<(usize, f64)> = (0..new_db.funcs.len())
                        .filter(|&k| !taken[k])
                        .map(|k| (k, rich_similarity(&old_db, &new_db, &matches, i, k)))
                        .collect();
                    near.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    r.candidates = near
                        .into_iter()
                        .take(3)
                        .map(|(k, s)| Candidate {
                            addr: new_db.funcs[k].start,
                            similarity: s,
                            signature: generate(new, new_db.funcs[k].start, loc.pat.style)
                                .map(|p| p.render()),
                        })
                        .collect();
                }
                r.verdict =
                    format!(
                    "BROKEN: matches {} in the new build and the function was not found; by hand",
                    if loc.new_hits.is_empty() { "nowhere" } else { "more than once" }
                );
            }
        }
        results.push(r);
    }
    Analysis {
        results,
        old_db,
        new_db,
        matches,
    }
}

// ------------------------------------------------------------------------------------------
// validation against what the maintainers committed

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Outcome {
    Correct,
    Wrong,
    Unplaced,
}

#[derive(Serialize)]
pub struct SigValidationRow {
    pub entry: String,
    /// The maintainers changed the signature text in this update.
    pub changed: bool,
    /// The old signature no longer matched once in the new build: the update really broke it.
    pub broken: bool,
    pub outcome: Outcome,
    /// StillValid, or the level the function match rests on, or the status that left it unplaced.
    pub confidence: String,
    pub predicted: Option<u64>,
    pub actual: u64,
    pub verdict: String,
    /// Old function and the true new one side by side: instructions/calls/strings, similarity,
    /// how many unmatched functions surround each of them.
    pub debug: String,
}

#[derive(Serialize)]
pub struct SigValidation {
    pub rows: Vec<SigValidationRow>,
    /// Entries left out and why (no truth to compare with).
    pub skipped: Vec<(String, String)>,
}

/// Predict from the gamedata BEFORE an update, compare with the signatures the maintainers
/// committed AFTER it: the truth is where their new signature matches in the new build, and a
/// prediction is right when it lands in the same function.
pub fn validate(
    old: &Image,
    new: &Image,
    before: &[SigEntry],
    after: &[SigEntry],
) -> SigValidation {
    let os = old.format.os();
    let new_code = new.section_bytes(&new.text).unwrap_or(&[]);
    let pick = |e: &SigEntry| {
        if os == "windows" {
            e.windows.clone()
        } else {
            e.linux.clone()
        }
    };
    let an = analyze(old, new, before);
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for (e, r) in before.iter().zip(&an.results) {
        if r.status == Status::Skipped {
            continue;
        }
        let Some(a) = after.iter().find(|a| a.name == e.name) else {
            skipped.push((e.name.clone(), "removed after the update".to_string()));
            continue;
        };
        let Some(truth_pat) = pick(a).as_deref().and_then(Pattern::parse) else {
            skipped.push((
                e.name.clone(),
                "no byte signature after the update".to_string(),
            ));
            continue;
        };
        let hits = truth_pat.find(new_code, 2);
        if hits.len() != 1 {
            skipped.push((
                e.name.clone(),
                format!(
                    "the maintainers' signature matches {} times in the new build",
                    hits.len()
                ),
            ));
            continue;
        }
        if r.status == Status::OldBroken {
            skipped.push((
                e.name.clone(),
                "the old signature does not match once in the old build".to_string(),
            ));
            continue;
        }
        let actual = new.text.lo + hits[0] as u64;
        let truth_fn = an
            .new_db
            .containing(actual)
            .map(|k| an.new_db.funcs[k].start);
        let predicted = match r.status {
            Status::StillValid | Status::Regenerated | Status::Located => r.new_addr,
            _ => None,
        };
        let outcome = match predicted {
            None => Outcome::Unplaced,
            Some(p) if p == actual => Outcome::Correct,
            Some(p)
                if an.new_db.containing(p).map(|k| an.new_db.funcs[k].start) == truth_fn
                    && truth_fn.is_some() =>
            {
                Outcome::Correct
            }
            Some(_) => Outcome::Wrong,
        };
        let confidence = match r.status {
            Status::StillValid => "StillValid".to_string(),
            Status::Regenerated | Status::Located => format!("{:?}", r.level.unwrap()),
            s => format!("{s:?}"),
        };
        let debug = (|| {
            let oi = an.old_db.containing(r.old_addr?)?;
            let ti = an.new_db.containing(actual)?;
            let (f, g) = (&an.old_db.funcs[oi], &an.new_db.funcs[ti]);
            let taken: std::collections::HashSet<usize> = an.matches.iter().flatten().map(|m| m.0).collect();
            let taken_vec: Vec<bool> = (0..an.new_db.funcs.len()).map(|k| taken.contains(&k)).collect();
            let gap_old = (1..200).take_while(|d| oi >= *d && an.matches[oi - d].is_none()).count()
                + (1..200).take_while(|d| oi + d < an.matches.len() && an.matches[oi + d].is_none()).count();
            let gap_new = (1..200).take_while(|d| ti >= *d && !taken.contains(&(ti - d))).count()
                + (1..200).take_while(|d| ti + d < an.new_db.funcs.len() && !taken.contains(&(ti + d))).count();
            let rich = rich_similarity(&an.old_db, &an.new_db, &an.matches, oi, ti);
            let better = (0..an.new_db.funcs.len())
                .filter(|&k| !taken.contains(&k) && k != ti)
                .filter(|&k| rich_similarity(&an.old_db, &an.new_db, &an.matches, oi, k) >= rich)
                .count();
            let back_better = (0..an.old_db.funcs.len())
                .filter(|&k| an.matches[k].is_none() && k != oi)
                .filter(|&k| rich_similarity(&an.old_db, &an.new_db, &an.matches, k, ti) >= rich)
                .count();
            let extra = format!(" | rich {rich:.2} rank {better} back {back_better}");
            Some(format!(
                "old #{} {:#x}+{} ins {} calls {} strs {} | truth #{} {:#x}+{} ins {} calls {} strs {} taken {} | sim {:.2} | unmatched around old {} new {} | old neighbours map to {:?} .. {:?}",
                oi, f.start, r.old_addr? - f.start, f.fp.instructions, f.calls.len(), f.strs.len(),
                ti, g.start, actual - g.start, g.fp.instructions, g.calls.len(), g.strs.len(), taken.contains(&ti),
                similarity(&an.old_db, &an.new_db, &an.matches, &taken_vec, oi, ti), gap_old, gap_new,
                (1..50).find_map(|d| an.matches.get(oi.wrapping_sub(d)).copied().flatten().map(|m| m.0)),
                (1..50).find_map(|d| an.matches.get(oi + d).copied().flatten().map(|m| m.0))
            ) + &extra)
        })()
        .unwrap_or_default();
        rows.push(SigValidationRow {
            debug,
            entry: e.name.clone(),
            changed: pick(e) != pick(a),
            broken: r.status != Status::StillValid && r.status != Status::Conflict,
            outcome,
            confidence,
            predicted,
            actual,
            verdict: r.verdict.clone(),
        });
    }
    SigValidation { rows, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_notations_parse_and_render_back() {
        let p = Pattern::parse("48 89 5C 24 ? 57 ?? 8B").unwrap();
        assert_eq!(p.style, Style::Spaced);
        assert_eq!(p.bytes.len(), 8);
        assert_eq!(p.bytes[4], None);
        assert_eq!(p.render(), "48 89 5C 24 ? 57 ? 8B");
        // KeyValues: the tokenizer has already dropped the backslashes
        let q = Pattern::parse("x40x53x2Ax2Ax48").unwrap();
        assert_eq!(q.style, Style::Escaped);
        assert_eq!(
            q.bytes,
            vec![Some(0x40), Some(0x53), None, None, Some(0x48)]
        );
        assert_eq!(q.render(), "\\x40\\x53\\x2A\\x2A\\x48");
        assert_eq!(Pattern::parse("\\x40\\x53\\x2A").unwrap().bytes.len(), 3);
        assert!(Pattern::parse("@Msg").is_none(), "a symbol, not bytes");
        assert!(Pattern::parse("? ? ?").is_none(), "nothing fixed");
        assert!(Pattern::parse("zz 11").is_none());
    }

    #[test]
    fn find_honours_wildcards_and_the_limit() {
        let hay = [
            0x55, 0x48, 0x89, 0xE5, 0x90, 0x55, 0x48, 0x8B, 0xE5, 0x55, 0x48,
        ];
        let p = Pattern::parse("55 48 ? E5").unwrap();
        assert_eq!(p.find(&hay, 10), vec![0, 5]);
        assert_eq!(p.find(&hay, 1), vec![0]);
        let q = Pattern::parse("? 48 89").unwrap();
        assert_eq!(
            q.find(&hay, 10),
            vec![0],
            "a leading wildcard still anchors correctly"
        );
        assert!(Pattern::parse("55 48 89 E5 90 55 48 8B E5 55 48 00")
            .unwrap()
            .find(&hay, 2)
            .is_empty());
    }

    #[test]
    fn library_from_the_file_name() {
        assert_eq!(
            library_of("/x/game/csgo/bin/linuxsteamrt64/libserver.so"),
            "server"
        );
        assert_eq!(library_of("C:\\x\\win64\\engine2.dll"), "engine");
        assert_eq!(library_of("libtier0.so"), "tier0");
        assert_eq!(
            library_of("libsteamnetworkingsockets.so"),
            "steamnetworkingsockets"
        );
    }

    #[test]
    fn historical_is_what_the_history_never_caught_wrong() {
        assert!(Trust::Strong.allows(Level::Strings));
        assert!(!Trust::Strong.allows(Level::CallGraph));
        assert!(Trust::Historical.allows(Level::CallGraph));
        assert!(Trust::Historical.allows(Level::Neighbours));
        // once wrong, and too few cases to say
        assert!(!Trust::Historical.allows(Level::Shape));
        assert!(!Trust::Historical.allows(Level::Context));
        assert!(Trust::Likely.allows(Level::Mutual));
    }
}
