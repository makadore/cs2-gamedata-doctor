//! The SDK check on the synthetic fixtures: the source of each fixture build doubles as "the SDK
//! headers" of that build (tests/fixtures/src/old.cpp and new.cpp).

use gamedata_doctor::elf::Image;
use gamedata_doctor::sdk::Headers;
use gamedata_doctor::sdkcheck::{check, Check, Verdict};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run(header: &str, ext: &str) -> Check {
    let old = Image::load(&fixture(&format!("old.{ext}"))).unwrap();
    let new = Image::load(&fixture(&format!("new.{ext}"))).unwrap();
    let os = old.format.os();
    let mut h = Headers::default();
    let text = std::fs::read_to_string(fixture(&format!("src/{header}.cpp"))).unwrap();
    h.parse(&text, &format!("{header}.cpp"), os);
    check(&old, &new, &h)
}

fn verdict(c: &Check, class: &str) -> Verdict {
    c.rows
        .iter()
        .find(|r| r.class == class)
        .map(|r| r.verdict)
        .unwrap_or(Verdict::Ok)
}

#[test]
fn headers_of_the_old_build_are_stale_where_a_slot_was_inserted() {
    for ext in ["so", "dll"] {
        let c = run("old", ext);
        // Base gained alpha_prime between alpha and beta: Base and everything laid out on top
        // of it no longer is what the old header says
        for class in ["Base", "Derived"] {
            assert_eq!(verdict(&c, class), Verdict::Broken, "{class} ({ext})");
        }
        let base = c.rows.iter().find(|r| r.class == "Base").unwrap();
        let beta = base.moved.iter().find(|m| m.method == "beta").unwrap();
        assert_eq!(beta.new, Some(beta.old + 1), "{ext}");
        assert!(
            base.moved.iter().all(|m| m.method != "alpha"),
            "alpha did not move ({ext})"
        );
        // Thing gained `five` at the end: nothing the old header declares moved
        for class in ["Thing", "IThing", "Solo", "Mixin"] {
            assert_eq!(verdict(&c, class), Verdict::Ok, "{class} ({ext})");
        }
        assert!(c.present >= 8, "{} classes found ({ext})", c.present);
    }
}

#[test]
fn headers_of_the_new_build_are_not_flagged() {
    for ext in ["so", "dll"] {
        let c = run("new", ext);
        let bad: Vec<String> = c
            .rows
            .iter()
            .filter(|r| r.verdict >= Verdict::Suspect)
            .map(|r| format!("{}: {}", r.class, r.detail))
            .collect();
        assert!(bad.is_empty(), "{ext}: {bad:?}");
    }
}

#[test]
fn the_patch_for_a_stale_header_puts_a_placeholder_where_the_slot_appeared() {
    for ext in ["so", "dll"] {
        let c = run("old", ext);
        let base = c.rows.iter().find(|r| r.class == "Base").unwrap();
        let fix = base.fix.as_ref().expect("a fix for Base");
        assert!(fix.certain, "{ext}");
        assert_eq!(fix.edits.len(), 1, "{ext}: {:?}", fix.edits);
        assert!(fix.edits[0].insert.is_some());
        assert_eq!(fix.edits[0].method, "beta");

        // every broken class asks for the same single line
        let edits: Vec<_> = c
            .rows
            .iter()
            .filter_map(|r| r.fix.as_ref())
            .flat_map(|f| f.edits.iter().cloned())
            .collect();
        let p = gamedata_doctor::sdkfix::patch(&fixture("src"), &edits).unwrap();
        assert_eq!(p.edits, 1, "{ext}");
        let text = &p.files["old.cpp"];
        let at = text
            .find("gamedata_doctor_unknown_")
            .expect("a placeholder");
        assert!(text.find("int alpha(int x);").unwrap() < at);
        assert!(at < text.find("virtual int beta").unwrap());
        let diff = String::from_utf8(p.diff.clone()).unwrap();
        assert!(
            diff.contains("+++ b/old.cpp")
                && diff.contains("\n+    virtual void gamedata_doctor_unknown_")
        );

        // the patched header describes the new build
        let old = Image::load(&fixture(&format!("old.{ext}"))).unwrap();
        let new = Image::load(&fixture(&format!("new.{ext}"))).unwrap();
        let mut h = Headers::default();
        h.parse(text, "old.cpp", old.format.os());
        let again = check(&old, &new, &h);
        let bad: Vec<&str> = again
            .rows
            .iter()
            .filter(|r| r.verdict >= Verdict::Suspect)
            .map(|r| r.class.as_str())
            .collect();
        assert!(bad.is_empty(), "{ext}: {bad:?}");
    }
}
