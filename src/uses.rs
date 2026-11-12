//! Where a plugin's source touches the classes an update broke. This reads text, not C++: a
//! call is `->Name(` or `.Name(`, a hook is `Class::Name` or `Class, Name` in a macro, a mention
//! is the class name anywhere outside a comment. The type of the object a call is made on is
//! not known, so a function name that other, intact classes declare too is reported apart.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// One class to look for.
#[derive(Clone, Debug)]
pub struct Needle {
    pub class: String,
    /// The functions whose slot moved or vanished.
    pub methods: Vec<String>,
    /// Of those, the names that an intact class declares as well.
    pub shared: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Kind {
    /// A function that moved, spelled together with its class: `Class::Name`, or
    /// `Class, Name` in a hook macro.
    Hook,
    /// `->Name(` or `.Name(` by a name that, among the classes of the headers, only the broken
    /// class has. The type of the object is still not known: a candidate, not a proof.
    Call,
    /// A call by a name that intact classes declare too.
    Maybe,
    /// The class name.
    Mention,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Use {
    pub kind: Kind,
    pub file: String,
    pub line: usize,
    pub what: String,
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The identifiers of a line with their byte ranges.
fn idents(line: &str) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if is_ident(b[i] as char) && !(b[i] as char).is_ascii_digit() {
            let s = i;
            while i < b.len() && is_ident(b[i] as char) {
                i += 1;
            }
            out.push((s, i));
        } else if is_ident(b[i] as char) {
            while i < b.len() && is_ident(b[i] as char) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The uses of every needle in one file's text; the index is the needle's.
pub fn scan_text(text: &str, file: &str, needles: &[Needle]) -> Vec<(usize, Use)> {
    let text = crate::sdk::strip_comments(text);
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let ids = idents(line);
        for (k, &(s, e)) in ids.iter().enumerate() {
            let word = &line[s..e];
            let before = line[..s].trim_end();
            let after = line[e..].trim_start();
            let prev = k.checked_sub(1).map(|p| &line[ids[p].0..ids[p].1]);
            for (ni, needle) in needles.iter().enumerate() {
                let mut hit = |kind: Kind| {
                    out.push((
                        ni,
                        Use {
                            kind,
                            file: file.to_string(),
                            line: n + 1,
                            what: word.to_string(),
                        },
                    ))
                };
                if word == needle.class {
                    hit(Kind::Mention);
                    continue;
                }
                if !needle.methods.iter().any(|m| m == word) {
                    continue;
                }
                // `Class::Name` (a hook, a member pointer) and `Class, Name` (a hook macro)
                let with_class = prev == Some(needle.class.as_str())
                    && (before.ends_with("::") || before.ends_with(','));
                let member =
                    (before.ends_with("->") || before.ends_with('.')) && after.starts_with('(');
                if with_class {
                    hit(Kind::Hook);
                } else if member {
                    hit(if needle.shared.iter().any(|m| m == word) {
                        Kind::Maybe
                    } else {
                        Kind::Call
                    });
                }
            }
        }
    }
    out
}

/// Every C++ file under the roots, without version-control and build folders and without
/// `skip` (the SDK itself declares what it is being searched for).
pub fn scan(roots: &[PathBuf], skip: &Path, needles: &[Needle]) -> (Vec<(usize, Use)>, usize) {
    let skip = skip.canonicalize().unwrap_or_else(|_| skip.to_path_buf());
    let mut out = Vec::new();
    let mut files = 0usize;
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else {
                    continue;
                };
                let path = e.path();
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                if ft.is_dir() {
                    let same = path.canonicalize().map(|p| p == skip).unwrap_or(false);
                    if !same
                        && !name.starts_with('.')
                        && !matches!(name.as_str(), "build" | "target" | "obj" | "node_modules")
                    {
                        stack.push(path);
                    }
                } else if ft.is_file()
                    && [".cpp", ".cc", ".cxx", ".c", ".h", ".hpp", ".hxx", ".inl"]
                        .iter()
                        .any(|x| name.ends_with(x))
                {
                    let Ok(bytes) = std::fs::read(&path) else {
                        continue;
                    };
                    files += 1;
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    out.extend(scan_text(
                        &String::from_utf8_lossy(&bytes),
                        &rel.to_string_lossy().replace('\\', "/"),
                        needles,
                    ));
                }
            }
        }
    }
    out.sort();
    (out, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needle() -> Vec<Needle> {
        vec![Needle {
            class: "IEngine".into(),
            methods: vec!["SpawnGroup".into(), "GetName".into()],
            shared: vec!["GetName".into()],
        }]
    }

    fn kinds(text: &str) -> Vec<(Kind, usize, String)> {
        scan_text(text, "a.cpp", &needle())
            .into_iter()
            .map(|(_, u)| (u.kind, u.line, u.what))
            .collect()
    }

    #[test]
    fn calls_hooks_and_mentions() {
        let found = kinds(
            "IEngine *g_engine;\n\
             void f() { g_engine->SpawnGroup(1); }\n\
             SH_DECL_HOOK1_void(IEngine, SpawnGroup, SH_NOATTRIB, 0, int);\n\
             auto p = &IEngine::SpawnGroup;\n",
        );
        assert!(found.contains(&(Kind::Mention, 1, "IEngine".into())));
        assert!(found.contains(&(Kind::Call, 2, "SpawnGroup".into())));
        assert!(found.contains(&(Kind::Hook, 3, "SpawnGroup".into())));
        assert!(found.contains(&(Kind::Hook, 4, "SpawnGroup".into())));
    }

    #[test]
    fn a_name_other_classes_have_is_only_a_maybe_and_comments_do_not_count() {
        let found = kinds(
            "x->GetName();\n\
             // g_engine->SpawnGroup(1);\n\
             int SpawnGroup = 3; Other::SpawnGroup(2); SpawnGroupFast();\n",
        );
        assert_eq!(found, vec![(Kind::Maybe, 1, "GetName".to_string())]);
    }
}
