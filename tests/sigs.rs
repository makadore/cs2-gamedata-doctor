//! Signatures on the synthetic fixtures: a signature cut for a function of the old build is
//! followed to the same function of the new build, on ELF and on PE. The functions are taken
//! from the vtables, so the right answer is known without any hard-coded bytes.

use gamedata_doctor::image::{Image, Target};
use gamedata_doctor::rtti;
use gamedata_doctor::sig::{analyze, generate, Pattern, SigEntry, Status, Style};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn slot(img: &Image, class: &str, index: usize) -> u64 {
    let r = rtti::analyze(img);
    match &r.vtables[r.primary[class]].slots[index] {
        Target::Addr(a) => *a,
        other => panic!("{class}[{index}] is {other:?}"),
    }
}

/// (class, old slot, new slot) of methods whose bodies are the same in both builds.
fn follow(old_name: &str, new_name: &str, cases: &[(&str, usize, usize)], windows: bool) {
    let old = Image::load(&fixture(old_name)).unwrap();
    let new = Image::load(&fixture(new_name)).unwrap();
    let mut entries = Vec::new();
    let mut expected = Vec::new();
    for (class, io, inew) in cases {
        let a = slot(&old, class, *io);
        let pat = generate(&old, a, Style::Spaced)
            .unwrap_or_else(|| panic!("no unique signature for {class}[{io}] in the old build"));
        let text = pat.render();
        // the pattern really is unique, and at that function
        let code = old.section_bytes(&old.text).unwrap();
        let hits = Pattern::parse(&text).unwrap().find(code, 4);
        assert_eq!(hits.len(), 1, "{class}[{io}]: {text}");
        assert_eq!(old.text.lo + hits[0] as u64, a);
        entries.push(SigEntry {
            name: format!("{class}_{io}"),
            library: "old".to_string(),
            linux: (!windows).then(|| text.clone()),
            windows: windows.then_some(text),
            multi: false,
        });
        expected.push(slot(&new, class, *inew));
    }
    let an = analyze(&old, &new, &entries);
    for ((r, want), (class, io, _)) in an.results.iter().zip(&expected).zip(cases) {
        assert!(
            matches!(r.status, Status::StillValid | Status::Regenerated),
            "{class}[{io}]: {:?} {}",
            r.status,
            r.verdict
        );
        assert_eq!(r.new_addr, Some(*want), "{class}[{io}]: {}", r.verdict);
        if let Some(sig) = &r.new_sig {
            let code = new.section_bytes(&new.text).unwrap();
            let hits = Pattern::parse(sig).unwrap().find(code, 4);
            assert_eq!(hits.len(), 1, "the regenerated signature is unique: {sig}");
            assert_eq!(new.text.lo + hits[0] as u64, *want);
        }
    }
}

#[test]
fn signatures_follow_their_functions_on_elf() {
    // Base: dtor dtor alpha beta gamma delta -> alpha_prime inserted after alpha
    follow(
        "old.so",
        "new.so",
        &[
            ("Base", 3, 4),
            ("Base", 5, 6),
            ("Thing", 5, 5),
            ("Solo", 2, 2),
        ],
        false,
    );
}

#[test]
fn signatures_follow_their_functions_on_pe() {
    // MSVC: one destructor slot
    follow(
        "old.dll",
        "new.dll",
        &[("Base", 2, 3), ("Base", 4, 5), ("Thing", 4, 4)],
        true,
    );
}

#[test]
fn a_signature_for_another_library_or_a_symbol_is_skipped() {
    let old = Image::load(&fixture("old.so")).unwrap();
    let new = Image::load(&fixture("new.so")).unwrap();
    let entries = vec![
        SigEntry {
            name: "Msg".into(),
            library: "old".into(),
            linux: Some("@Msg".into()),
            windows: None,
            multi: false,
        },
        SigEntry {
            name: "Elsewhere".into(),
            library: "tier0".into(),
            linux: Some("55 48 89 E5 41 57".into()),
            windows: None,
            multi: false,
        },
        SigEntry {
            name: "Nowhere".into(),
            library: "old".into(),
            linux: Some("DE AD BE EF DE AD BE EF 01 02 03".into()),
            windows: None,
            multi: false,
        },
    ];
    let an = analyze(&old, &new, &entries);
    assert_eq!(an.results[0].status, Status::Skipped);
    assert_eq!(an.results[1].status, Status::Skipped);
    assert_eq!(
        an.results[2].status,
        Status::OldBroken,
        "matches nowhere in the old build"
    );
}
