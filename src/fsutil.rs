//! Replacing a file someone else owns: back it up under a content-derived name, write a temporary
//! next to it, fsync, rename. A crash at any point leaves either the old file or the new one, never
//! half of either; a second run over the same content makes no second backup and overwrites none;
//! two runs at once cannot share a temporary; a file that changed after it was read is not written.

use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `content` to `target` atomically and return the backup made, if any.
///
/// - `backup`: when `target` exists, its current bytes are saved first as `backup_path(target,
///   bytes)`, unless that file already exists (the same content was saved by an earlier run); the
///   backup is fsynced before the target is touched.
/// - `expect`: when given, the write is refused if the target's bytes differ from it, that is, the
///   file changed after it was read. The window between this check and the rename remains.
///
/// The temporary is created with `create_new`, so two processes writing the same file do not share
/// it. Permission bits are carried over; on Unix the owner is carried over when the caller may
/// (root), otherwise the call fails rather than leave the file under a new owner. ACLs and extended
/// attributes are not carried.
pub fn replace_file(
    target: &Path,
    content: &str,
    backup: bool,
    expect: Option<&[u8]>,
) -> Result<Option<PathBuf>> {
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = target
        .file_name()
        .with_context(|| format!("{}: not a file path", target.display()))?
        .to_string_lossy()
        .into_owned();
    let existing = fs::metadata(target).ok();
    let current = match &existing {
        Some(_) => Some(fs::read(target).with_context(|| format!("read {}", target.display()))?),
        None => None,
    };
    match (expect, &current) {
        (Some(exp), Some(cur)) if exp != cur.as_slice() => bail!(
            "{}: changed since it was read, nothing written",
            target.display()
        ),
        (Some(_), None) => bail!(
            "{}: gone since it was read, nothing written",
            target.display()
        ),
        _ => {}
    }
    let mut made = None;
    if let (true, Some(cur)) = (backup, &current) {
        let bak = backup_path(target, cur);
        if bak.exists() {
            // the name is derived from a short hash: the file under it must really be this
            // content, or the only copy of the original would be a stranger's file
            let kept = fs::read(&bak).with_context(|| format!("read {}", bak.display()))?;
            if kept != *cur {
                bail!(
                    "{}: holds other content than the file being replaced; move it away, nothing written",
                    bak.display()
                );
            }
        } else {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&bak)
                .with_context(|| format!("backup {}", bak.display()))?;
            f.write_all(cur)?;
            f.sync_all()?;
            if let Some(meta) = &existing {
                fs::set_permissions(&bak, meta.permissions())?;
            }
        }
        made = Some(bak);
    }
    // one name per target, not per process: two runs over one file must collide here
    let tmp = dir.join(format!(".{name}.gamedata-doctor-tmp"));
    let mut ours = false; // only a temporary this call created may be removed on failure
    let written = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| {
                format!(
                    "create {} (another gamedata-doctor writing the same file, or what an interrupted run left: remove it then)",
                    tmp.display()
                )
            })?;
        ours = true;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
            #[cfg(unix)]
            carry_owner(&tmp, meta)?;
        }
        fs::rename(&tmp, target)
            .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
        Ok(())
    })();
    if written.is_err() && ours {
        let _ = fs::remove_file(&tmp);
    }
    written?;
    // make the rename itself durable where the platform allows it
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(made)
}

#[cfg(unix)]
fn carry_owner(tmp: &Path, original: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (uid, gid) = (original.uid(), original.gid());
    let mine = fs::metadata(tmp)?;
    if mine.uid() == uid && mine.gid() == gid {
        return Ok(());
    }
    std::os::unix::fs::chown(tmp, Some(uid), Some(gid)).with_context(|| {
        format!(
            "the file belongs to uid {uid} gid {gid}: run gamedata-doctor as that user, or as root, so the \
             owner is kept"
        )
    })
}

/// `<file>.<8 hex digits of the content's hash>.bak` next to the file: one backup per distinct
/// content, never overwritten.
pub fn backup_path(target: &Path, content: &[u8]) -> PathBuf {
    let h = xxhash_rust::xxh3::xxh3_64(content) & 0xffff_ffff;
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{h:08x}.bak"));
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gamedata-doctor-fsutil-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn replaces_content_keeps_a_backup_and_leaves_no_temporary() {
        let d = scratch("basic");
        let target = d.join("gamedata.json");
        fs::write(&target, "old").unwrap();
        let bak = replace_file(&target, "new", true, Some(b"old"))
            .unwrap()
            .expect("a backup");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(fs::read_to_string(&bak).unwrap(), "old");
        let bak_name = bak.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            bak_name.starts_with("gamedata.json.")
                && bak_name.ends_with(".bak")
                && bak_name.len() == "gamedata.json.".len() + 12,
            "{bak_name}"
        );
        assert_eq!(names(&d), vec!["gamedata.json".to_string(), bak_name]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_second_run_keeps_the_first_backup_and_adds_its_own() {
        let d = scratch("twice");
        let target = d.join("g.json");
        fs::write(&target, "v1").unwrap();
        let b1 = replace_file(&target, "v2", true, None).unwrap().unwrap();
        let b2 = replace_file(&target, "v3", true, None).unwrap().unwrap();
        assert_ne!(b1, b2, "one backup per distinct content");
        assert_eq!(fs::read_to_string(&b1).unwrap(), "v1");
        assert_eq!(fs::read_to_string(&b2).unwrap(), "v2");
        assert_eq!(fs::read_to_string(&target).unwrap(), "v3");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_existing_backup_of_the_same_content_is_never_overwritten() {
        let d = scratch("keep");
        let target = d.join("g.json");
        fs::write(&target, "orig").unwrap();
        let bak = backup_path(&target, b"orig");
        fs::write(&bak, "orig").unwrap();
        let before = fs::metadata(&bak).unwrap().modified().unwrap();
        let made = replace_file(&target, "new", true, None).unwrap().unwrap();
        assert_eq!(made, bak);
        assert_eq!(fs::read_to_string(&bak).unwrap(), "orig");
        assert_eq!(fs::metadata(&bak).unwrap().modified().unwrap(), before);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_backup_name_taken_by_other_content_stops_the_write() {
        let d = scratch("taken");
        let target = d.join("g.json");
        fs::write(&target, "orig").unwrap();
        let bak = backup_path(&target, b"orig");
        fs::write(&bak, "not the original").unwrap();
        let err = replace_file(&target, "new", true, None).unwrap_err();
        assert!(err.to_string().contains("holds other content"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "orig");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_file_that_vanished_after_it_was_read_is_not_recreated() {
        let d = scratch("gone");
        let target = d.join("g.json");
        let err = replace_file(&target, "new", true, Some(b"what we read")).unwrap_err();
        assert!(err.to_string().contains("gone since it was read"), "{err}");
        assert!(!target.exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_file_changed_since_it_was_read_is_not_written() {
        let d = scratch("changed");
        let target = d.join("g.json");
        fs::write(&target, "someone else's version").unwrap();
        let err = replace_file(&target, "new", true, Some(b"what we read")).unwrap_err();
        assert!(
            err.to_string().contains("changed since it was read"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "someone else's version"
        );
        assert_eq!(
            names(&d),
            vec!["g.json".to_string()],
            "no backup, no temporary"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_writers_of_one_file_do_not_share_the_temporary() {
        let d = scratch("race");
        let target = d.join("g.json");
        fs::write(&target, "old").unwrap();
        // whatever the other run's process id is, the temporary has one name
        let tmp = d.join(".g.json.gamedata-doctor-tmp");
        fs::write(&tmp, "half-written by another run").unwrap();
        let err = replace_file(&target, "new", false, None).unwrap_err();
        assert!(err.to_string().contains("another gamedata-doctor"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(&tmp).unwrap(),
            "half-written by another run",
            "the other run's file is left alone"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_target_is_created_without_a_backup() {
        let d = scratch("create");
        let target = d.join("fresh.json");
        let made = replace_file(&target, "x", true, None).unwrap();
        assert!(made.is_none());
        assert_eq!(fs::read_to_string(&target).unwrap(), "x");
        assert_eq!(names(&d), vec!["fresh.json".to_string()]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_failed_write_touches_nothing() {
        let d = scratch("fail");
        let target = d.join("no-such-dir").join("gamedata.json");
        let err = replace_file(&target, "new", false, None).unwrap_err();
        assert!(err.to_string().contains("create"), "{err}");
        assert!(!target.exists());
        assert!(
            fs::read_dir(&d).unwrap().next().is_none(),
            "nothing created"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn permission_bits_survive_the_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("perm");
        let target = d.join("gamedata.json");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        replace_file(&target, "new", false, None).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        fs::remove_dir_all(&d).unwrap();
    }
}
