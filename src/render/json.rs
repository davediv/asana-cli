//! gh's JSON output flags, shared by every command that has a `--json`:
//! `--json [fields]` selects the top-level keys to print (all of them when
//! no list is given) and `-q, --jq <expression>` filters the result through
//! a jq program (the `jaq` crates, so no jq binary is needed).
//!
//! A piece adopts them by flattening [`JsonArgs`] into its `Args`, calling
//! [`JsonArgs::check`] from its `Prevalidate` (so a bad field or a lone
//! `--jq` fails before the token is read, as gh's do), and handing the raw
//! API value to [`print`] with the fields it allows. The same list, as a
//! `JSON_FIELDS` constant on its `Prevalidate` impl, is what `--help`
//! prints under JSON FIELDS.
//!
//! Output is pretty JSON without colour, like every `--json` here; under
//! `--jq` each result prints on its own, a string result bare (no quotes)
//! the way `gh --jq` prints it, anything else as pretty JSON.

// Shared helpers the pieces adopt one loop after they land.
#![allow(dead_code)]

use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context as _, Result};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

/// `--json [fields]` and `-q, --jq <expression>`, for `#[command(flatten)]`.
///
/// `--json` alone prints every field; `--json gid,name` (or `--json gid
/// --json name`) keeps those. A dotted field (`assignee.name`) keeps that
/// key inside its parent. `--jq` needs `--json`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct JsonArgs {
    /// Output JSON with the specified fields
    #[arg(
        long,
        value_name = "fields",
        num_args = 0..=1,
        default_missing_value = "",
        action = clap::ArgAction::Append
    )]
    pub json: Vec<String>,

    /// Filter JSON output using a jq expression
    #[arg(short = 'q', long, value_name = "expression")]
    pub jq: Option<String>,
}

impl JsonArgs {
    /// `--json` was given, with or without fields.
    pub fn is_json(&self) -> bool {
        !self.json.is_empty()
    }

    /// The fields asked for, in order and without duplicates; empty means
    /// all of them.
    pub fn fields(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for f in self.json.iter().flat_map(|s| s.split(',')) {
            let f = f.trim();
            if !f.is_empty() && !out.contains(&f) {
                out.push(f);
            }
        }
        out
    }

    /// gh's checks, for `Prevalidate`: `--jq` without `--json`, and a field
    /// outside `allowed` (a dotted field is judged by its first segment).
    /// Both are plain errors (gh prints them without the usage block).
    pub fn check(&self, allowed: &[&str]) -> Result<()> {
        if self.jq.is_some() && !self.is_json() {
            bail!("cannot use `--jq` without specifying `--json`");
        }
        check_fields(&self.fields(), allowed)
    }
}

/// gh's `Unknown JSON field: "foo"` followed by the allowed list, one per
/// line.
pub fn check_fields(fields: &[&str], allowed: &[&str]) -> Result<()> {
    for field in fields {
        let head = field.split('.').next().unwrap_or(field);
        if !allowed.contains(&head) {
            let mut msg = format!("Unknown JSON field: \"{field}\"\nAvailable fields:");
            let mut sorted: Vec<&str> = allowed.to_vec();
            sorted.sort_unstable();
            for a in sorted {
                let _ = write!(msg, "\n  {a}");
            }
            return Err(anyhow!(msg));
        }
    }
    Ok(())
}

/// `value` with only `fields` kept: on an object the named top-level keys
/// (a missing one is `null`, as gh prints it), on an array each object in
/// it. A dotted field keeps only that part of its parent (`assignee.name`
/// gives `{"assignee": {"name": ..}}`; two dotted fields under one parent
/// merge), through arrays too (`projects.name`). Anything else is left as
/// it is.
pub fn select(value: &Value, fields: &[&str]) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| select(v, fields)).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for field in fields {
                let (head, rest) = match field.split_once('.') {
                    Some((h, r)) => (h, Some(r)),
                    None => (*field, None),
                };
                let picked = match (map.get(head), rest) {
                    (None, _) => Value::Null,
                    (Some(v), None) => v.clone(),
                    (Some(v), Some(rest)) => prune(v, rest),
                };
                match out.get_mut(head) {
                    Some(existing) => merge(existing, picked),
                    None => {
                        out.insert(head.to_string(), picked);
                    }
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `value` reduced to the dotted `path` inside it: the key on an object,
/// each element's on an array, `null` where the path leads nowhere.
fn prune(value: &Value, path: &str) -> Value {
    match value {
        Value::Object(_) => select(value, &[path]),
        Value::Array(items) => Value::Array(items.iter().map(|v| prune(v, path)).collect()),
        Value::Null => Value::Null,
        other => other.clone(),
    }
}

/// Fold `add` into `into`: objects key by key, arrays element by element,
/// anything else replaced.
fn merge(into: &mut Value, add: Value) {
    match (into, add) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        a.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (x, y) in a.iter_mut().zip(b) {
                merge(x, y);
            }
        }
        (into, add) => *into = add,
    }
}

/// Print `value` on stdout as `args` ask: the fields selected, the jq
/// expression applied, pretty JSON without colour. `allowed` is the
/// command's field list, checked here as well as in `check` so a piece
/// that forgot the latter still refuses an unknown field.
pub fn print(value: &Value, args: &JsonArgs, allowed: &[&str]) -> Result<()> {
    let text = render(value, args, allowed)?;
    print!("{text}");
    Ok(())
}

/// What [`print`] prints.
pub fn render(value: &Value, args: &JsonArgs, allowed: &[&str]) -> Result<String> {
    args.check(allowed)?;
    let fields = args.fields();
    let selected;
    let value = if fields.is_empty() {
        value
    } else {
        selected = select(value, &fields);
        &selected
    };
    let Some(expr) = args.jq.as_deref() else {
        return Ok(format!("{}\n", pretty(value)?));
    };
    let mut out = String::new();
    for result in jq(expr, value)? {
        match result {
            Val::TStr(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
            other => {
                let compact = other.to_string();
                match serde_json::from_str::<Value>(&compact) {
                    Ok(v) => out.push_str(&pretty(&v)?),
                    Err(_) => out.push_str(&compact),
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn pretty(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).context("could not serialise to JSON")
}

/// Run the jq program `expr` over `input` and collect its outputs.
pub fn jq(expr: &str, input: &Value) -> Result<Vec<Val>> {
    let expr = expr.trim();
    if expr.is_empty() {
        bail!("invalid jq expression: it is empty");
    }
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let arena = Arena::default();
    let modules = Loader::new(defs)
        .load(
            &arena,
            File {
                code: expr,
                path: (),
            },
        )
        .map_err(|errs| load_error(expr, errs))?;
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(compile_error)?;
    let text = serde_json::to_string(input).context("could not serialise to JSON")?;
    let input = jaq_json::read::parse_single(text.as_bytes())
        .map_err(|e| anyhow!("could not read the JSON for --jq: {e}"))?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut outs = Vec::new();
    for out in filter.id.run((ctx, input)).map(unwrap_valr) {
        outs.push(out.map_err(|e| anyhow!("jq: {e}"))?);
    }
    Ok(outs)
}

/// `invalid jq expression: expected <what> near "<found>"`, from the first
/// lex or parse error jaq reports.
fn load_error(expr: &str, errs: jaq_core::load::Errors<&str, ()>) -> anyhow::Error {
    use jaq_core::load::Error;
    let mut what: Option<(String, String)> = None;
    for (_, err) in &errs {
        let found = match err {
            Error::Io(items) => items
                .first()
                .map(|(path, msg)| (format!("could not load {path}: {msg}"), String::new())),
            Error::Lex(items) => items.first().map(|(expect, found)| {
                (format!("expected {}", expect.as_str()), found.to_string())
            }),
            Error::Parse(items) => items.first().map(|(expect, found)| {
                (format!("expected {}", expect.as_str()), found.to_string())
            }),
        };
        if found.is_some() {
            what = found;
            break;
        }
    }
    let (what, found) = what.unwrap_or_else(|| ("could not parse it".to_string(), String::new()));
    let found = found.trim();
    if found.is_empty() {
        anyhow!("invalid jq expression: {what} at the end of \"{expr}\"")
    } else {
        anyhow!("invalid jq expression: {what} near \"{found}\"")
    }
}

/// `invalid jq expression: undefined filter "foo"`, from the first symbol
/// jaq could not resolve.
fn compile_error(errs: jaq_core::compile::Errors<&str, ()>) -> anyhow::Error {
    let first = errs
        .iter()
        .flat_map(|(_, list)| list.iter())
        .next()
        .map(|(name, undefined)| format!("undefined {} \"{name}\"", undefined.as_str()))
        .unwrap_or_else(|| "it could not be compiled".to_string());
    anyhow!("invalid jq expression: {first}")
}

/// The JSON FIELDS list for a help screen: gh's `text.FormatSlice`, the
/// names sorted and joined with `, `, wrapped so that no line (indent
/// included) is wider than `limit`, each line under `indent`.
pub fn format_fields(fields: &[&str], indent: &str, limit: usize) -> String {
    let mut names: Vec<&str> = fields.to_vec();
    names.sort_unstable();
    names.dedup();
    let mut out = String::new();
    let mut line = String::new();
    let last = names.len().saturating_sub(1);
    for (i, name) in names.iter().enumerate() {
        let piece = if i == last {
            name.to_string()
        } else {
            format!("{name},")
        };
        if line.is_empty() {
            line.push_str(indent);
            line.push_str(&piece);
            continue;
        }
        if line.chars().count() + 1 + piece.chars().count() > limit {
            out.push_str(&line);
            out.push('\n');
            line.clear();
            line.push_str(indent);
            line.push_str(&piece);
        } else {
            line.push(' ');
            line.push_str(&piece);
        }
    }
    if !line.is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::json;

    use super::*;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        json: JsonArgs,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        task: Option<String>,
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("asana").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn json_flag_takes_an_optional_field_list() {
        let none = parse(&[]);
        assert!(!none.json.is_json());
        assert!(none.json.fields().is_empty());

        let bare = parse(&["--json"]);
        assert!(bare.json.is_json());
        assert!(bare.json.fields().is_empty());

        let some = parse(&["--json", "gid,name", "--limit", "3"]);
        assert_eq!(some.json.fields(), vec!["gid", "name"]);
        assert_eq!(some.limit, 3);

        // gh's StringSlice: repeated flags accumulate; spaces and repeats fold.
        let twice = parse(&["--json", "gid", "--json", " name , gid"]);
        assert_eq!(twice.json.fields(), vec!["gid", "name"]);

        // A following flag is not swallowed as the field list.
        let flag_after = parse(&["--json", "--limit", "2"]);
        assert!(flag_after.json.is_json());
        assert!(flag_after.json.fields().is_empty());
        assert_eq!(flag_after.limit, 2);

        let jq = parse(&["--json", "gid", "-q", ".[].gid"]);
        assert_eq!(jq.json.jq.as_deref(), Some(".[].gid"));
        let jq = parse(&["--jq", ".[]", "--json"]);
        assert_eq!(jq.json.jq.as_deref(), Some(".[]"));
        assert!(jq.json.is_json());
    }

    #[test]
    fn jq_needs_json_and_fields_must_be_known() {
        let lone = parse(&["--jq", "."]);
        assert_eq!(
            lone.json.check(&["gid"]).unwrap_err().to_string(),
            "cannot use `--jq` without specifying `--json`"
        );
        let bad = parse(&["--json", "gid,nope"]);
        assert_eq!(
            bad.json.check(&["name", "gid"]).unwrap_err().to_string(),
            "Unknown JSON field: \"nope\"\nAvailable fields:\n  gid\n  name"
        );
        let dotted = parse(&["--json", "assignee.name"]);
        assert!(dotted.json.check(&["assignee"]).is_ok());
        assert!(dotted.json.check(&["name"]).is_err());
        assert!(parse(&["--json"]).json.check(&[]).is_ok());
        assert!(parse(&[]).json.check(&[]).is_ok());
    }

    #[test]
    fn select_keeps_named_keys_on_objects_and_arrays() {
        let task = json!({
            "gid": "1",
            "name": "Fix",
            "assignee": { "gid": "9", "name": "Jane", "email": "j@x.y" },
            "projects": [{ "gid": "5", "name": "P" }],
            "followers": []
        });
        assert_eq!(
            select(&task, &["gid", "name"]),
            json!({ "gid": "1", "name": "Fix" })
        );
        // A missing field is null, as gh prints it.
        assert_eq!(
            select(&task, &["gid", "due_on"]),
            json!({ "gid": "1", "due_on": null })
        );
        // Dotted paths keep the parent's shape and merge under it.
        assert_eq!(
            select(&task, &["assignee.name", "assignee.gid", "projects.name"]),
            json!({
                "assignee": { "gid": "9", "name": "Jane" },
                "projects": [{ "name": "P" }]
            })
        );
        let list = json!([task, { "gid": "2", "name": "Two", "assignee": null }]);
        assert_eq!(
            select(&list, &["gid", "assignee.name"]),
            json!([
                { "gid": "1", "assignee": { "name": "Jane" } },
                { "gid": "2", "assignee": null }
            ])
        );
        // Order is the order asked for.
        let picked = select(&task, &["name", "gid"]);
        let keys: Vec<&String> = picked.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 2);
        // Scalars are left alone.
        assert_eq!(select(&json!("x"), &["gid"]), json!("x"));
        assert_eq!(select(&json!(null), &["gid"]), json!(null));
    }

    #[test]
    fn jq_expressions_run_over_the_selected_value() {
        let list = json!([
            { "gid": "1", "name": "Fix the login page", "completed": false },
            { "gid": "2", "name": "Write docs", "completed": true }
        ]);
        let names: Vec<String> = jq(".[].name", &list)
            .unwrap()
            .into_iter()
            .map(|v| v.to_string())
            .collect();
        assert_eq!(names, vec!["\"Fix the login page\"", "\"Write docs\""]);

        let args = parse(&["--json", "gid,name", "--jq", ".[].name"]);
        assert_eq!(
            render(&list, &args.json, &["gid", "name", "completed"]).unwrap(),
            "Fix the login page\nWrite docs\n"
        );
        // Objects and arrays print as pretty JSON, numbers and booleans bare.
        let args = parse(&["--json", "--jq", "map(select(.completed)) | .[0]"]);
        assert_eq!(
            render(&list, &args.json, &["gid", "name", "completed"]).unwrap(),
            "{\n  \"completed\": true,\n  \"gid\": \"2\",\n  \"name\": \"Write docs\"\n}\n"
        );
        let args = parse(&["--json", "--jq", "length, (.[0].completed), null"]);
        assert_eq!(render(&list, &args.json, &[]).unwrap(), "2\nfalse\nnull\n");
        // Standard library filters are there.
        let args = parse(&["--json", "--jq", "[.[] | .gid | tonumber] | add"]);
        assert_eq!(render(&list, &args.json, &[]).unwrap(), "3\n");
        // Selection happens before the filter: an unselected key is gone.
        let args = parse(&["--json", "gid", "--jq", ".[0] | keys"]);
        assert_eq!(
            render(&list, &args.json, &["gid", "name"]).unwrap(),
            "[\n  \"gid\"\n]\n"
        );
        // No fields, no jq: the whole value, pretty.
        let args = parse(&["--json"]);
        assert_eq!(
            render(&json!({ "a": 1 }), &args.json, &["a"]).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn bad_jq_expressions_are_reported_before_anything_prints() {
        let list = json!([{ "gid": "1" }]);
        let err = jq(".[", &list).unwrap_err().to_string();
        assert!(err.starts_with("invalid jq expression: expected "), "{err}");
        let err = jq("nope", &list).unwrap_err().to_string();
        assert_eq!(err, "invalid jq expression: undefined filter \"nope\"");
        let err = jq("   ", &list).unwrap_err().to_string();
        assert_eq!(err, "invalid jq expression: it is empty");
        let err = jq(".[0] | .gid + 1", &list).unwrap_err().to_string();
        assert!(err.starts_with("jq: cannot "), "{err}");
        let err = jq("if 1", &list).unwrap_err().to_string();
        assert!(
            err.starts_with("invalid jq expression: expected then"),
            "{err}"
        );
    }

    /// gh's list, line for line (`reference/gh/issue-list-help.txt`).
    #[test]
    fn json_fields_wrap_like_gh() {
        let fields = [
            "assignees",
            "author",
            "blockedBy",
            "blocking",
            "body",
            "closed",
            "closedAt",
            "closedByPullRequestsReferences",
            "comments",
            "createdAt",
            "id",
            "isPinned",
            "issueType",
            "labels",
            "milestone",
            "number",
            "parent",
            "projectCards",
            "projectItems",
            "reactionGroups",
            "state",
            "stateReason",
            "subIssues",
            "subIssuesSummary",
            "title",
            "updatedAt",
            "url",
        ];
        let expected = [
            "  assignees, author, blockedBy, blocking, body, closed, closedAt,",
            "  closedByPullRequestsReferences, comments, createdAt, id, isPinned, issueType,",
            "  labels, milestone, number, parent, projectCards, projectItems, reactionGroups,",
            "  state, stateReason, subIssues, subIssuesSummary, title, updatedAt, url",
            "",
        ]
        .join("\n");
        assert_eq!(format_fields(&fields, "  ", 80), expected);
        // Unsorted input comes out sorted; one field is one line.
        assert_eq!(format_fields(&["b", "a"], "  ", 80), "  a, b\n");
        assert_eq!(format_fields(&["only"], "  ", 80), "  only\n");
        assert_eq!(format_fields(&[], "  ", 80), "");
        // Narrower terminals wrap sooner; a name never splits.
        let narrow = format_fields(&fields, "  ", 40);
        for line in narrow.lines() {
            assert!(line.len() <= 40, "{line}");
        }
        assert!(narrow.contains("\n  closedByPullRequestsReferences,\n"));
    }
}
