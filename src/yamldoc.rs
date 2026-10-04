//! YAML that you can edit by hand and from the app.
//!
//! Loading goes through serde_yaml into plain JSON values. Saving re-writes the file in one tidy style
//! (mappings indented by 2, list dashes indented by 2 under their key, prompts as `|` blocks, short lists
//! on one line) and merges into the file that is already there: keys keep their order, list entries are
//! matched by `id` / `name`, values that didn't change keep their exact text (quoting, block style), and
//! comments stay attached to the key or list entry they were written above or beside.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

// ---- loading ---------------------------------------------------------------------------------

/// Parse a YAML document into plain values (`null` for an empty file).
/// Errors read "line L, column C: problem".
pub fn parse(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    match serde_yaml::from_str::<serde_yaml::Value>(text) {
        Ok(v) => Ok(to_json(v)),
        Err(e) => {
            let msg = e.to_string();
            match e.location() {
                Some(loc) => {
                    let problem = msg.split(" at line ").next().unwrap_or(&msg).trim_end_matches(',').to_string();
                    Err(format!("line {}, column {}: {}", loc.line(), loc.column(), problem))
                }
                None => Err(msg),
            }
        }
    }
}

fn to_json(v: serde_yaml::Value) -> Value {
    use serde_yaml::Value as Y;
    match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(u) = n.as_u64() {
                u.into()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                serde_json::Number::from_f64(f).map(Value::Number).unwrap_or_else(|| Value::String(n.to_string()))
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(items.into_iter().map(to_json).collect()),
        Y::Mapping(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                let key = match to_json(k) {
                    Value::String(s) => s,
                    Value::Null => "null".into(),
                    other => other.to_string(),
                };
                out.insert(key, to_json(v));
            }
            Value::Object(out)
        }
        Y::Tagged(t) => to_json(t.value),
    }
}

// ---- what the old file looked like: comments and the exact text of each value ------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Seg {
    K(String),
    I(usize),
}
type DocPath = Vec<Seg>;

#[derive(Default, Debug)]
struct Note {
    before: Vec<String>,       // comment and blank lines just above
    inline: Option<String>,    // "  # comment" after the value
    raw: Option<(String, Vec<(usize, String)>)>, // value text on the line, then (extra indent, text) lines below
}

#[derive(Default, Debug)]
struct Layout {
    header: Vec<String>,
    footer: Vec<String>,
    notes: HashMap<DocPath, Note>,
}

#[derive(PartialEq)]
enum Kind {
    Map,
    Seq,
}

struct Frame {
    col: usize,
    kind: Kind,
    path: DocPath,
    next: usize,
    child: Option<DocPath>, // a key or dash whose value continues on the next lines
}

struct Scanner<'a> {
    lines: Vec<&'a str>,
    i: usize,
    stack: Vec<Frame>,
    pending: Vec<String>,
    out: Layout,
    seen_content: bool,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r##"^(?:"((?:[^"\\]|\\.)*)"|'((?:[^']|'')*)'|([^\s'"#\[\]{},&*!|>%@`?-][^#]*?|-[^\s#][^#]*?))[ \t]*:(?:[ \t]+(.*)|$)"##)
            .unwrap()
    })
}

/// Split "value  # comment" into the value and the comment (with its leading spaces).
fn split_comment(rest: &str) -> (String, Option<String>) {
    let chars: Vec<char> = rest.chars().collect();
    let (mut single, mut double, mut esc) = (false, false, false);
    for (i, &c) in chars.iter().enumerate() {
        if double {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                double = false;
            }
            continue;
        }
        if single {
            if c == '\'' {
                single = false;
            }
            continue;
        }
        match c {
            '"' if i == 0 || "[{, :".contains(chars[i - 1]) => double = true,
            '\'' if i == 0 || "[{, :".contains(chars[i - 1]) => single = true,
            '#' if i == 0 || chars[i - 1] == ' ' || chars[i - 1] == '\t' => {
                let value: String = chars[..i].iter().collect::<String>().trim_end().to_string();
                let mut start = i;
                while start > 0 && (chars[start - 1] == ' ' || chars[start - 1] == '\t') {
                    start -= 1;
                }
                let comment: String = chars[start..].iter().collect();
                return (value, Some(if start == 0 { format!(" {comment}") } else { comment }));
            }
            _ => {}
        }
    }
    (rest.trim_end().to_string(), None)
}

impl<'a> Scanner<'a> {
    fn run(text: &'a str) -> Layout {
        let mut s = Scanner { lines: text.lines().collect(), i: 0, stack: vec![], pending: vec![], out: Layout::default(), seen_content: false };
        if s.scan().is_none() {
            // Something this reader doesn't follow (anchors, complex keys…): keep the header, drop the rest.
            s.out.notes.clear();
            s.out.footer.clear();
        }
        s.out
    }

    fn scan(&mut self) -> Option<()> {
        while self.i < self.lines.len() {
            let line = self.lines[self.i];
            if line.contains('\t') && !is_blank_or_comment(line) && line.trim_start_matches(' ').starts_with('\t') {
                return None;
            }
            if is_blank_or_comment(line) {
                self.pending.push(line.trim_end().to_string());
                self.i += 1;
                continue;
            }
            let t = line.trim_start();
            if !self.seen_content && (t.starts_with("---") || t.starts_with('%')) && self.stack.is_empty() {
                self.pending.push(line.trim_end().to_string());
                self.i += 1;
                continue;
            }
            if !self.seen_content {
                self.out.header = std::mem::take(&mut self.pending);
                self.seen_content = true;
            }
            let col = indent_of(line);
            self.i += 1;
            self.node(col, &line[col..])?;
        }
        self.out.footer = std::mem::take(&mut self.pending);
        Some(())
    }

    fn note(&mut self, path: &DocPath) -> &mut Note {
        self.out.notes.entry(path.clone()).or_default()
    }

    fn attach_pending(&mut self, path: &DocPath) {
        let before = std::mem::take(&mut self.pending);
        if !before.is_empty() {
            self.note(path).before.extend(before);
        }
    }

    fn node(&mut self, col: usize, text: &str) -> Option<()> {
        if text == "-" || text.starts_with("- ") {
            return self.dash(col, text);
        }
        if let Some(caps) = key_re().captures(text) {
            let key = if let Some(m) = caps.get(1) {
                unescape_double(m.as_str())
            } else if let Some(m) = caps.get(2) {
                m.as_str().replace("''", "'")
            } else {
                caps.get(3)?.as_str().trim_end().to_string()
            };
            let rest = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            return self.key(col, key, rest);
        }
        None
    }

    fn dash(&mut self, col: usize, text: &str) -> Option<()> {
        while self.stack.last().is_some_and(|f| f.col > col) {
            self.stack.pop();
        }
        let top = self.stack.last_mut()?;
        let path = if top.kind == Kind::Seq && top.col == col {
            let mut p = top.path.clone();
            p.push(Seg::I(top.next));
            top.next += 1;
            p
        } else if top.child.is_some() && (top.col < col || (top.kind == Kind::Map && top.col == col)) {
            let parent = top.child.take()?;
            let mut p = parent.clone();
            p.push(Seg::I(0));
            self.stack.push(Frame { col, kind: Kind::Seq, path: parent, next: 1, child: None });
            p
        } else {
            return None;
        };
        self.attach_pending(&path);
        let after = &text[1..];
        let rest = after.trim_start_matches(' ');
        let inner = col + 1 + (after.len() - rest.len());
        self.stack.last_mut()?.child = Some(path.clone());
        if rest.is_empty() || rest.starts_with('#') {
            if rest.starts_with('#') {
                self.note(&path).inline = Some(format!(" {rest}"));
            }
            return Some(());
        }
        if rest == "-" || rest.starts_with("- ") || key_re().is_match(rest) {
            return self.node(inner, rest);
        }
        self.stack.last_mut()?.child = None;
        self.value(col, &path, rest)
    }

    fn key(&mut self, col: usize, key: String, rest: &str) -> Option<()> {
        while self.stack.last().is_some_and(|f| f.col > col || (f.kind == Kind::Seq && f.col == col)) {
            self.stack.pop();
        }
        let is_sibling = self.stack.last().is_some_and(|f| f.kind == Kind::Map && f.col == col);
        if !is_sibling {
            let parent = match self.stack.last_mut() {
                None if col == 0 => vec![],
                Some(top) if top.col < col && top.child.is_some() => top.child.take()?,
                _ => return None,
            };
            self.stack.push(Frame { col, kind: Kind::Map, path: parent, next: 0, child: None });
        }
        let top = self.stack.last_mut()?;
        let mut path = top.path.clone();
        path.push(Seg::K(key));
        top.child = None;
        self.attach_pending(&path);
        let (value, comment) = split_comment(rest);
        if value.is_empty() {
            if let Some(c) = comment {
                self.note(&path).inline = Some(c);
            }
            self.stack.last_mut()?.child = Some(path);
            return Some(());
        }
        self.value(col, &path, rest)
    }

    /// The value written after "key:" or "- ", plus any lines that belong to it.
    fn value(&mut self, col: usize, path: &DocPath, rest: &str) -> Option<()> {
        let first = rest.trim_start();
        if first.starts_with('&') || first.starts_with('*') || first.starts_with('!') {
            return None;
        }
        let (value, comment) = if first.starts_with('|') || first.starts_with('>') { (first.trim_end().to_string(), None) } else { split_comment(first) };
        let mut cont = vec![];
        let flow = value.starts_with('[') || value.starts_with('{');
        let mut depth = if flow { bracket_depth(&value) } else { 0 };
        let mut j = self.i;
        while j < self.lines.len() {
            let line = self.lines[j];
            if line.trim().is_empty() {
                j += 1;
                continue;
            }
            if flow && depth <= 0 {
                break;
            }
            if indent_of(line) <= col || (line.trim_start().starts_with('#') && !value.starts_with('|') && !value.starts_with('>') && !flow) {
                break;
            }
            if flow {
                depth += bracket_depth(line);
            }
            j += 1;
            // blank lines in between belong to the value too
            for k in self.i..j {
                let l = self.lines[k];
                cont.push(if l.trim().is_empty() { (0, String::new()) } else { (indent_of(l) - col, l.trim_start().trim_end_matches(['\r']).to_string()) });
            }
            self.i = j;
        }
        let note = self.note(path);
        note.raw = Some((value, cont));
        note.inline = comment;
        Some(())
    }
}

fn bracket_depth(s: &str) -> i32 {
    let (mut d, mut quote) = (0, None::<char>);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => d += 1,
                ']' | '}' => d -= 1,
                '#' => break,
                _ => {}
            },
        }
    }
    d
}

fn unescape_double(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

// ---- writing ---------------------------------------------------------------------------------------

/// The YAML text for `new`. When `old_text` is an existing mapping document, merge into it (keeping its
/// comments, order and the text of unchanged values); otherwise start a fresh file with `header` as its comment.
pub fn dump(old_text: Option<&str>, new: &Value, header: &str) -> String {
    let old = old_text.and_then(|t| parse(t).ok()).filter(Value::is_object);
    let layout = match (old_text, &old) {
        (Some(t), Some(_)) => Scanner::run(t),
        _ => Layout {
            header: header
                .trim_end_matches('\n')
                .split('\n')
                .filter(|_| !header.is_empty())
                .map(|l| if l.is_empty() { "#".to_string() } else { format!("# {l}") })
                .collect(),
            ..Layout::default()
        },
    };
    let em = Emitter { layout: &layout, old: old.as_ref() };
    let mut lines = layout.header.clone();
    match new {
        Value::Object(m) if !m.is_empty() => lines.extend(em.map(m, 0, Some(vec![]))),
        other => lines.push(fresh_scalar(other, 0).0),
    }
    lines.extend(layout.footer.iter().cloned());
    lines.join("\n") + "\n"
}

struct Emitter<'a> {
    layout: &'a Layout,
    old: Option<&'a Value>,
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

fn child(path: &Option<DocPath>, seg: Seg) -> Option<DocPath> {
    path.as_ref().map(|p| {
        let mut p = p.clone();
        p.push(seg);
        p
    })
}

/// Python's `old == new` for values that are left untouched (a bool never equals a number).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn item_key(v: &Value) -> Option<String> {
    let o = v.as_object()?;
    o.get("id").filter(|x| crate::util::truthy(x)).or_else(|| o.get("name")).map(crate::util::py_str)
}

impl Emitter<'_> {
    fn old_at(&self, path: &Option<DocPath>) -> Option<&Value> {
        let mut v = self.old?;
        for seg in path.as_ref()? {
            v = match seg {
                Seg::K(k) => v.get(k)?,
                Seg::I(i) => v.get(i)?,
            };
        }
        Some(v)
    }

    fn note(&self, path: &Option<DocPath>) -> Option<&Note> {
        self.layout.notes.get(path.as_ref()?)
    }

    fn map(&self, m: &Map<String, Value>, col: usize, path: Option<DocPath>) -> Vec<String> {
        let old = self.old_at(&path).and_then(Value::as_object);
        // keys already in the file keep their place; new ones go at the end
        let mut keys: Vec<&String> = old.map(|o| o.keys().filter(|k| m.contains_key(*k)).collect()).unwrap_or_default();
        keys.extend(m.keys().filter(|k| !old.is_some_and(|o| o.contains_key(*k))));
        let mut lines = vec![];
        for k in keys {
            let p = if old.is_some_and(|o| o.contains_key(k)) { child(&path, Seg::K(k.clone())) } else { None };
            lines.extend(self.entry(&fmt_key(k), &m[k], col, p));
        }
        lines
    }

    /// "key: value" (or "key:" and the value's lines below it).
    fn entry(&self, key: &str, v: &Value, col: usize, path: Option<DocPath>) -> Vec<String> {
        let note = self.note(&path);
        let old = self.old_at(&path);
        let mut lines: Vec<String> = note.map(|n| n.before.clone()).unwrap_or_default();
        let inline = note.and_then(|n| n.inline.clone()).unwrap_or_default();
        let head = format!("{}{key}:", pad(col));
        let unchanged = old.is_some_and(|o| same(o, v));
        if let (true, Some((first, cont))) = (unchanged, note.and_then(|n| n.raw.as_ref())) {
            lines.push(format!("{head} {first}{inline}"));
            lines.extend(cont.iter().map(|(rel, t)| if t.is_empty() { String::new() } else { format!("{}{t}", pad(col + rel)) }));
            return lines;
        }
        match v {
            Value::Object(m) if !m.is_empty() => {
                lines.push(format!("{head}{inline}"));
                lines.extend(self.map(m, col + 2, path));
            }
            Value::Array(a) if !a.is_empty() && !(flow_ok(a) && !(unchanged && old.is_some_and(Value::is_array))) => {
                lines.push(format!("{head}{inline}"));
                lines.extend(self.seq(a, col + 2, path));
            }
            Value::Null => lines.push(format!("{head}{inline}")),
            _ => {
                let (text, block) = fresh_scalar(v, col);
                lines.push(format!("{head} {text}{inline}"));
                lines.extend(block);
            }
        }
        lines
    }

    fn seq(&self, items: &[Value], col: usize, path: Option<DocPath>) -> Vec<String> {
        let old = self.old_at(&path).and_then(Value::as_array);
        let mut lines = vec![];
        for (i, item) in items.iter().enumerate() {
            // list entries are matched by id / name (like the builder's steps); plain lists by position if unchanged
            let j = match old {
                Some(o) if items.iter().all(Value::is_object) => {
                    let k = item_key(item);
                    o.iter().rposition(|x| x.is_object() && item_key(x) == k)
                }
                Some(o) if o.len() == items.len() && o.iter().zip(items).all(|(a, b)| same(a, b)) => Some(i),
                _ => None,
            };
            let p = j.and_then(|j| child(&path, Seg::I(j)));
            lines.extend(self.item(item, col, p));
        }
        lines
    }

    fn item(&self, v: &Value, col: usize, path: Option<DocPath>) -> Vec<String> {
        let note = self.note(&path);
        let mut lines: Vec<String> = note.map(|n| n.before.clone()).unwrap_or_default();
        let inline = note.and_then(|n| n.inline.clone()).unwrap_or_default();
        let unchanged = self.old_at(&path).is_some_and(|o| same(o, v));
        if let (true, Some((first, cont))) = (unchanged, note.and_then(|n| n.raw.as_ref())) {
            lines.push(format!("{}- {first}{inline}", pad(col)));
            lines.extend(cont.iter().map(|(rel, t)| if t.is_empty() { String::new() } else { format!("{}{t}", pad(col + rel)) }));
            return lines;
        }
        match v {
            Value::Object(m) if !m.is_empty() => {
                let body = self.map(m, col + 2, path);
                // comments above the first key go above the dash
                let first = body.iter().position(|l| !is_blank_or_comment(l)).unwrap_or(0);
                lines.extend(body[..first].iter().cloned());
                for (n, l) in body[first..].iter().enumerate() {
                    lines.push(if n == 0 { format!("{}- {}", pad(col), &l[col + 2..]) } else { l.clone() });
                }
            }
            Value::Array(a) if !a.is_empty() && !flow_ok(a) => {
                let body = self.seq(a, col + 2, None);
                for (n, l) in body.iter().enumerate() {
                    lines.push(if n == 0 { format!("{}- {}", pad(col), &l[col + 2..]) } else { l.clone() });
                }
            }
            _ => {
                let (text, block) = fresh_scalar(v, col);
                lines.push(format!("{}- {text}{inline}", pad(col)));
                lines.extend(block);
            }
        }
        lines
    }
}

/// Short lists of one-line values are written on one line: [Read, Grep]
fn flow_ok(items: &[Value]) -> bool {
    items.iter().all(|x| match x {
        Value::String(s) => !s.contains('\n'),
        Value::Number(_) => true,
        _ => false,
    })
}

fn fmt_key(k: &str) -> String {
    if plain_ok(k, false) { k.to_string() } else { quote(k) }
}

/// A value written fresh: (text after "key: ", lines below it for a block). `col` is the key's column.
fn fresh_scalar(v: &Value, col: usize) -> (String, Vec<String>) {
    match v {
        Value::Null => (String::new(), vec![]),
        Value::Bool(b) => (b.to_string(), vec![]),
        Value::Number(n) => (n.to_string(), vec![]),
        Value::String(s) => {
            if (s.contains('\n') || s.chars().count() > 90) && literal_ok(s) {
                return literal(s, col + 2);
            }
            (scalar(s, false), vec![])
        }
        Value::Array(a) if a.is_empty() => ("[]".into(), vec![]),
        Value::Object(m) if m.is_empty() => ("{}".into(), vec![]),
        Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .map(|x| match x {
                    Value::String(s) => scalar(s, true),
                    other => fresh_scalar(other, col).0,
                })
                .collect();
            (format!("[{}]", parts.join(", ")), vec![])
        }
        Value::Object(_) => (v.to_string(), vec![]), // only reached for nested values in flow lists
    }
}

fn literal_ok(s: &str) -> bool {
    !s.trim().is_empty() && !s.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{feff}')
}

/// A `|` block: chomping keeps the text's trailing newlines exactly.
fn literal(s: &str, indent: usize) -> (String, Vec<String>) {
    let body = s.trim_end_matches('\n');
    let trailing = s.len() - body.len();
    let chomp = match trailing {
        0 => "-",
        1 => "",
        _ => "+",
    };
    let indicator = if body.starts_with(' ') || body.starts_with('\n') { "2" } else { "" };
    let mut lines: Vec<String> = body.split('\n').map(|l| if l.is_empty() { String::new() } else { format!("{}{l}", pad(indent)) }).collect();
    lines.extend(std::iter::repeat_n(String::new(), trailing.saturating_sub(1)));
    (format!("|{indicator}{chomp}"), lines)
}

fn scalar(s: &str, flow: bool) -> String {
    if plain_ok(s, flow) {
        s.to_string()
    } else {
        quote(s)
    }
}

fn quote(s: &str) -> String {
    let special = s.chars().any(|c| c.is_control() || c == '\u{feff}');
    if !special {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() || c == '\u{feff}' => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else {
                    out.push_str(&format!("\\u{n:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Can this string be written without quotes and still read back as the same string?
fn plain_ok(s: &str, flow: bool) -> bool {
    static SPECIAL: OnceLock<Regex> = OnceLock::new();
    let special = SPECIAL.get_or_init(|| {
        Regex::new(
            r"(?x)^(?:
              ~|null|Null|NULL|true|True|TRUE|false|False|FALSE|y|Y|yes|Yes|YES|n|N|no|No|NO|on|On|ON|off|Off|OFF
            | [-+]?(?:\.[0-9]+|[0-9][0-9_]*(?:\.[0-9_]*)?)(?:[eE][-+]?[0-9]+)?
            | 0x[0-9a-fA-F_]+ | 0o[0-7_]+ | 0b[01_]+
            | [-+]?\.(?:inf|Inf|INF) | \.(?:nan|NaN|NAN)
            | [-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?
            | [0-9]{4}-[0-9]{1,2}-[0-9]{1,2}.*
            | <<
            )$",
        )
        .unwrap()
    });
    let Some(first) = s.chars().next() else { return false };
    if s.trim() != s || special.is_match(s) {
        return false;
    }
    if s.chars().any(|c| c.is_control() || c == '\u{feff}') {
        return false;
    }
    if "#,[]{}&*!|>'\"%@`".contains(first) {
        return false;
    }
    if "-?:".contains(first) {
        let second = s.chars().nth(1);
        if second.is_none_or(|c| c == ' ') || (flow && second.is_some_and(|c| ",[]{}".contains(c))) {
            return false;
        }
    }
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return false;
    }
    if flow && s.chars().any(|c| ",[]{}".contains(c)) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fresh_file_reads_back() {
        let v = json!({"name": "Fix: it", "cwd": "~/code", "n": 3, "flag": false, "tags": ["Read", "a, b", "yes"],
                       "steps": [{"id": "a", "prompt": "line one\nline two\n"}, {"id": "b", "prompt": "x".repeat(95)}],
                       "empty": [], "obj": {}, "lead": " spaced", "multi": "  indented\nnext"});
        let text = dump(None, &v, "Header\n\nmore");
        assert!(text.starts_with("# Header\n#\n# more\nname: 'Fix: it'\n"), "{text}");
        assert!(text.contains("tags: [Read, 'a, b', 'yes']"), "{text}");
        assert!(text.contains("  - id: a\n    prompt: |\n      line one\n      line two\n"), "{text}");
        assert_eq!(parse(&text).unwrap(), v);
    }

    #[test]
    fn merge_keeps_comments_and_unchanged_text() {
        let old = "# my header\nname: Flow   # the name\ncwd: \"~/x\"\nsteps:\n  # first step\n  - id: a\n    prompt: |\n      keep\n      me\n  - id: b\n    name: B   # bee\n    prompt: two\n# footer\n";
        let new = json!({"name": "Flow", "cwd": "~/x", "steps": [
            {"id": "b", "name": "B2", "prompt": "two"},
            {"id": "a", "prompt": "keep\nme\n"},
            {"id": "c", "prompt": "new"}]});
        let text = dump(Some(old), &new, "unused");
        assert_eq!(
            text,
            "# my header\nname: Flow   # the name\ncwd: \"~/x\"\nsteps:\n  - id: b\n    name: B2   # bee\n    prompt: two\n  # first step\n  - id: a\n    prompt: |\n      keep\n      me\n  - id: c\n    prompt: new\n# footer\n"
        );
        assert_eq!(parse(&text).unwrap(), new);
    }

    #[test]
    fn indentless_lists_and_flow_lists() {
        let old = "agents:\n  x:\n    tools: [Read,\n      Grep]\nsteps:\n- id: a\n  agents: [x]\n";
        let new = json!({"agents": {"x": {"tools": ["Read", "Grep"]}}, "steps": [{"id": "a", "agents": ["x", "z"]}]});
        let text = dump(Some(old), &new, "");
        assert_eq!(text, "agents:\n  x:\n    tools: [Read,\n      Grep]\nsteps:\n  - id: a\n    agents: [x, z]\n");
        assert_eq!(parse(&text).unwrap(), new);
    }

    #[test]
    fn errors_have_line_and_column() {
        let e = parse("name: a\nsteps: [\n").unwrap_err();
        assert!(e.starts_with("line "), "{e}");
    }
}
