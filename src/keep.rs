//! Keeping a build for later. An update overwrites the binaries it replaces, and every
//! comparison needs the build before it. The tool reads nothing but the libraries, so the game
//! itself (tens of gigabytes) does not have to be kept: `keep` copies the `.so` / `.dll` files
//! and `steam.inf` into a folder named after the version, and that folder then works wherever a
//! game directory does.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Every `.so` and `.dll` under a directory (a game install or a depot download), without the
/// `addons` folders: Metamod ships a loader shim that is called `libserver.so` too.
pub fn library_files(dir: &Path) -> Vec<PathBuf> {
    walk(dir, None).0
}

/// As `library_files`, without the folder `skip`, and with the number of folders and entries
/// that could not be read.
fn walk(dir: &Path, skip: Option<&Path>) -> (Vec<PathBuf>, usize) {
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            unreadable += 1;
            continue;
        };
        for e in rd {
            let Ok(e) = e else {
                unreadable += 1;
                continue;
            };
            let Ok(ft) = e.file_type() else {
                unreadable += 1;
                continue;
            };
            let path = e.path();
            if ft.is_dir() {
                let skipped = skip.is_some_and(|s| path.canonicalize().is_ok_and(|p| p == s));
                if e.file_name() != "addons" && !skipped {
                    stack.push(path);
                }
            } else if ft.is_file() {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                if name.ends_with(".so") || name.ends_with(".dll") {
                    out.push(path);
                }
            }
        }
    }
    (out, unreadable)
}

/// The `steam.inf` of a game directory (the directory itself, `csgo/`, `game/csgo/`) and the
/// version it states: `PatchVersion-ServerVersion`, e.g. `1.41.9.7-2000943`.
pub fn version_of(game: &Path) -> Option<(PathBuf, String)> {
    for rel in ["steam.inf", "csgo/steam.inf", "game/csgo/steam.inf"] {
        let p = game.join(rel);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(key))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let patch = field("PatchVersion=")?;
        let name = match field("ServerVersion=") {
            Some(build) => format!("{patch}-{build}"),
            None => patch,
        };
        return Some((p, name));
    }
    None
}

pub struct Kept {
    pub dir: PathBuf,
    pub files: usize,
    pub bytes: u64,
    /// The folder was there already; nothing was copied.
    pub already: bool,
}

/// Copy the libraries of `game` into `store/<version>-<os>`. `only`: library names as a
/// gamedata file spells them (`server`, `engine`); empty means every library. `name` overrides
/// the folder name, and is required when the directory has no `steam.inf`.
pub fn keep(game: &Path, store: &Path, only: &[String], name: Option<&str>) -> Result<Kept> {
    if !game.is_dir() {
        bail!("{}: not a directory", game.display());
    }
    let inf = version_of(game);
    // a store inside the game directory must not be copied into itself
    std::fs::create_dir_all(store).with_context(|| format!("creating {}", store.display()))?;
    let store_real = store.canonicalize().ok();
    let (mut files, unreadable) = walk(game, store_real.as_deref());
    if unreadable > 0 {
        bail!(
            "{}: {unreadable} folder(s) or entries could not be read; a copy made now would be short of what is there",
            game.display()
        );
    }
    files.sort();
    if !only.is_empty() {
        files.retain(|f| {
            let lib = crate::sig::library_of(&f.to_string_lossy().to_ascii_lowercase());
            only.iter().any(|o| o.eq_ignore_ascii_case(&lib))
        });
    }
    if files.is_empty() {
        bail!("{}: no libraries found", game.display());
    }
    let ext = |e: &str| {
        files
            .iter()
            .any(|f| f.to_string_lossy().to_ascii_lowercase().ends_with(e))
    };
    let os = match (ext(".so"), ext(".dll")) {
        (true, false) => "linux",
        (false, true) => "windows",
        _ => "mixed",
    };
    let folder = match (name, &inf) {
        (Some(n), _) => n.to_string(),
        (None, Some((_, v))) => format!("{v}-{os}"),
        (None, None) => bail!(
            "{}: no steam.inf here, say what to call this build with --name",
            game.display()
        ),
    };
    if folder.is_empty() || folder.contains(['/', '\\']) || folder.starts_with('.') {
        bail!("{folder}: not a usable folder name");
    }
    let dir = store.join(&folder);
    let count = |d: &Path| {
        let f = library_files(d);
        let bytes = f
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        (f.len(), bytes)
    };
    let mut sources = files;
    if let Some((p, _)) = &inf {
        sources.push(p.clone());
    }
    if dir.exists() {
        same_build(game, &dir, &sources)?;
        let (n, bytes) = count(&dir);
        return Ok(Kept {
            dir,
            files: n,
            bytes,
            already: true,
        });
    }
    // into a side folder first: a copy cut short must not look like a kept build. The folder
    // is this run's own: a second run at the same time gets another, and neither removes the
    // other's.
    let partial = store.join(format!(".{folder}.{}.partial", std::process::id()));
    std::fs::create_dir(&partial).with_context(|| {
        format!(
            "creating {} (left by an interrupted run? remove it)",
            partial.display()
        )
    })?;
    for src in &sources {
        let rel = src.strip_prefix(game).unwrap_or(src);
        let dst = partial.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(e) = std::fs::copy(src, &dst) {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e).with_context(|| format!("copying {}", src.display()));
        }
    }
    let lost = match std::fs::rename(&partial, &dir) {
        Ok(()) => false,
        Err(e) => {
            // another run put a folder of this name there first: ours is not needed, and what
            // is there has to be this very build, like any folder found at the start
            let _ = std::fs::remove_dir_all(&partial);
            if !dir.exists() {
                return Err(e).with_context(|| format!("moving the copy to {}", dir.display()));
            }
            same_build(game, &dir, &sources)?;
            true
        }
    };
    let (n, bytes) = count(&dir);
    Ok(Kept {
        dir,
        files: n,
        bytes,
        already: lost,
    })
}

/// `dir` holds exactly what a copy of `sources` (paths under `game`) would: every file there
/// and equal byte for byte, and no library besides. Anything else under that name is another
/// build or a short copy, and comparing against it would mislead.
fn same_build(game: &Path, dir: &Path, sources: &[PathBuf]) -> Result<()> {
    let mut expected: Vec<PathBuf> = Vec::new();
    for src in sources {
        let rel = src.strip_prefix(game).unwrap_or(src);
        let kept = dir.join(rel);
        let same = match (std::fs::read(src), std::fs::read(&kept)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            bail!(
                "{} is there, but {} in it is missing or differs from the game's: another build under this name? Move it away or give --name",
                dir.display(),
                rel.display()
            );
        }
        expected.push(kept);
    }
    let (have, unreadable) = walk(dir, None);
    if unreadable > 0 {
        bail!("{}: part of it could not be read", dir.display());
    }
    if let Some(extra) = have.iter().find(|f| !expected.contains(f)) {
        bail!(
            "{} holds {}, which this build of the game does not have: another build under this name? Move it away or give --name",
            dir.display(),
            extra.strip_prefix(dir).unwrap_or(extra).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("gamedata-doctor-keep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn only_the_libraries_and_the_version_file_are_kept() {
        let root = scratch("a");
        let game = root.join("game");
        put(
            &game,
            "csgo/steam.inf",
            "ClientVersion=7\nServerVersion=2000943\nPatchVersion=1.41.9.7\n",
        );
        put(&game, "csgo/bin/linuxsteamrt64/libserver.so", "server");
        put(&game, "bin/linuxsteamrt64/libengine2.so", "engine");
        put(&game, "csgo/addons/metamod/bin/libserver.so", "shim");
        put(&game, "csgo/pak01_000.vpk", "ten gigabytes");
        let store = root.join("store");

        let k = keep(&game, &store, &[], None).unwrap();
        assert!(!k.already);
        assert_eq!(k.files, 2);
        assert_eq!(k.dir, store.join("1.41.9.7-2000943-linux"));
        assert!(k.dir.join("csgo/steam.inf").is_file());
        assert!(k.dir.join("csgo/bin/linuxsteamrt64/libserver.so").is_file());
        assert!(!k.dir.join("csgo/addons").exists());
        assert!(!k.dir.join("csgo/pak01_000.vpk").exists());
        // the kept folder states the same version as the game did
        assert_eq!(version_of(&k.dir).unwrap().1, "1.41.9.7-2000943");

        // a second call leaves the first copy alone
        let again = keep(&game, &store, &[], None).unwrap();
        assert!(again.already);
        // the same version name over other files is not "already kept"
        put(&game, "bin/linuxsteamrt64/libengine2.so", "a later build");
        let err = keep(&game, &store, &[], None).err().expect("must refuse");
        assert!(err.to_string().contains("differs"), "{err}");
        assert_eq!(
            std::fs::read_to_string(k.dir.join("bin/linuxsteamrt64/libengine2.so")).unwrap(),
            "engine"
        );
        // no side folder is left behind
        let left: Vec<_> = std::fs::read_dir(&store)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".partial"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_chosen_library_and_a_chosen_name() {
        let root = scratch("b");
        let game = root.join("game");
        put(&game, "csgo/bin/win64/server.dll", "server");
        put(&game, "bin/win64/engine2.dll", "engine");
        let store = root.join("store");
        assert!(
            keep(&game, &store, &[], None).is_err(),
            "no steam.inf, no name"
        );
        let k = keep(&game, &store, &["engine".to_string()], Some("before")).unwrap();
        assert_eq!(k.files, 1);
        assert!(k.dir.join("bin/win64/engine2.dll").is_file());
        assert!(keep(&game, &store, &[], Some("../x")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_store_inside_the_game_is_not_copied_into_itself() {
        let root = scratch("c");
        let game = root.join("game");
        put(
            &game,
            "csgo/steam.inf",
            "ServerVersion=1\nPatchVersion=1.0\n",
        );
        put(&game, "bin/libengine2.so", "engine");
        let store = game.join("kept");
        let first = keep(&game, &store, &[], None).unwrap();
        assert_eq!(first.files, 1);
        // a second build: the first snapshot under the game folder is not part of it
        put(
            &game,
            "csgo/steam.inf",
            "ServerVersion=2\nPatchVersion=1.0\n",
        );
        let second = keep(&game, &store, &[], None).unwrap();
        assert!(!second.already);
        assert_eq!(second.files, 1, "only the game's own library");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_of_the_name_is_this_build_only_if_it_holds_the_same_files_and_no_more() {
        let root = scratch("d");
        let game = root.join("game");
        put(&game, "bin/libengine2.so", "engine");
        put(&game, "bin/libtier0.so", "tier0");
        let sources = vec![game.join("bin/libengine2.so"), game.join("bin/libtier0.so")];
        // what a run that lost the race for the name finds there: checked the same way
        let dir = root.join("store/v1");
        put(&dir, "bin/libengine2.so", "engine");
        assert!(same_build(&game, &dir, &sources).is_err(), "a short copy");
        put(&dir, "bin/libtier0.so", "tier0");
        assert!(same_build(&game, &dir, &sources).is_ok());
        put(&dir, "bin/libextra.so", "from another build");
        let err = same_build(&game, &dir, &sources).unwrap_err();
        assert!(err.to_string().contains("libextra.so"), "{err}");
        std::fs::remove_file(dir.join("bin/libextra.so")).unwrap();
        put(&dir, "bin/libtier0.so", "other bytes");
        assert!(same_build(&game, &dir, &sources).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
