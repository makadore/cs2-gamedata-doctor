//! The command line as a script sees it: exit codes and the account `apply` gives of a file.
//! A run that did not check everything must not look like one that did.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gamedata-doctor-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_gamedata-doctor"))
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Two "game directories" holding the fixture library under a game-like name.
fn game_dirs(d: &Path) -> (PathBuf, PathBuf) {
    let (old, new) = (d.join("old"), d.join("new"));
    for (dir, lib) in [(&old, "old.so"), (&new, "new.so")] {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(fixture(lib), dir.join("bin/libfix.so")).unwrap();
    }
    (old, new)
}

#[test]
fn strict_passes_only_when_every_entry_is_checked() {
    let d = scratch("strict");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");

    // one entry, proven unchanged: complete
    std::fs::write(&g, r#"{ "Thing_four": { "offsets": { "linux": 5 } } }"#).unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--strict",
        "--json",
    ]);
    assert_eq!(code, 0, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["complete"], true);
    assert_eq!(v["entries"][0]["status"], "checked_same");

    // "the same index, on weak evidence" is not a check: strict fails
    std::fs::write(
        &g,
        r#"{ "Thing_four": { "offsets": { "linux": 5 } }, "Twins_left": { "offsets": { "linux": 2 } } }"#,
    )
    .unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--force",
        "--strict",
        "--json",
    ]);
    assert_eq!(code, 2, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["complete"], false);
    let statuses: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["status"].as_str().unwrap())
        .collect();
    assert!(statuses.contains(&"same_unproven"), "{statuses:?}");

    // a name the tool cannot tie to a class was not checked at all
    std::fs::write(&g, r#"{ "BareBeta": { "offsets": { "linux": 3 } } }"#).unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--force",
        "--strict",
    ]);
    assert_eq!(code, 2, "{stdout}");
    assert!(
        stdout.contains("1 NOT CHECKED") && stdout.contains("UNCHECKED BareBeta"),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_signature_whose_library_is_missing_is_reported_not_dropped() {
    let d = scratch("missing");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");
    std::fs::write(
        &g,
        r#"{ "Thing_four": { "offsets": { "linux": 5 } },
             "Elsewhere": { "signatures": { "library": "nothere", "linux": "55 48 89 E5 41 57" } } }"#,
    )
    .unwrap();
    let args = [
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--json",
    ];
    let (code, stdout, _) = run(&args);
    assert_eq!(code, 0, "without --strict the status is in the report");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["complete"], false);
    let e = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["entry"] == "Elsewhere")
        .expect("the entry is accounted for");
    assert_eq!(e["status"], "not_checked");
    assert!(e["detail"].as_str().unwrap().contains("nothere"));

    let mut strict = args.to_vec();
    strict.extend(["--force", "--strict"]);
    assert_eq!(run(&strict).0, 2);

    // `sigs` on its own: an unchecked library is not a clean exit either
    let (code, _, stderr) = run(&["sigs", s(&old), s(&new), "--gamedata", s(&g)]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("incomplete"), "{stderr}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn nothing_comparable_and_unreadable_files_are_errors() {
    let d = scratch("errors");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");
    std::fs::write(&g, r#"{ "Thing_four": { "offsets": { "linux": 5 } } }"#).unwrap();

    // the two directories share no library
    std::fs::rename(new.join("bin/libfix.so"), new.join("bin/libother.so")).unwrap();
    let (code, _, stderr) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("were not checked"), "{stderr}");
    assert!(!out.exists(), "nothing is written when nothing was checked");
    std::fs::rename(new.join("bin/libother.so"), new.join("bin/libfix.so")).unwrap();

    // a gamedata file that does not parse
    std::fs::write(&g, r#"{ "Thing_four": { "signatures": { "linux": "55 48" "#).unwrap();
    let (code, _, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
    ]);
    assert_eq!(code, 1);
    assert!(!out.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn out_does_not_replace_an_existing_file_unasked() {
    let d = scratch("out");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");
    std::fs::write(&g, r#"{ "Thing_four": { "offsets": { "linux": 5 } } }"#).unwrap();
    std::fs::write(&out, "someone's file").unwrap();
    let args = [
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
    ];
    let (code, _, stderr) = run(&args);
    assert_eq!(code, 1);
    assert!(stderr.contains("--force"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "someone's file");
    let mut forced = args.to_vec();
    forced.push("--force");
    assert_eq!(run(&forced).0, 0);
    assert!(std::fs::read_to_string(&out)
        .unwrap()
        .contains("Thing_four"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn what_is_out_of_scope_does_not_fail_a_strict_run() {
    let d = scratch("scope");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");

    // the class map says the entry is not a virtual-table index at all
    std::fs::write(&g, r#"{ "BareBeta": { "offsets": { "linux": 3 } } }"#).unwrap();
    let map = d.join("g.map");
    std::fs::write(&map, "BareBeta -").unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--strict",
        "--class-map",
        s(&map),
        "--json",
    ]);
    assert_eq!(code, 0, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["entries"][0]["status"], "out_of_scope");

    // a signature for the other platform only is nothing to check here, not a gap
    std::fs::write(
        &g,
        r#"{ "WinOnly": { "signatures": { "library": "fix", "windows": "48 89 5C 24 08" } } }"#,
    )
    .unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--force",
        "--strict",
    ]);
    assert_eq!(code, 0, "{stdout}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_incomplete_strict_run_changes_nothing_on_disk() {
    let d = scratch("nowrite");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    // Base_beta moves (and would be written), Twins_left is unproven: not complete
    let original = r#"{ "Base_beta": { "offsets": { "linux": 3 } }, "Twins_left": { "offsets": { "linux": 2 } } }"#;
    std::fs::write(&g, original).unwrap();
    let names = |d: &Path| {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let before = names(&d);

    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--in-place",
        "--strict",
        "--json",
    ]);
    assert_eq!(code, 2, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["complete"], false);
    assert_eq!(v["file_written"], false);
    assert_eq!(std::fs::read_to_string(&g).unwrap(), original);
    assert_eq!(names(&d), before, "no backup, no temporary");

    let out = d.join("out.json");
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--strict",
    ]);
    assert_eq!(code, 2);
    assert!(stdout.contains("NOT WRITTEN"), "{stdout}");
    assert!(!out.exists());

    // without --strict the draft is written and the report says what is open
    let (code, _, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
    ]);
    assert_eq!(code, 0);
    assert!(std::fs::read_to_string(&out)
        .unwrap()
        .contains("\"linux\": 4"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_index_beyond_its_table_is_unchecked_until_a_person_says_it_is_a_field() {
    let d = scratch("beyond");
    let (old, new) = game_dirs(&d);
    let g = d.join("g.json");
    let out = d.join("out.json");
    std::fs::write(&g, r#"{ "Base_field": { "offsets": { "linux": 40 } } }"#).unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--strict",
        "--json",
    ]);
    assert_eq!(code, 2, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["entries"][0]["status"], "not_checked");

    let map = d.join("g.map");
    std::fs::write(&map, "Base_field -").unwrap();
    let (code, stdout, _) = run(&[
        "apply",
        s(&old),
        s(&new),
        "--gamedata",
        s(&g),
        "--out",
        s(&out),
        "--strict",
        "--class-map",
        s(&map),
        "--json",
    ]);
    assert_eq!(code, 0, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["entries"][0]["status"], "out_of_scope");
    let _ = std::fs::remove_dir_all(&d);
}
