//! SDK headers against two builds of a library: which classes the headers describe no longer
//! have the table the headers promise. A plugin compiled against such a header calls the wrong
//! slot, and the only cure is a fixed header and a rebuild.

use crate::diff::Confidence;
use crate::fp::Fingerprinter;
use crate::image::Image;
use crate::report::{align_base_tables, BaseAlign};
use crate::rtti::{self, BaseLookup};
use crate::sdk::Headers;
use serde::Serialize;
use std::collections::HashMap;

/// Worst last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Verdict {
    Ok,
    /// Nothing can be said. The header declares more slots than the table had in the OLD
    /// build (a header misread, a table cut short, a header stale before), or it has the size
    /// of the NEW table (written for the new build: its order cannot be checked from the old).
    Unchecked,
    /// Slots moved, but only pairs matched by position say so.
    Suspect,
    /// The part of the table the header declares changed: slots moved, vanished or were
    /// inserted, or the header declares more slots than the new table has.
    Broken,
    /// The class is not in the new build at all.
    Gone,
}

#[derive(Clone, Debug, Serialize)]
pub struct Moved {
    pub method: String,
    pub old: usize,
    /// `None`: no counterpart in the new table.
    pub new: Option<usize>,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub class: String,
    /// `file:line` of the declaration.
    pub header: String,
    pub header_slots: usize,
    /// The header's names line up with the slots (see `sdk::Layout::exact`).
    pub header_exact: bool,
    /// The class whose table was compared: the class itself or one that implements it.
    pub implementation: Option<String>,
    pub old_slots: Option<usize>,
    pub new_slots: Option<usize>,
    pub verdict: Verdict,
    pub detail: String,
    pub moved: Vec<Moved>,
    /// For a broken table: the header edits that make it describe the new table again.
    pub fix: Option<crate::sdkfix::Fix>,
    /// Why there is no fix, when one was looked for.
    pub no_fix: String,
}

pub struct Check {
    /// Everything but `Ok`, worst first (`Unchecked` rows included).
    pub rows: Vec<Row>,
    /// Header classes found in the old build.
    pub present: usize,
    pub ok: usize,
}

/// Implementations compared per class: a base of hundreds of entity classes says the same thing
/// in each of them.
const MAX_IMPLEMENTATIONS: usize = 6;

pub fn check(old: &Image, new: &Image, headers: &Headers) -> Check {
    let os = old.format.os();
    let ro = rtti::analyze(old);
    let rn = rtti::analyze(new);
    let mut fo = Fingerprinter::new(old, &ro.func_starts);
    let mut fnew = Fingerprinter::new(new, &rn.func_starts);
    let mut cache: HashMap<(String, String), Option<BaseAlign>> = HashMap::new();
    let (mut rows, mut present, mut ok) = (Vec::new(), 0usize, 0usize);

    for (name, class) in &headers.classes {
        let Some(layout) = headers.layout(name, os) else {
            continue;
        };
        if !ro.class_ti.contains_key(name) {
            continue;
        }
        present += 1;
        let n = layout.slots.len();
        let mut row = Row {
            class: name.clone(),
            header: format!("{}:{}", class.file, class.line),
            header_slots: n,
            header_exact: layout.exact,
            implementation: None,
            old_slots: None,
            new_slots: None,
            verdict: Verdict::Ok,
            detail: String::new(),
            moved: Vec::new(),
            fix: None,
            no_fix: String::new(),
        };
        let old_impls: Vec<String> = ro
            .candidates(name)
            .into_iter()
            .filter(|(_, l)| matches!(l, BaseLookup::NonVirtual(_)))
            .map(|(c, _)| c)
            .collect();
        if !rn.class_ti.contains_key(name) {
            row.verdict = Verdict::Gone;
            row.detail = format!(
                "not in the new build; the header declares {n} virtual function(s){}",
                if old_impls.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", implemented in the old build by {}",
                        old_impls
                            .iter()
                            .take(2)
                            .map(|c| c.chars().take(48).collect::<String>())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
            rows.push(row);
            continue;
        }
        // the class's own table first, then the classes that implement it
        let mut subjects: Vec<String> = Vec::new();
        if ro.primary.contains_key(name) && rn.primary.contains_key(name) {
            subjects.push(name.clone());
        }
        subjects.extend(
            old_impls
                .iter()
                .filter(|c| matches!(rn.subobject_offset(c, name), BaseLookup::NonVirtual(_)))
                .take(MAX_IMPLEMENTATIONS)
                .cloned(),
        );
        let mut compared = false;
        let mut unusable: Option<(String, usize, usize)> = None;
        for subject in subjects {
            let Some(a) =
                align_base_tables(&ro, &rn, &mut fo, &mut fnew, &mut cache, &subject, name)
            else {
                continue;
            };
            compared = true;
            let mut verdict = Verdict::Ok;
            let mut detail = String::new();
            let mut moved = Vec::new();
            let mut fix: Result<Option<crate::sdkfix::Fix>, String> = Ok(None);
            if n > a.n_old && n != a.n_new {
                // the header does not describe this table in the old build: try the next one
                unusable.get_or_insert((subject.clone(), a.n_old, a.n_new));
                continue;
            }
            if a.n_old != a.n_new && n == a.n_new {
                // the header has the size of the new table: it was written for the new build,
                // so the old table says nothing about it, and the binaries carry no names to
                // tell whether its new declarations sit where the new slots are
                row.verdict = Verdict::Unchecked;
                row.detail = format!(
                    "the header declares {n} slots, as many as the new table (the old had {}): written for the new build; where its new declarations sit cannot be checked",
                    a.n_old
                );
                row.moved.clear();
                row.fix = None;
                row.no_fix.clear();
                row.old_slots = Some(a.n_old);
                row.new_slots = Some(a.n_new);
                row.implementation = Some(subject);
                break;
            }
            if n > a.n_new {
                verdict = Verdict::Broken;
                detail = format!(
                    "the header declares {n} slots, the new table has {}",
                    a.n_new
                );
            }
            if a.bodiless {
                // an abstract table has no bodies to pair; its size is the interface's size
                if a.n_old != a.n_new && verdict < Verdict::Broken {
                    verdict = Verdict::Broken;
                    detail = format!("the abstract table went {} -> {}", a.n_old, a.n_new);
                }
            } else {
                for i in 0..n.min(a.n_old) {
                    if a.map[i] != Some(i) {
                        moved.push(Moved {
                            method: layout
                                .slots
                                .get(i)
                                .filter(|_| layout.exact)
                                .cloned()
                                .unwrap_or_else(|| format!("#{i}")),
                            old: i,
                            new: a.map[i],
                            confidence: a.confidence[i],
                        });
                    }
                }
                if !moved.is_empty() {
                    // a fingerprint has to say a declared slot moved; pairs made by position
                    // alone cannot tell an insertion in the middle from one at the end
                    let proven = moved
                        .iter()
                        .filter(|m| m.confidence <= Confidence::LikelyByShape)
                        .count();
                    let sure = proven > 0;
                    let v = if sure {
                        Verdict::Broken
                    } else {
                        Verdict::Suspect
                    };
                    if v > verdict || detail.is_empty() {
                        let shown: Vec<String> = moved
                            .iter()
                            .take(4)
                            .map(|m| match m.new {
                                Some(j) => format!("{} {} -> {j}", m.method, m.old),
                                None => format!("{} {} -> gone", m.method, m.old),
                            })
                            .collect();
                        detail = format!(
                            "{} of the {} declared slot(s) moved ({}): {}{}",
                            moved.len(),
                            n.min(a.n_old),
                            if sure {
                                format!("{proven} proven by fingerprint")
                            } else {
                                "by position only, the new slot may sit at the end".to_string()
                            },
                            shown.join(", "),
                            if moved.len() > 4 { ", ..." } else { "" }
                        );
                    }
                    verdict = verdict.max(v);
                    if sure {
                        fix = if !layout.exact {
                            Err("the order of the header's declarations is not certain".into())
                        } else {
                            crate::sdkfix::plan(n, &a.map, &a.confidence).and_then(
                                |(ops, certain)| {
                                    let edits = crate::sdkfix::edits(&layout, &ops)?;
                                    Ok(Some(crate::sdkfix::Fix { edits, certain }))
                                },
                            )
                        };
                    }
                }
            }
            if row.implementation.is_none() || verdict > row.verdict {
                row.verdict = verdict;
                row.detail = detail;
                row.moved = moved;
                match fix {
                    Ok(f) => {
                        row.fix = f;
                        row.no_fix.clear();
                    }
                    Err(why) => {
                        row.fix = None;
                        row.no_fix = why;
                    }
                }
                row.old_slots = Some(a.n_old);
                row.new_slots = Some(a.n_new);
                row.implementation = Some(subject);
            }
        }
        if row.implementation.is_none() {
            if let Some((subject, a, b)) = unusable {
                row.verdict = Verdict::Unchecked;
                row.detail = format!("the header declares {n} slots, the old table has {a}");
                row.old_slots = Some(a);
                row.new_slots = Some(b);
                row.implementation = Some(subject);
            }
        }
        if !compared && !old_impls.is_empty() {
            let left = rn
                .candidates(name)
                .into_iter()
                .filter(|(_, l)| matches!(l, BaseLookup::NonVirtual(_)))
                .count();
            if left == 0 {
                row.verdict = Verdict::Gone;
                row.detail = format!(
                    "nothing implements it in the new build (the old one had {}: {})",
                    old_impls.len(),
                    old_impls
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if row.verdict == Verdict::Ok {
            ok += 1;
        } else {
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| b.verdict.cmp(&a.verdict).then(a.class.cmp(&b.class)));
    Check { rows, present, ok }
}
