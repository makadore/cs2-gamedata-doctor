//! Reading the offset entries of the gamedata formats used around CS2 and placing each entry on a
//! class by its name.
//!
//! Supported files:
//! - CounterStrikeSharp `gamedata.json`: `{ "Entry": { "offsets": { "linux": N, "windows": M } } }`
//! - CS2Fixes `*.jsonc` (comments and trailing commas allowed): `{ "Offsets": { "Entry": { "linux": N } } }`
//! - Valve KeyValues `*.games.txt` (CS2Fixes before May 2026, cs2kz, most Metamod plugins):
//!   `"Games" { "csgo" { "Offsets" { "Entry" { "linux" "N" "windows" "M" } } } }`
//!
//! Entry names are `Class_Method`, `Class::Method` or a bare name; the class is the longest
//! `_`-separated prefix (before any `::`) that the binary knows.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffsetEntry {
    pub name: String,
    pub linux: Option<i64>,
    pub windows: Option<i64>,
}

pub fn load(path: &Path) -> Result<Vec<OffsetEntry>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(
        &text,
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
    )
    .with_context(|| format!("parsing {}", path.display()))
}

/// Parse gamedata text; `ext` picks the syntax (`json`, `jsonc`, anything else = KeyValues).
pub fn parse(text: &str, ext: &str) -> Result<Vec<OffsetEntry>> {
    // CS2Fixes' 2026 files start with a UTF-8 byte order mark
    let text = text.trim_start_matches('\u{feff}');
    match ext {
        "json" | "jsonc" => {
            let v: Value = serde_json::from_str(&strip_jsonc(text))?;
            Ok(from_json(&v))
        }
        _ => {
            let kv = kv::parse(text)?;
            Ok(from_kv(&kv))
        }
    }
}

fn num(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn from_json(root: &Value) -> Vec<OffsetEntry> {
    let mut out = Vec::new();
    let Value::Object(map) = root else { return out };
    for (name, entry) in map {
        // CS2Fixes style: a top-level "Offsets" object
        if name == "Offsets" {
            if let Value::Object(entries) = entry {
                for (n, body) in entries {
                    out.push(OffsetEntry {
                        name: n.clone(),
                        linux: num(body.get("linux")),
                        windows: num(body.get("windows")),
                    });
                }
            }
            continue;
        }
        // CounterStrikeSharp style: "offsets" inside each entry
        if let Some(off) = entry.get("offsets") {
            out.push(OffsetEntry {
                name: name.clone(),
                linux: num(off.get("linux")),
                windows: num(off.get("windows")),
            });
        }
    }
    out
}

fn from_kv(root: &kv::Node) -> Vec<OffsetEntry> {
    let mut out = Vec::new();
    fn walk(node: &kv::Node, out: &mut Vec<OffsetEntry>) {
        let kv::Node::Block(children) = node else {
            return;
        };
        for (key, child) in children {
            if key == "Offsets" {
                if let kv::Node::Block(entries) = child {
                    for (name, body) in entries {
                        if let kv::Node::Block(fields) = body {
                            let get = |k: &str| {
                                fields
                                    .iter()
                                    .find(|(f, _)| f == k)
                                    .and_then(|(_, v)| match v {
                                        kv::Node::Leaf(s) => s.trim().parse().ok(),
                                        _ => None,
                                    })
                            };
                            out.push(OffsetEntry {
                                name: name.clone(),
                                linux: get("linux"),
                                windows: get("windows"),
                            });
                        }
                    }
                }
            } else {
                walk(child, out);
            }
        }
    }
    walk(root, &mut out);
    out
}

/// Remove `//` and `/* */` comments and trailing commas so serde_json accepts JSONC.
pub fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    // trailing commas: ",  }" or ",  ]"
    let mut cleaned = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    cleaned
}

/// The longest `_`-separated prefix of the entry name (before any `::`) that is a known class.
pub fn class_for<F: Fn(&str) -> bool>(entry: &str, known: F) -> Option<String> {
    let base = entry.split("::").next().unwrap_or(entry);
    let parts: Vec<&str> = base.split('_').collect();
    (1..=parts.len())
        .rev()
        .map(|n| parts[..n].join("_"))
        .find(|cand| known(cand))
}

/// `ISource2GameEntities::CheckTransmit`, `IVEngineServer2_...`: an interface rather than a class.
/// Its index refers to the interface's table inside some implementation, which has to be
/// resolved through the implementation's base offsets (not implemented yet).
pub fn is_interface_name(entry: &str) -> bool {
    let base = entry.split("::").next().unwrap_or(entry);
    let mut chars = base.chars();
    matches!((chars.next(), chars.next()), (Some('I'), Some(c)) if c.is_ascii_uppercase())
}

/// A minimal Valve KeyValues reader: quoted or bare tokens, `{}` blocks, `//` comments.
pub mod kv {
    use anyhow::{bail, Result};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Node {
        Leaf(String),
        Block(Vec<(String, Node)>),
    }

    enum Tok {
        Str(String),
        Open,
        Close,
    }

    fn tokens(text: &str) -> Vec<Tok> {
        let b = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace() {
                i += 1;
            } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else if c == b'"' {
                let mut s = Vec::new();
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        s.push(b[i + 1]);
                        i += 2;
                    } else {
                        s.push(b[i]);
                        i += 1;
                    }
                }
                i += 1;
                out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
            } else if c == b'{' {
                out.push(Tok::Open);
                i += 1;
            } else if c == b'}' {
                out.push(Tok::Close);
                i += 1;
            } else {
                let start = i;
                while i < b.len()
                    && !b[i].is_ascii_whitespace()
                    && b[i] != b'{'
                    && b[i] != b'}'
                    && b[i] != b'"'
                {
                    i += 1;
                }
                out.push(Tok::Str(String::from_utf8_lossy(&b[start..i]).into_owned()));
            }
        }
        out
    }

    pub fn parse(text: &str) -> Result<Node> {
        let toks = tokens(text);
        let mut pos = 0;
        let node = block(&toks, &mut pos)?;
        if pos != toks.len() {
            bail!("KeyValues: unbalanced braces");
        }
        Ok(node)
    }

    fn block(toks: &[Tok], pos: &mut usize) -> Result<Node> {
        let mut items = Vec::new();
        while *pos < toks.len() {
            match &toks[*pos] {
                Tok::Close => return Ok(Node::Block(items)),
                Tok::Open => bail!("KeyValues: a block without a key"),
                Tok::Str(key) => {
                    *pos += 1;
                    match toks.get(*pos) {
                        Some(Tok::Open) => {
                            *pos += 1;
                            let child = block(toks, pos)?;
                            if !matches!(toks.get(*pos), Some(Tok::Close)) {
                                bail!("KeyValues: missing closing brace for {key}");
                            }
                            *pos += 1;
                            items.push((key.clone(), child));
                        }
                        Some(Tok::Str(v)) => {
                            *pos += 1;
                            items.push((key.clone(), Node::Leaf(v.clone())));
                        }
                        _ => bail!("KeyValues: key {key} without a value"),
                    }
                }
            }
        }
        Ok(Node::Block(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(c: &str) -> bool {
        [
            "CCSPlayer_ItemServices",
            "CBaseEntity",
            "CSource2GameEntities",
            "CCSPlayerController",
        ]
        .contains(&c)
    }

    #[test]
    fn longest_known_prefix_wins() {
        assert_eq!(
            class_for("CCSPlayer_ItemServices_GiveNamedItem", known).as_deref(),
            Some("CCSPlayer_ItemServices")
        );
        assert_eq!(
            class_for("CBaseEntity_Teleport", known).as_deref(),
            Some("CBaseEntity")
        );
        assert_eq!(
            class_for("CBaseEntity::Precache", known).as_deref(),
            Some("CBaseEntity"),
            "KeyValues style names"
        );
        assert_eq!(
            class_for("CCSPlayerController_ChangeTeam", known).as_deref(),
            Some("CCSPlayerController")
        );
        assert_eq!(
            class_for("CBaseEntity", known).as_deref(),
            Some("CBaseEntity"),
            "a bare class name is fine"
        );
    }

    #[test]
    fn interfaces_are_recognised_but_not_guessed() {
        assert_eq!(
            class_for("ISource2GameEntities::CheckTransmit", known),
            None,
            "no silent I -> C substitution"
        );
        assert!(is_interface_name("ISource2GameEntities::CheckTransmit"));
        assert!(is_interface_name("IVEngineServer2_ServerCommand"));
        assert!(
            !is_interface_name("Interesting_Thing"),
            "a leading I before a lowercase letter is a word"
        );
        assert!(!is_interface_name("CBaseEntity_Teleport"));
    }

    #[test]
    fn unknown_names_are_not_placed() {
        assert_eq!(class_for("GameEntitySystem", known), None);
        assert_eq!(class_for("Interesting_Thing", known), None);
    }

    #[test]
    fn counterstrikesharp_json() {
        let e = parse(
            r#"{"A_x":{"offsets":{"linux":3,"windows":4}},"B":{"signatures":{"linux":"55"}},"C_y":{"offsets":{"linux":7}},"D_z":{"offsets":{"linux":"5"}}}"#,
            "json",
        )
        .unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(
            (e[0].name.as_str(), e[0].linux, e[0].windows),
            ("A_x", Some(3), Some(4))
        );
        assert_eq!(
            (e[1].name.as_str(), e[1].linux, e[1].windows),
            ("C_y", Some(7), None)
        );
        assert_eq!(
            (e[2].name.as_str(), e[2].linux),
            ("D_z", Some(5)),
            "a numeric string is accepted"
        );
    }

    #[test]
    fn cs2fixes_jsonc_with_comments_and_trailing_commas() {
        let text = r#"{
            "Signatures": { "UTIL_SayText": { "library": "server", "linux": "55 48 // not a comment inside a string" } },
            // the vtable indices
            "Offsets": {
                /* block comment */
                "CBaseEntity::Precache": { "windows": 6, "linux": 7, },
                "GetHammerUniqueId": { "windows": 111, "linux": 110 },
            },
        }"#;
        let e = parse(text, "jsonc").unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(
            (e[0].name.as_str(), e[0].linux, e[0].windows),
            ("CBaseEntity::Precache", Some(7), Some(6))
        );
        assert_eq!(
            (e[1].name.as_str(), e[1].linux),
            ("GetHammerUniqueId", Some(110))
        );
    }

    #[test]
    fn valve_keyvalues_games_txt() {
        let text = r#"
        "Games"
        {
            "csgo"
            {
                "Signatures"
                {
                    "UTIL_ClientPrint" { "library" "server" "linux" "\x55\x48\x89\xE5" }
                }
                // String: "%s<%i><%s><%s>" ChangeTeam()
                "Offsets"
                {
                    "CCSPlayerController_ChangeTeam"
                    {
                        "windows"		"102"
                        "linux"			"101"
                    }
                    "CBaseEntity::Precache" { "windows" "6" "linux" "7" }
                    "NoNumbers" { "windows" "" }
                }
                "Patches" { "ServerMovementUnlock" { "linux" "90 90" } }
            }
        }"#;
        let e = parse(text, "txt").unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(
            (e[0].name.as_str(), e[0].linux, e[0].windows),
            ("CCSPlayerController_ChangeTeam", Some(101), Some(102))
        );
        assert_eq!(
            (e[1].name.as_str(), e[1].linux, e[1].windows),
            ("CBaseEntity::Precache", Some(7), Some(6))
        );
        assert_eq!(
            (e[2].name.as_str(), e[2].linux, e[2].windows),
            ("NoNumbers", None, None)
        );
    }

    #[test]
    fn a_byte_order_mark_is_ignored() {
        let kv = parse(
            "\u{feff}\"Games\" { \"csgo\" { \"Offsets\" { \"A_x\" { \"linux\" \"4\" } } } }",
            "txt",
        )
        .unwrap();
        assert_eq!((kv[0].name.as_str(), kv[0].linux), ("A_x", Some(4)));
        let js = parse(
            "\u{feff}{ \"Offsets\": { \"A_x\": { \"linux\": 4 } } }",
            "jsonc",
        )
        .unwrap();
        assert_eq!((js[0].name.as_str(), js[0].linux), ("A_x", Some(4)));
    }

    #[test]
    fn broken_keyvalues_is_an_error() {
        assert!(parse(
            r#""Games" { "csgo" { "Offsets" { "A" { "linux" "1" } }"#,
            "txt"
        )
        .is_err());
        assert!(parse(r#"{ "Offsets": { "A": { "linux": 1 }"#, "jsonc").is_err());
    }
}

/// Entry name -> class, for gamedata whose names do not carry the class (`Teleport`,
/// `ControllerChangeTeam` in cs2kz). File format: one `Entry Class` pair per line, `#` comments.
// ------------------------------------------------------------------------------------------
// signatures
use crate::sig::SigEntry;

pub fn load_sigs(path: &Path) -> Result<Vec<SigEntry>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_sigs(
        &text,
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
    )
    .with_context(|| format!("parsing {}", path.display()))
}

/// The signature entries of a gamedata file, in file order: CounterStrikeSharp's `"signatures"`
/// object inside an entry, CS2Fixes' top-level `"Signatures"`, a KeyValues `"Signatures"` block.
pub fn parse_sigs(text: &str, ext: &str) -> Result<Vec<SigEntry>> {
    let text = text.trim_start_matches('\u{feff}');
    let mut out = Vec::new();
    match ext {
        "json" | "jsonc" => {
            let v: Value = serde_json::from_str(&strip_jsonc(text))?;
            let s = |b: &Value, k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
            let mut push = |name: &str, body: &Value| {
                out.push(SigEntry {
                    name: name.to_string(),
                    library: s(body, "library").unwrap_or_default(),
                    linux: s(body, "linux"),
                    windows: s(body, "windows"),
                    multi: body.get("allow_multi_match").is_some_and(|v| {
                        v.as_bool() == Some(true)
                            || v.as_i64() == Some(1)
                            || v.as_str() == Some("1")
                    }),
                })
            };
            if let Value::Object(map) = &v {
                for (name, entry) in map {
                    if name == "Signatures" {
                        if let Value::Object(entries) = entry {
                            for (n, body) in entries {
                                push(n, body);
                            }
                        }
                    } else if let Some(body) = entry.get("signatures") {
                        push(name, body);
                    }
                }
            }
        }
        _ => {
            fn walk(node: &kv::Node, out: &mut Vec<SigEntry>) {
                let kv::Node::Block(children) = node else {
                    return;
                };
                for (key, child) in children {
                    if key == "Signatures" {
                        if let kv::Node::Block(entries) = child {
                            for (name, body) in entries {
                                if let kv::Node::Block(fields) = body {
                                    let get = |k: &str| {
                                        fields.iter().find(|(f, _)| f == k).and_then(|(_, v)| {
                                            match v {
                                                kv::Node::Leaf(s) => Some(s.clone()),
                                                _ => None,
                                            }
                                        })
                                    };
                                    out.push(SigEntry {
                                        name: name.clone(),
                                        library: get("library").unwrap_or_default(),
                                        linux: get("linux"),
                                        windows: get("windows"),
                                        multi: get("allow_multi_match").as_deref() == Some("1"),
                                    });
                                }
                            }
                        }
                    } else {
                        walk(child, out);
                    }
                }
            }
            walk(&kv::parse(text)?, &mut out);
        }
    }
    Ok(out)
}

/// Replace the quoted value after the `os` key inside the entry's block with `new_value`
/// (written as is: the caller renders it in the file's own notation). Everything else in the
/// text stays byte for byte. `None` when the entry or the key is not there.
pub fn rewrite_sig(text: &str, entry: &str, os: &str, new_value: &str) -> Option<String> {
    let key = format!("\"{entry}\"");
    let os_key = format!("\"{os}\"");
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(&key) {
        let after = search_from + rel + key.len();
        search_from = after;
        // the entry's block must open right after the name (whitespace, a colon, comments aside)
        let open_rel = text[after..].find('{')?;
        if text[after..after + open_rel].contains('"') {
            continue; // this occurrence is a value or sits in a comment, not the entry's key
        }
        let open = after + open_rel;
        let mut depth = 0i32;
        let mut close = None;
        for (k, c) in text[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + k);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let block = &text[open..close];
        let from = block.find("\"signatures\"").unwrap_or(0);
        let Some(k) = block[from..].find(&os_key) else {
            continue;
        };
        let at = open + from + k + os_key.len();
        let q1 = at + text[at..close].find('"')?;
        if text[at..q1]
            .chars()
            .any(|c| !(c.is_whitespace() || c == ':'))
        {
            continue;
        }
        let q2 = q1 + 1 + text[q1 + 1..close].find('"')?;
        let mut t = String::with_capacity(text.len() + new_value.len());
        t.push_str(&text[..q1 + 1]);
        t.push_str(new_value);
        t.push_str(&text[q2..]);
        return Some(t);
    }
    None
}

#[cfg(test)]
mod sig_tests {
    use super::*;

    #[test]
    fn signatures_are_read_from_all_three_formats() {
        let css = r#"{"A": {"signatures": {"library": "server", "windows": "48 89 ?", "linux": "55 48"}}, "B": {"offsets": {"linux": 3}}}"#;
        let e = parse_sigs(css, "json").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].name.as_str(), e[0].library.as_str()), ("A", "server"));
        assert_eq!(e[0].linux.as_deref(), Some("55 48"));
        let fixes = "{\n // c\n \"Signatures\": { \"U\": { \"library\": \"server\", \"linux\": \"55 ? 48\", }, },\n \"Offsets\": {} }";
        let e = parse_sigs(fixes, "jsonc").unwrap();
        assert_eq!(e[0].linux.as_deref(), Some("55 ? 48"));
        let kvt = "\"Games\" { \"csgo\" { \"Signatures\" { \"S\" { \"library\" \"server\" \"linux\" \"\\x55\\x48\\x2A\" } } } }";
        let e = parse_sigs(kvt, "txt").unwrap();
        assert_eq!(
            e[0].linux.as_deref(),
            Some("x55x48x2A"),
            "the tokenizer drops the backslashes"
        );
    }

    #[test]
    fn rewrite_sig_changes_only_the_value() {
        let css = "{\n  \"A\": {\n    \"signatures\": {\n      \"library\": \"server\",\n      \"windows\": \"48 89 ?\",\n      \"linux\": \"55 48\"\n    }\n  }\n}\n";
        let out = rewrite_sig(css, "A", "linux", "55 48 89 E5").unwrap();
        assert_eq!(out, css.replace("\"55 48\"", "\"55 48 89 E5\""));
        let kvt = "\"Signatures\"\n{\n\t// \"S\" is called from X\n\t\"S\"\n\t{\n\t\t\"library\"\t\"server\"\n\t\t\"windows\"\t\"\\x40\\x53\"\n\t\t\"linux\"\t\t\"\\x55\\x48\"\n\t}\n}\n";
        let out = rewrite_sig(kvt, "S", "windows", "\\x40\\x53\\x2A").unwrap();
        assert!(out.contains("\"windows\"\t\"\\x40\\x53\\x2A\""), "{out}");
        assert!(
            out.contains("\"linux\"\t\t\"\\x55\\x48\""),
            "linux untouched"
        );
        assert!(
            out.contains("// \"S\" is called from X"),
            "the comment is not the entry"
        );
        assert!(rewrite_sig(kvt, "Nope", "linux", "x").is_none());
    }
}

pub type ClassMap = HashMap<String, String>;

/// The class maps shipped with the tool, for the known gamedata files whose entry names do
/// not carry the class: picked by the file's path. Returns the project and its map.
pub fn builtin_class_map(gamedata: &Path) -> Option<(&'static str, ClassMap)> {
    const MAPS: &[(&str, &str)] = &[
        ("cs2kz", include_str!("../classmaps/cs2kz.map")),
        ("cs2fixes", include_str!("../classmaps/cs2fixes.map")),
        (
            "counterstrikesharp",
            include_str!("../classmaps/counterstrikesharp.map"),
        ),
    ];
    // the file's name, or a folder above it (addons/counterstrikesharp/gamedata/gamedata.json)
    let name = gamedata.to_string_lossy().to_ascii_lowercase();
    let (project, text) = MAPS.iter().find(|(key, _)| name.contains(key))?;
    Some((project, parse_class_map(text).ok()?))
}

pub fn load_class_map(path: &Path) -> Result<ClassMap> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_class_map(&text)
}

pub fn parse_class_map(text: &str) -> Result<ClassMap> {
    let mut map = ClassMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(entry), Some(class), None) => {
                map.insert(entry.to_string(), class.to_string());
            }
            _ => anyhow::bail!(
                "class map line {}: expected `Entry Class`, got {line:?}",
                n + 1
            ),
        }
    }
    Ok(map)
}

#[cfg(test)]
mod class_map_tests {
    use super::*;

    #[test]
    fn parses_pairs_and_ignores_comments() {
        let m = parse_class_map("# cs2kz\nTeleport CBaseEntity\n\nControllerChangeTeam CCSPlayerController # trailing\n").unwrap();
        assert_eq!(m.get("Teleport").map(String::as_str), Some("CBaseEntity"));
        assert_eq!(
            m.get("ControllerChangeTeam").map(String::as_str),
            Some("CCSPlayerController")
        );
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse_class_map("Teleport\n").is_err());
        assert!(parse_class_map("A B C\n").is_err());
    }
}

/// Write new linux indices into gamedata text without disturbing anything else: the entry's
/// block is located by its name, and only the number after its `linux` key is replaced. Works
/// for CounterStrikeSharp JSON, CS2Fixes JSONC and Valve KeyValues alike. Returns the new text
/// and the entries that could not be found.
pub fn rewrite(text: &str, changes: &[(String, i64)]) -> (String, Vec<String>) {
    rewrite_os(text, changes, "linux")
}

/// `rewrite` for either index key: `linux` or `windows`.
pub fn rewrite_os(text: &str, changes: &[(String, i64)], os: &str) -> (String, Vec<String>) {
    let mut out = text.to_string();
    let mut missing = Vec::new();
    for (entry, new_index) in changes {
        match rewrite_one(&out, entry, *new_index, os) {
            Some(t) => out = t,
            None => missing.push(entry.clone()),
        }
    }
    (out, missing)
}

fn rewrite_one(text: &str, entry: &str, new_index: i64, os: &str) -> Option<String> {
    let key = format!("\"{entry}\"");
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(&key) {
        let name_at = search_from + rel;
        let after = name_at + key.len();
        // the entry's block: from its first `{` to the matching `}`
        let open = after + text[after..].find('{')?;
        let mut depth = 0i32;
        let mut close = None;
        for (k, c) in text[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + k);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let block = &text[open..=close];
        // `"linux": 24`, `"linux" "24"`, `"linux":"24"`: keep everything but the digits
        if let Some(m) = find_os_number(block, os) {
            let (num_start, num_end) = (open + m.0, open + m.1);
            let mut t = String::with_capacity(text.len() + 4);
            t.push_str(&text[..num_start]);
            t.push_str(&new_index.to_string());
            t.push_str(&text[num_end..]);
            return Some(t);
        }
        search_from = after;
    }
    None
}

/// Byte range of the number that follows the first `"<os>"` key in a block.
fn find_os_number(block: &str, os: &str) -> Option<(usize, usize)> {
    let key = format!("\"{os}\"");
    let at = block.find(&key)? + key.len();
    let rest = &block.as_bytes()[at..];
    let mut i = 0;
    while i < rest.len()
        && (rest[i] == b' ' || rest[i] == b'\t' || rest[i] == b':' || rest[i] == b'"')
    {
        i += 1;
    }
    let start = i;
    if i < rest.len() && rest[i] == b'-' {
        i += 1;
    }
    while i < rest.len() && rest[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || (i == start + 1 && rest[start] == b'-') {
        return None;
    }
    Some((at + start, at + i))
}

#[cfg(test)]
mod rewrite_tests {
    use super::*;

    #[test]
    fn json_keeps_formatting_and_changes_only_the_number() {
        let src = "{\n  \"A_x\": {\n    \"offsets\": { \"linux\": 24, \"windows\": 25 }\n  },\n  \"B_y\": {\n    \"offsets\": { \"linux\": 7, \"windows\": 8 }\n  }\n}\n";
        let (out, missing) = rewrite(src, &[("A_x".into(), 25), ("Nope".into(), 1)]);
        assert_eq!(out, "{\n  \"A_x\": {\n    \"offsets\": { \"linux\": 25, \"windows\": 25 }\n  },\n  \"B_y\": {\n    \"offsets\": { \"linux\": 7, \"windows\": 8 }\n  }\n}\n");
        assert_eq!(missing, vec!["Nope"]);
        assert_eq!(parse(&out, "json").unwrap()[0].linux, Some(25));
    }

    #[test]
    fn keyvalues_quoted_numbers() {
        let src = "\"Games\"\n{\n\t\"csgo\"\n\t{\n\t\t\"Offsets\"\n\t\t{\n\t\t\t// comment\n\t\t\t\"Teleport\"\n\t\t\t{\n\t\t\t\t\"windows\"\t\t\"163\"\n\t\t\t\t\"linux\"\t\t\t\"164\"\n\t\t\t}\n\t\t}\n\t}\n}\n";
        let (out, missing) = rewrite(src, &[("Teleport".into(), 165)]);
        assert!(missing.is_empty());
        assert!(out.contains("\"linux\"\t\t\t\"165\""), "{out}");
        assert!(out.contains("\"windows\"\t\t\"163\""), "windows untouched");
        assert!(out.contains("// comment"), "comments survive");
        assert_eq!(parse(&out, "txt").unwrap()[0].linux, Some(165));
    }

    #[test]
    fn jsonc_with_comments_and_a_name_that_also_appears_in_a_comment() {
        let src = "{\n  // CBaseEntity::Use is called from ...\n  \"Offsets\": {\n    \"CBaseEntity::Use\": { \"windows\": 138, \"linux\": 139 },\n  }\n}\n";
        let (out, missing) = rewrite(src, &[("CBaseEntity::Use".into(), 147)]);
        assert!(missing.is_empty());
        assert!(out.contains("\"linux\": 147"), "{out}");
        assert!(
            out.contains("// CBaseEntity::Use is called from"),
            "the comment is untouched"
        );
        assert_eq!(parse(&out, "jsonc").unwrap()[0].linux, Some(147));
    }

    #[test]
    fn a_block_without_a_linux_key_is_reported_missing() {
        let src = "{ \"A\": { \"offsets\": { \"windows\": 3 } } }";
        let (out, missing) = rewrite(src, &[("A".into(), 9)]);
        assert_eq!(out, src);
        assert_eq!(missing, vec!["A"]);
    }
}
