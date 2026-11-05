//! The replace extension, written with miyu-ext: a tool that edits files the way the built-in `edit` does. It declares
//! its path parameter, writes only where the core's check allowed (`call.checked()`) and reports the change, so the
//! timeline shows the diff and a file she read stays known. Build it, put the binary in the package's `bin/` as
//! `miyu-replace` (`miyu-replace.exe` on Windows), then `miyu pkg install` the folder. The manifest is the Python
//! example's (`python/examples/replace/package.toml`).

use miyu_ext::{Extension, Reply, Tool};
use serde_json::json;

fn main() {
    if std::env::args().nth(1).as_deref() != Some("serve") {
        eprintln!("usage: miyu-replace serve (started by the Miyu core)");
        std::process::exit(2);
    }
    extension().run();
}

/// The extension, apart from `main` so the tests run it against the fake core.
pub fn extension() -> Extension {
    let mut ext = Extension::new("replace", "0.1.0");
    ext.tool(
        Tool::new(
            "replace_text",
            "Replace one exact piece of text in a file. `old` must occur exactly once.",
        )
        .schema(json!({"type": "object",
            "properties": {"file_path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}},
            "required": ["file_path", "old", "new"]}))
        .access("write")
        .display_name(json!({"en": "Replace", "zh": "替换"}))
        .subject("file_path")
        .paths(json!([{"parameter": "file_path", "write": true}])),
        |args, call| {
            let given = args["file_path"].as_str().unwrap_or_default();
            // Where the core's check allowed writing: the argument resolved, links followed. Write here, not the raw
            // argument.
            let Some(path) = call.checked().first().map(|path| path.to_string()) else {
                return Reply::failed("file_path is missing.");
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => return Reply::failed(format!("Can't read {given}: {error}")),
            };
            let (old, new) = (
                args["old"].as_str().unwrap_or_default(),
                args["new"].as_str().unwrap_or_default(),
            );
            let count = if old.is_empty() { 0 } else { text.matches(old).count() };
            if count != 1 {
                return Reply::failed(format!(
                    "`old` occurs {count} times in {given}; it must occur exactly once."
                ));
            }
            if let Err(error) = std::fs::write(&path, text.replacen(old, new, 1)) {
                return Reply::failed(format!("Can't write {given}: {error}"));
            }
            Reply::text(format!("Replaced in {given}.")).changed(&path)
        },
    );
    ext
}
