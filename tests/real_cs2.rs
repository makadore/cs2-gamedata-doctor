//! Regression against the real case that motivated the tool: the CS2 update of 22 October 2026
//! (depot 2347773, manifests 4676054823281784086 -> 5491505037461703699) added one virtual to
//! IVEngineServer2. The expected slot is established independently of gamedata-doctor from the hl2sdk
//! headers at commit 2261b6d0: IAppSystem 11 + ISource2Engine 7 + IsSpawnGroupLoadedForClient
//! as declaration 47 of IVEngineServer2 = slot 65.
//!
//! Valve's binaries cannot be shipped, so this test is ignored unless the two files are given:
//!   VTDIFF_CS2_ENGINE_OLD=/path/to/2026-10-20/libengine2.so
//!   VTDIFF_CS2_ENGINE_NEW=/path/to/2026-10-22/libengine2.so
//!   cargo test --test real_cs2 -- --ignored

use gamedata_doctor::elf::Image;
use gamedata_doctor::report::compare;
use std::path::PathBuf;

const EXPECTED_SLOT: usize = 11 + 7 + 47;

#[test]
#[ignore = "needs the two libengine2.so builds, see the file header"]
fn engine_server_gained_slot_65_in_the_22_october_update() {
    let old = PathBuf::from(std::env::var("VTDIFF_CS2_ENGINE_OLD").expect("VTDIFF_CS2_ENGINE_OLD"));
    let new = PathBuf::from(std::env::var("VTDIFF_CS2_ENGINE_NEW").expect("VTDIFF_CS2_ENGINE_NEW"));
    let old = Image::load(&old).unwrap();
    let new = Image::load(&new).unwrap();
    let rep = compare(
        &old,
        &new,
        &Vec::<PathBuf>::new(),
        &Default::default(),
        "linux",
    )
    .unwrap();

    let es = rep
        .changed
        .iter()
        .find(|c| c.class == "CEngineServer")
        .expect("CEngineServer must be reported");
    assert_eq!(
        es.new_slots,
        es.old_slots + 1,
        "exactly one virtual was added"
    );
    assert_eq!(es.inserted_new, vec![EXPECTED_SLOT]);
    assert!(
        es.shifts
            .iter()
            .any(|s| s.from == EXPECTED_SLOT && s.to == es.old_slots - 1 && s.delta == 1),
        "everything from slot {EXPECTED_SLOT} to the end moves by one: {:?}",
        es.shifts
    );
    assert!(
        es.unmatched_old.is_empty(),
        "no slot of a stable engine interface should go missing"
    );
    // duplicate stubs elsewhere in the table may be paired by position; the insertion and the
    // shifted tail behind it must not depend on any guess
    for g in &es.uncertain_regions {
        assert!(
            g.old_to <= EXPECTED_SLOT,
            "an uncertain region overlaps the shifted tail: {g:?}"
        );
    }
    assert!(!rep.ambiguous.iter().any(|a| a == "CEngineServer"));
}
