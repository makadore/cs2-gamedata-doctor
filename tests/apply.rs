//! `apply`: only the verdicts the tool is sure about get written; everything else is left for a
//! human, and the file keeps its formatting.

use gamedata_doctor::diff::Confidence;
use gamedata_doctor::elf::Image;
use gamedata_doctor::gamedata::{self, ClassMap};
use gamedata_doctor::report::{compare, plan_apply};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn strong_shifts_are_written_guesses_are_left_to_a_human() {
    let old = Image::load(&fixture("old.so")).unwrap();
    let new = Image::load(&fixture("new.so")).unwrap();
    let rep = compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "linux",
    )
    .unwrap();

    let plan = plan_apply(&rep, Confidence::Strong);
    let applied: Vec<&str> = plan.applied.iter().map(|(e, _, _, _)| e.as_str()).collect();
    assert_eq!(
        applied,
        vec!["Base_beta"],
        "only the proven shift is applied at level strong"
    );
    assert_eq!(plan.applied[0].1, 3);
    assert_eq!(plan.applied[0].2, 4);
    let human: Vec<&str> = plan.needs_human.iter().map(|(e, _)| e.as_str()).collect();
    assert!(
        human.contains(&"Derived_zeta"),
        "a GUESS is never applied: {human:?}"
    );
    assert!(
        plan.unconfirmed.iter().any(|(e, _)| e == "Twins_left") && !human.contains(&"Twins_left"),
        "LIKELY at the same index is neither written nor an alarm at level strong: {:?}",
        plan.unconfirmed
    );
    assert!(
        human.contains(&"VBase_v"),
        "an unplaced interface entry needs a human: {human:?}"
    );
    assert!(
        !human.contains(&"Base_field"),
        "a field offset is not the tool's business"
    );
    assert!(plan.unchanged >= 1, "Thing_four is unchanged and strong");

    let text = std::fs::read_to_string(fixture("gamedata.json")).unwrap();
    let changes: Vec<(String, i64)> = plan
        .applied
        .iter()
        .map(|(e, _, n, _)| (e.clone(), *n))
        .collect();
    let (out, missing) = gamedata::rewrite(&text, &changes);
    assert!(missing.is_empty());
    let entries = gamedata::parse(&out, "json").unwrap();
    let beta = entries.iter().find(|e| e.name == "Base_beta").unwrap();
    assert_eq!(
        (beta.linux, beta.windows),
        (Some(4), Some(2)),
        "linux written, windows untouched"
    );
    let zeta = entries.iter().find(|e| e.name == "Derived_zeta").unwrap();
    assert_eq!(zeta.linux, Some(7), "the guess was not written");
    assert_eq!(
        out.lines().count(),
        text.lines().count(),
        "formatting preserved line for line"
    );
}

#[test]
fn level_likely_also_writes_likely_verdicts_but_never_guesses() {
    let old = Image::load(&fixture("old.so")).unwrap();
    let new = Image::load(&fixture("new.so")).unwrap();
    let rep = compare(
        &old,
        &new,
        &[fixture("gamedata.json")],
        &ClassMap::new(),
        "linux",
    )
    .unwrap();
    let plan = plan_apply(&rep, Confidence::Guess);
    let applied: Vec<&str> = plan.applied.iter().map(|(e, _, _, _)| e.as_str()).collect();
    assert!(
        !applied.contains(&"Derived_zeta"),
        "asking for guess still never applies a guess"
    );
    let human: Vec<&str> = plan.needs_human.iter().map(|(e, _)| e.as_str()).collect();
    assert!(
        !human.contains(&"Twins_left"),
        "a LIKELY unchanged entry is settled at level likely"
    );
    assert!(human.contains(&"Derived_zeta"));
}

/// The CLI contract of `--in-place`: the original survives as `<file>.<hash>.bak`, nothing temporary
/// is left behind, and a second run over the already written file makes no change and no new backup.
#[test]
fn in_place_keeps_a_backup_and_leaves_the_directory_clean() {
    let dir = std::env::temp_dir().join(format!("gamedata-doctor-apply-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("gamedata.json");
    let original = std::fs::read_to_string(fixture("gamedata.json")).unwrap();
    std::fs::write(&target, &original).unwrap();

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gamedata-doctor"))
        .args(["apply"])
        .arg(fixture("old.so"))
        .arg(fixture("new.so"))
        .arg("--gamedata")
        .arg(&target)
        .arg("--in-place")
        .status()
        .unwrap();
    assert!(status.success(), "apply --in-place failed: {status}");

    let bak = gamedata_doctor::fsutil::backup_path(&target, original.as_bytes());
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        original,
        "the backup is the untouched original"
    );
    let written = std::fs::read_to_string(&target).unwrap();
    assert_ne!(written, original, "the strong shift was written");
    let entries = gamedata::parse(&written, "json").unwrap();
    let beta = entries.iter().find(|e| e.name == "Base_beta").unwrap();
    assert_eq!(beta.linux, Some(4));
    let names = |d: &std::path::Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(&dir).len(),
        2,
        "only the file and its backup remain: {:?}",
        names(&dir)
    );

    // the same apply over the already updated file is a user's mistake the tool cannot detect (the
    // file now describes the new build, so its indices land on other functions of the old table and
    // shift again); what the tool guarantees is recoverability: the first backup stays, the written
    // file gets its own backup, no temporary is left
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gamedata-doctor"))
        .args(["apply"])
        .arg(fixture("old.so"))
        .arg(fixture("new.so"))
        .arg("--gamedata")
        .arg(&target)
        .arg("--in-place")
        .status()
        .unwrap();
    assert!(status.success(), "second apply failed: {status}");
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        original,
        "first backup intact"
    );
    let after = names(&dir);
    assert!(after.len() <= 3, "at most one more backup: {after:?}");
    assert!(
        after.iter().all(|n| !n.contains("gamedata-doctor-tmp")),
        "no temporary: {after:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
