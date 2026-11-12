//! Comparing two images class by class and mapping gamedata offsets; the result is a plain
//! data structure that the CLI prints and tests inspect.

use crate::diff::{self, Confidence, Gap, ShiftRange};
use crate::elf::Image;
use crate::fp::{Fingerprinter, Fp};
use crate::gamedata::{self, ClassMap};
use crate::rtti;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Serialize)]
pub struct ImageInfo {
    pub path: String,
    pub version: String,
    pub classes: usize,
    pub slots: usize,
}

#[derive(Serialize)]
pub struct ClassReport {
    pub class: String,
    pub old_slots: usize,
    pub new_slots: usize,
    /// Non-zero shifts of matched old slots.
    pub shifts: Vec<ShiftRange>,
    pub unmatched_old: Vec<usize>,
    pub inserted_new: Vec<usize>,
    /// Matched slots whose body changed.
    pub changed_in_place: usize,
    /// Pairs matched by code shape or by position between identical-looking bodies.
    pub likely_pairs: usize,
    /// Pairs matched by position between differing bodies.
    pub guessed_pairs: usize,
    /// Regions paired by position where bodies or counts differ.
    pub uncertain_regions: Vec<Gap>,
}

/// Why an entry stands where it stands. Decisions are taken on this, never on the wording
/// of the verdict.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A new index was found.
    Placed,
    /// The class is here and the index is inside its table, but the slot has no counterpart.
    Unmatched,
    /// The index is beyond the class's table: stale, tied to the wrong class, or not a
    /// virtual-table index at all. The binary cannot tell which.
    BeyondTable,
    /// The entry could not be tied to a class.
    NoClass,
    /// This library has nothing to say: the class is not in it, or nothing here implements the
    /// interface.
    NothingHere,
    /// The class is here but the entry could not be checked (an ambiguous name, an
    /// implementation that cannot be followed).
    Unresolved,
    /// Declared not to be a virtual-table index (`Entry -` in the class map).
    OutOfScope,
}

#[derive(Serialize, Clone)]
pub struct GamedataResult {
    pub file: String,
    pub entry: String,
    pub class: Option<String>,
    pub old_index: Option<i64>,
    pub new_index: Option<i64>,
    /// How sure the mapping is; None when the entry could not be placed at all.
    pub confidence: Option<Confidence>,
    /// `unchanged`, `SHIFT`, `LIKELY`, `GUESS`, `UNMATCHED`, or why it was not placed.
    pub verdict: String,
    /// For an interface entry: the implementation whose table answered.
    pub via: Option<String>,
    /// The class is known to this library (it has its typeinfo), whether or not the entry
    /// could be placed there.
    pub class_present: bool,
    pub kind: Kind,
    /// For an interface entry: implementations that were checked, and those that could not be.
    pub implementations_checked: Vec<String>,
    pub implementations_unresolved: Vec<String>,
}

#[derive(Serialize)]
pub struct Report {
    pub old: ImageInfo,
    pub new: ImageInfo,
    pub common_classes: usize,
    pub only_old: Vec<String>,
    pub only_new: Vec<String>,
    /// Names owned by several primary tables in either build; their verdicts are never certain.
    pub ambiguous: Vec<String>,
    pub same_size: usize,
    pub size_changed: usize,
    pub identical_slots: usize,
    pub compared_slots: usize,
    /// Classes with only likely (not proven) pairs and nothing else to report; a soft signal.
    pub classes_with_likely_pairs_only: usize,
    /// Classes whose table changed, lost slots, or holds guessed pairs; largest size change first.
    pub changed: Vec<ClassReport>,
    pub gamedata: Vec<GamedataResult>,
}

struct SlotMap {
    map: Vec<Option<usize>>,
    confidence: Vec<Confidence>,
    n_old: usize,
    n_new: usize,
    /// Every slot is a null or an imported stub: an abstract interface's own table.
    bodiless: bool,
    /// Which old slots have a body of their own (the destructor MSVC emits for an abstract
    /// class, the one method an abstract class implements): such a slot is placed directly even
    /// when the table as a whole is bodiless.
    bodies: Vec<bool>,
}

/// An interface's own table: pure stubs (nulls, `__cxa_pure_virtual`, `_purecall`) and at most
/// one real body, the destructor MSVC still emits for an abstract class. Such a table says
/// nothing about where its methods are; the implementations do.
fn is_bodiless(fps: &[Fp]) -> bool {
    let bodies = fps.iter().filter(|f| f.instructions > 0).count();
    bodies <= 1 && bodies < fps.len()
}

pub fn compare(
    old: &Image,
    new: &Image,
    gamedata_files: &[impl AsRef<Path>],
    class_map: &ClassMap,
    os: &str,
) -> Result<Report> {
    if os != "linux" && os != "windows" {
        bail!("--os {os}: use linux or windows");
    }
    for img in [old, new] {
        if img.format.os() != os {
            bail!(
                "{}: a {:?} image carries {} indices, not {os}",
                img.path,
                img.format,
                img.format.os()
            );
        }
    }
    let ro = rtti::analyze(old);
    let rn = rtti::analyze(new);
    let mut fo = Fingerprinter::new(old, &ro.func_starts);
    let mut fnew = Fingerprinter::new(new, &rn.func_starts);

    let mut maps: BTreeMap<String, SlotMap> = BTreeMap::new();
    let mut changed = Vec::new();
    let (mut same_size, mut size_changed, mut identical, mut compared) =
        (0usize, 0usize, 0usize, 0usize);
    let mut likely_only = 0usize;
    let mut only_old = Vec::new();
    for (class, &io) in &ro.primary {
        let Some(&inew) = rn.primary.get(class) else {
            only_old.push(class.clone());
            continue;
        };
        let a = &ro.vtables[io].slots;
        let b = &rn.vtables[inew].slots;
        let fa: Vec<Fp> = a.iter().map(|t| fo.of(t)).collect();
        let fb: Vec<Fp> = b.iter().map(|t| fnew.of(t)).collect();
        compared += a.len();
        identical += fa
            .iter()
            .zip(&fb)
            .filter(|(x, y)| x.exact == y.exact)
            .count();
        if a.len() == b.len() {
            same_size += 1;
        } else {
            size_changed += 1;
        }
        let al = diff::align(&fa, &fb);
        let shifts: Vec<ShiftRange> = diff::shift_ranges(&al.map)
            .into_iter()
            .filter(|r| r.delta != 0)
            .collect();
        let bodiless = is_bodiless(&fa);
        let hard = a.len() != b.len()
            || !shifts.is_empty()
            || !al.removed.is_empty()
            || (al.guessed_pairs > 0 && !bodiless);
        if hard {
            changed.push(ClassReport {
                class: class.clone(),
                old_slots: a.len(),
                new_slots: b.len(),
                shifts,
                unmatched_old: al.removed.clone(),
                inserted_new: al.inserted.clone(),
                changed_in_place: al.changed_in_place,
                likely_pairs: al.likely_pairs,
                guessed_pairs: al.guessed_pairs,
                uncertain_regions: al.gaps.clone(),
            });
        } else if al.likely_pairs > 0 && !bodiless {
            likely_only += 1;
        }
        maps.insert(
            class.clone(),
            SlotMap {
                map: al.map,
                confidence: al.confidence,
                n_old: a.len(),
                n_new: b.len(),
                bodiless,
                bodies: fa.iter().map(|f| f.instructions > 0).collect(),
            },
        );
    }
    let only_new: Vec<String> = rn
        .primary
        .keys()
        .filter(|c| !ro.primary.contains_key(*c))
        .cloned()
        .collect();
    let ambiguous: Vec<String> = ro.ambiguous.union(&rn.ambiguous).cloned().collect();
    changed.sort_by_key(|c| std::cmp::Reverse((c.new_slots as i64 - c.old_slots as i64).abs()));

    let mut results = Vec::new();
    let mut base_cache: HashMap<(String, String), Option<BaseAlign>> = HashMap::new();
    for file in gamedata_files {
        let file = file.as_ref();
        for e in gamedata::load(file)? {
            let idx = if os == "windows" { e.windows } else { e.linux };
            let known = |c: &str| maps.contains_key(c) || ro.class_ti.contains_key(c);
            let class = class_map
                .get(&e.name)
                .filter(|c| known(c))
                .cloned()
                .or_else(|| gamedata::class_for(&e.name, known));
            let (mut checked, mut unresolved_list) = (Vec::new(), Vec::new());
            let (new_index, confidence, verdict, via) = match (&class, idx) {
                (None, _) if class_map.get(&e.name).is_some() => (
                    None,
                    None,
                    format!(
                        "class {} from the class map is not in the binary",
                        class_map.get(&e.name).unwrap()
                    ),
                    None,
                ),
                (None, _) if gamedata::is_interface_name(&e.name) => (
                    None,
                    None,
                    "interface entry: no class of that name in the binary".to_string(),
                    None,
                ),
                (None, _) | (_, None) => (
                    None,
                    None,
                    "not a vtable entry we can place".to_string(),
                    None,
                ),
                (Some(c), Some(_)) if ambiguous.iter().any(|a| a == c) => (
                    None,
                    None,
                    format!("ambiguous: several classes demangle to {c}, no certain index"),
                    None,
                ),
                (Some(c), Some(i))
                    if !maps.contains_key(c)
                        || (maps[c].bodiless
                            && !maps[c].bodies.get(i as usize).copied().unwrap_or(false)) =>
                {
                    // an interface: its own table has no bodies (or no table at all) and this slot
                    // is a stub, so ask the classes that implement it, each through the table
                    // serving that base; a slot with a body of its own is placed directly below
                    {
                        let p = resolve_via_implementers(
                            &ro,
                            &rn,
                            &mut fo,
                            &mut fnew,
                            &mut base_cache,
                            c,
                            i,
                        );
                        checked = p.checked;
                        unresolved_list = p.unresolved;
                        (p.new_index, p.confidence, p.verdict, p.via)
                    }
                }
                (Some(c), Some(i)) => {
                    let m = &maps[c];
                    if i < 0 || i as usize >= m.n_old {
                        (None, None, format!("index beyond the old table ({} slots): stale, tied to the wrong class, or a field offset and not a vtable slot", m.n_old), None)
                    } else {
                        match m.map[i as usize] {
                            None => (None, None, format!("UNMATCHED: no counterpart found, check by hand ({} -> {} slots)", m.n_old, m.n_new), None),
                            Some(j) => {
                                let conf = m.confidence[i as usize];
                                (Some(j as i64), Some(conf), verdict_text(conf, i, j, m.n_old, m.n_new), None)
                            }
                        }
                    }
                }
            };
            let class_present = class
                .as_ref()
                .is_some_and(|c| ro.class_ti.contains_key(c) || rn.class_ti.contains_key(c));
            // the reason, from the same facts the arms above went by
            let kind = match (&class, idx) {
                _ if new_index.is_some() => Kind::Placed,
                (None, _)
                    if class_map.get(&e.name).is_some() || gamedata::is_interface_name(&e.name) =>
                {
                    Kind::NothingHere
                }
                (None, _) | (_, None) => Kind::NoClass,
                (Some(c), Some(_)) if ambiguous.iter().any(|a| a == c) => Kind::Unresolved,
                (Some(c), Some(i)) => {
                    let through_implementers = !maps.contains_key(c)
                        || (maps[c].bodiless
                            && !maps[c].bodies.get(i as usize).copied().unwrap_or(false));
                    if through_implementers {
                        // no vote at all: nothing here implements it
                        if checked.is_empty() && unresolved_list.is_empty() {
                            Kind::NothingHere
                        } else {
                            Kind::Unresolved
                        }
                    } else if i < 0 || i as usize >= maps[c].n_old {
                        Kind::BeyondTable
                    } else {
                        Kind::Unmatched
                    }
                }
            };
            results.push(GamedataResult {
                file: file.display().to_string(),
                entry: e.name,
                class_present,
                kind,
                class,
                old_index: idx,
                new_index,
                confidence,
                verdict,
                via,
                implementations_checked: checked,
                implementations_unresolved: unresolved_list,
            });
        }
    }

    Ok(Report {
        old: ImageInfo {
            path: old.path.clone(),
            version: old.version.clone(),
            classes: ro.primary.len(),
            slots: ro
                .primary
                .values()
                .map(|&i| ro.vtables[i].slots.len())
                .sum(),
        },
        new: ImageInfo {
            path: new.path.clone(),
            version: new.version.clone(),
            classes: rn.primary.len(),
            slots: rn
                .primary
                .values()
                .map(|&i| rn.vtables[i].slots.len())
                .sum(),
        },
        common_classes: maps.len(),
        only_old,
        only_new,
        ambiguous,
        same_size,
        size_changed,
        identical_slots: identical,
        compared_slots: compared,
        classes_with_likely_pairs_only: likely_only,
        changed,
        gamedata: results,
    })
}

// ------------------------------------------------------------------------------------------
// Validation against what maintainers actually committed after an update.

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Predicted index equals the maintainers' new index.
    Correct,
    /// Predicted index differs from the maintainers' new index.
    Wrong,
    /// The tool declined to place the entry (field offset, interface, ambiguous, unmatched).
    Unplaced,
}

#[derive(Serialize)]
pub struct ValidationRow {
    pub entry: String,
    pub class: Option<String>,
    pub old_index: i64,
    /// What the maintainers wrote after the update.
    pub actual_new: i64,
    pub predicted_new: Option<i64>,
    pub confidence: Option<Confidence>,
    pub outcome: Outcome,
    pub verdict: String,
}

#[derive(Serialize, Default, Clone, Copy)]
pub struct Tally {
    pub correct: usize,
    pub wrong: usize,
}

#[derive(Serialize)]
pub struct Validation {
    pub rows: Vec<ValidationRow>,
    pub strong: Tally,
    pub likely: Tally,
    pub guess: Tally,
    pub unplaced: usize,
    /// Entries the maintainers changed that the tool did not place: silent misses.
    pub unplaced_but_changed: usize,
    /// Entries whose index the maintainers changed in this update.
    pub changed_by_maintainers: usize,
}

/// Compare the tool's prediction (from the OLD gamedata and the two binaries) with the NEW
/// gamedata the maintainers committed. Only entries present in both files with an index for the
/// binaries' OS are judged.
pub fn validate(
    old: &Image,
    new: &Image,
    before: &Path,
    after: &Path,
    class_map: &ClassMap,
) -> Result<Validation> {
    let os = old.format.os();
    let rep = compare(old, new, &[before], class_map, os)?;
    let after_entries = gamedata::load(after)?;
    let mut rows = Vec::new();
    let (mut strong, mut likely, mut guess) =
        (Tally::default(), Tally::default(), Tally::default());
    let (mut unplaced, mut unplaced_but_changed, mut changed) = (0usize, 0usize, 0usize);
    for g in &rep.gamedata {
        let Some(old_index) = g.old_index else {
            continue;
        };
        let Some(actual_new) = after_entries
            .iter()
            .find(|e| e.name == g.entry)
            .and_then(|e| if os == "windows" { e.windows } else { e.linux })
        else {
            continue;
        };
        if actual_new != old_index {
            changed += 1;
        }
        let outcome = match g.new_index {
            None => Outcome::Unplaced,
            Some(p) if p == actual_new => Outcome::Correct,
            Some(_) => Outcome::Wrong,
        };
        match (outcome, g.confidence) {
            (Outcome::Unplaced, _) => {
                unplaced += 1;
                if actual_new != old_index {
                    unplaced_but_changed += 1;
                }
            }
            (o, Some(Confidence::Strong)) => bump(&mut strong, o),
            (o, Some(Confidence::LikelyByShape | Confidence::LikelyByPosition)) => {
                bump(&mut likely, o)
            }
            (o, Some(Confidence::Guess)) | (o, None) => bump(&mut guess, o),
        }
        rows.push(ValidationRow {
            entry: g.entry.clone(),
            class: g.class.clone(),
            old_index,
            actual_new,
            predicted_new: g.new_index,
            confidence: g.confidence,
            outcome,
            verdict: g.verdict.clone(),
        });
    }
    Ok(Validation {
        rows,
        strong,
        likely,
        guess,
        unplaced,
        unplaced_but_changed,
        changed_by_maintainers: changed,
    })
}

fn bump(t: &mut Tally, o: Outcome) {
    match o {
        Outcome::Correct => t.correct += 1,
        Outcome::Wrong => t.wrong += 1,
        Outcome::Unplaced => {}
    }
}

// ------------------------------------------------------------------------------------------
// Verdict wording and interface resolution through implementations.

fn verdict_text(conf: Confidence, i: i64, j: usize, n_old: usize, n_new: usize) -> String {
    let sizes = format!("({n_old} -> {n_new} slots)");
    match (conf, j as i64 == i) {
        (Confidence::Strong, true) => format!("unchanged: strong fingerprint match {sizes}"),
        (Confidence::Strong, false) => {
            format!("SHIFT {i} -> {j}: strong fingerprint match {sizes}")
        }
        (Confidence::LikelyByShape, _) => {
            format!("LIKELY {i} -> {j}: matched by code shape only, the body changed {sizes}")
        }
        (Confidence::LikelyByPosition, _) => {
            format!("LIKELY {i} -> {j}: same-shaped twins placed by their neighbours, call targets not compared {sizes}")
        }
        (Confidence::Guess, _) => format!(
            "GUESS {i} -> {j}: similarity guess inside a changed region, check by hand {sizes}"
        ),
    }
}

/// Alignment of the table that serves one base inside one class, in both builds.
pub(crate) struct BaseAlign {
    pub(crate) map: Vec<Option<usize>>,
    pub(crate) confidence: Vec<Confidence>,
    pub(crate) n_old: usize,
    pub(crate) n_new: usize,
    /// Every slot is a null or an imported stub: an abstract class in the chain.
    pub(crate) bodiless: bool,
}

pub(crate) fn align_base_tables<'a>(
    ro: &rtti::Rtti,
    rn: &rtti::Rtti,
    fo: &mut Fingerprinter,
    fnew: &mut Fingerprinter,
    cache: &'a mut HashMap<(String, String), Option<BaseAlign>>,
    class: &str,
    base: &str,
) -> Option<&'a BaseAlign> {
    let key = (class.to_string(), base.to_string());
    if !cache.contains_key(&key) {
        let computed = match (
            ro.table_for_base(class, base),
            rn.table_for_base(class, base),
        ) {
            (Ok(a), Ok(b)) => {
                let fa: Vec<Fp> = a.slots.iter().map(|t| fo.of(t)).collect();
                let fb: Vec<Fp> = b.slots.iter().map(|t| fnew.of(t)).collect();
                let bodiless = is_bodiless(&fa);
                let al = diff::align(&fa, &fb);
                Some(BaseAlign {
                    map: al.map,
                    confidence: al.confidence,
                    n_old: a.slots.len(),
                    n_new: b.slots.len(),
                    bodiless,
                })
            }
            _ => None,
        };
        cache.insert(key.clone(), computed);
    }
    cache.get(&key).and_then(|c| c.as_ref())
}

/// One implementation's answer about an interface slot.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum VoteKind {
    Resolved {
        new_index: Option<usize>,
        confidence: Confidence,
        n_old: usize,
        n_new: usize,
    },
    /// The table holds no bodies (an abstract class in the chain): nothing to compare, does not
    /// block the verdict.
    Abstract,
    /// Could not be checked; the reason says why. Any of these blocks the verdict.
    Unresolved(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Vote {
    pub class: String,
    pub kind: VoteKind,
}

pub(crate) struct Placement {
    pub new_index: Option<i64>,
    pub confidence: Option<Confidence>,
    pub verdict: String,
    pub via: Option<String>,
    pub checked: Vec<String>,
    pub unresolved: Vec<String>,
}

/// Implementations above this many are not voted on: the entry stays unplaced with that reason.
const MAX_IMPLEMENTATIONS: usize = 64;

/// Place an interface's slot through the classes implementing it: every class that has the
/// interface in its ancestry is listed, each is checked through the table serving that base,
/// and an index is given only when every one of them could be checked and all agree.
fn resolve_via_implementers(
    ro: &rtti::Rtti,
    rn: &rtti::Rtti,
    fo: &mut Fingerprinter,
    fnew: &mut Fingerprinter,
    cache: &mut HashMap<(String, String), Option<BaseAlign>>,
    iface: &str,
    i: i64,
) -> Placement {
    let mut names: BTreeSet<String> = ro.candidates(iface).into_iter().map(|(c, _)| c).collect();
    names.extend(rn.candidates(iface).into_iter().map(|(c, _)| c));
    let votes: Vec<Vote> = names
        .into_iter()
        .map(|c| {
            let kind = match (
                ro.subobject_offset(&c, iface),
                rn.subobject_offset(&c, iface),
            ) {
                (rtti::BaseLookup::NotABase, _) | (_, rtti::BaseLookup::NotABase) => {
                    VoteKind::Unresolved("implements the interface in one build only".into())
                }
                (rtti::BaseLookup::Damaged, _) | (_, rtti::BaseLookup::Damaged) => {
                    VoteKind::Unresolved("its hierarchy record is damaged".into())
                }
                (rtti::BaseLookup::Virtual, _) | (_, rtti::BaseLookup::Virtual) => {
                    VoteKind::Unresolved("reaches the interface through a virtual base".into())
                }
                _ => match align_base_tables(ro, rn, fo, fnew, cache, &c, iface) {
                    None => VoteKind::Unresolved("no table serving the base in both builds".into()),
                    Some(a) if a.bodiless => VoteKind::Abstract,
                    Some(a) if i < 0 || i as usize >= a.n_old => VoteKind::Unresolved(format!(
                        "old index {i} beyond its {}-slot table",
                        a.n_old
                    )),
                    Some(a) => VoteKind::Resolved {
                        new_index: a.map[i as usize],
                        confidence: a.confidence[i as usize],
                        n_old: a.n_old,
                        n_new: a.n_new,
                    },
                },
            };
            Vote { class: c, kind }
        })
        .collect();
    consensus(i, votes, MAX_IMPLEMENTATIONS)
}

/// The decision rule, kept free of binaries so it can be tested on its own.
pub(crate) fn consensus(i: i64, votes: Vec<Vote>, limit: usize) -> Placement {
    let unplaced = |verdict: String, checked: Vec<String>, unresolved: Vec<String>| Placement {
        new_index: None,
        confidence: None,
        verdict,
        via: None,
        checked,
        unresolved,
    };
    if votes.is_empty() {
        return unplaced(
            "interface entry: no implementation in either build, not placed".into(),
            vec![],
            vec![],
        );
    }
    let all: Vec<String> = votes.iter().map(|v| v.class.clone()).collect();
    if votes.len() > limit {
        return unplaced(
            format!(
                "interface entry: {} implementations exceed the limit of {limit}, not resolved",
                votes.len()
            ),
            vec![],
            all,
        );
    }
    let unresolved: Vec<(&str, &str)> = votes
        .iter()
        .filter_map(|v| match &v.kind {
            VoteKind::Unresolved(why) => Some((v.class.as_str(), why.as_str())),
            _ => None,
        })
        .collect();
    let resolved: Vec<(&str, Option<usize>, Confidence, usize, usize)> = votes
        .iter()
        .filter_map(|v| match &v.kind {
            VoteKind::Resolved {
                new_index,
                confidence,
                n_old,
                n_new,
            } => Some((v.class.as_str(), *new_index, *confidence, *n_old, *n_new)),
            _ => None,
        })
        .collect();
    let abstract_n = votes
        .iter()
        .filter(|v| v.kind == VoteKind::Abstract)
        .count();
    let checked: Vec<String> = resolved.iter().map(|r| r.0.to_string()).collect();
    if !unresolved.is_empty() {
        let shown: Vec<String> = unresolved
            .iter()
            .take(6)
            .map(|(c, why)| format!("{c}: {why}"))
            .collect();
        let more = if unresolved.len() > 6 {
            format!(", {} more", unresolved.len() - 6)
        } else {
            String::new()
        };
        return unplaced(
            format!(
                "interface entry: {} implementation(s), {} checked, {} not checkable ({}{more}); no index without a full check",
                votes.len(),
                resolved.len(),
                unresolved.len(),
                shown.join("; ")
            ),
            checked,
            unresolved.iter().map(|(c, _)| c.to_string()).collect(),
        );
    }
    if resolved.is_empty() {
        return unplaced(
            format!(
                "interface entry: only abstract implementations ({abstract_n}), nothing to compare"
            ),
            vec![],
            vec![],
        );
    }
    let answers: BTreeSet<Option<usize>> = resolved.iter().map(|r| r.1).collect();
    if answers.len() > 1 {
        let detail: Vec<String> = resolved
            .iter()
            .map(|(c, j, _, _, _)| {
                format!(
                    "{c}: {}",
                    j.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
                )
            })
            .collect();
        return unplaced(
            format!(
                "ambiguous: implementations disagree ({})",
                detail.join(", ")
            ),
            checked,
            vec![],
        );
    }
    let (first, j, _, n_old, n_new) = resolved[0];
    let weakest = resolved.iter().map(|r| r.2).max().unwrap();
    let suffix = format!(
        " via {first} ({} implementation(s) checked, all agree)",
        resolved.len()
    );
    match j {
        None => Placement {
            new_index: None,
            confidence: None,
            verdict: format!("UNMATCHED: no counterpart found in the implementations{suffix}"),
            via: Some(first.to_string()),
            checked,
            unresolved: vec![],
        },
        Some(j) => Placement {
            new_index: Some(j as i64),
            confidence: Some(weakest),
            verdict: verdict_text(weakest, i, j, n_old, n_new) + &suffix,
            via: Some(first.to_string()),
            checked,
            unresolved: vec![],
        },
    }
}

#[cfg(test)]
mod consensus_tests {
    use super::*;

    fn ok(class: &str, j: usize, conf: Confidence) -> Vote {
        Vote {
            class: class.into(),
            kind: VoteKind::Resolved {
                new_index: Some(j),
                confidence: conf,
                n_old: 20,
                n_new: 21,
            },
        }
    }

    #[test]
    fn every_implementation_checked_and_agreeing_gives_the_index_with_the_weakest_confidence() {
        let mut votes: Vec<Vote> = (0..17)
            .map(|k| ok(&format!("C{k:02}"), 14, Confidence::Strong))
            .collect();
        votes[5].kind = VoteKind::Resolved {
            new_index: Some(14),
            confidence: Confidence::LikelyByShape,
            n_old: 20,
            n_new: 21,
        };
        let p = consensus(13, votes, 64);
        assert_eq!(p.new_index, Some(14));
        assert_eq!(
            p.confidence,
            Some(Confidence::LikelyByShape),
            "the weakest voice sets the level"
        );
        assert_eq!(p.checked.len(), 17);
        assert!(
            p.verdict
                .contains("17 implementation(s) checked, all agree"),
            "{}",
            p.verdict
        );
    }

    #[test]
    fn the_seventeenth_implementation_can_still_veto() {
        let mut votes: Vec<Vote> = (0..16)
            .map(|k| ok(&format!("C{k:02}"), 14, Confidence::Strong))
            .collect();
        votes.push(ok("C16", 15, Confidence::Strong));
        let p = consensus(13, votes, 64);
        assert_eq!(p.new_index, None);
        assert!(
            p.verdict.starts_with("ambiguous: implementations disagree"),
            "{}",
            p.verdict
        );
        assert!(p.verdict.contains("C16: 15"), "{}", p.verdict);
    }

    #[test]
    fn one_uncheckable_implementation_blocks_the_verdict() {
        let mut votes: Vec<Vote> = (0..3)
            .map(|k| ok(&format!("C{k}"), 14, Confidence::Strong))
            .collect();
        votes.push(Vote {
            class: "CVirtual".into(),
            kind: VoteKind::Unresolved("reaches the interface through a virtual base".into()),
        });
        let p = consensus(13, votes, 64);
        assert_eq!(p.new_index, None);
        assert_eq!(p.checked, vec!["C0", "C1", "C2"]);
        assert_eq!(p.unresolved, vec!["CVirtual"]);
        assert!(p.verdict.contains("3 checked, 1 not checkable (CVirtual: reaches the interface through a virtual base)"), "{}", p.verdict);
    }

    #[test]
    fn more_implementations_than_the_limit_means_no_index() {
        let votes: Vec<Vote> = (0..5)
            .map(|k| ok(&format!("C{k}"), 14, Confidence::Strong))
            .collect();
        let p = consensus(13, votes, 4);
        assert_eq!(p.new_index, None);
        assert_eq!(p.unresolved.len(), 5);
        assert!(p.verdict.contains("exceed the limit of 4"), "{}", p.verdict);
    }

    #[test]
    fn abstract_links_in_the_chain_do_not_block_and_do_not_vote() {
        let votes = vec![
            ok("CImpl", 14, Confidence::Strong),
            Vote {
                class: "IMiddle".into(),
                kind: VoteKind::Abstract,
            },
        ];
        let p = consensus(13, votes, 64);
        assert_eq!(p.new_index, Some(14));
        assert_eq!(p.checked, vec!["CImpl"]);
        let only = consensus(
            13,
            vec![Vote {
                class: "IMiddle".into(),
                kind: VoteKind::Abstract,
            }],
            64,
        );
        assert_eq!(only.new_index, None);
        assert!(
            only.verdict.contains("only abstract implementations"),
            "{}",
            only.verdict
        );
    }

    #[test]
    fn nothing_to_vote_on() {
        let p = consensus(13, vec![], 64);
        assert!(
            p.verdict.contains("no implementation in either build"),
            "{}",
            p.verdict
        );
    }
}

// ------------------------------------------------------------------------------------------
// Applying the verdicts to a gamedata file.

#[derive(Serialize)]
pub struct ApplyPlan {
    /// Entries whose linux index will be written: (entry, old, new, confidence).
    pub applied: Vec<(String, i64, i64, Confidence)>,
    /// Entries that moved (or could not be placed) but need a human: (entry, verdict).
    pub needs_human: Vec<(String, String)>,
    /// Entries found unchanged at the chosen level.
    pub unchanged: usize,
    /// Entries whose best pairing says "same index" but with weaker evidence than the chosen
    /// level: nothing to write, nothing known to be wrong: (entry, verdict).
    pub unconfirmed: Vec<(String, String)>,
    /// The names behind `unchanged`.
    pub unchanged_entries: Vec<String>,
    /// Entries nothing could be said about: the name could not be tied to a class, or the
    /// index is beyond its class's table: (entry, reason). Not a verdict; a strict run counts
    /// them.
    pub not_checked: Vec<(String, String)>,
    /// Entries a person declared not to be virtual-table indices (`Entry -` in the class map):
    /// (entry, verdict). Nothing else puts an entry here.
    pub out_of_scope: Vec<(String, String)>,
}

/// Which verdicts may be written: `Strong` only, or `Strong` and the two `Likely` levels.
/// `Guess` is never applied.
pub fn plan_apply(report: &Report, max_level: Confidence) -> ApplyPlan {
    plan_rows(&report.gamedata, max_level)
}

/// One verdict per entry out of the verdicts of several libraries (rows of one platform, in
/// file order per library). A class lives in one library, an interface may be implemented in
/// several: the entry is placed when every library that places it says the same index, with the
/// confidence of the surest of them; two different answers are left to a person.
pub fn merge_libraries(rows: &[GamedataResult]) -> Vec<GamedataResult> {
    let mut order: Vec<&str> = Vec::new();
    for r in rows {
        if !order.contains(&r.entry.as_str()) {
            order.push(&r.entry);
        }
    }
    let mut out = Vec::new();
    for entry in order {
        let group: Vec<&GamedataResult> = rows.iter().filter(|r| r.entry == entry).collect();
        let mut placed: Vec<&GamedataResult> = group
            .iter()
            .copied()
            .filter(|r| r.new_index.is_some() && r.confidence.is_some())
            .collect();
        placed.sort_by_key(|r| r.confidence);
        let Some(best) = placed.first() else {
            // nobody placed it: the row that got furthest says why
            let told = group
                .iter()
                .copied()
                .find(|r| !matches!(r.kind, Kind::NothingHere | Kind::NoClass))
                .unwrap_or(group[0]);
            out.push(told.clone());
            continue;
        };
        let mut row = (*best).clone();
        let mut answers: Vec<i64> = placed.iter().filter_map(|r| r.new_index).collect();
        answers.sort_unstable();
        answers.dedup();
        // a library that knows the class, had something to check and could not place the entry
        // is not a silent abstention: its doubt stands against the others' answer. A library
        // where nothing implements the interface has nothing to say.
        let doubter = group.iter().copied().find(|r| {
            r.class_present
                && (r.new_index.is_none() || r.confidence.is_none())
                // an index beyond a table cannot be about that table: a class of the same name
                // in another library (the client's twin of a server class) is not a doubt
                && !matches!(
                    r.kind,
                    Kind::NothingHere | Kind::OutOfScope | Kind::BeyondTable
                )
        });
        if let (1, Some(d)) = (answers.len(), doubter) {
            row.new_index = None;
            row.confidence = None;
            row.verdict = format!(
                "one library places it at {}, another has the class and could not: {}",
                answers[0], d.verdict
            );
        } else if answers.len() > 1 {
            row.new_index = None;
            row.confidence = None;
            row.verdict = format!(
                "the libraries disagree ({}), check by hand",
                answers
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
        }
        out.push(row);
    }
    out
}

pub fn plan_rows(rows: &[GamedataResult], max_level: Confidence) -> ApplyPlan {
    let max_level = max_level.min(Confidence::LikelyByPosition);
    let mut plan = ApplyPlan {
        applied: Vec::new(),
        needs_human: Vec::new(),
        unchanged: 0,
        unconfirmed: Vec::new(),
        unchanged_entries: Vec::new(),
        not_checked: Vec::new(),
        out_of_scope: Vec::new(),
    };
    for g in rows {
        let Some(old) = g.old_index else { continue };
        match (g.new_index, g.confidence) {
            (Some(new), Some(conf)) if conf <= max_level => {
                if new == old {
                    plan.unchanged += 1;
                    plan.unchanged_entries.push(g.entry.clone());
                } else {
                    plan.applied.push((g.entry.clone(), old, new, conf));
                }
            }
            (Some(new), Some(_)) if new == old => {
                plan.unconfirmed.push((g.entry.clone(), g.verdict.clone()))
            }
            (_, _) if g.kind == Kind::OutOfScope => {
                plan.out_of_scope.push((g.entry.clone(), g.verdict.clone()))
            }
            (_, _) if g.kind == Kind::NoClass => plan.not_checked.push((
                g.entry.clone(),
                "not tied to a class with a virtual table: name its class in a class map, or mark it `-` there if it is not a virtual-table index".to_string(),
            )),
            (_, _) if g.kind == Kind::BeyondTable => plan.not_checked.push((
                g.entry.clone(),
                format!(
                    "{}; if it is a field offset, mark it `-` in a class map",
                    g.verdict
                ),
            )),
            _ => plan.needs_human.push((g.entry.clone(), g.verdict.clone())),
        }
    }
    plan
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn row(entry: &str, new: Option<i64>, conf: Option<Confidence>) -> GamedataResult {
        GamedataResult {
            file: "g.json".into(),
            entry: entry.into(),
            class: Some("C".into()),
            class_present: new.is_some(),
            kind: if new.is_some() {
                Kind::Placed
            } else {
                Kind::NothingHere
            },
            old_index: Some(3),
            new_index: new,
            confidence: conf,
            verdict: "v".into(),
            via: None,
            implementations_checked: Vec::new(),
            implementations_unresolved: Vec::new(),
        }
    }

    #[test]
    fn the_library_that_has_the_class_answers() {
        let rows = vec![
            row("A", None, None),
            row("B", None, None),
            row("A", Some(4), Some(Confidence::Strong)),
            row("B", None, None),
        ];
        let m = merge_libraries(&rows);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].new_index, Some(4));
        assert_eq!(m[1].new_index, None);
        let plan = plan_rows(&m, Confidence::Strong);
        assert_eq!(plan.applied.len(), 1);
        assert_eq!(plan.needs_human.len(), 1);
    }

    #[test]
    fn two_libraries_with_different_answers_leave_it_to_a_person() {
        let rows = vec![
            row("A", Some(4), Some(Confidence::Strong)),
            row("A", Some(5), Some(Confidence::Strong)),
            row("B", Some(7), Some(Confidence::LikelyByPosition)),
            row("B", Some(7), Some(Confidence::Strong)),
        ];
        let m = merge_libraries(&rows);
        assert_eq!(m[0].new_index, None);
        assert!(m[0].verdict.contains("4 / 5"), "{}", m[0].verdict);
        assert_eq!(m[1].new_index, Some(7));
        assert_eq!(m[1].confidence, Some(Confidence::Strong));
    }

    #[test]
    fn a_library_that_has_the_class_and_could_not_place_it_stops_the_others() {
        let mut doubt = row("A", None, None);
        doubt.class_present = true;
        doubt.kind = Kind::Unresolved;
        let m = merge_libraries(&[row("A", Some(4), Some(Confidence::Strong)), doubt]);
        assert_eq!(m[0].new_index, None);
        assert_eq!(plan_rows(&m, Confidence::Strong).needs_human.len(), 1);
    }

    #[test]
    fn a_same_named_class_too_small_for_the_index_is_not_a_doubt() {
        let mut twin = row("A", None, None);
        twin.class_present = true;
        twin.kind = Kind::BeyondTable;
        let m = merge_libraries(&[row("A", Some(4), Some(Confidence::Strong)), twin]);
        assert_eq!(m[0].new_index, Some(4));
    }

    #[test]
    fn only_a_persons_mark_takes_an_entry_out_of_scope() {
        let mut beyond = row("Field", None, None);
        beyond.kind = Kind::BeyondTable;
        // the wording must not matter: this one says "field offset" and is still not checked
        beyond.verdict = "index beyond the old table: a field offset".into();
        let mut marked = row("Marked", None, None);
        marked.kind = Kind::OutOfScope;
        let plan = plan_rows(&[beyond, marked], Confidence::Strong);
        assert_eq!(plan.not_checked.len(), 1);
        assert_eq!(plan.not_checked[0].0, "Field");
        assert_eq!(plan.out_of_scope.len(), 1);
        assert_eq!(plan.out_of_scope[0].0, "Marked");
    }
}
