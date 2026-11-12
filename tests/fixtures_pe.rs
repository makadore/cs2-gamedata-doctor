//! The same synthetic classes compiled by MSVC (`tests/fixtures/old.dll`, `new.dll`, built by
//! `build.cmd`): the Windows path through PE loading, MSVC RTTI and the `windows` gamedata key
//! must tell the same story as the ELF path, in MSVC's layout: one destructor slot, and an
//! overrider of a secondary base's method does not get a slot in the primary table.

use gamedata_doctor::diff::Confidence;
use gamedata_doctor::gamedata::{self, ClassMap};
use gamedata_doctor::image::{Format, Image};
use gamedata_doctor::report::{compare, plan_apply, Report};
use gamedata_doctor::rtti::{self, BaseLookup};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn report() -> Report {
    let old = Image::load(&fixture("old.dll")).expect("old.dll");
    let new = Image::load(&fixture("new.dll")).expect("new.dll");
    let map = gamedata::load_class_map(&fixture("classmap.txt")).unwrap();
    compare(&old, &new, &[fixture("gamedata.json")], &map, "windows").unwrap()
}

#[test]
fn loads_as_pe_with_exact_function_ranges() {
    let img = Image::load(&fixture("old.dll")).unwrap();
    assert_eq!(img.format, Format::Pe);
    assert_ne!(img.base, 0, "a PE image has a preferred base");
    assert!(
        img.func_ranges.len() >= 30,
        ".pdata gives every function its range: {}",
        img.func_ranges.len()
    );
    assert!(
        img.relocs.len() >= 50,
        "{} relocated pointers",
        img.relocs.len()
    );
}

#[test]
fn finds_every_synthetic_class_with_msvc_slot_counts() {
    let img = Image::load(&fixture("old.dll")).unwrap();
    let r = rtti::analyze(&img);
    let slots = |c: &str| r.vtables[r.primary[c]].slots.len();
    assert_eq!(
        slots("Base"),
        5,
        "one destructor slot, then alpha beta gamma delta"
    );
    assert_eq!(slots("Derived"), 7);
    assert_eq!(slots("IThing"), 4);
    assert_eq!(slots("Thing"), 5);
    assert_eq!(slots("Mixin"), 2);
    assert_eq!(
        slots("Both"),
        5,
        "Both's override of Mixin::mix lives only in the Mixin table under MSVC"
    );
    assert_eq!(slots("IExtra"), 2);
    assert_eq!(slots("Combo"), 5);
    assert_eq!(slots("Twins"), 3);
    assert_eq!(slots("Solo"), 2);
    assert_eq!(slots("VBase"), 2);
    assert_eq!(slots("Thing2"), 4);
    assert!(
        !r.primary.contains_key("VExtra") && !r.primary.contains_key("Diamond"),
        "a class whose only table serves a virtual base has no primary table under MSVC"
    );
    assert!(
        r.class_ti.contains_key("VExtra") && r.class_ti.contains_key("Diamond"),
        "but both are known from their locators"
    );
    assert_eq!(r.subobject_offset("VExtra", "IExtra"), BaseLookup::Virtual);
    assert_eq!(r.subobject_offset("Diamond", "VBase"), BaseLookup::Virtual);
    assert_eq!(
        r.subobject_offset("Both", "Base"),
        BaseLookup::NonVirtual(0)
    );
    let mixin_in_both = r.subobject_offset("Both", "Mixin");
    assert!(
        matches!(mixin_in_both, BaseLookup::NonVirtual(o) if o > 0),
        "{mixin_in_both:?}"
    );
    let t = r.table_for_base("Both", "Mixin").unwrap();
    assert_eq!(
        t.slots.len(),
        2,
        "the Mixin part of Both: destructor and mix"
    );
    assert_eq!(
        r.table_for_base("Combo", "IExtra").unwrap().slots.len(),
        2,
        "the IExtra part of Combo"
    );
    assert!(r.ambiguous.is_empty());
}

#[test]
fn reports_the_inserted_slot_and_the_shift_behind_it() {
    let rep = report();
    let base = rep
        .changed
        .iter()
        .find(|c| c.class == "Base")
        .expect("Base changed");
    assert_eq!((base.old_slots, base.new_slots), (5, 6));
    assert_eq!(
        base.inserted_new,
        vec![2],
        "alpha_prime after the destructor and alpha"
    );
    assert_eq!(base.shifts.len(), 1);
    assert_eq!(
        (base.shifts[0].from, base.shifts[0].to, base.shifts[0].delta),
        (2, 4, 1)
    );
    let thing = rep
        .changed
        .iter()
        .find(|c| c.class == "Thing")
        .expect("Thing changed");
    assert_eq!(thing.inserted_new, vec![5], "five appended");
    assert!(thing.shifts.is_empty());
    let derived = rep.changed.iter().find(|c| c.class == "Derived").unwrap();
    assert_eq!(
        derived.changed_in_place + derived.guessed_pairs,
        2,
        "zeta rewritten inside the shift"
    );
    for c in ["Mixin", "IThing", "IExtra", "Twins", "Solo", "VBase"] {
        assert!(
            !rep.changed.iter().any(|x| x.class == c),
            "{c} did not change"
        );
    }
}

#[test]
fn maps_windows_gamedata_offsets_to_the_new_build() {
    let rep = report();
    let g = |e: &str| rep.gamedata.iter().find(|g| g.entry == e).unwrap();

    let beta = g("Base_beta");
    assert_eq!((beta.old_index, beta.new_index), (Some(2), Some(3)));
    assert_eq!(beta.confidence, Some(Confidence::Strong));
    assert!(beta.verdict.starts_with("SHIFT 2 -> 3"), "{}", beta.verdict);

    let zeta = g("Derived_zeta");
    assert_eq!((zeta.old_index, zeta.new_index), (Some(6), Some(7)));
    assert_eq!(zeta.confidence, Some(Confidence::Guess));

    let four = g("Thing_four");
    assert_eq!(four.new_index, Some(4));
    assert_eq!(four.confidence, Some(Confidence::Strong));
    assert!(four.verdict.starts_with("unchanged"), "{}", four.verdict);

    let left = g("Twins_left");
    assert_eq!(left.new_index, Some(1));
    assert_eq!(left.confidence, Some(Confidence::LikelyByPosition));

    let solo = g("Solo_only");
    assert_eq!(solo.new_index, Some(1));
    assert_eq!(
        solo.confidence,
        Some(Confidence::Strong),
        "the known blind spot"
    );

    let bare = g("BareBeta");
    assert_eq!(bare.new_index, Some(3), "placed on Base by the class map");

    let two = g("IThing_two");
    assert_eq!(two.new_index, Some(2));
    assert_eq!(two.implementations_checked, vec!["Thing", "Thing2"]);
    assert!(
        two.verdict
            .contains("2 implementation(s) checked, all agree"),
        "{}",
        two.verdict
    );

    let extra = g("IExtra_extra");
    assert_eq!(
        extra.new_index, None,
        "VExtra reaches IExtra through a virtual base"
    );
    assert_eq!(extra.implementations_checked, vec!["Combo"]);
    assert_eq!(extra.implementations_unresolved, vec!["VExtra"]);
    assert!(extra.verdict.contains("virtual base"), "{}", extra.verdict);

    assert_eq!(g("VBase_v").new_index, None);
    assert!(g("Base_field").verdict.contains("field offset"));
    assert!(g("Unknown_Thing").verdict.contains("not a vtable entry"));
    assert!(g("INotHere::Method").verdict.contains("no class"));
    assert!(g("MappedToNowhere").verdict.contains("not in the binary"));
}

#[test]
fn apply_writes_the_windows_key_and_leaves_linux_alone() {
    let rep = report();
    let plan = plan_apply(&rep, Confidence::Strong);
    let applied: Vec<&str> = plan.applied.iter().map(|(e, _, _, _)| e.as_str()).collect();
    assert_eq!(applied, vec!["Base_beta", "BareBeta"]);
    let text = std::fs::read_to_string(fixture("gamedata.json")).unwrap();
    let changes: Vec<(String, i64)> = plan
        .applied
        .iter()
        .map(|(e, _, n, _)| (e.clone(), *n))
        .collect();
    let (out, missing) = gamedata::rewrite_os(&text, &changes, "windows");
    assert!(missing.is_empty());
    let entries = gamedata::parse(&out, "json").unwrap();
    let beta = entries.iter().find(|e| e.name == "Base_beta").unwrap();
    assert_eq!(
        (beta.linux, beta.windows),
        (Some(3), Some(3)),
        "windows written, linux untouched"
    );
    assert_eq!(out.lines().count(), text.lines().count());
}

#[test]
fn a_pe_image_and_an_elf_image_are_not_compared() {
    let old = Image::load(&fixture("old.dll")).unwrap();
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
    assert!(err.to_string().contains("carries linux indices"), "{err}");
    let err = compare(
        &old,
        &old,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "linux",
    )
    .err()
    .expect("must refuse");
    assert!(err.to_string().contains("carries windows indices"), "{err}");
}

#[test]
fn an_abstract_class_s_own_implemented_method_is_placed_through_its_own_table() {
    let rep = report();
    let real = rep
        .gamedata
        .iter()
        .find(|g| g.entry == "IHalf_real")
        .unwrap();
    assert_eq!((real.old_index, real.new_index), (Some(1), Some(1)));
    assert_eq!(real.confidence, Some(Confidence::Strong));
    assert!(real.via.is_none(), "placed directly: {:?}", real.via);
    assert!(real.implementations_checked.is_empty());
}
