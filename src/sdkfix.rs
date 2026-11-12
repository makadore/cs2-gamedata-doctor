//! A patch for stale SDK headers. Where the comparison of two builds proves that slots were
//! inserted into or removed from the part of a table a header declares, the header can be made
//! to describe the new table again without knowing what the new functions are: a placeholder
//! virtual goes where a slot appeared, the declaration goes where a slot vanished. Code built
//! against the patched header calls the right slots again; code that used a removed function
//! stops compiling, which is the honest outcome.

use crate::diff::Confidence;
use crate::sdk::Layout;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A slot that no old slot maps to sits in front of header slot `before`.
    Insert { before: usize, new_slot: usize },
    /// Header slot `slot` has no counterpart in the new table.
    Remove { slot: usize },
}

/// What has to happen to the first `n` slots of a table (the ones a header declares) so that
/// they line up with the new table. `map` and `conf` are the alignment of the old table. The
/// flag says the places are certain: every slot next to a change was paired by its own
/// exact fingerprint, not by shape and not by position among look-alikes.
pub fn plan(
    n: usize,
    map: &[Option<usize>],
    conf: &[Confidence],
) -> Result<(Vec<Op>, bool), String> {
    let n = n.min(map.len());
    let mut ops = Vec::new();
    let mut expected = 0usize;
    for (i, m) in map.iter().enumerate().take(n) {
        match m {
            None => ops.push(Op::Remove { slot: i }),
            Some(j) => {
                if *j < expected {
                    return Err(format!(
                        "slot {i} went backwards to {j}: the table was reordered"
                    ));
                }
                for k in expected..*j {
                    ops.push(Op::Insert {
                        before: i,
                        new_slot: k,
                    });
                }
                expected = j + 1;
            }
        }
    }
    // an exact fingerprint only: a matching shape has been wrong before
    let proven = |i: usize| map[i].is_some() && conf[i] == Confidence::Strong;
    let certain = ops.iter().all(|op| match *op {
        Op::Insert { before, .. } => proven(before) && (before == 0 || proven(before - 1)),
        Op::Remove { slot } => {
            (slot == 0 || proven(slot - 1)) && (slot + 1 >= n || proven(slot + 1))
        }
    });
    Ok((ops, certain))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Edit {
    /// Header path relative to the SDK root, `/`-separated.
    pub file: String,
    /// First line of the declaration the edit is anchored to (1-based).
    pub line: usize,
    pub end_line: usize,
    /// `Some(slot)`: put a placeholder for that new slot in front of the declaration; `None`:
    /// take the declaration out.
    pub insert: Option<usize>,
    /// The class and function the declaration belongs to.
    pub class: String,
    pub method: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Fix {
    pub edits: Vec<Edit>,
    /// False: the changes are right in number, but a slot next to one of them was paired by
    /// position only, so the place may be off by a few declarations.
    pub certain: bool,
}

/// The header lines the operations touch.
pub fn edits(layout: &Layout, ops: &[Op]) -> Result<Vec<Edit>, String> {
    let mut out = Vec::new();
    for op in ops {
        let (slot, insert) = match *op {
            Op::Insert { before, new_slot } => (before, Some(new_slot)),
            Op::Remove { slot } => (slot, None),
        };
        let d = layout
            .decl
            .get(slot)
            .ok_or_else(|| format!("slot {slot} is not in the header"))?;
        if d.line == 0 {
            return Err(format!("no source line for slot {slot}"));
        }
        match (insert, d.half) {
            (Some(_), 2) => {
                return Err("a slot appeared between the two destructor slots".to_string())
            }
            (None, 1 | 2) => return Err("a destructor slot vanished".to_string()),
            _ => {}
        }
        out.push(Edit {
            file: d.file.clone(),
            line: d.line,
            end_line: d.end_line,
            insert,
            class: d.class.clone(),
            method: layout.slots[slot].clone(),
        });
    }
    Ok(out)
}

/// Bytes as text and back without loss: SDK headers are not all UTF-8.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

fn unlatin1(text: &str) -> Vec<u8> {
    text.chars().map(|c| c as u32 as u8).collect()
}

pub struct Patch {
    /// A unified diff against the SDK root (`patch -p1`, `git apply`).
    pub diff: Vec<u8>,
    /// The patched files, keyed by relative path, as text for `Headers::load_patched`.
    pub files: BTreeMap<String, String>,
    pub edits: usize,
}

/// Apply the edits to the headers under `root` in memory.
pub fn patch(root: &Path, edits: &[Edit]) -> Result<Patch> {
    let mut by_file: BTreeMap<&str, Vec<&Edit>> = BTreeMap::new();
    let mut seen: Vec<(&str, usize, Option<usize>)> = Vec::new();
    for e in edits {
        // the same base class is reached through many classes and libraries
        let key = (e.file.as_str(), e.line, e.insert);
        if !seen.contains(&key) {
            seen.push(key);
            by_file.entry(&e.file).or_default().push(e);
        }
    }
    let mut out = Patch {
        diff: Vec::new(),
        files: BTreeMap::new(),
        edits: seen.len(),
    };
    for (file, mut list) in by_file {
        let bytes = std::fs::read(root.join(file)).with_context(|| format!("reading {file}"))?;
        let text = latin1(&bytes);
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        // from the bottom up, so the line numbers above stay valid; placeholders of one place
        // in slot order
        list.sort_by_key(|e| (std::cmp::Reverse(e.line), std::cmp::Reverse(e.insert)));
        let mut new: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        for e in list {
            if e.line == 0 || e.end_line < e.line || e.end_line > lines.len() {
                bail!(
                    "{file}:{}: the declaration is not where the reader saw it",
                    e.line
                );
            }
            let first = lines[e.line - 1];
            match e.insert {
                Some(slot) => {
                    let indent: String = first
                        .chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .collect();
                    let eol = if first.ends_with("\r\n") {
                        "\r\n"
                    } else {
                        "\n"
                    };
                    new.insert(
                        e.line - 1,
                        format!("{indent}virtual void gamedata_doctor_unknown_{slot}() {{}}{eol}"),
                    );
                }
                None => {
                    let body: String = lines[e.line - 1..e.end_line].concat();
                    let name = e.method.split(' ').next().unwrap_or(&e.method);
                    let own = body.trim_start().starts_with("virtual")
                        && body.contains(name)
                        && body.matches(';').count() <= 1
                        && body.trim_end().ends_with([';', '}']);
                    if !own {
                        bail!(
                            "{file}:{}: {} shares its lines with something else, take it out by hand",
                            e.line,
                            e.method
                        );
                    }
                    new.drain(e.line - 1..e.end_line);
                }
            }
        }
        let patched: String = new.concat();
        let d = similar::TextDiff::from_lines(text.as_str(), patched.as_str())
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{file}"), &format!("b/{file}"))
            .to_string();
        out.diff.extend(unlatin1(&d));
        out.files.insert(
            file.to_string(),
            String::from_utf8_lossy(&unlatin1(&patched)).into_owned(),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Confidence::{LikelyByPosition, LikelyByShape, Strong};

    #[test]
    fn an_inserted_slot_is_put_in_front_of_the_first_one_that_moved() {
        // a b c d -> a b X c d
        let map = [Some(0), Some(1), Some(3), Some(4)];
        let (ops, certain) = plan(4, &map, &[Strong; 4]).unwrap();
        assert_eq!(
            ops,
            vec![Op::Insert {
                before: 2,
                new_slot: 2
            }]
        );
        assert!(certain);
    }

    #[test]
    fn what_happens_behind_the_declared_part_is_not_the_headers_business() {
        let map = [Some(0), Some(1), Some(3), Some(4)];
        let (ops, _) = plan(2, &map, &[Strong; 4]).unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn a_removed_slot_and_two_inserted_ones() {
        // a b c d -> a X Y c d   (b gone)
        let map = [Some(0), None, Some(3), Some(4)];
        let (ops, certain) = plan(4, &map, &[Strong; 4]).unwrap();
        assert_eq!(
            ops,
            vec![
                Op::Remove { slot: 1 },
                Op::Insert {
                    before: 2,
                    new_slot: 1
                },
                Op::Insert {
                    before: 2,
                    new_slot: 2
                },
            ]
        );
        // the slot before the insertion has no counterpart: the place is not proven
        assert!(!certain);
    }

    #[test]
    fn look_alikes_next_to_the_change_make_the_place_uncertain() {
        let map = [Some(0), Some(1), Some(3), Some(4)];
        let conf = [Strong, LikelyByPosition, Strong, Strong];
        assert!(!plan(4, &map, &conf).unwrap().1);
        let conf = [LikelyByPosition, Strong, Strong, Strong];
        assert!(plan(4, &map, &conf).unwrap().1);
        // a neighbour matched by shape only is not proof either
        let conf = [Strong, Strong, LikelyByShape, Strong];
        assert!(!plan(4, &map, &conf).unwrap().1);
    }

    #[test]
    fn a_reordered_table_is_refused() {
        assert!(plan(3, &[Some(1), Some(0), Some(2)], &[Strong; 3]).is_err());
    }
}
