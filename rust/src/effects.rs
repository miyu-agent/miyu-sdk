//! What [`crate::FakeCore`] checks the way the core does (the core's design 63, sections 2 to 4): the declarations in a
//! tool's spec, the paths a call may touch (`checked`) and the effects a reply reports. The Python `FakeCore` has the
//! same checks.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::content_hash;

/// The core reads at most this much of a file for the effects; bigger ones are dropped as `too_big`.
const MOST_BYTES: u64 = 20 * 1024 * 1024;

/// A trash location is at most this many characters.
const MOST_TRASH_CHARS: usize = 4096;

/// The `bad_tool` problem in what `spec` declares about paths, background parameters and commands, if any. `null` is the
/// same as not written.
pub(crate) fn declared_problem(spec: &Value) -> Option<&'static str> {
    let schema = &spec["input_schema"];
    let property = |name: &str| schema["properties"].as_object()?.get(name);
    if let Some(paths) = spec.get("paths").filter(|paths| !paths.is_null()) {
        let Some(paths) = paths.as_array() else {
            return Some("paths");
        };
        let mut names = HashSet::new();
        for one in paths {
            let Some(fields) = one.as_object() else {
                return Some("paths");
            };
            let name = one["parameter"].as_str();
            let write = match one.get("write") {
                None => Some(false),
                Some(write) => write.as_bool(),
            };
            let known = fields
                .keys()
                .all(|key| key == "parameter" || key == "write");
            let (Some(name), Some(write), true) = (name, write, known) else {
                return Some("paths");
            };
            let textual = property(name).is_some_and(|property| match property["type"].as_str() {
                Some("string") => true,
                Some("array") => property["items"]["type"].as_str() == Some("string"),
                _ => false,
            });
            if !textual || !names.insert(name) || (write && spec["access"] != "write") {
                return Some("paths");
            }
        }
    }
    if let Some(background) = spec
        .get("background_parameters")
        .filter(|names| !names.is_null())
    {
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|required| required.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(names) = background.as_array() else {
            return Some("background_parameters");
        };
        let mut distinct = HashSet::new();
        for name in names {
            let fits = name.as_str().is_some_and(|name| {
                property(name).is_some() && !required.contains(&name) && distinct.insert(name)
            });
            if !fits {
                return Some("background_parameters");
            }
        }
    }
    match spec.get("runs_commands") {
        None | Some(Value::Null | Value::Bool(_)) => None,
        Some(_) => Some("runs_commands"),
    }
}

/// Where paths are resolved from: the call's working directory and the home folder.
#[derive(Clone)]
pub(crate) struct Place {
    pub(crate) cwd: PathBuf,
    pub(crate) home: Option<PathBuf>,
}

impl Place {
    /// Where `input` really is: relative to `cwd`, `~` to `home`, links followed; a missing tail is appended to its
    /// nearest existing parent. `None` when it can't be told.
    pub(crate) fn real(&self, input: &str) -> Option<PathBuf> {
        let path = match tilde(input) {
            Some(rest) => self.home.as_ref()?.join(rest),
            None => self.cwd.join(input),
        };
        if let Ok(real) = std::fs::canonicalize(&path) {
            return Some(usable(real));
        }
        let components: Vec<Component<'_>> = path.components().collect();
        for keep in (1..components.len()).rev() {
            let prefix: PathBuf = components[..keep].iter().collect();
            let Ok(mut real) = std::fs::canonicalize(&prefix) else {
                continue;
            };
            for component in &components[keep..] {
                match component {
                    Component::Normal(part) => real.push(part),
                    Component::CurDir => {}
                    _ => return None,
                }
            }
            return Some(usable(real));
        }
        None
    }
}

/// What follows `~` and a separator (or `~` alone), the way the core reads a home-relative path.
fn tilde(input: &str) -> Option<&str> {
    if input == "~" {
        return Some("");
    }
    input
        .strip_prefix("~/")
        .or_else(|| cfg!(windows).then(|| input.strip_prefix("~\\")).flatten())
}

/// Without Windows' `\\?\`, as the core hands `checked` over.
fn usable(real: PathBuf) -> PathBuf {
    let text = real.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => real,
    }
}

/// One path the check allowed: where it really is, and whether for writing.
pub(crate) struct Judged {
    pub(crate) real: PathBuf,
    pub(crate) write: bool,
}

/// The declared paths in `args`, in order: a string gives one, an array each of its strings; anything else none.
pub(crate) fn judge(paths: &Value, args: &Value, place: &Place) -> Vec<Judged> {
    let mut judged = Vec::new();
    for one in paths.as_array().into_iter().flatten() {
        let write = one["write"].as_bool().unwrap_or(false);
        let value = &args[one["parameter"].as_str().unwrap_or_default()];
        let given: Vec<&str> = match value {
            Value::String(path) => vec![path.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        judged.extend(
            given
                .into_iter()
                .filter_map(|path| place.real(path))
                .map(|real| Judged { real, write }),
        );
    }
    judged
}

/// A file at one moment.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Content {
    /// Not there: a file missing before the call is a new one.
    Missing,
    /// The whole of it.
    Bytes(Vec<u8>),
    /// Over [`MOST_BYTES`], not read.
    TooBig,
    /// Not a regular file, or unreadable.
    Unreadable,
}

/// The file at `real` now.
pub(crate) fn copy(real: &Path) -> Content {
    let metadata = match std::fs::metadata(real) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Content::Missing,
        Err(_) => return Content::Unreadable,
    };
    if !metadata.is_file() {
        return Content::Unreadable;
    }
    let mut bytes = Vec::new();
    match std::fs::File::open(real)
        .and_then(|file| file.take(MOST_BYTES + 1).read_to_end(&mut bytes))
    {
        Ok(_) if bytes.len() as u64 > MOST_BYTES => Content::TooBig,
        Ok(_) => Content::Bytes(bytes),
        Err(_) => Content::Unreadable,
    }
}

/// The files the call may write, read before it is sent.
pub(crate) fn befores(judged: &[Judged]) -> HashMap<PathBuf, Content> {
    judged
        .iter()
        .filter(|one| one.write)
        .map(|one| (one.real.clone(), copy(&one.real)))
        .collect()
}

/// What one call's effects came to: the kept ones (paths as real locations) and the dropped ones as
/// `{"tool", "kind", "why"}`.
#[derive(Default)]
pub(crate) struct Vetted {
    pub(crate) kept: Vec<Value>,
    pub(crate) dropped: Vec<Value>,
}

/// What the vetting needs about the call.
pub(crate) struct Context<'a> {
    pub(crate) tool: &'a str,
    pub(crate) place: &'a Place,
    pub(crate) judged: &'a [Judged],
    pub(crate) befores: &'a HashMap<PathBuf, Content>,
}

/// Vet the `effects` of the tool result `result`.
pub(crate) fn vet(result: &Value, context: &Context<'_>) -> Vetted {
    let mut vetted = Vetted::default();
    let mut drop = |kind: &str, why: &str| {
        vetted
            .dropped
            .push(json!({"tool": context.tool, "kind": kind, "why": why}));
    };
    let items = match result.get("effects") {
        None | Some(Value::Null) => return vetted,
        Some(Value::Array(items)) => items,
        Some(_) => {
            drop("-", "shape");
            return vetted;
        }
    };
    let error = result["error"] == true;
    let mut afters = HashMap::new();
    let mut reported = HashSet::new();
    let mut kept = Vec::new();
    for item in items {
        let kind = item["kind"].as_str().unwrap_or("-");
        if kind == "todo.written" && error {
            continue;
        }
        match one(item, context, &mut afters) {
            Ok(effect) => {
                let path = effect["path"].as_str().map(String::from);
                if path.is_some() && !reported.insert((kind.to_string(), path)) {
                    drop(kind, "shape");
                } else {
                    kept.push(effect);
                }
            }
            Err(why) => drop(kind, why),
        }
    }
    vetted.kept = kept;
    vetted
}

/// One effect: kept (with the real path), or why it is dropped.
fn one(
    item: &Value,
    context: &Context<'_>,
    afters: &mut HashMap<PathBuf, Content>,
) -> Result<Value, &'static str> {
    let kind = item["kind"].as_str().ok_or("shape")?;
    if !["file.changed", "file.read", "file.trashed", "todo.written"].contains(&kind) {
        return Err("kind");
    }
    if kind == "todo.written" {
        if !todo_list(&item["todos"]) || item.get("done").is_some_and(|done| !todo_list(done)) {
            return Err("shape");
        }
        let mut effect = json!({"kind": kind, "todos": item["todos"]});
        if item["done"].as_array().is_some_and(|done| !done.is_empty()) {
            effect["done"] = item["done"].clone();
        }
        return Ok(effect);
    }
    let path = item["path"].as_str().ok_or("shape")?;
    match kind {
        "file.changed" => {
            let hashes_fit = ["before", "after"]
                .iter()
                .all(|key| item[*key].is_null() || item[*key].as_str().is_some_and(is_hash));
            let texts_fit = ["before_text", "after_text"]
                .iter()
                .all(|key| item[*key].is_null() || item[*key].is_string());
            if !hashes_fit || !texts_fit {
                return Err("shape");
            }
            let real = checked(context, path, true).ok_or("not_checked")?;
            let before = match context.befores.get(&real) {
                Some(Content::Missing) => None,
                Some(Content::Bytes(bytes)) => Some(bytes.clone()),
                Some(Content::TooBig) => return Err("too_big"),
                Some(Content::Unreadable) | None => return Err("mismatch"),
            };
            let after = bytes(now(afters, &real))?;
            if before.as_ref() == Some(&after) {
                return Err("unchanged");
            }
            if !agrees(item, before.as_deref(), &after) {
                return Err("mismatch");
            }
            Ok(json!({"kind": kind, "path": real.to_string_lossy()}))
        }
        "file.read" => {
            let lines = &item["lines"];
            if !lines.is_null() {
                let pair: Option<Vec<u64>> = lines
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .and_then(|pair| pair.iter().map(Value::as_u64).collect());
                match pair.as_deref() {
                    Some([from, to]) if *from != 0 && from <= to => {}
                    _ => return Err("shape"),
                }
            }
            let real = checked(context, path, false).ok_or("not_checked")?;
            bytes(now(afters, &real))?;
            let mut effect = json!({"kind": kind, "path": real.to_string_lossy()});
            if !lines.is_null() {
                effect["lines"] = lines.clone();
            }
            Ok(effect)
        }
        _ => {
            let trash = item["trash"].as_str().ok_or("shape")?;
            if trash.chars().count() > MOST_TRASH_CHARS {
                return Err("shape");
            }
            let real = checked(context, path, true).ok_or("not_checked")?;
            if std::fs::symlink_metadata(&real).is_ok() {
                return Err("mismatch");
            }
            Ok(json!({"kind": kind, "path": real.to_string_lossy(), "trash": trash}))
        }
    }
}

/// The real location of `path` when the check allowed it (for writing, when `write`).
fn checked(context: &Context<'_>, path: &str, write: bool) -> Option<PathBuf> {
    let real = context.place.real(path)?;
    context
        .judged
        .iter()
        .any(|one| one.real == real && (one.write || !write))
        .then_some(real)
}

/// The file at `real` now, read once per reply.
fn now<'a>(afters: &'a mut HashMap<PathBuf, Content>, real: &Path) -> &'a Content {
    afters
        .entry(real.to_path_buf())
        .or_insert_with(|| copy(real))
}

/// The bytes read, or why there are none.
fn bytes(content: &Content) -> Result<Vec<u8>, &'static str> {
    match content {
        Content::Bytes(bytes) => Ok(bytes.clone()),
        Content::TooBig => Err("too_big"),
        Content::Missing | Content::Unreadable => Err("mismatch"),
    }
}

/// `sha256:` and 64 lowercase hex digits.
fn is_hash(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The hashes and texts `item` carries (if any) match what was read; `before` is `None` for a new file.
fn agrees(item: &Value, before: Option<&[u8]>, after: &[u8]) -> bool {
    let fits = |key: &str, content: Option<&[u8]>, text: bool| match item[key].as_str() {
        None => true,
        Some(claimed) if text => content == Some(claimed.as_bytes()),
        Some(claimed) => content.is_some_and(|content| content_hash(content) == claimed),
    };
    fits("before", before, false)
        && fits("after", Some(after), false)
        && fits("before_text", before, true)
        && fits("after_text", Some(after), true)
}

/// `[{"content", "status"}]` with strings.
fn todo_list(todos: &Value) -> bool {
    todos.as_array().is_some_and(|todos| {
        todos
            .iter()
            .all(|todo| todo["content"].is_string() && todo["status"].is_string())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use serde_json::json;

    use super::{Context, Place, vet};

    /// `Reply` always writes a list: an extension writing its own JSON could send anything, and the core drops it whole
    /// (the Python test sends it through a raw result dict).
    #[test]
    fn effects_that_are_not_a_list_are_dropped_whole() {
        let place = Place {
            cwd: PathBuf::from("/"),
            home: None,
        };
        let befores = HashMap::new();
        let context = Context {
            tool: "t",
            place: &place,
            judged: &[],
            befores: &befores,
        };
        let vetted = vet(&json!({"effects": {"kind": "file.changed"}}), &context);
        assert_eq!(
            vetted.dropped,
            [json!({"tool": "t", "kind": "-", "why": "shape"})]
        );
        assert!(vetted.kept.is_empty());
        let vetted = vet(&json!({"effects": null}), &context);
        assert!(vetted.dropped.is_empty(), "null is not written");
    }
}
