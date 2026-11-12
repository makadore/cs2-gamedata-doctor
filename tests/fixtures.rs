//! End-to-end checks on the synthetic fixtures in tests/fixtures (see src/*.cpp and build.sh
//! there for what the two builds contain).

use gamedata_doctor::diff::Confidence;
use gamedata_doctor::elf::{Image, Target};
use gamedata_doctor::gamedata::ClassMap;
use gamedata_doctor::report::{compare, Report};
use gamedata_doctor::rtti;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn report() -> Report {
    let old = Image::load(&fixture("old.so")).expect("old.so");
    let new = Image::load(&fixture("new.so")).expect("new.so");
    compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "linux",
    )
    .expect("compare")
}

#[test]
fn finds_every_synthetic_class_with_the_expected_slot_count() {
    let img = Image::load(&fixture("old.so")).expect("old.so");
    let r = rtti::analyze(&img);
    let slots = |c: &str| {
        r.vtables[*r
            .primary
            .get(c)
            .unwrap_or_else(|| panic!("class {c} not found"))]
        .slots
        .len()
    };
    // two destructor slots + the declared virtuals
    assert_eq!(slots("Base"), 6);
    assert_eq!(slots("Derived"), 8);
    assert_eq!(slots("IThing"), 5);
    assert_eq!(slots("Thing"), 6);
    assert_eq!(slots("Mixin"), 3);
    // Base's six plus Both::mix: an override of a method from a non-primary base gets its own
    // slot in the primary table (Itanium ABI), the Mixin table only holds a thunk to it.
    assert_eq!(slots("Both"), 7);
    assert_eq!(slots("IExtra"), 3);
    assert_eq!(slots("Combo"), 7);
    assert_eq!(slots("Twins"), 4);
    assert_eq!(slots("Solo"), 3);
    assert!(
        r.ambiguous.is_empty(),
        "every class name is unique here: {:?}",
        r.ambiguous
    );

    let secondary = |c: &str| {
        r.vtables
            .iter()
            .find(|v| v.class == c && v.offset_to_top != 0)
            .unwrap_or_else(|| panic!("{c} needs a secondary vtable"))
    };
    assert_eq!(
        secondary("Both").slots.len(),
        3,
        "the Mixin part of Both: two destructors and mix, nothing from the typeinfo that follows"
    );
    assert_eq!(
        secondary("Combo").slots.len(),
        3,
        "the IExtra part of Combo: two destructors and extra"
    );

    let ithing = &r.vtables[r.primary["IThing"]];
    assert_eq!(
        &ithing.slots[0..2],
        &[Target::Null, Target::Null],
        "GCC zeroes the destructor slots of an abstract class"
    );
    assert!(ithing.slots[2..]
        .iter()
        .all(|s| matches!(s, Target::Sym(n) if n == "__cxa_pure_virtual")));
}

#[test]
fn reports_the_inserted_slot_and_the_shift_behind_it() {
    let rep = report();
    let class = |c: &str| {
        rep.changed
            .iter()
            .find(|x| x.class == c)
            .unwrap_or_else(|| panic!("{c} should be reported as changed"))
    };

    let base = class("Base");
    assert_eq!((base.old_slots, base.new_slots), (6, 7));
    assert_eq!(
        base.inserted_new,
        vec![3],
        "alpha_prime lands after the two destructors and alpha"
    );
    assert!(
        base.shifts
            .iter()
            .any(|s| s.from == 3 && s.to == 5 && s.delta == 1),
        "beta, gamma, delta move by one: {:?}",
        base.shifts
    );
    assert!(base.unmatched_old.is_empty());
    assert_eq!(
        (base.guessed_pairs, base.changed_in_place),
        (0, 0),
        "every Base slot is matched by evidence"
    );

    let derived = class("Derived");
    assert_eq!((derived.old_slots, derived.new_slots), (8, 9));
    assert_eq!(derived.inserted_new, vec![3]);
    assert!(
        derived
            .shifts
            .iter()
            .any(|s| s.from == 3 && s.to == 7 && s.delta == 1),
        "{:?}",
        derived.shifts
    );
    assert_eq!(
        derived.changed_in_place, 1,
        "zeta's body changed, nothing else"
    );
    assert_eq!(
        derived.guessed_pairs, 1,
        "the rewritten zeta can only be placed by position"
    );

    let thing = class("Thing");
    assert_eq!((thing.old_slots, thing.new_slots), (6, 7));
    assert_eq!(thing.inserted_new, vec![6], "five is appended");
    assert!(
        thing.shifts.is_empty(),
        "an appended slot shifts nothing: {:?}",
        thing.shifts
    );

    for c in ["Both", "Combo"] {
        let x = class(c);
        assert_eq!(x.new_slots, x.old_slots + 1, "{c} inherits Base's new slot");
        assert!(
            x.shifts.iter().any(|s| s.from == 3 && s.delta == 1),
            "{c} inherits Base's shift: {:?}",
            x.shifts
        );
    }

    for c in ["Mixin", "IThing", "IExtra", "Twins", "Solo"] {
        assert!(
            !rep.changed.iter().any(|x| x.class == c),
            "{c} has no hard change and must not be in the changed list"
        );
    }
    assert_eq!(
        rep.classes_with_likely_pairs_only, 1,
        "Twins is the soft signal"
    );
    assert!(rep.only_old.is_empty() && rep.only_new.is_empty() && rep.ambiguous.is_empty());
}

/// A non-virtual function between two virtuals changes its body; no slot's fingerprint may move.
/// Guards the "ret followed by padding" boundary heuristic in fp.rs (REVIEW.md 3.1 for .eh_frame).
#[test]
fn a_changed_non_virtual_neighbour_does_not_touch_any_slot() {
    let rep = report();
    let base = rep
        .changed
        .iter()
        .find(|x| x.class == "Base")
        .expect("Base changed");
    assert_eq!(
        base.changed_in_place, 0,
        "only Base::helper changed, and it has no slot"
    );
    let derived = rep
        .changed
        .iter()
        .find(|x| x.class == "Derived")
        .expect("Derived changed");
    assert_eq!(derived.changed_in_place, 1, "zeta only");
}

#[test]
fn maps_gamedata_offsets_to_the_new_build() {
    let rep = report();
    let g = |e: &str| {
        rep.gamedata
            .iter()
            .find(|x| x.entry == e)
            .unwrap_or_else(|| panic!("entry {e}"))
    };

    assert_eq!(g("Base_beta").class.as_deref(), Some("Base"));
    assert_eq!(g("Base_beta").new_index, Some(4));
    assert_eq!(g("Base_beta").confidence, Some(Confidence::Strong));
    assert!(
        g("Base_beta").verdict.starts_with("SHIFT 3 -> 4"),
        "{}",
        g("Base_beta").verdict
    );

    // zeta was rewritten (exact and coarse both differ): its new place is a positional guess
    assert_eq!(g("Derived_zeta").new_index, Some(8));
    assert_eq!(g("Derived_zeta").confidence, Some(Confidence::Guess));
    assert!(
        g("Derived_zeta").verdict.starts_with("GUESS 7 -> 8"),
        "{}",
        g("Derived_zeta").verdict
    );

    assert_eq!(g("Thing_four").new_index, Some(5));
    assert_eq!(g("Thing_four").confidence, Some(Confidence::Strong));
    assert!(
        g("Thing_four").verdict.starts_with("unchanged"),
        "{}",
        g("Thing_four").verdict
    );

    // abstract interfaces are answered through every class implementing them: IThing has Thing
    // and Thing2, both through the primary base, both unchanged; IExtra has Combo (secondary
    // table) and VExtra (virtual base), and one uncheckable implementation blocks the verdict
    let two = g("IThing_two");
    assert_eq!(two.new_index, Some(3), "{}", two.verdict);
    assert_eq!(two.via.as_deref(), Some("Thing"));
    assert_eq!(two.implementations_checked, vec!["Thing", "Thing2"]);
    assert_eq!(two.confidence, Some(Confidence::Strong));
    assert!(
        two.verdict
            .contains("via Thing (2 implementation(s) checked, all agree)"),
        "{}",
        two.verdict
    );
    let extra = g("IExtra_extra");
    assert_eq!(extra.new_index, None, "{}", extra.verdict);
    assert_eq!(extra.implementations_checked, vec!["Combo"]);
    assert_eq!(extra.implementations_unresolved, vec!["VExtra"]);
    assert!(
        extra
            .verdict
            .contains("VExtra: reaches the interface through a virtual base"),
        "{}",
        extra.verdict
    );
    // a base reached only through virtual inheritance has no static offset: not placed
    let vb = g("VBase_v");
    assert_eq!(vb.new_index, None, "{}", vb.verdict);
    assert!(vb.verdict.contains("virtual base"), "{}", vb.verdict);
    assert_eq!(g("INotHere::Method").class, None);
    assert!(
        g("INotHere::Method").verdict.contains("interface"),
        "{}",
        g("INotHere::Method").verdict
    );

    assert!(
        g("Base_field").verdict.contains("beyond the old table"),
        "{}",
        g("Base_field").verdict
    );
    assert_eq!(g("Unknown_Thing").class, None);
    assert!(
        !rep.gamedata.iter().any(|x| x.entry == "SomeSignature"),
        "signature entries are not offsets"
    );
}

/// Twins::left and Twins::right have identical code and only differ in the function they call;
/// the new build swaps the targets. Fingerprints cannot see that, so the tool must not claim
/// the slot is unchanged.
#[test]
fn same_shaped_methods_with_swapped_targets_are_never_reported_as_proven() {
    let rep = report();
    let g = rep
        .gamedata
        .iter()
        .find(|x| x.entry == "Twins_left")
        .expect("Twins_left");
    assert_eq!(g.class.as_deref(), Some("Twins"));
    assert_eq!(g.new_index, Some(2), "by position the slot is still 2");
    assert_eq!(g.confidence, Some(Confidence::LikelyByPosition));
    assert!(g.verdict.starts_with("LIKELY 2 -> 2"), "{}", g.verdict);
    assert!(
        !g.verdict.starts_with("unchanged"),
        "an invisible swap must not come out as a fact"
    );
}

/// Solo::only keeps its exact shape but calls target_b instead of target_a in the new build.
/// Fingerprints mask call targets, so the slot comes out as the strongest level the tool has.
/// This test pins the blind spot: `Strong` is a structural match, never a semantic proof, and
/// the verdict text says so.
#[test]
fn a_same_shaped_replacement_of_a_unique_method_is_the_known_blind_spot() {
    let rep = report();
    let g = rep
        .gamedata
        .iter()
        .find(|x| x.entry == "Solo_only")
        .expect("Solo_only");
    assert_eq!(g.new_index, Some(2));
    assert_eq!(g.confidence, Some(Confidence::Strong));
    assert!(
        g.verdict.starts_with("unchanged: strong fingerprint match"),
        "{}",
        g.verdict
    );
    assert!(
        !rep.changed.iter().any(|c| c.class == "Solo"),
        "nothing visible changed in Solo's table"
    );
}

/// `BareBeta` names no class; a class map says it is Base::beta, which moved 3 -> 4.
#[test]
fn a_class_map_places_entries_whose_names_carry_no_class() {
    let old = Image::load(&fixture("old.so")).unwrap();
    let new = Image::load(&fixture("new.so")).unwrap();
    let without = compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "linux",
    )
    .unwrap();
    let g = without
        .gamedata
        .iter()
        .find(|x| x.entry == "BareBeta")
        .expect("BareBeta");
    assert_eq!(g.class, None);
    assert_eq!(g.new_index, None);

    let map = gamedata_doctor::gamedata::load_class_map(&fixture("classmap.txt")).unwrap();
    let with = compare(&old, &new, &[fixture("gamedata.json")], &map, "linux").unwrap();
    let g = with
        .gamedata
        .iter()
        .find(|x| x.entry == "BareBeta")
        .expect("BareBeta");
    assert_eq!(g.class.as_deref(), Some("Base"));
    assert_eq!(g.new_index, Some(4));
    assert!(g.verdict.starts_with("SHIFT 3 -> 4"), "{}", g.verdict);
    let missing = with
        .gamedata
        .iter()
        .find(|x| x.entry == "MappedToNowhere")
        .expect("MappedToNowhere");
    assert!(
        missing.verdict.contains("not in the binary"),
        "{}",
        missing.verdict
    );
}

#[test]
fn an_abstract_class_s_own_implemented_method_is_placed_through_its_own_table() {
    // IHalf implements `real` and leaves `pure` to Half: the table is mostly stubs, but the slot
    // the entry points at has a body, so it is placed directly, not through the implementations
    let rep = report();
    let real = rep
        .gamedata
        .iter()
        .find(|g| g.entry == "IHalf_real")
        .unwrap();
    assert_eq!((real.old_index, real.new_index), (Some(2), Some(2)));
    assert_eq!(real.confidence, Some(Confidence::Strong));
    assert!(real.verdict.starts_with("unchanged"), "{}", real.verdict);
    assert!(real.via.is_none(), "placed directly: {:?}", real.via);
    assert!(real.implementations_checked.is_empty());
}

#[test]
fn an_elf_image_only_answers_for_linux_indices() {
    let old = Image::load(&fixture("old.so")).unwrap();
    let new = Image::load(&fixture("new.so")).unwrap();
    let err = compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "windows",
    )
    .err()
    .expect("must refuse");
    assert!(
        err.to_string()
            .contains("carries linux indices, not windows"),
        "{err}"
    );
    assert!(compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "plan9"
    )
    .is_err());
}
