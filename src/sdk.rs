//! What an SDK's headers say about virtual tables: for every class that declares virtual
//! functions, their names in declaration order. Compared with the tables of a binary this tells
//! whether a header still describes the game: everything compiled against a stale header calls
//! the wrong slot, and no gamedata file can cure that.
//!
//! This is not a C++ parser. It strips comments, evaluates the platform `#if`s it understands,
//! finds class bodies and reads the statements that carry the word `virtual`. Macros that
//! declare virtuals, templates and exotic syntax are beyond it, so a layout is evidence to be
//! checked against a binary, never a fact on its own.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Method {
    pub name: String,
    /// Number of parameters: what tells an override from an overload here.
    pub params: usize,
    pub dtor: bool,
    /// Declared with `override`: never a new slot.
    pub overrides: bool,
    /// The parameter list with names and defaults taken off, as text: tells `F(int)` from
    /// `F(float)` when both have one parameter.
    pub sig: String,
    /// The lines of the declaration in its header, 1-based; 0 when not known.
    pub line: usize,
    pub end_line: usize,
    /// The class that declares it.
    pub owner: String,
}

/// Where a slot of a layout is declared.
#[derive(Clone, Debug, PartialEq)]
pub struct Decl {
    pub class: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    /// 0: the whole declaration; 1 and 2: the two slots of an Itanium destructor.
    pub half: u8,
}

#[derive(Clone, Debug)]
pub struct Class {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub bases: Vec<String>,
    /// Own `virtual` declarations in order.
    pub methods: Vec<Method>,
    /// The line of the closing brace.
    pub end_line: usize,
    /// A conditional block the reader could not decide overlaps the class: what it declares on
    /// this platform is a guess.
    pub uncertain: bool,
}

#[derive(Default)]
pub struct Headers {
    pub classes: BTreeMap<String, Class>,
    pub files: usize,
    /// Header files or folders that could not be read: the picture is incomplete by that much.
    pub unreadable: usize,
}

/// The slots of a class's primary table as the headers describe them.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub slots: Vec<String>,
    /// One per slot.
    pub decl: Vec<Decl>,
    /// False when a base is not in the headers, or the order cannot be trusted (MSVC groups
    /// overloads and reverses them): the count may still be right, the names may not line up.
    pub exact: bool,
}

const LINUX_MACROS: &[&str] = &[
    "LINUX",
    "_LINUX",
    "__linux__",
    "POSIX",
    "_POSIX",
    "GNUC",
    "__GNUC__",
    "COMPILER_GCC",
    "PLATFORM_64BITS",
    "X64BITS",
    "__x86_64__",
    "__cplusplus",
];
const WINDOWS_MACROS: &[&str] = &[
    "_WIN32",
    "WIN32",
    "_WIN64",
    "WIN64",
    "_MSC_VER",
    "COMPILER_MSVC",
    "COMPILER_MSVC64",
    "PLATFORM_64BITS",
    "X64BITS",
    "_M_X64",
    "__cplusplus",
];

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Comments out, string and character literals emptied; newlines kept so line numbers hold.
pub(crate) fn strip_comments(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                if b[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            while i < b.len() && b[i] != c && b[i] != '\n' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < b.len() && b[i] == c {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `defined(X)`, `!defined(X)`, `X`, `!X`, `0`, `1`, joined by `||` or `&&`. `None` when the
/// expression is beyond that: the caller then keeps the block.
fn pp_cond(expr: &str, defined: &[&str]) -> Option<bool> {
    let expr = expr.trim();
    if let Some((a, b)) = expr.split_once("||") {
        return match (pp_cond(a, defined), pp_cond(b, defined)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        };
    }
    if let Some((a, b)) = expr.split_once("&&") {
        return match (pp_cond(a, defined), pp_cond(b, defined)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
    }
    let (neg, rest) = match expr.strip_prefix('!') {
        Some(r) => (true, r.trim()),
        None => (false, expr),
    };
    let rest = rest.trim_start_matches('(').trim_end_matches(')').trim();
    let v = if rest == "0" {
        false
    } else if rest == "1" {
        true
    } else {
        let name = rest
            .strip_prefix("defined")
            .map(|r| {
                r.trim()
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .trim()
            })
            .unwrap_or(rest);
        if name.is_empty() || !name.chars().all(is_ident) {
            return None;
        }
        defined.contains(&name)
    };
    Some(v != neg)
}

/// Preprocessor lines out; the branches a platform does not take blanked. Also the line ranges
/// (1-based, inclusive) of the conditional blocks with a condition that could not be decided:
/// which branch the compiler takes there is a guess.
fn preprocess(text: &str, defined: &[&str]) -> (String, Vec<(usize, usize)>) {
    // (this branch is live, some branch of the block was live, the enclosing block is live)
    let mut stack: Vec<(bool, bool, bool)> = Vec::new();
    // per open block: the line of its first undecided condition
    let mut doubt: Vec<Option<usize>> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut continued = false;
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let t = line.trim_start();
        let live = stack.last().map(|s| s.0 && s.2).unwrap_or(true);
        if continued {
            continued = line.trim_end().ends_with('\\');
            out.push('\n');
            continue;
        }
        if let Some(d) = t.strip_prefix('#') {
            continued = line.trim_end().ends_with('\\');
            let d = d.trim_start();
            let word: String = d.chars().take_while(|c| is_ident(*c)).collect();
            let arg = d[word.len()..].trim();
            match word.as_str() {
                "ifdef" => {
                    let v = defined.contains(&arg);
                    stack.push((v, v, live));
                    doubt.push(None);
                }
                "ifndef" => {
                    let v = !defined.contains(&arg);
                    stack.push((v, v, live));
                    doubt.push(None);
                }
                "if" => {
                    let c = pp_cond(arg, defined);
                    let v = c.unwrap_or(true);
                    stack.push((v, v, live));
                    doubt.push(c.is_none().then_some(n));
                }
                "elif" => {
                    if let Some(s) = stack.last_mut() {
                        let c = pp_cond(arg, defined);
                        if c.is_none() && !s.1 {
                            if let Some(d) = doubt.last_mut() {
                                d.get_or_insert(n);
                            }
                        }
                        let v = !s.1 && c.unwrap_or(true);
                        s.0 = v;
                        s.1 |= v;
                    }
                }
                "else" => {
                    if let Some(s) = stack.last_mut() {
                        s.0 = !s.1;
                        s.1 = true;
                    }
                }
                "endif" => {
                    stack.pop();
                    if let Some(Some(from)) = doubt.pop() {
                        ranges.push((from, n));
                    }
                }
                _ => {}
            }
            out.push('\n');
            continue;
        }
        if live {
            out.push_str(line);
        }
        out.push('\n');
    }
    // a block never closed is in doubt to the end of the file
    let last = text.lines().count();
    ranges.extend(doubt.into_iter().flatten().map(|from| (from, last)));
    (out, ranges)
}

/// `template <...>` prefixes off the front of a statement.
fn strip_templates(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix("template") else {
            return s;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('<') {
            return s;
        }
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => s = &rest[e..],
            None => return s,
        }
    }
}

/// A statement that opens a class body: `class X`, `struct X : public A, B`, `abstract_class X`.
fn class_head(stmt: &str) -> Option<(String, Vec<String>)> {
    // access labels and macros of the enclosing scope may be glued in front
    let stmt = stmt.rsplit([';', '}']).next().unwrap_or(stmt);
    let mut stmt = stmt.trim_start();
    // an access label of the enclosing class glued in front
    loop {
        let rest = ["public", "private", "protected"]
            .iter()
            .find_map(|l| stmt.strip_prefix(l))
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix(':'))
            .filter(|r| !r.starts_with(':'));
        match rest {
            Some(r) => stmt = r.trim_start(),
            None => break,
        }
    }
    let stmt = strip_templates(stmt);
    if stmt.contains('(') || stmt.contains('=') {
        return None;
    }
    let words: Vec<&str> = stmt
        .split(|c: char| c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    let k = words
        .iter()
        .position(|w| matches!(*w, "class" | "struct" | "abstract_class"))?;
    if words[..k].iter().any(|w| matches!(*w, "enum" | "friend")) {
        return None;
    }
    let (head, bases) = match stmt.split_once(':') {
        // `::` in a qualified name is not a base list
        Some((h, b)) if !b.starts_with(':') && !h.ends_with(':') => (h, b),
        _ => (stmt, ""),
    };
    let name = head
        .split(|c: char| c.is_whitespace())
        .rfind(|w| !w.is_empty() && *w != "final")?;
    let name: String = name.chars().take_while(|c| is_ident(*c)).collect();
    if name.is_empty() || matches!(name.as_str(), "class" | "struct" | "abstract_class") {
        return None;
    }
    let bases = bases
        .split(',')
        .filter_map(|b| {
            let b = b.split(|c: char| c.is_whitespace()).rfind(|w| {
                !w.is_empty() && !matches!(*w, "public" | "private" | "protected" | "virtual")
            })?;
            let b: String = b.chars().take_while(|c| is_ident(*c)).collect();
            (!b.is_empty()).then_some(b)
        })
        .collect();
    Some((name, bases))
}

/// A class-level statement that declares a virtual function: the word `virtual`, or `override`.
fn method(stmt: &str) -> Option<Method> {
    let at = stmt.match_indices("virtual").find(|(i, _)| {
        let before = stmt[..*i].chars().next_back();
        let after = stmt[i + 7..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    });
    let decl = match at {
        Some((i, _)) => &stmt[i + 7..],
        // `int f() override;` is virtual without saying so
        None if stmt.split(|c: char| !is_ident(c)).any(|w| w == "override") => {
            stmt.rsplit(':').next().unwrap_or(stmt)
        }
        None => return None,
    };
    let open = decl.find('(')?;
    let head = decl[..open].trim_end();
    let name: String = head
        .chars()
        .rev()
        .take_while(|c| is_ident(*c) || *c == '~')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if name.is_empty() {
        return None;
    }
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in decl[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let inside = decl[open + 1..close].trim();
    let params = if inside.is_empty() || inside == "void" {
        0
    } else {
        let (mut d, mut n) = (0i32, 1usize);
        for c in inside.chars() {
            match c {
                '(' | '<' | '[' => d += 1,
                ')' | '>' | ']' => d -= 1,
                ',' if d == 0 => n += 1,
                _ => {}
            }
        }
        n
    };
    let sig = param_types(inside);
    let tail = &decl[close + 1..];
    let overrides = tail.split(|c: char| !is_ident(c)).any(|w| w == "override");
    Some(Method {
        dtor: name.starts_with('~'),
        name,
        params,
        overrides,
        sig,
        line: 0,
        end_line: 0,
        owner: String::new(),
    })
}

/// A parameter list reduced to its types: defaults off, the parameter's name off (the last
/// identifier of a parameter that has more before it), no spaces.
fn param_types(inside: &str) -> String {
    if inside.is_empty() || inside == "void" {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let (mut d, mut cur) = (0i32, String::new());
    for c in inside.chars() {
        match c {
            '(' | '<' | '[' => d += 1,
            ')' | '>' | ']' => d -= 1,
            ',' if d == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
        .iter()
        .map(|p| {
            let p = p.split('=').next().unwrap_or(p).trim();
            let bare = p.trim_end_matches(is_ident).trim_end();
            let kept = if bare.is_empty() { p } else { bare };
            kept.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Headers {
    /// Every `.h` / `.hpp` under `root`, read for `os` (`linux` or `windows`).
    pub fn load(root: &Path, os: &str) -> Result<Headers> {
        Headers::load_patched(root, os, &BTreeMap::new())
    }

    /// As `load`, with the text of some files (keyed by path relative to `root`) taken from
    /// `patched` instead of the disk.
    pub fn load_patched(
        root: &Path,
        os: &str,
        patched: &BTreeMap<String, String>,
    ) -> Result<Headers> {
        let mut h = Headers::default();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let rd = match std::fs::read_dir(&d) {
                Ok(rd) => rd,
                Err(e) if d == root => {
                    return Err(e).with_context(|| format!("reading {}", d.display()))
                }
                Err(_) => {
                    h.unreadable += 1;
                    continue;
                }
            };
            for e in rd {
                let Ok(e) = e else {
                    h.unreadable += 1;
                    continue;
                };
                let Ok(ft) = e.file_type() else {
                    h.unreadable += 1;
                    continue;
                };
                let path = e.path();
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                if ft.is_dir() {
                    if !name.starts_with('.') && name != "thirdparty" && name != "protobuf" {
                        stack.push(path);
                    }
                } else if ft.is_file() && (name.ends_with(".h") || name.ends_with(".hpp")) {
                    let Ok(bytes) = std::fs::read(&path) else {
                        h.unreadable += 1;
                        continue;
                    };
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    match patched.get(&rel) {
                        Some(text) => h.parse(text, &rel, os),
                        None => h.parse(&String::from_utf8_lossy(&bytes), &rel, os),
                    }
                    h.files += 1;
                }
            }
        }
        Ok(h)
    }

    /// Adds the classes of one header. A class seen twice keeps the declaration with more
    /// virtual functions (a forward stub must not shadow the real one).
    pub fn parse(&mut self, text: &str, file: &str, os: &str) {
        let defined = if os == "windows" {
            WINDOWS_MACROS
        } else {
            LINUX_MACROS
        };
        let (text, doubt) = preprocess(&strip_comments(text), defined);
        // one entry per open brace: the class it opened, if any
        let mut scopes: Vec<Option<usize>> = Vec::new();
        let mut found: Vec<Class> = Vec::new();
        let mut stmt = String::new();
        // inside a class: the statement an inline body interrupted, and the depth it began at
        let mut pending: Option<(String, usize, usize)> = None;
        let mut line = 1usize;
        // where the statement being read began: its first character, and its `virtual`
        let (mut first_line, mut virt_line) = (None::<usize>, None::<usize>);
        let place = |mut m: Method, from: usize, to: usize, owner: &str| {
            m.line = from;
            m.end_line = to;
            m.owner = owner.to_string();
            m
        };
        for c in text.chars() {
            match c {
                '\n' => {
                    line += 1;
                    stmt.push(' ');
                }
                '{' => {
                    let head = class_head(&stmt);
                    let in_class = matches!(scopes.last(), Some(Some(_)));
                    match head {
                        Some((name, bases)) => {
                            found.push(Class {
                                name,
                                file: file.to_string(),
                                line,
                                bases,
                                methods: Vec::new(),
                                end_line: line,
                                uncertain: false,
                            });
                            scopes.push(Some(found.len() - 1));
                        }
                        None => {
                            if in_class && pending.is_none() {
                                pending = Some((
                                    stmt.clone(),
                                    scopes.len(),
                                    virt_line.or(first_line).unwrap_or(line),
                                ));
                            }
                            scopes.push(None);
                        }
                    }
                    stmt.clear();
                    (first_line, virt_line) = (None, None);
                }
                '}' => {
                    if let Some(Some(ci)) = scopes.pop() {
                        found[ci].end_line = line;
                    }
                    stmt.clear();
                    (first_line, virt_line) = (None, None);
                    if let Some((p, depth, from)) = &pending {
                        if scopes.len() == *depth {
                            if let (Some(Some(ci)), Some(m)) = (scopes.last(), method(p)) {
                                let owner = found[*ci].name.clone();
                                found[*ci].methods.push(place(m, *from, line, &owner));
                            }
                            pending = None;
                        }
                    }
                }
                ';' => {
                    if pending.is_none() {
                        if let (Some(Some(ci)), Some(m)) = (scopes.last(), method(&stmt)) {
                            let owner = found[*ci].name.clone();
                            let from = virt_line.or(first_line).unwrap_or(line);
                            found[*ci].methods.push(place(m, from, line, &owner));
                        }
                    }
                    stmt.clear();
                    (first_line, virt_line) = (None, None);
                }
                _ => {
                    if first_line.is_none() && !c.is_whitespace() {
                        first_line = Some(line);
                    }
                    stmt.push(c);
                    if virt_line.is_none() && stmt.ends_with("virtual") {
                        virt_line = Some(line);
                    }
                }
            }
        }
        for mut c in found {
            c.uncertain = doubt
                .iter()
                .any(|&(from, to)| from <= c.end_line.max(c.line) && c.line <= to);
            match self.classes.get(&c.name) {
                Some(have) if have.methods.len() >= c.methods.len() => {}
                _ => {
                    self.classes.insert(c.name.clone(), c);
                }
            }
        }
    }

    /// The primary table of `name` as the headers describe it. `None` when the class is not in
    /// the headers or declares and inherits no virtual function.
    pub fn layout(&self, name: &str, os: &str) -> Option<Layout> {
        let mut methods: Vec<Method> = Vec::new();
        let mut exact = true;
        self.collect(name, &mut methods, &mut exact, os != "windows", 0)?;
        if methods.is_empty() {
            return None;
        }
        let mut slots = Vec::new();
        let mut decl = Vec::new();
        for m in &methods {
            let d = |half: u8| Decl {
                class: m.owner.clone(),
                file: self
                    .classes
                    .get(&m.owner)
                    .map(|c| c.file.clone())
                    .unwrap_or_default(),
                line: m.line,
                end_line: m.end_line,
                half,
            };
            if m.dtor && os != "windows" {
                // Itanium: the complete-object destructor, then the deleting one
                slots.push(format!("{} [complete]", m.name));
                slots.push(format!("{} [deleting]", m.name));
                decl.push(d(1));
                decl.push(d(2));
            } else {
                slots.push(m.name.clone());
                decl.push(d(0));
            }
        }
        if os == "windows" {
            // MSVC lays a group of overloads out together and in reverse: the names of a class
            // with overloads do not line up with declaration order
            let mut names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
            names.sort_unstable();
            if names.windows(2).any(|w| w[0] == w[1]) {
                exact = false;
            }
        }
        Some(Layout { slots, decl, exact })
    }

    fn collect(
        &self,
        name: &str,
        out: &mut Vec<Method>,
        exact: &mut bool,
        itanium: bool,
        depth: u32,
    ) -> Option<()> {
        let c = self.classes.get(name)?;
        if depth > 24 {
            return None;
        }
        if c.uncertain {
            *exact = false;
        }
        // the primary base is the first base with a table; bases the headers do not know make
        // the layout a guess
        let mut primary_done = false;
        for b in &c.bases {
            if primary_done {
                break;
            }
            let before = out.len();
            match self.collect(b, out, exact, itanium, depth + 1) {
                Some(()) if out.len() > before => primary_done = true,
                Some(()) => {}
                None => *exact = false,
            }
        }
        // only what the bases declared can be overridden: two own functions of one name and one
        // parameter count are overloads, each with a slot
        let from_bases = out.len();
        for m in &c.methods {
            let inherited = out[..from_bases].iter().any(|o| {
                (m.dtor && o.dtor) || (!m.dtor && o.name == m.name && o.params == m.params)
            });
            if inherited {
                // the same name and parameter count, but the types read differently: this may
                // be an overload with a slot of its own, not an override
                let differs = out[..from_bases].iter().any(|o| {
                    !m.dtor && !o.dtor && o.name == m.name && o.params == m.params && o.sig != m.sig
                });
                if differs && !m.overrides {
                    *exact = false;
                }
                continue;
            }
            if m.overrides {
                // an override of a function that only a non-primary base declares: the Itanium ABI
                // gives it a slot in the primary table as well, MSVC does not
                let elsewhere = c.bases.iter().any(|b| self.declares(b, m, 0));
                if !(itanium && elsewhere) {
                    continue;
                }
            }
            out.push(m.clone());
        }
        Some(())
    }
}

impl Headers {
    /// Does `class` or one of its bases declare this virtual function?
    fn declares(&self, class: &str, m: &Method, depth: u32) -> bool {
        let Some(c) = self.classes.get(class) else {
            return false;
        };
        depth < 24
            && (c
                .methods
                .iter()
                .any(|o| (m.dtor && o.dtor) || (o.name == m.name && o.params == m.params))
                || c.bases.iter().any(|b| self.declares(b, m, depth + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(text: &str, os: &str) -> Headers {
        let mut h = Headers::default();
        h.parse(text, "x.h", os);
        h
    }

    const SAMPLE: &str = r#"
        class Forward;
        abstract_class IBase
        {
        public:
            virtual ~IBase() {}
            virtual void A() = 0;            // first
            virtual int B( int x, const char *y = "a;b{" ) = 0;
            /* virtual void Gone() = 0; */
            void NotVirtual();
        };
        class IDerived : public IBase
        {
        public:
            virtual void A() override;
            virtual int B( int x, const char *y );
            virtual void B();
            virtual void C() { if ( true ) { call(); } }
            virtual void C( int );
            virtual void C( float );
            virtual void D() = 0;
        #ifdef _WIN32
            virtual void WinOnly() = 0;
        #else
            virtual void LinuxOnly() = 0;
        #endif
        };
        enum class Kind { A, B };
        struct Plain { int x; };
    "#;

    #[test]
    fn declaration_order_overrides_and_destructors() {
        let h = headers(SAMPLE, "linux");
        let base = h.layout("IBase", "linux").unwrap();
        assert_eq!(
            base.slots,
            ["~IBase [complete]", "~IBase [deleting]", "A", "B"]
        );
        assert!(base.exact);
        let d = h.layout("IDerived", "linux").unwrap();
        // A and the two-parameter B override; the parameterless B is an overload and a new slot
        assert_eq!(
            d.slots,
            [
                "~IBase [complete]",
                "~IBase [deleting]",
                "A",
                "B",
                "B",
                "C",
                "C",
                "C",
                "D",
                "LinuxOnly"
            ]
        );
        assert!(h.layout("Plain", "linux").is_none());
        assert!(h.layout("Kind", "linux").is_none());
        assert!(h.layout("Forward", "linux").is_none());
    }

    #[test]
    fn windows_takes_its_own_branch_and_one_destructor_slot() {
        let h = headers(SAMPLE, "windows");
        let d = h.layout("IDerived", "windows").unwrap();
        assert_eq!(d.slots.len(), 9);
        assert_eq!(d.slots[0], "~IBase");
        assert_eq!(d.slots.last().unwrap(), "WinOnly");
        // two functions called B: MSVC's order is not the declaration order
        assert!(!d.exact);
    }

    #[test]
    fn an_unknown_base_makes_the_layout_inexact() {
        let h = headers(
            "class X : public Unknown { public: virtual void F(); };",
            "linux",
        );
        let l = h.layout("X", "linux").unwrap();
        assert_eq!(l.slots, ["F"]);
        assert!(!l.exact);
    }

    #[test]
    fn templates_and_nested_classes() {
        let h = headers(
            "template < class T, int N > class Outer : public IThing { public: \
             struct Inner { virtual void I(); }; virtual void O(); };",
            "linux",
        );
        assert_eq!(h.layout("Inner", "linux").unwrap().slots, ["I"]);
        assert_eq!(h.layout("Outer", "linux").unwrap().slots, ["O"]);
    }

    #[test]
    fn an_override_of_a_second_base_gets_a_primary_slot_on_linux_only() {
        let text = "struct A { virtual void a(); }; struct M { virtual void m(); };
                    struct Both : A, M { void a() override; void m() override; };";
        let h = headers(text, "linux");
        assert_eq!(h.layout("Both", "linux").unwrap().slots, ["a", "m"]);
        let h = headers(text, "windows");
        assert_eq!(h.layout("Both", "windows").unwrap().slots, ["a"]);
    }

    #[test]
    fn preprocessor_expressions() {
        assert_eq!(
            pp_cond("defined( _WIN32 ) || defined(LINUX)", LINUX_MACROS),
            Some(true)
        );
        assert_eq!(pp_cond("!defined(_WIN32)", WINDOWS_MACROS), Some(false));
        assert_eq!(pp_cond("0", LINUX_MACROS), Some(false));
        assert_eq!(pp_cond("FOO > 3", LINUX_MACROS), None);
    }

    #[test]
    fn a_condition_the_reader_cannot_decide_makes_the_class_a_guess() {
        let text = "
            class Sure { virtual void a(); };
            class Unsure {
            #if SOME_VERSION >= 3
                virtual void extra();
            #endif
                virtual void b();
            };
            class Decided {
            #if defined(_WIN32) && SOME_VERSION >= 3
                virtual void win_only();
            #endif
                virtual void c();
            };";
        let h = headers(text, "linux");
        assert!(h.layout("Sure", "linux").unwrap().exact);
        assert!(!h.layout("Unsure", "linux").unwrap().exact);
        // false && unknown is false: nothing to guess on Linux
        let d = h.layout("Decided", "linux").unwrap();
        assert!(d.exact);
        assert_eq!(d.slots, vec!["c"]);
    }

    #[test]
    fn the_same_name_with_other_parameter_types_is_not_taken_for_an_override() {
        let text = "
            class B { virtual void f(int x); virtual void g(const char *name); };
            class Same : public B { virtual void f(int value); virtual void g(const char *p); };
            class Other : public B { virtual void f(float x); };";
        let h = headers(text, "linux");
        assert!(
            h.layout("Same", "linux").unwrap().exact,
            "names of parameters do not matter"
        );
        assert!(!h.layout("Other", "linux").unwrap().exact);
    }
}
