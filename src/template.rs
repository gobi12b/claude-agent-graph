//! Run inputs and `{{ … }}` templating in prompts, commands, checks, judge criteria and delivery settings.
//!
//! The grammar is deliberately small, so it reads the same everywhere and can't run code:
//!   {{ inputs.bug }}                      a run input
//!   {{ steps.find.output }}               the latest result of a step (its last 4000 characters)
//!   {{ run.id }}  {{ run.folder }}  {{ run.project }}  {{ run.workflow }}
//!   {{ inputs.bug | default("none") }}    filters: default("…"), slug, trim, json
//!   {{ "{{" }}                            a quoted string, e.g. to write a literal {{
//! In shell commands and checks every value is single-quoted, so an input can't inject shell syntax.

use serde_json::{json, Map, Value};

use crate::util::*;

pub const INPUT_TYPES: [&str; 5] = ["string", "text", "number", "boolean", "choice"];
const FILTERS: [&str; 4] = ["default", "slug", "trim", "json"];
const RUN_FIELDS: [&str; 4] = ["id", "folder", "project", "workflow"];

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Path(Vec<String>),
    Lit(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Filter {
    name: String,
    arg: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    Sub(Expr, Vec<Filter>),
}

fn parse_string(chars: &[char], i: &mut usize) -> Res<String> {
    let quote = chars[*i];
    *i += 1;
    let mut out = String::new();
    while *i < chars.len() {
        let c = chars[*i];
        *i += 1;
        if c == '\\' && *i < chars.len() {
            out.push(chars[*i]);
            *i += 1;
        } else if c == quote {
            return Ok(out);
        } else {
            out.push(c);
        }
    }
    err("a quoted string isn't closed")
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i].is_whitespace() {
        *i += 1;
    }
}

fn ident(chars: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_' || chars[*i] == '-') {
        *i += 1;
    }
    chars[start..*i].iter().collect()
}

fn parse_expr(src: &str) -> Res<(Expr, Vec<Filter>)> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    skip_ws(&chars, &mut i);
    let expr = if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
        Expr::Lit(parse_string(&chars, &mut i)?)
    } else {
        let mut path = vec![];
        loop {
            let part = ident(&chars, &mut i);
            if part.is_empty() {
                return Err(format!("“{{{{{src}}}}}” isn't a value I understand (try inputs.<name>)"));
            }
            path.push(part);
            if i < chars.len() && chars[i] == '.' {
                i += 1;
            } else {
                break;
            }
        }
        Expr::Path(path)
    };
    let mut filters = vec![];
    loop {
        skip_ws(&chars, &mut i);
        if i >= chars.len() {
            break;
        }
        if chars[i] != '|' {
            return Err(format!("unexpected “{}” in “{{{{{src}}}}}”", chars[i]));
        }
        i += 1;
        skip_ws(&chars, &mut i);
        let name = ident(&chars, &mut i);
        if !FILTERS.contains(&name.as_str()) {
            return Err(format!("unknown filter “{name}” (use {})", FILTERS.join(", ")));
        }
        skip_ws(&chars, &mut i);
        let mut arg = None;
        if i < chars.len() && chars[i] == '(' {
            i += 1;
            skip_ws(&chars, &mut i);
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                arg = Some(parse_string(&chars, &mut i)?);
            }
            skip_ws(&chars, &mut i);
            if i >= chars.len() || chars[i] != ')' {
                return Err(format!("filter “{name}” needs a quoted value and a closing bracket"));
            }
            i += 1;
        }
        if name == "default" && arg.is_none() {
            return err("default needs a value, e.g. default(\"none\")");
        }
        filters.push(Filter { name, arg });
    }
    Ok((expr, filters))
}

fn parse(text: &str) -> Res<Vec<Part>> {
    let mut parts = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            parts.push(Part::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        let end = find_close(after).ok_or("a “{{” has no matching “}}”")?;
        let (expr, filters) = parse_expr(&after[..end])?;
        parts.push(Part::Sub(expr, filters));
        rest = &after[end + 2..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest.to_string()));
    }
    Ok(parts)
}

/// The "}}" that closes a substitution, skipping any inside quoted strings.
fn find_close(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if s[i..].starts_with("}}") {
            return Some(i);
        }
    }
    None
}

/// Check a template's references against the workflow: its inputs and step ids. Returns the step ids it uses.
pub fn check(text: &str, inputs: &Map<String, Value>, step_ids: &[String]) -> Res<Vec<String>> {
    let mut used = vec![];
    for part in parse(text)? {
        let Part::Sub(Expr::Path(path), _) = part else { continue };
        let at = format!("{{{{ {} }}}}", path.join("."));
        match path[0].as_str() {
            "inputs" => {
                if path.len() != 2 {
                    return Err(format!("{at}: write inputs.<name>"));
                }
                if !inputs.contains_key(&path[1]) {
                    return Err(format!("{at}: there's no input called “{}” (add it under inputs)", path[1]));
                }
            }
            "steps" => {
                if path.len() != 3 || path[2] != "output" {
                    return Err(format!("{at}: write steps.<step id>.output"));
                }
                if !step_ids.contains(&path[1]) {
                    return Err(format!("{at}: there's no step with the id “{}”", path[1]));
                }
                used.push(path[1].clone());
            }
            "run" => {
                if path.len() != 2 || !RUN_FIELDS.contains(&path[1].as_str()) {
                    return Err(format!("{at}: use run.{}", RUN_FIELDS.join(", run.")));
                }
            }
            other => return Err(format!("{at}: “{other}” isn't something a template can use (inputs, steps or run)")),
        }
    }
    Ok(used)
}

pub fn has_template(text: &str) -> bool {
    text.contains("{{")
}

fn lookup<'a>(ctx: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(ctx, |v, k| v.get(k))
}

/// A value as template text: strings as they are, whole numbers without ".0", null as "".
pub fn as_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(t) => t.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.fract() == 0.0 && x.abs() < 1e15 => format!("{}", x as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Fill in a template. ctx: {"inputs": {…}, "steps": {id: {"output": …}}, "run": {…}}.
/// shell: quote each value for bash (commands and checks).
pub fn render(text: &str, ctx: &Value, shell: bool) -> Res<String> {
    if !has_template(text) {
        return Ok(text.to_string());
    }
    let mut out = String::new();
    for part in parse(text)? {
        match part {
            Part::Text(t) => out.push_str(&t),
            Part::Sub(expr, filters) => {
                let mut v = match &expr {
                    Expr::Lit(t) => json!(t),
                    Expr::Path(p) => lookup(ctx, p).cloned().unwrap_or(Value::Null),
                };
                let literal = matches!(expr, Expr::Lit(_));
                for fl in &filters {
                    v = match fl.name.as_str() {
                        "default" if as_text(&v).trim().is_empty() => json!(fl.arg.clone().unwrap_or_default()),
                        "slug" => json!(slug(&as_text(&v), "run", 40)),
                        "trim" => json!(as_text(&v).trim()),
                        "json" => json!(v.to_string()),
                        _ => v,
                    };
                }
                let text = as_text(&v);
                // a bare literal like {{ "{{" }} is the author's own text, not a value to quote
                out.push_str(&if shell && !literal { shell_quote(&text) } else { text });
            }
        }
    }
    Ok(out)
}

// ---- inputs ------------------------------------------------------------------------------------------

/// Validate a workflow's `inputs:` (a mapping of name -> settings, or a list with names) into a clean mapping.
pub fn clean_inputs(raw: Option<&Value>) -> Res<Map<String, Value>> {
    let items: Vec<(String, Value)> = match raw {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Some(Value::Array(a)) => a.iter().map(|v| (s(v, "name"), v.clone())).collect(),
        Some(_) => return err("“inputs” should be a mapping of input names to their settings."),
    };
    let name_re = regex::Regex::new("^[A-Za-z_][A-Za-z0-9_]{0,39}$").unwrap();
    let mut out = Map::new();
    for (name, spec) in items {
        if !name_re.is_match(&name) {
            return Err(format!("Input name “{name}” should be letters, digits and underscores, starting with a letter."));
        }
        if out.contains_key(&name) {
            return Err(format!("Two inputs are called “{name}”."));
        }
        // shorthand: `bug: What's going wrong?` is a required text input with that description
        let spec = match spec {
            Value::String(d) => json!({"description": d, "required": true}),
            Value::Null => json!({}),
            v if v.is_object() => v,
            _ => return Err(format!("Input “{name}” should be a mapping (description, type, required, default, options).")),
        };
        let typ = s_or(&spec, "type", "string");
        if !INPUT_TYPES.contains(&typ.as_str()) {
            return Err(format!("Input “{name}” has an unknown type “{typ}” (use {}).", INPUT_TYPES.join(", ")));
        }
        let options: Vec<String> = arr(&spec, "options").iter().map(as_text).collect();
        if typ == "choice" && options.is_empty() {
            return Err(format!("Input “{name}” is a choice, so it needs options."));
        }
        let default = spec.get("default").cloned().unwrap_or(Value::Null);
        let default = if default.is_null() { default } else { coerce(&name, &typ, &options, &default)? };
        out.insert(name, json!({"description": s(&spec, "description").trim(), "type": typ, "required": b(&spec, "required"),
                                "default": default, "options": options}));
    }
    Ok(out)
}

fn coerce(name: &str, typ: &str, options: &[String], v: &Value) -> Res<Value> {
    let text = as_text(v);
    match typ {
        "number" => match v {
            Value::Number(_) => Ok(v.clone()),
            _ => text.trim().parse::<f64>().map(|x| json!(x)).map_err(|_| format!("Input “{name}” should be a number, not “{text}”.")),
        },
        "boolean" => match v {
            Value::Bool(_) => Ok(v.clone()),
            _ => match text.trim().to_lowercase().as_str() {
                "true" | "yes" | "y" | "1" | "on" => Ok(json!(true)),
                "false" | "no" | "n" | "0" | "off" | "" => Ok(json!(false)),
                _ => Err(format!("Input “{name}” should be true or false, not “{text}”.")),
            },
        },
        "choice" => {
            if options.contains(&text) {
                Ok(json!(text))
            } else {
                Err(format!("Input “{name}” should be one of {}, not “{text}”.", options.join(", ")))
            }
        }
        _ => Ok(json!(text)),
    }
}

/// The values a run uses: what was given, else each input's default. Fails on missing required inputs,
/// listing all of them. Values for inputs the workflow doesn't define are an error too (likely a typo).
pub fn resolve_inputs(defs: &Map<String, Value>, given: &Value) -> Res<Map<String, Value>> {
    let given = given.as_object().cloned().unwrap_or_default();
    if let Some(k) = given.keys().find(|k| !defs.contains_key(*k)) {
        let known: Vec<&String> = defs.keys().collect();
        return Err(if known.is_empty() {
            format!("This workflow has no inputs, but “{k}” was given.")
        } else {
            format!("This workflow has no input called “{k}” (its inputs: {}).", known.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))
        });
    }
    let mut out = Map::new();
    let mut missing = vec![];
    for (name, d) in defs {
        let typ = s(d, "type");
        let options: Vec<String> = str_list(d, "options");
        let v = match given.get(name) {
            Some(v) if !(v.is_null() || (v.is_string() && as_text(v).trim().is_empty() && typ != "boolean")) => coerce(name, &typ, &options, v)?,
            _ => d["default"].clone(),
        };
        if v.is_null() {
            if b(d, "required") {
                missing.push(name.clone());
                continue;
            }
            out.insert(name.clone(), json!(if typ == "boolean" { json!(false) } else { json!("") }));
        } else {
            out.insert(name.clone(), v);
        }
    }
    if !missing.is_empty() {
        return Err(format!("Missing required input{}: {}.", if missing.len() > 1 { "s" } else { "" }, missing.join(", ")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Value {
        json!({"inputs": {"bug": "Login fails on Safari", "n": 3.0, "empty": ""},
               "steps": {"find": {"output": "it's the cookie"}}, "run": {"id": "r1"}})
    }

    #[test]
    fn renders_values_and_filters() {
        let c = ctx();
        assert_eq!(render("Fix: {{ inputs.bug }}", &c, false).unwrap(), "Fix: Login fails on Safari");
        assert_eq!(render("{{inputs.n}}", &c, false).unwrap(), "3");
        assert_eq!(render("{{ inputs.empty | default(\"none\") }}", &c, false).unwrap(), "none");
        assert_eq!(render("agent/{{ run.id }}-{{ inputs.bug | slug }}", &c, false).unwrap(), "agent/r1-login-fails-on-safari");
        assert_eq!(render("{{ steps.find.output }}", &c, false).unwrap(), "it's the cookie");
        assert_eq!(render("{{ \"{{\" }} x }}", &c, false).unwrap(), "{{ x }}");
        assert_eq!(render("no template", &c, false).unwrap(), "no template");
    }

    #[test]
    fn shell_values_are_quoted() {
        let c = json!({"inputs": {"x": "a'; rm -rf / #"}});
        assert_eq!(render("echo {{ inputs.x }}", &c, true).unwrap(), "echo 'a'\\''; rm -rf / #'");
    }

    #[test]
    fn check_reports_unknown_references() {
        let inputs = clean_inputs(Some(&json!({"bug": "What's wrong?"}))).unwrap();
        let ids = vec!["find".to_string()];
        assert_eq!(check("{{ inputs.bug }} {{ steps.find.output }}", &inputs, &ids).unwrap(), vec!["find"]);
        assert!(check("{{ inputs.bgu }}", &inputs, &ids).unwrap_err().contains("no input called"));
        assert!(check("{{ steps.fix.output }}", &inputs, &ids).unwrap_err().contains("no step"));
        assert!(check("{{ env.HOME }}", &inputs, &ids).is_err());
        assert!(check("{{ inputs.bug | upper }}", &inputs, &ids).unwrap_err().contains("unknown filter"));
        assert!(check("{{ inputs.bug", &inputs, &ids).is_err());
    }

    #[test]
    fn inputs_resolve_with_defaults_and_types() {
        let defs = clean_inputs(Some(&json!({
            "bug": "What's wrong?",
            "severity": {"type": "choice", "options": ["low", "high"], "default": "low"},
            "count": {"type": "number"},
            "dry": {"type": "boolean"},
        })))
        .unwrap();
        let v = resolve_inputs(&defs, &json!({"bug": "x", "count": "2"})).unwrap();
        assert_eq!(Value::Object(v), json!({"bug": "x", "severity": "low", "count": 2.0, "dry": false}));
        assert!(resolve_inputs(&defs, &json!({})).unwrap_err().contains("Missing required input: bug"));
        assert!(resolve_inputs(&defs, &json!({"bug": "x", "severity": "mid"})).is_err());
        assert!(resolve_inputs(&defs, &json!({"bug": "x", "sevrity": "low"})).unwrap_err().contains("no input called"));
        assert!(clean_inputs(Some(&json!({"x": {"type": "choice"}}))).is_err());
    }
}
