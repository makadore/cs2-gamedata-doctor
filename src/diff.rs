//! Aligning two slot sequences of the same class.
//!
//! Anchors first: slots whose exact fingerprint occurs once on each side are matched with a
//! Myers diff, which pins the stable parts of the table. The gaps between anchors are then
//! re-aligned the same way with the coarse fingerprint (opcodes only), and whatever is still
//! unresolved is paired by position.
//!
//! Every matched pair carries a [`Confidence`]. Only an exact, unique anchor is strong evidence: the
//! fingerprints mask call targets and data references, so two functions of the same shape that
//! call different things hash equal, and a coarse match only says "same opcodes". Callers must
//! never present anything below `Strong` as a fact, and `Strong` itself as a structural match.

use crate::fp::Fp;
use serde::Serialize;
use similar::{capture_diff_slices, Algorithm, DiffOp};
use std::collections::HashMap;
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Confidence {
    /// Exact fingerprint unique on both sides: the strongest structural evidence available,
    /// still not a semantic proof, because call targets and data references are masked.
    Strong,
    /// Coarse fingerprint (opcodes and operand kinds) unique on both sides; the body changed.
    LikelyByShape,
    /// Paired by position between anchors; both bodies have the same exact fingerprint, which
    /// may still hide different call targets.
    LikelyByPosition,
    /// Paired by position between anchors; the bodies differ.
    Guess,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Gap {
    pub old_from: usize,
    pub old_to: usize,
    pub new_from: usize,
    pub new_to: usize,
}

pub struct Alignment {
    /// old slot index -> new slot index, None when the old slot has no counterpart.
    pub map: Vec<Option<usize>>,
    /// How each mapped old slot was matched (meaningless where `map` is None).
    pub confidence: Vec<Confidence>,
    pub inserted: Vec<usize>,
    pub removed: Vec<usize>,
    /// Matched slots whose exact fingerprint differs (the body changed).
    pub changed_in_place: usize,
    /// Pairs matched below `Strong` but above `Guess`.
    pub likely_pairs: usize,
    /// Pairs that are guesses.
    pub guessed_pairs: usize,
    /// Regions paired by position where bodies differ or counts differ.
    pub gaps: Vec<Gap>,
}

pub fn align(old: &[Fp], new: &[Fp]) -> Alignment {
    let mut out = Alignment {
        map: vec![None; old.len()],
        confidence: vec![Confidence::Guess; old.len()],
        inserted: Vec::new(),
        removed: Vec::new(),
        changed_in_place: 0,
        likely_pairs: 0,
        guessed_pairs: 0,
        gaps: Vec::new(),
    };
    align_rec(old, new, 0, 0, 0, &mut out);
    out.inserted.sort_unstable();
    out.removed.sort_unstable();
    out
}

fn tokens(old: &[Fp], new: &[Fp], level: u8) -> (Vec<u64>, Vec<u64>) {
    let key = |f: &Fp| if level == 0 { f.exact } else { f.coarse };
    let mut co: HashMap<u64, u32> = HashMap::new();
    let mut cn: HashMap<u64, u32> = HashMap::new();
    for f in old {
        *co.entry(key(f)).or_default() += 1;
    }
    for f in new {
        *cn.entry(key(f)).or_default() += 1;
    }
    let eligible = |k: u64| co.get(&k) == Some(&1) && cn.get(&k) == Some(&1);
    let salt = |side: u64, i: usize| {
        xxh3_64(
            &[
                side.to_le_bytes(),
                (i as u64).to_le_bytes(),
                (level as u64).to_le_bytes(),
            ]
            .concat(),
        ) | 1 << 63
    };
    let to = old
        .iter()
        .enumerate()
        .map(|(i, f)| if eligible(key(f)) { key(f) } else { salt(1, i) })
        .collect();
    let tn = new
        .iter()
        .enumerate()
        .map(|(i, f)| if eligible(key(f)) { key(f) } else { salt(2, i) })
        .collect();
    (to, tn)
}

fn anchors(to: &[u64], tn: &[u64]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, to, tn) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                pairs.push((old_index + k, new_index + k));
            }
        }
    }
    pairs
}

fn record(out: &mut Alignment, i: usize, j: usize, same_body: bool, conf: Confidence) {
    out.map[i] = Some(j);
    out.confidence[i] = conf;
    if !same_body {
        out.changed_in_place += 1;
    }
    match conf {
        Confidence::Strong => {}
        Confidence::LikelyByShape | Confidence::LikelyByPosition => out.likely_pairs += 1,
        Confidence::Guess => out.guessed_pairs += 1,
    }
}

/// Similarity of two slots nothing stronger could pair: identical bodies 1.0, the same opcode
/// shape 0.9, otherwise how close the instruction counts are (at most 0.6). A null or imported
/// stub never resembles code.
fn similarity(a: &Fp, b: &Fp) -> f32 {
    if a.exact == b.exact {
        return 1.0;
    }
    if a.instructions == 0 || b.instructions == 0 {
        return 0.0;
    }
    if a.coarse == b.coarse {
        return 0.9;
    }
    let (x, y) = (a.instructions as f32, b.instructions as f32);
    0.6 * (1.0 - (x - y).abs() / x.max(y))
}

/// Global alignment (Needleman-Wunsch) of two unresolved regions. Skipping a slot costs a
/// little, so unrelated slots are still paired when counts match; on a tie a skip wins over
/// a pair, which keeps the extra slots of an unequal region at its end.
fn align_similar(old: &[Fp], new: &[Fp]) -> Vec<(usize, usize)> {
    const GAP: f32 = -0.25;
    let (n, m) = (old.len(), new.len());
    let mut score = vec![vec![0f32; m + 1]; n + 1];
    let mut from = vec![vec![0u8; m + 1]; n + 1]; // 0 diagonal, 1 skip old, 2 skip new
    for i in 1..=n {
        score[i][0] = i as f32 * GAP;
        from[i][0] = 1;
    }
    for j in 1..=m {
        score[0][j] = j as f32 * GAP;
        from[0][j] = 2;
    }
    for i in 1..=n {
        for j in 1..=m {
            let d = score[i - 1][j - 1] + similarity(&old[i - 1], &new[j - 1]);
            let u = score[i - 1][j] + GAP;
            let l = score[i][j - 1] + GAP;
            // a tie between pairing and skipping goes to the skip: the extra slots of an
            // unequal region then sit at its end, exactly where positional pairing put them
            let (best, f) = if d > u && d > l {
                (d, 0)
            } else if u >= l {
                (u, 1)
            } else {
                (l, 2)
            };
            score[i][j] = best;
            from[i][j] = f;
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        match from[i][j] {
            0 => {
                pairs.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    pairs.reverse();
    pairs
}

fn align_rec(old: &[Fp], new: &[Fp], ob: usize, nb: usize, level: u8, out: &mut Alignment) {
    if old.is_empty() && new.is_empty() {
        return;
    }
    if old.is_empty() {
        out.inserted.extend(nb..nb + new.len());
        return;
    }
    if new.is_empty() {
        out.removed.extend(ob..ob + old.len());
        return;
    }
    if level >= 2 {
        // Nothing matched by evidence: align the region by body similarity (identical bodies,
        // same opcode shape, then instruction count), ties keeping the positional order, so an
        // inserted small method does not swallow the place of a large one behind it.
        let pairs = align_similar(old, new);
        let (mut mapped_old, mut mapped_new) = (vec![false; old.len()], vec![false; new.len()]);
        let mut guessed = old.len() != new.len();
        for (i, j) in pairs {
            let same = old[i].exact == new[j].exact;
            let conf = if same {
                Confidence::LikelyByPosition
            } else {
                Confidence::Guess
            };
            record(out, ob + i, nb + j, same, conf);
            guessed |= !same;
            mapped_old[i] = true;
            mapped_new[j] = true;
        }
        if guessed {
            out.gaps.push(Gap {
                old_from: ob,
                old_to: ob + old.len(),
                new_from: nb,
                new_to: nb + new.len(),
            });
        }
        out.removed
            .extend((0..old.len()).filter(|&i| !mapped_old[i]).map(|i| ob + i));
        out.inserted
            .extend((0..new.len()).filter(|&j| !mapped_new[j]).map(|j| nb + j));
        return;
    }
    let (to, tn) = tokens(old, new, level);
    let pairs = anchors(&to, &tn);
    if pairs.is_empty() {
        return align_rec(old, new, ob, nb, level + 1, out);
    }
    let conf = if level == 0 {
        Confidence::Strong
    } else {
        Confidence::LikelyByShape
    };
    let (mut po, mut pn) = (0usize, 0usize);
    for (i, j) in pairs {
        align_rec(&old[po..i], &new[pn..j], ob + po, nb + pn, level, out);
        record(out, ob + i, nb + j, old[i].exact == new[j].exact, conf);
        po = i + 1;
        pn = j + 1;
    }
    align_rec(&old[po..], &new[pn..], ob + po, nb + pn, level, out);
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ShiftRange {
    pub from: usize,
    pub to: usize,
    pub delta: i64,
}

/// Consecutive matched old indices with the same delta, as ranges; zero deltas are kept so a
/// caller can see both the stable and the shifted parts.
pub fn shift_ranges(map: &[Option<usize>]) -> Vec<ShiftRange> {
    let mut out: Vec<ShiftRange> = Vec::new();
    for (i, m) in map.iter().enumerate() {
        let Some(j) = m else { continue };
        let delta = *j as i64 - i as i64;
        match out.last_mut() {
            Some(r) if r.delta == delta && r.to + 1 == i => r.to = i,
            _ => out.push(ShiftRange {
                from: i,
                to: i,
                delta,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Confidence::*;
    use super::*;

    /// exact = x, coarse = x / 10: values 20 and 21 share a coarse hash, 20 and 30 do not.
    fn fp(x: u64) -> Fp {
        Fp {
            exact: x,
            coarse: x / 10,
            instructions: 5,
        }
    }
    fn fps(xs: &[u64]) -> Vec<Fp> {
        xs.iter().map(|&x| fp(x)).collect()
    }
    fn mapped(a: &Alignment) -> Vec<Option<usize>> {
        a.map.clone()
    }

    #[test]
    fn insertion_shifts_everything_behind_it() {
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 20, 99, 30, 40]));
        assert_eq!(mapped(&a), vec![Some(0), Some(1), Some(3), Some(4)]);
        assert_eq!(a.inserted, vec![2]);
        assert!(a.removed.is_empty());
        assert!(
            a.confidence.iter().all(|c| *c == Strong),
            "every pair is an exact anchor"
        );
        let r = shift_ranges(&a.map);
        assert_eq!(r.len(), 2);
        assert_eq!((r[1].from, r[1].to, r[1].delta), (2, 3, 1));
    }

    #[test]
    fn removal_shifts_back() {
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 30, 40]));
        assert_eq!(mapped(&a), vec![Some(0), None, Some(1), Some(2)]);
        assert_eq!(a.removed, vec![1]);
        assert_eq!(shift_ranges(&a.map)[1].delta, -1);
    }

    #[test]
    fn a_coarse_match_is_likely_not_proven() {
        // 21 keeps 20's opcodes but not its constants: a strong candidate, not a fact
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 21, 30, 40]));
        assert_eq!(mapped(&a), vec![Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(a.changed_in_place, 1);
        assert_eq!(a.confidence[1], LikelyByShape);
        assert_eq!(a.likely_pairs, 1);
        assert!(a.gaps.is_empty());
    }

    #[test]
    fn a_method_replaced_by_another_of_the_same_shape_is_not_proven_either() {
        // 25 has 20's shape (same coarse) but is a different function: coarse cannot tell
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 25, 30, 40]));
        assert_eq!(a.map[1], Some(1));
        assert_ne!(a.confidence[1], Strong);
    }

    #[test]
    fn an_equal_length_region_without_evidence_is_a_guess() {
        // B and C were rewritten beyond recognition: same count on both sides, no proof of order
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 55, 66, 40]));
        assert_eq!(mapped(&a), vec![Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(a.confidence, vec![Strong, Guess, Guess, Strong]);
        assert_eq!(
            a.gaps,
            vec![Gap {
                old_from: 1,
                old_to: 3,
                new_from: 1,
                new_to: 3
            }]
        );
        assert_eq!((a.changed_in_place, a.guessed_pairs), (2, 2));
    }

    #[test]
    fn twins_paired_by_position_are_likely_not_proven() {
        // two identical-looking stubs: swapping them would be invisible, so neither is proven
        let a = align(&fps(&[10, 7, 7, 40]), &fps(&[10, 7, 7, 40]));
        assert_eq!(mapped(&a), vec![Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(
            a.confidence,
            vec![Strong, LikelyByPosition, LikelyByPosition, Strong]
        );
        assert_eq!(
            (a.changed_in_place, a.likely_pairs, a.guessed_pairs),
            (0, 2, 0)
        );
        assert!(
            a.gaps.is_empty(),
            "same bodies: not a changed region, only a soft order"
        );
    }

    #[test]
    fn coarse_hash_rescues_a_function_whose_constants_changed() {
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 20, 99, 31, 40]));
        assert_eq!(mapped(&a), vec![Some(0), Some(1), Some(3), Some(4)]);
        assert_eq!(a.inserted, vec![2]);
        assert_eq!(a.changed_in_place, 1);
        assert_eq!(a.confidence[2], LikelyByShape);
    }

    #[test]
    fn unequal_unresolved_regions_are_guesses() {
        let a = align(&fps(&[10, 1, 2, 40]), &fps(&[10, 3, 4, 5, 40]));
        assert_eq!(a.map[0], Some(0));
        assert_eq!(a.map[3], Some(4));
        assert_eq!(a.confidence[1], Guess);
        assert_eq!(a.confidence[2], Guess);
        assert_eq!(
            a.gaps,
            vec![Gap {
                old_from: 1,
                old_to: 3,
                new_from: 1,
                new_to: 4
            }]
        );
        assert_eq!(a.inserted, vec![3]);
    }

    #[test]
    fn insertion_and_removal_with_the_same_final_size() {
        // B removed, X inserted later: size stays 5, but C and D move back and E stays
        let a = align(&fps(&[10, 20, 30, 40, 50]), &fps(&[10, 30, 40, 99, 50]));
        assert_eq!(mapped(&a), vec![Some(0), None, Some(1), Some(2), Some(4)]);
        assert_eq!(a.removed, vec![1]);
        assert_eq!(a.inserted, vec![3]);
        let r = shift_ranges(&a.map);
        assert_eq!(
            r,
            vec![
                ShiftRange {
                    from: 0,
                    to: 0,
                    delta: 0
                },
                ShiftRange {
                    from: 2,
                    to: 3,
                    delta: -1
                },
                ShiftRange {
                    from: 4,
                    to: 4,
                    delta: 0
                },
            ]
        );
    }

    #[test]
    fn several_insertions_in_one_table() {
        let a = align(
            &fps(&[10, 20, 30, 40]),
            &fps(&[10, 91, 20, 92, 93, 30, 40, 94]),
        );
        assert_eq!(mapped(&a), vec![Some(0), Some(2), Some(5), Some(6)]);
        assert_eq!(a.inserted, vec![1, 3, 4, 7]);
        assert!(a.removed.is_empty());
    }

    #[test]
    fn insertion_at_the_very_start_and_at_the_very_end() {
        let a = align(&fps(&[10, 20, 30]), &fps(&[99, 10, 20, 30]));
        assert_eq!(mapped(&a), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(a.inserted, vec![0]);
        let b = align(&fps(&[10, 20, 30]), &fps(&[10, 20, 30, 99]));
        assert_eq!(mapped(&b), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(b.inserted, vec![3]);
        assert!(shift_ranges(&b.map).iter().all(|r| r.delta == 0));
    }

    #[test]
    fn reordered_methods_are_followed_not_guessed() {
        let a = align(&fps(&[10, 20, 30, 40]), &fps(&[10, 30, 20, 40]));
        // Myers keeps the longer common run; the moved slot is reported as removed + inserted
        let moved: Vec<usize> = a
            .map
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_none())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            moved.len(),
            1,
            "exactly one slot has no counterpart in order: {:?}",
            a.map
        );
        assert_eq!(a.inserted.len(), 1);
        assert_eq!(a.guessed_pairs, 0, "no positional guessing here");
    }

    fn fpi(exact: u64, coarse: u64, instructions: u32) -> Fp {
        Fp {
            exact,
            coarse,
            instructions,
        }
    }

    #[test]
    fn an_insertion_inside_a_changed_region_lands_before_the_large_method() {
        // A anchors; then old [B: 40 instructions] vs new [X: 5, Y: 5, B': 41, rewritten]
        let old = vec![fpi(10, 1, 5), fpi(20, 2, 40)];
        let new = vec![fpi(10, 1, 5), fpi(91, 9, 5), fpi(92, 9, 5), fpi(25, 8, 41)];
        let a = align(&old, &new);
        assert_eq!(
            a.map,
            vec![Some(0), Some(3)],
            "B follows its size, not its position: {:?}",
            a.map
        );
        assert_eq!(a.inserted, vec![1, 2]);
        assert!(a.removed.is_empty());
        assert_eq!(a.confidence[1], Guess, "still a guess, only a better one");
    }

    #[test]
    fn a_removal_inside_a_changed_region_is_placed_by_similarity_too() {
        // old [S: 5, M: 30, L: 60] vs new [M': 31, L': 61]: the small one is the removed slot
        let old = vec![
            fpi(10, 1, 5),
            fpi(1, 11, 5),
            fpi(2, 12, 30),
            fpi(3, 13, 60),
            fpi(40, 4, 5),
        ];
        let new = vec![fpi(10, 1, 5), fpi(5, 15, 31), fpi(6, 16, 61), fpi(40, 4, 5)];
        let a = align(&old, &new);
        assert_eq!(
            a.map,
            vec![Some(0), None, Some(1), Some(2), Some(3)],
            "{:?}",
            a.map
        );
        assert_eq!(a.removed, vec![1]);
    }

    #[test]
    fn stubs_never_resemble_code() {
        // a null slot and a real function must not be paired just to avoid a gap
        let old = vec![
            fpi(10, 1, 5),
            fpi(0x4E55_4C4C, 0x4E55_4C4C, 0),
            fpi(40, 4, 5),
        ];
        let new = vec![
            fpi(10, 1, 5),
            fpi(77, 7, 12),
            fpi(0x4E55_4C4C, 0x4E55_4C4C, 0),
            fpi(40, 4, 5),
        ];
        let a = align(&old, &new);
        assert_eq!(
            a.map[1],
            Some(2),
            "the null slot follows its twin: {:?}",
            a.map
        );
        assert_eq!(a.inserted, vec![1]);
    }

    #[test]
    fn empty_inputs() {
        let a = align(&[], &fps(&[1, 2]));
        assert_eq!(a.inserted, vec![0, 1]);
        let b = align(&fps(&[1, 2]), &[]);
        assert_eq!(b.removed, vec![0, 1]);
        assert!(shift_ranges(&[]).is_empty());
    }
}
