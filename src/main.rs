use anyhow::Result;
use clap::{Parser, Subcommand};
use gamedata_doctor::elf::{Image, Target};
use gamedata_doctor::keep::library_files;
use gamedata_doctor::{report, rtti};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "gamedata-doctor",
    version,
    about = "What a CS2 update broke in your plugin: gamedata offsets and signatures, SDK headers"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the classes and vtable sizes of one binary
    Dump {
        bin: PathBuf,
        /// Print the slots of this class
        #[arg(long)]
        class: Option<String>,
    },
    /// Compare the vtables of two builds and map gamedata offsets old -> new
    Diff {
        old: PathBuf,
        new: PathBuf,
        /// gamedata.json files whose `offsets` entries should be mapped (repeatable)
        #[arg(long = "gamedata")]
        gamedata: Vec<PathBuf>,
        /// Which offsets field to read from gamedata: linux or windows; by default the one the
        /// binaries carry (ELF = linux, PE = windows)
        #[arg(long)]
        os: Option<String>,
        /// List every changed class, not only the first 30
        #[arg(long)]
        all: bool,
        /// Entry -> class map for gamedata whose names do not carry the class (`Teleport CBaseEntity` per line)
        #[arg(long = "class-map")]
        class_map: Option<PathBuf>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Write what the tool is sure about into a copy of the gamedata file: the indices that
    /// moved and new signatures for the broken ones. Everything else is left untouched and
    /// listed for a human. Two files, or two game directories for every library
    Apply {
        old: PathBuf,
        new: PathBuf,
        /// gamedata file to update
        #[arg(long)]
        gamedata: PathBuf,
        /// where to write the updated file (required unless --in-place)
        #[arg(long)]
        out: Option<PathBuf>,
        /// overwrite the gamedata file itself; the original is kept next to it as <file>.<hash>.bak
        #[arg(long)]
        in_place: bool,
        /// let --out replace a file that exists (no backup is made)
        #[arg(long)]
        force: bool,
        /// what may be written: strong (default), historical (also the signature levels that
        /// were never wrong on the history the tool was tuned on) or likely; a guess never
        #[arg(long, default_value = "strong")]
        level: String,
        /// Entry -> class map for gamedata whose names do not carry the class
        #[arg(long = "class-map")]
        class_map: Option<PathBuf>,
        /// all or nothing: unless every entry of the file was checked and none is left to a
        /// person, unproven or unchecked, write nothing and exit with status 2
        #[arg(long)]
        strict: bool,
        /// Machine-readable summary
        #[arg(long)]
        json: bool,
    },
    /// Copy the libraries of a game directory (nothing else: a few hundred megabytes, not the
    /// game) into a folder named after its version, to compare a later build against. The
    /// folder works wherever a game directory does
    Keep {
        game: PathBuf,
        /// where the kept builds live; the copy goes to <store>/<version>-<os>
        #[arg(long)]
        store: PathBuf,
        /// keep only these libraries (as a gamedata file names them: server, engine, tier0);
        /// repeatable. Default: every library
        #[arg(long = "lib")]
        libs: Vec<String>,
        /// folder name instead of the version from steam.inf
        #[arg(long)]
        name: Option<String>,
    },
    /// SDK headers against two builds: classes whose table no longer is what the headers declare
    /// (slots moved, removed, inserted, or the class gone). Two files, or two game directories
    /// for every library found in both
    Sdk {
        old: PathBuf,
        new: PathBuf,
        /// root of the SDK headers (hl2sdk / s2sdk checkout)
        #[arg(long)]
        sdk: PathBuf,
        /// also list the classes that could not be checked
        #[arg(long)]
        all: bool,
        /// print what the headers declare for this class, slot by slot, and stop
        #[arg(long)]
        class: Option<String>,
        /// the source of a plugin built on this SDK (repeatable): under every finding, where
        /// that source calls a function that moved or names a class that is gone
        #[arg(long)]
        source: Vec<PathBuf>,
        /// with --source: list every use, the uncertain ones too
        #[arg(long)]
        verbose: bool,
        /// write a unified diff for the SDK, as a proposal to review: a placeholder where a
        /// slot appeared, the declaration out where a slot vanished (only where the place is
        /// proven by exact fingerprints on both sides)
        #[arg(long)]
        patch: Option<PathBuf>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Follow the byte signatures of a gamedata file from the old build to the new one: which
    /// still match, which broke, and a new signature for every broken one whose function was found
    Sigs {
        old: PathBuf,
        new: PathBuf,
        /// gamedata file with the signatures
        #[arg(long)]
        gamedata: PathBuf,
        /// write a copy of the gamedata with the regenerated signatures
        #[arg(long)]
        out: Option<PathBuf>,
        /// let --out replace a file that exists
        #[arg(long)]
        force: bool,
        /// evidence a regenerated signature needs to be written: strong (exact fingerprint or
        /// own strings), historical (also the levels never wrong in the history) or likely (all)
        #[arg(long, default_value = "strong")]
        level: String,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// History check for signatures: predict from the gamedata BEFORE an update, compare with
    /// the signatures the maintainers committed AFTER it
    ValidateSigs {
        old: PathBuf,
        new: PathBuf,
        #[arg(long)]
        before: PathBuf,
        #[arg(long)]
        after: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Check the tool against history: predict from the gamedata BEFORE an update and compare
    /// with what the maintainers committed AFTER it
    Validate {
        old: PathBuf,
        new: PathBuf,
        /// gamedata file as it was before the update
        #[arg(long)]
        before: PathBuf,
        /// gamedata file as the maintainers fixed it after the update
        #[arg(long)]
        after: PathBuf,
        /// Entry -> class map for gamedata whose names do not carry the class (`Teleport CBaseEntity` per line)
        #[arg(long = "class-map")]
        class_map: Option<PathBuf>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    // `gamedata-doctor dump big.so | head` must end quietly, not with a broken-pipe panic
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match Cli::parse().cmd {
        Cmd::Dump { bin, class } => dump(&bin, class.as_deref()),
        Cmd::Diff {
            old,
            new,
            gamedata,
            os,
            all,
            class_map,
            json,
        } => diff(
            &old,
            &new,
            &gamedata,
            class_map.as_deref(),
            os.as_deref(),
            all,
            json,
        ),
        Cmd::Apply {
            old,
            new,
            gamedata,
            out,
            in_place,
            force,
            level,
            class_map,
            strict,
            json,
        } => apply(
            &old,
            &new,
            &gamedata,
            out.as_deref(),
            in_place,
            force,
            &level,
            class_map.as_deref(),
            strict,
            json,
        ),
        Cmd::Validate {
            old,
            new,
            before,
            after,
            class_map,
            json,
        } => validate(&old, &new, &before, &after, class_map.as_deref(), json),
        Cmd::Keep {
            game,
            store,
            libs,
            name,
        } => {
            let k = gamedata_doctor::keep::keep(&game, &store, &libs, name.as_deref())?;
            eprintln!(
                "{}: {} librar{}, {:.0} MB{}",
                if k.already { "already kept" } else { "kept" },
                k.files,
                if k.files == 1 { "y" } else { "ies" },
                k.bytes as f64 / 1048576.0,
                if k.already { " (left as it was)" } else { "" }
            );
            // the folder alone on stdout: OLD=$(gamedata-doctor keep ...)
            println!("{}", k.dir.display());
            Ok(())
        }
        Cmd::Sdk {
            old,
            new,
            sdk,
            all,
            class,
            source,
            verbose,
            patch,
            json,
        } => sdk_check(
            &old,
            &new,
            &sdk,
            all,
            class.as_deref(),
            &source,
            verbose,
            patch.as_deref(),
            json,
        ),
        Cmd::Sigs {
            old,
            new,
            gamedata,
            out,
            force,
            level,
            json,
        } => sigs(&old, &new, &gamedata, out.as_deref(), force, &level, json),
        Cmd::ValidateSigs {
            old,
            new,
            before,
            after,
            json,
        } => {
            let (o, n) = (Image::load(&old)?, Image::load(&new)?);
            let v = gamedata_doctor::sig::validate(
                &o,
                &n,
                &gamedata_doctor::gamedata::load_sigs(&before)?,
                &gamedata_doctor::gamedata::load_sigs(&after)?,
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for r in &v.rows {
                    println!(
                        "  {:9} {:11} {:44} {}",
                        format!("{:?}", r.outcome),
                        r.confidence,
                        r.entry,
                        r.verdict
                    );
                    if r.outcome != gamedata_doctor::sig::Outcome::Correct {
                        println!("            {}", r.debug);
                    }
                }
                for (e, why) in &v.skipped {
                    println!("  skipped   {e:44} {why}");
                }
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sdk_check(
    old_p: &Path,
    new_p: &Path,
    sdk: &Path,
    all: bool,
    class: Option<&str>,
    source: &[PathBuf],
    verbose: bool,
    patch: Option<&Path>,
    json: bool,
) -> Result<()> {
    use gamedata_doctor::sdkcheck::Verdict;
    if let Some(class) = class {
        for os in ["linux", "windows"] {
            let h = gamedata_doctor::sdk::Headers::load(sdk, os)?;
            match (h.classes.get(class), h.layout(class, os)) {
                (Some(c), Some(l)) => {
                    println!(
                        "{class} ({os}): {} slot(s), {}:{}{}",
                        l.slots.len(),
                        c.file,
                        c.line,
                        if l.exact { "" } else { ", order not certain" }
                    );
                    for (i, s) in l.slots.iter().enumerate() {
                        println!("  [{i:3}] {s}");
                    }
                }
                _ => println!("{class} ({os}): not in the headers, or no virtual functions"),
            }
        }
        return Ok(());
    }
    let pairs = library_pairs(old_p, new_p)?;
    let none = std::collections::BTreeMap::new();
    let (out, present, ok, libs, files) = sdk_rows(&pairs, sdk, &none)?;
    if libs == 0 {
        anyhow::bail!(
            "nothing to check: {} pair(s) of libraries found in the two paths, {} header file(s) read under {}, and no class the headers declare is in those libraries",
            pairs.len(),
            files,
            sdk.display()
        );
    }
    let uses = if source.is_empty() {
        None
    } else {
        Some(source_uses(&out, sdk, source)?)
    };
    if json {
        let rows: Vec<serde_json::Value> = out
            .iter()
            .map(|(lib, r)| {
                let mut v = serde_json::to_value(r).unwrap_or_default();
                v["library"] = serde_json::Value::String(lib.clone());
                if let Some(u) = uses.as_ref().and_then(|u| u.by_class.get(&r.class)) {
                    v["uses"] = serde_json::to_value(u).unwrap_or_default();
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        // one line per finding; the same class in several libraries is one finding
        let mut lines: Vec<(Verdict, String, Vec<String>, Vec<String>)> = Vec::new();
        for (lib, r) in &out {
            if r.verdict == Verdict::Unchecked && !all {
                continue;
            }
            let sizes = match (r.old_slots, r.new_slots) {
                (Some(a), Some(b)) => format!(" [{a} -> {b} slots]"),
                _ => String::new(),
            };
            let via = match &r.implementation {
                Some(i) if *i != r.class => {
                    format!(" via {}", i.chars().take(48).collect::<String>())
                }
                _ => String::new(),
            };
            let text = format!("{}{via}{sizes}: {} ({})", r.class, r.detail, r.header);
            // a class that is gone is one finding, whatever implemented it in each library
            let gone = format!("{}:", r.class);
            let same = |l: &(Verdict, String, Vec<String>, Vec<String>)| {
                l.0 == r.verdict
                    && (l.1 == text || (r.verdict == Verdict::Gone && l.1.starts_with(&gone)))
            };
            match lines.iter_mut().find(|l| same(l)) {
                Some(l) => l.2.push(lib.clone()),
                None => {
                    let mut extra = fix_lines(r);
                    if let Some(u) = &uses {
                        extra.extend(use_lines(u, &r.class, verbose));
                    }
                    lines.push((r.verdict, text, vec![lib.clone()], extra))
                }
            }
        }
        lines.sort_by_key(|l| std::cmp::Reverse(l.0));
        for (v, text, libs, fix) in &lines {
            let mark = match v {
                Verdict::Gone => "GONE     ",
                Verdict::Broken => "BROKEN   ",
                Verdict::Suspect => "suspect  ",
                Verdict::Unchecked => "unchecked",
                Verdict::Ok => "ok       ",
            };
            println!("{mark} {text} [{}]", libs.join(", "));
            for f in fix {
                println!("            {f}");
            }
        }
        let count = |v: Verdict| out.iter().filter(|r| r.1.verdict == v).count();
        println!(
            "{} header class(es) found in {} librar{}: {} unchanged, {} BROKEN, {} GONE, {} suspect, {} not checkable ({} header file(s) read)",
            present,
            libs,
            if libs == 1 { "y" } else { "ies" },
            ok,
            count(Verdict::Broken),
            count(Verdict::Gone),
            count(Verdict::Suspect),
            count(Verdict::Unchecked),
            files
        );
        if let Some(u) = &uses {
            let touched = u.by_class.values().filter(|v| !v.is_empty()).count();
            println!(
                "source: {} file(s) read; {} of the {} broken or gone class(es) are used there",
                u.files,
                touched,
                u.by_class.len()
            );
        }
    }
    if let Some(patch) = patch {
        let edits: Vec<gamedata_doctor::sdkfix::Edit> = out
            .iter()
            .filter_map(|(_, r)| r.fix.as_ref())
            .filter(|f| f.certain)
            .flat_map(|f| f.edits.iter().cloned())
            .collect();
        let broken = |rows: &SdkRows| {
            let mut names: Vec<String> = rows
                .iter()
                .filter(|r| r.1.verdict == Verdict::Broken)
                .map(|r| r.1.class.clone())
                .collect();
            names.sort();
            names.dedup();
            names
        };
        let before = broken(&out);
        if edits.is_empty() {
            eprintln!(
                "no patch written: none of the {} broken class(es) has a fix with a proven place",
                before.len()
            );
        } else {
            let p = gamedata_doctor::sdkfix::patch(sdk, &edits)?;
            std::fs::write(patch, &p.diff)?;
            let (again, ..) = sdk_rows(&pairs, sdk, &p.files)?;
            let after = broken(&again);
            let cured: Vec<&String> = before.iter().filter(|c| !after.contains(c)).collect();
            eprintln!(
                "wrote {}, a proposal to review: {} edit(s) in {} header(s); with it {} of {} broken class(es) have a header of the new table's size{}. Build what uses the headers and run it before trusting the patch",
                patch.display(),
                p.edits,
                p.files.len(),
                cured.len(),
                before.len(),
                if after.is_empty() {
                    String::new()
                } else {
                    format!("; still broken: {}", after.join(", "))
                }
            );
        }
    }
    Ok(())
}

struct SourceUses {
    by_class: std::collections::BTreeMap<String, Vec<gamedata_doctor::uses::Use>>,
    files: usize,
}

/// Where the plugin source touches each broken or gone class of the findings.
fn source_uses(rows: &SdkRows, sdk: &Path, source: &[PathBuf]) -> Result<SourceUses> {
    use gamedata_doctor::sdkcheck::Verdict;
    use gamedata_doctor::uses::Needle;
    let h = gamedata_doctor::sdk::Headers::load(sdk, "linux")?;
    let plain = |s: &str| s.split(' ').next().unwrap_or(s).to_string();
    let mut needles: Vec<Needle> = Vec::new();
    let mut family: Vec<String> = Vec::new();
    for (_, r) in rows {
        if !matches!(r.verdict, Verdict::Broken | Verdict::Gone)
            || needles.iter().any(|n| n.class == r.class)
        {
            continue;
        }
        let layout = h.layout(&r.class, "linux");
        let moved: Vec<String> = r
            .moved
            .iter()
            .filter(|m| !m.method.starts_with('#'))
            .map(|m| plain(&m.method))
            .collect();
        // without a list of moved slots (a class that is gone, a table that only shrank) any
        // of its functions may be the one
        let mut methods = match (&layout, moved.is_empty()) {
            (Some(l), true) => l.slots.iter().map(|s| plain(s)).collect(),
            _ => moved,
        };
        methods.retain(|m| !m.starts_with('~'));
        methods.sort();
        methods.dedup();
        family.push(r.class.clone());
        if let Some(l) = &layout {
            family.extend(l.decl.iter().map(|d| d.class.clone()));
        }
        needles.push(Needle {
            class: r.class.clone(),
            methods,
            shared: Vec::new(),
        });
    }
    for n in &mut needles {
        n.shared = n
            .methods
            .iter()
            .filter(|m| {
                h.classes
                    .values()
                    .any(|c| !family.contains(&c.name) && c.methods.iter().any(|x| &x.name == *m))
            })
            .cloned()
            .collect();
    }
    let (found, files) = gamedata_doctor::uses::scan(source, sdk, &needles);
    let mut by_class: std::collections::BTreeMap<String, Vec<gamedata_doctor::uses::Use>> = needles
        .iter()
        .map(|n| (n.class.clone(), Vec::new()))
        .collect();
    for (i, u) in found {
        by_class
            .entry(needles[i].class.clone())
            .or_default()
            .push(u);
    }
    Ok(SourceUses { by_class, files })
}

/// The uses of one class, as lines for a person. Everything here is a candidate found in
/// text: the uncertain kinds are counted, and listed only when asked.
fn use_lines(u: &SourceUses, class: &str, verbose: bool) -> Vec<String> {
    use gamedata_doctor::uses::Kind;
    let Some(list) = u.by_class.get(class) else {
        return Vec::new();
    };
    if list.is_empty() {
        return vec!["in the source: no use found".to_string()];
    }
    let count = |k: Kind| list.iter().filter(|x| x.kind == k).count();
    let mut out = vec![format!(
        "in the source (text search, candidates): {} with the class spelled out, {} call(s) by a name only this class has, {} by a name other classes have too, {} mention(s) of the class",
        count(Kind::Hook),
        count(Kind::Call),
        count(Kind::Maybe),
        count(Kind::Mention)
    )];
    let shown: Vec<&gamedata_doctor::uses::Use> = list
        .iter()
        .filter(|x| verbose || matches!(x.kind, Kind::Hook | Kind::Call))
        .collect();
    let limit = if verbose { shown.len() } else { 6 };
    for x in shown.iter().take(limit) {
        out.push(format!(
            "  {}:{}  {}{}",
            x.file,
            x.line,
            x.what,
            match x.kind {
                Kind::Hook => "  (with the class)",
                Kind::Call => "",
                Kind::Maybe => "  (maybe)",
                Kind::Mention => "  (the class)",
            }
        ));
    }
    if !verbose && list.len() > limit.min(shown.len()) {
        out.push(format!(
            "  ... {} more (--verbose lists all)",
            list.len() - limit.min(shown.len())
        ));
    }
    out
}

/// The header edits of one finding, as lines for a person.
fn fix_lines(r: &gamedata_doctor::sdkcheck::Row) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(f) = &r.fix {
        for e in &f.edits {
            out.push(match e.insert {
                Some(slot) => format!(
                    "fix: a placeholder for new slot {slot} in front of {}::{} ({}:{})",
                    e.class, e.method, e.file, e.line
                ),
                None => format!(
                    "fix: take out {}::{} ({}:{})",
                    e.class, e.method, e.file, e.line
                ),
            });
        }
        if !f.certain && !f.edits.is_empty() {
            out.push(
                "the place is not proven (look-alike neighbours): by hand, not in the patch"
                    .to_string(),
            );
        }
    } else if !r.no_fix.is_empty() {
        out.push(format!("no automatic fix: {}", r.no_fix));
    }
    out
}

type SdkRows = Vec<(String, gamedata_doctor::sdkcheck::Row)>;

/// The check over every pair: the findings with their library, the classes found, the
/// classes unchanged, the libraries that had any, the header files read.
fn sdk_rows(
    pairs: &[(PathBuf, PathBuf)],
    sdk: &Path,
    patched: &std::collections::BTreeMap<String, String>,
) -> Result<(SdkRows, usize, usize, usize, usize)> {
    use gamedata_doctor::sdkcheck::check;
    let mut headers: Option<(&'static str, gamedata_doctor::sdk::Headers)> = None;
    let mut out = Vec::new();
    let (mut present, mut ok, mut libs) = (0usize, 0usize, 0usize);
    for (o, n) in pairs {
        // a directory holds things that are not libraries of the game: skip what does not load
        let (Ok(old), Ok(new)) = (Image::load(o), Image::load(n)) else {
            continue;
        };
        let os = old.format.os();
        if new.format.os() != os {
            continue;
        }
        if headers.as_ref().map(|h| h.0) != Some(os) {
            headers = Some((
                os,
                gamedata_doctor::sdk::Headers::load_patched(sdk, os, patched)?,
            ));
        }
        let h = &headers.as_ref().unwrap().1;
        let c = check(&old, &new, h);
        if c.present == 0 {
            continue;
        }
        libs += 1;
        present += c.present;
        ok += c.ok;
        let lib = gamedata_doctor::sig::library_of(&old.path);
        out.extend(c.rows.into_iter().map(|r| (lib.clone(), r)));
    }
    let files = headers.as_ref().map(|h| h.1.files).unwrap_or(0);
    if let Some(n) = headers.as_ref().map(|h| h.1.unreadable).filter(|n| *n > 0) {
        eprintln!(
            "{n} header file(s) or folder(s) under {} could not be read: the check is short by that much",
            sdk.display()
        );
    }
    Ok((out, present, ok, libs, files))
}

/// What to compare: the two files, or every library file name two directories share.
fn library_pairs(old_p: &Path, new_p: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    if old_p.is_dir() && new_p.is_dir() {
        let new_files = library_files(new_p);
        let mut old_files = library_files(old_p);
        old_files.sort();
        for o in old_files {
            let name = o.file_name().map(|n| n.to_ascii_lowercase());
            // the largest file of that name on the new side, as for signatures
            let n = new_files
                .iter()
                .filter(|f| f.file_name().map(|n| n.to_ascii_lowercase()) == name)
                .max_by_key(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0));
            if let Some(n) = n {
                pairs.push((o, n.clone()));
            }
        }
    } else if old_p.is_dir() || new_p.is_dir() {
        anyhow::bail!("give two files or two directories");
    } else {
        pairs.push((old_p.to_path_buf(), new_p.to_path_buf()));
    }
    Ok(pairs)
}

/// The binary of one gamedata `library` among `files`: the largest file of that name and
/// extension (a game ships small stand-ins of some names beside the real library).
fn pick_library(files: &[PathBuf], lib: &str, ext: &str) -> Option<PathBuf> {
    files
        .iter()
        .filter(|f| {
            let s = f.to_string_lossy().to_ascii_lowercase();
            s.ends_with(ext) && gamedata_doctor::sig::library_of(&s) == lib
        })
        .max_by_key(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
        .cloned()
}

type SigRows = Vec<(&'static str, gamedata_doctor::sig::SigResult)>;
/// Two directories were given; the pairs of binaries to compare; the libraries not found.
type SigPairs = (bool, Vec<(PathBuf, PathBuf)>, Vec<String>);

/// The binaries the signatures of a gamedata file live in: the two files, or for two
/// directories the library of every entry. The flag says "directories".
fn sig_pairs(
    old_p: &Path,
    new_p: &Path,
    entries: &[gamedata_doctor::sig::SigEntry],
) -> Result<SigPairs> {
    let lib_of_entry = |library: &str| {
        if library.is_empty() {
            "server".to_string()
        } else {
            library.to_ascii_lowercase()
        }
    };
    let dirs = old_p.is_dir() && new_p.is_dir();
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    if dirs {
        let (old_files, new_files) = (library_files(old_p), library_files(new_p));
        let mut libs: Vec<String> = Vec::new();
        for e in entries {
            let l = lib_of_entry(&e.library);
            if !libs.contains(&l) {
                libs.push(l);
            }
        }
        for l in &libs {
            let pick = [".so", ".dll"].iter().find_map(|ext| {
                Some((
                    pick_library(&old_files, l, ext)?,
                    pick_library(&new_files, l, ext)?,
                ))
            });
            match pick {
                Some(pair) => pairs.push(pair),
                None => {
                    eprintln!(
                        "library {l}: not found in both directories, {} signature(s) NOT checked",
                        entries
                            .iter()
                            .filter(|e| &lib_of_entry(&e.library) == l)
                            .count()
                    );
                    absent.push(l.clone());
                }
            }
        }
    } else if old_p.is_dir() || new_p.is_dir() {
        anyhow::bail!("give two files or two directories");
    } else {
        pairs.push((old_p.to_path_buf(), new_p.to_path_buf()));
    }
    Ok((dirs, pairs, absent))
}

/// What a person needs to judge one signature: the evidence behind a new one, the candidates
/// for one that was not found.
fn print_sig_detail(r: &gamedata_doctor::sig::SigResult) {
    use gamedata_doctor::sig::Status;
    if let Some(s) = &r.new_sig {
        println!("  {:44}   {s}", "");
    }
    if let (Some(l), Status::Regenerated | Status::Located) = (r.level, r.status) {
        let (right, judged) = gamedata_doctor::sig::history(l);
        println!(
            "  {:44}   evidence {l:?}{}; in the history this level was right {right} of {judged} times",
            "",
            r.similarity
                .map(|v| format!(", likeness {:.0} %", v * 100.0))
                .unwrap_or_default()
        );
    }
    for c in &r.candidates {
        println!(
            "  {:44}   candidate {:#x}, likeness {:.0} %: {}",
            "",
            c.addr,
            c.similarity * 100.0,
            c.signature.as_deref().unwrap_or("(no unique signature)")
        );
    }
}

fn sig_run(
    pairs: &[(PathBuf, PathBuf)],
    dirs: bool,
    entries: &[gamedata_doctor::sig::SigEntry],
    quiet: bool,
) -> Result<SigRows> {
    use gamedata_doctor::sig::{library_of, Status};
    let mut all: SigRows = Vec::new();
    for (o, n) in pairs {
        let t0 = Instant::now();
        let old = Image::load(o)?;
        let new = Image::load(n)?;
        let os = old.format.os();
        if new.format.os() != os {
            anyhow::bail!("the two binaries are for different platforms");
        }
        let an = gamedata_doctor::sig::analyze(&old, &new, entries);
        if !quiet {
            let count = |s: Status| an.results.iter().filter(|r| r.status == s).count();
            println!(
                "{} signatures for {} ({os}): {} still valid, {} regenerated, {} located only, {} conflicts, {} not found, {} unusable in the old build; {:.1}s",
                an.results.iter().filter(|r| r.status != Status::Skipped).count(),
                library_of(&old.path),
                count(Status::StillValid),
                count(Status::Regenerated),
                count(Status::Located),
                count(Status::Conflict),
                count(Status::NotFound),
                count(Status::OldBroken),
                t0.elapsed().as_secs_f32()
            );
            for r in an
                .results
                .iter()
                .filter(|r| r.status != Status::Skipped && r.status != Status::StillValid)
            {
                println!("  {:44} {}", r.entry, r.verdict);
                print_sig_detail(r);
            }
        }
        for r in an.results {
            // with several libraries every entry is "skipped" by all the others
            if !dirs || r.status != Status::Skipped {
                all.push((os, r));
            }
        }
    }
    Ok(all)
}

/// Put the regenerated signatures the trust level allows into the file text. Returns the text
/// and the indices (into `all`) of the rows written and of those held back.
fn sig_write(
    mut text: String,
    all: &SigRows,
    trust: gamedata_doctor::sig::Trust,
) -> Result<(String, Vec<usize>, Vec<usize>)> {
    use gamedata_doctor::sig::Status;
    let (mut written, mut held) = (Vec::new(), Vec::new());
    for (i, (os, r)) in all.iter().enumerate() {
        let (Status::Regenerated, Some(sig), Some(l)) = (r.status, &r.new_sig, r.level) else {
            continue;
        };
        if !trust.allows(l) {
            held.push(i);
            continue;
        }
        match gamedata_doctor::gamedata::rewrite_sig(&text, &r.entry, os, sig) {
            Some(t) => {
                text = t;
                written.push(i);
            }
            None => anyhow::bail!(
                "could not find the {os} signature of {} in the file text",
                r.entry
            ),
        }
    }
    Ok((text, written, held))
}

fn sigs(
    old_p: &Path,
    new_p: &Path,
    gamedata: &Path,
    out: Option<&Path>,
    force: bool,
    level: &str,
    json: bool,
) -> Result<()> {
    let Some(trust) = gamedata_doctor::sig::Trust::parse(level) else {
        anyhow::bail!("--level {level}: use strong, historical or likely");
    };
    if let Some(out) = out {
        refuse_existing(out, force)?;
    }
    let entries = gamedata_doctor::gamedata::load_sigs(gamedata)?;
    let (dirs, pairs, absent) = sig_pairs(old_p, new_p, &entries)?;
    let all = sig_run(&pairs, dirs, &entries, json)?;
    if json {
        let rows: Vec<&gamedata_doctor::sig::SigResult> = all.iter().map(|x| &x.1).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    }
    if let Some(out) = out {
        let text = std::fs::read_to_string(gamedata)?;
        let (text, written, held) = sig_write(text, &all, trust)?;
        gamedata_doctor::fsutil::replace_file(out, &text, false, None)?;
        eprintln!(
            "wrote {}: {} signature(s) replaced at level {level}, {} regenerated but held back for a human",
            out.display(),
            written.len(),
            held.len()
        );
    }
    if !absent.is_empty() {
        // the report above is not the whole file: do not let a script read it as one
        eprintln!(
            "incomplete: the signatures of {} were not checked",
            absent.join(", ")
        );
        std::process::exit(2);
    }
    Ok(())
}

/// One line of the account `apply` gives of a gamedata file: every entry it read ends in
/// exactly one of these.
#[derive(serde::Serialize)]
struct EntryStatus {
    entry: String,
    os: String,
    /// `offset` or `signature`.
    kind: &'static str,
    /// `checked_same`, `written`, `needs_human`, `same_unproven`, `not_checked`, `out_of_scope`.
    status: &'static str,
    detail: String,
}

/// `--out` never replaces a file that is already there unless told to: only `--in-place` makes
/// a backup.
fn refuse_existing(out: &Path, force: bool) -> Result<()> {
    if out.exists() && !force {
        anyhow::bail!(
            "{} exists; --force overwrites it (no backup is made), --in-place keeps one",
            out.display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply(
    old_p: &Path,
    new_p: &Path,
    gamedata: &Path,
    out: Option<&Path>,
    in_place: bool,
    force: bool,
    level: &str,
    class_map: Option<&Path>,
    strict: bool,
    json: bool,
) -> Result<()> {
    use gamedata_doctor::diff::Confidence;
    use gamedata_doctor::sig::{Status, Trust};
    let (max_level, trust) = match level {
        "strong" => (Confidence::Strong, Trust::Strong),
        "historical" => (Confidence::Strong, Trust::Historical),
        "likely" => (Confidence::LikelyByPosition, Trust::Likely),
        other => {
            anyhow::bail!(
                "--level {other}: use strong, historical or likely (a guess is never applied)"
            )
        }
    };
    let target = match (out, in_place) {
        (Some(p), false) => {
            refuse_existing(p, force)?;
            p.to_path_buf()
        }
        (None, true) => gamedata.to_path_buf(),
        (Some(_), true) => anyhow::bail!("--out and --in-place exclude each other"),
        (None, false) => anyhow::bail!(
            "say where to write: --out FILE, or --in-place to overwrite the gamedata file"
        ),
    };
    let map = load_map(class_map, &[gamedata])?;
    let text = std::fs::read_to_string(gamedata)?;
    let mut rewritten = text.clone();
    let mut account: Vec<EntryStatus> = Vec::new();

    // the indices, per platform; in a directory every library answers for the classes it has
    let dirs = old_p.is_dir() && new_p.is_dir();
    let mut by_os: Vec<(&'static str, Vec<report::GamedataResult>)> = Vec::new();
    if !gamedata_doctor::gamedata::load(gamedata)?.is_empty() {
        for (o, n) in library_pairs(old_p, new_p)? {
            let (old, new) = if dirs {
                // a directory holds things that are not libraries of the game; an entry whose
                // class is in none of the libraries that did load is reported as such below
                let (Ok(a), Ok(b)) = (Image::load(&o), Image::load(&n)) else {
                    continue;
                };
                if a.format.os() != b.format.os() {
                    continue;
                }
                (a, b)
            } else {
                (Image::load(&o)?, Image::load(&n)?)
            };
            let os = old.format.os();
            let rep = report::compare(&old, &new, &[gamedata], &map, os)?;
            match by_os.iter_mut().find(|x| x.0 == os) {
                Some(x) => x.1.extend(rep.gamedata),
                None => by_os.push((os, rep.gamedata)),
            }
        }
        if by_os.is_empty() {
            anyhow::bail!(
                "no library of the game could be compared in {} and {}: the indices of {} were not checked",
                old_p.display(),
                new_p.display(),
                gamedata.display()
            );
        }
    }
    let mut plans: Vec<(&'static str, report::ApplyPlan)> = Vec::new();
    for (os, rows) in &by_os {
        let mut merged = report::merge_libraries(rows);
        for r in &mut merged {
            if map.get(&r.entry).map(String::as_str) == Some("-") {
                r.new_index = None;
                r.confidence = None;
                r.kind = report::Kind::OutOfScope;
                r.verdict =
                    "marked `-` in the class map: a field offset, not a virtual-table index"
                        .to_string();
            }
        }
        let plan = report::plan_rows(&merged, max_level);
        let changes: Vec<(String, i64)> = plan
            .applied
            .iter()
            .map(|(e, _, n, _)| (e.clone(), *n))
            .collect();
        let (t, missing) = gamedata_doctor::gamedata::rewrite_os(&rewritten, &changes, os);
        if !missing.is_empty() {
            anyhow::bail!(
                "could not find the {os} index of {} in the file text",
                missing.join(", ")
            );
        }
        rewritten = t;
        let mut put = |entry: &str, status: &'static str, detail: String| {
            account.push(EntryStatus {
                entry: entry.to_string(),
                os: os.to_string(),
                kind: "offset",
                status,
                detail,
            })
        };
        for e in &plan.unchanged_entries {
            put(e, "checked_same", String::new());
        }
        for (e, o, n, c) in &plan.applied {
            put(e, "written", format!("{o} -> {n} ({c:?})"));
        }
        for (e, v) in &plan.needs_human {
            put(e, "needs_human", v.clone());
        }
        for (e, v) in &plan.unconfirmed {
            put(e, "same_unproven", v.clone());
        }
        for (e, v) in &plan.not_checked {
            put(e, "not_checked", v.clone());
        }
        for (e, v) in &plan.out_of_scope {
            put(e, "out_of_scope", v.clone());
        }
        plans.push((os, plan));
    }

    // the signatures: a file whose signatures cannot be read is an error, not "no signatures"
    let entries = gamedata_doctor::gamedata::load_sigs(gamedata)?;
    let mut sig_rows: SigRows = Vec::new();
    let (mut written, mut held) = (Vec::new(), Vec::new());
    let mut platforms: Vec<&'static str> = by_os.iter().map(|x| x.0).collect();
    if !entries.is_empty() {
        let (d, pairs, _) = sig_pairs(old_p, new_p, &entries)?;
        sig_rows = sig_run(&pairs, d, &entries, true)?;
        (rewritten, written, held) = sig_write(rewritten, &sig_rows, trust)?;
        for (o, _) in &pairs {
            let os = if o.to_string_lossy().to_ascii_lowercase().ends_with(".dll") {
                "windows"
            } else {
                "linux"
            };
            if !platforms.contains(&os) {
                platforms.push(os);
            }
        }
        if platforms.is_empty() {
            // nothing was compared at all: every signature the file has is unchecked
            platforms = vec!["linux", "windows"];
        }
    }
    let valid = sig_rows
        .iter()
        .filter(|r| r.1.status == Status::StillValid)
        .count();
    let sig_human: Vec<usize> = (0..sig_rows.len())
        .filter(|i| {
            held.contains(i)
                || matches!(
                    sig_rows[*i].1.status,
                    Status::Located | Status::Conflict | Status::NotFound | Status::OldBroken
                )
        })
        .collect();
    for (i, (os, r)) in sig_rows.iter().enumerate() {
        let status = if written.contains(&i) {
            "written"
        } else if sig_human.contains(&i) {
            "needs_human"
        } else if r.status == Status::StillValid {
            "checked_same"
        } else {
            continue; // skipped here: accounted for below
        };
        account.push(EntryStatus {
            entry: r.entry.clone(),
            os: os.to_string(),
            kind: "signature",
            status,
            detail: r.verdict.clone(),
        });
    }
    // every signature the file carries for a platform in play, that no library answered for
    let mut sig_unchecked: Vec<(String, &'static str, String)> = Vec::new();
    for e in &entries {
        for os in &platforms {
            let sig = if *os == "windows" {
                &e.windows
            } else {
                &e.linux
            };
            let Some(sig) = sig else { continue };
            if sig.trim_start().starts_with('@') {
                account.push(EntryStatus {
                    entry: e.name.clone(),
                    os: os.to_string(),
                    kind: "signature",
                    status: "out_of_scope",
                    detail: "a symbol name, not a byte signature".to_string(),
                });
                continue;
            }
            let answered = sig_rows
                .iter()
                .any(|(o, r)| o == os && r.entry == e.name && r.status != Status::Skipped);
            if !answered {
                let lib = if e.library.is_empty() {
                    "server"
                } else {
                    &e.library
                };
                sig_unchecked.push((
                    e.name.clone(),
                    os,
                    format!(
                        "library {lib} was not compared (not given, or not found on both sides)"
                    ),
                ));
            }
        }
    }
    for (e, os, why) in &sig_unchecked {
        account.push(EntryStatus {
            entry: e.clone(),
            os: os.to_string(),
            kind: "signature",
            status: "not_checked",
            detail: why.clone(),
        });
    }

    let count = |s: &str| account.iter().filter(|a| a.status == s).count();
    let (humans, unproven, unchecked) = (
        count("needs_human"),
        count("same_unproven"),
        count("not_checked"),
    );
    // complete: nothing the file asks about was left without a verdict the tool stands behind
    let complete = humans + unproven + unchecked == 0;
    // a strict run that is not complete changes nothing on disk: the status must not arrive
    // after the file was already replaced
    let write = complete || !strict;
    // in place: the original is kept as <file>.<hash>.bak and the file must still be what was read;
    // either way the write is temp + rename, so a crash leaves the old file or the new one
    let backup = if write {
        gamedata_doctor::fsutil::replace_file(
            &target,
            &rewritten,
            in_place,
            in_place.then_some(text.as_bytes()),
        )?
    } else {
        None
    };
    if json {
        let doc = serde_json::json!({
            "schema_version": 2,
            "file": target.display().to_string(),
            "level": level,
            "complete": complete,
            "file_written": write,
            "entries": account,
            "signatures": sig_rows
                .iter()
                .filter(|(_, r)| r.status != Status::Skipped)
                .map(|(os, r)| {
                    let mut v = serde_json::to_value(r).unwrap_or_default();
                    v["os"] = serde_json::Value::String(os.to_string());
                    v
                })
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        println!(
            "{} {}: {} index(es) and {} signature(s) changed at level {level}; {} index(es) unchanged, {} signature(s) still valid; {humans} need a human, {unproven} index(es) look unmoved without proof, {unchecked} NOT CHECKED",
            if write {
                "wrote"
            } else {
                "NOT WRITTEN (--strict, the check is not complete)"
            },
            target.display(),
            plans.iter().map(|p| p.1.applied.len()).sum::<usize>(),
            written.len(),
            plans.iter().map(|p| p.1.unchanged).sum::<usize>(),
            valid
        );
        if let Some(b) = &backup {
            println!("  backup    {}", b.display());
        }
        let several = plans.len() > 1;
        let tag = |os: &str| {
            if several {
                format!(" ({os})")
            } else {
                String::new()
            }
        };
        for (os, plan) in &plans {
            for (e, o, n, c) in &plan.applied {
                println!("  applied   {e:48} {o:>4} -> {n:<4} {c:?}{}", tag(os));
            }
        }
        for i in &written {
            let (os, r) = &sig_rows[*i];
            println!("  applied   {:48} new signature{}", r.entry, tag(os));
            print_sig_detail(r);
        }
        for (os, plan) in &plans {
            for (e, v) in &plan.needs_human {
                println!("  human     {e:48} {v}{}", tag(os));
            }
        }
        for (os, plan) in &plans {
            for (e, v) in &plan.unconfirmed {
                println!("  same?     {e:48} {v}{}", tag(os));
            }
        }
        for i in &sig_human {
            let (os, r) = &sig_rows[*i];
            println!(
                "  human     {:48} {}{}{}",
                r.entry,
                r.verdict,
                if held.contains(i) {
                    " (held back at this level)"
                } else {
                    ""
                },
                tag(os)
            );
            print_sig_detail(r);
        }
        for (os, plan) in &plans {
            for (e, v) in &plan.not_checked {
                println!("  UNCHECKED {e:48} {v}{}", tag(os));
            }
        }
        for (e, os, why) in &sig_unchecked {
            println!("  UNCHECKED {e:48} {why}{}", tag(os));
        }
    }
    if strict && !complete {
        std::process::exit(2);
    }
    Ok(())
}

fn validate(
    old_p: &Path,
    new_p: &Path,
    before: &Path,
    after: &Path,
    class_map: Option<&Path>,
    json: bool,
) -> Result<()> {
    let old = Image::load(old_p)?;
    let new = Image::load(new_p)?;
    let map = load_map(class_map, &[before])?;
    let v = report::validate(&old, &new, before, after, &map)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!(
        "{} entries judged ({} changed by the maintainers); strong {}/{} correct, likely {}/{}, guess {}/{}, unplaced {} (of which changed: {})",
        v.rows.len(),
        v.changed_by_maintainers,
        v.strong.correct,
        v.strong.correct + v.strong.wrong,
        v.likely.correct,
        v.likely.correct + v.likely.wrong,
        v.guess.correct,
        v.guess.correct + v.guess.wrong,
        v.unplaced,
        v.unplaced_but_changed
    );
    for r in &v.rows {
        let mark = match r.outcome {
            report::Outcome::Correct => "ok   ",
            report::Outcome::Wrong => "WRONG",
            report::Outcome::Unplaced => "  -  ",
        };
        println!(
            "  {mark} {:48} {:>4} -> {:>4} actual, {:>4} predicted  {}",
            r.entry,
            r.old_index,
            r.actual_new,
            r.predicted_new
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".into()),
            r.verdict
        );
    }
    Ok(())
}

/// The class map given on the command line, or the shipped one for a gamedata file the tool
/// knows by name.
fn load_map(
    path: Option<&Path>,
    gamedata: &[&Path],
) -> Result<gamedata_doctor::gamedata::ClassMap> {
    if let Some(p) = path {
        return gamedata_doctor::gamedata::load_class_map(p);
    }
    let mut map = gamedata_doctor::gamedata::ClassMap::new();
    for g in gamedata {
        if let Some((project, m)) = gamedata_doctor::gamedata::builtin_class_map(g) {
            eprintln!(
                "{}: using the built-in class map for {project} ({} names; --class-map replaces it)",
                g.display(),
                m.len()
            );
            map.extend(m);
        }
    }
    Ok(map)
}

fn dump(bin: &Path, class: Option<&str>) -> Result<()> {
    let t0 = Instant::now();
    let img = Image::load(bin)?;
    let r = rtti::analyze(&img);
    let fdes = img.func_ranges.len();
    println!(
        "{} ({}, {:?}): {} relocations, {} type names, {} typeinfo, {} vtables ({} primary classes, {} ambiguous names), {} functions with exact ranges, {:.2}s",
        img.path,
        img.version,
        img.format,
        img.relocs.len(),
        r.n_names,
        r.n_typeinfo,
        r.vtables.len(),
        r.primary.len(),
        r.ambiguous.len(),
        fdes,
        t0.elapsed().as_secs_f32()
    );
    match class {
        Some(c) => {
            let Some(&i) = r.primary.get(c) else {
                println!("class {c} not found");
                return Ok(());
            };
            let vt = &r.vtables[i];
            println!(
                "{} primary vtable at {:#x}, {} slots",
                vt.class,
                vt.addr,
                vt.slots.len()
            );
            for (k, s) in vt.slots.iter().enumerate() {
                match s {
                    Target::Addr(a) => println!("  [{k:3}] {a:#x}"),
                    Target::Sym(n) => println!("  [{k:3}] {n}"),
                    Target::Null => println!("  [{k:3}] (null)"),
                }
            }
        }
        None => {
            for (c, &i) in &r.primary {
                println!("{:5} {}", r.vtables[i].slots.len(), c);
            }
        }
    }
    Ok(())
}

fn diff(
    old_p: &Path,
    new_p: &Path,
    gamedata: &[PathBuf],
    class_map: Option<&Path>,
    os: Option<&str>,
    all: bool,
    json: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let old = Image::load(old_p)?;
    let new = Image::load(new_p)?;
    let map = load_map(
        class_map,
        &gamedata.iter().map(|g| g.as_path()).collect::<Vec<_>>(),
    )?;
    let os = os.unwrap_or(old.format.os());
    let rep = report::compare(&old, &new, gamedata, &map, os)?;
    let total_s = t0.elapsed().as_secs_f32();

    if json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
        return Ok(());
    }

    println!(
        "old {} ({}): {} classes, {} slots\nnew {} ({}): {} classes, {} slots",
        rep.old.version,
        rep.old.path,
        rep.old.classes,
        rep.old.slots,
        rep.new.version,
        rep.new.path,
        rep.new.classes,
        rep.new.slots
    );
    println!(
        "common classes {}, only in old {}, only in new {}; same size {}, size changed {}; identical slots at the same index {}/{} ({:.1}%); {:.2}s",
        rep.common_classes,
        rep.only_old.len(),
        rep.only_new.len(),
        rep.same_size,
        rep.size_changed,
        rep.identical_slots,
        rep.compared_slots,
        100.0 * rep.identical_slots as f64 / rep.compared_slots.max(1) as f64,
        total_s
    );
    println!(
        "classes with shifted, inserted, removed or guessed slots: {}; classes with only likely (unproven) pairs: {}",
        rep.changed.len(),
        rep.classes_with_likely_pairs_only
    );
    let limit = if all { rep.changed.len() } else { 30 };
    for c in rep.changed.iter().take(limit) {
        let shifts: Vec<String> = c
            .shifts
            .iter()
            .map(|r| {
                if r.from == r.to {
                    format!("{}{:+}", r.from, r.delta)
                } else {
                    format!("{}..{}{:+}", r.from, r.to, r.delta)
                }
            })
            .collect();
        let gaps: Vec<String> = c
            .uncertain_regions
            .iter()
            .map(|g| {
                format!(
                    "old {}..{} <-> new {}..{}",
                    g.old_from, g.old_to, g.new_from, g.new_to
                )
            })
            .collect();
        println!(
            "  {}: {} -> {} slots; shifted: {}; inserted new: {:?}; unmatched old: {:?}; changed in place: {}; likely pairs: {}; guessed pairs: {}{}",
            c.class,
            c.old_slots,
            c.new_slots,
            if shifts.is_empty() { "none".to_string() } else { shifts.join(", ") },
            c.inserted_new,
            c.unmatched_old,
            c.changed_in_place,
            c.likely_pairs,
            c.guessed_pairs,
            if gaps.is_empty() { String::new() } else { format!("; guessed regions: {}", gaps.join("; ")) }
        );
    }
    if rep.changed.len() > limit {
        println!("  ... {} more (use --all)", rep.changed.len() - limit);
    }
    if !rep.gamedata.is_empty() {
        println!("\ngamedata offsets ({os}):");
        for g in &rep.gamedata {
            println!(
                "  {:50} {:>5}  {}",
                g.entry,
                g.old_index
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "-".into()),
                g.verdict
            );
        }
    }
    Ok(())
}
