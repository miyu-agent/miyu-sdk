//! The hello extension, written with miyu-ext: build it, put the binary in the package's `bin/` as `miyu-hello`
//! (`miyu-hello.exe` on Windows), then `miyu pkg install` the folder. The manifest is the Python example's
//! (`python/examples/hello/package.toml`).

use miyu_ext::{Extension, Reply, Tool};
use serde_json::{Value, json};

fn main() {
    if std::env::args().nth(1).as_deref() != Some("serve") {
        eprintln!("usage: miyu-hello serve (started by the Miyu core)");
        std::process::exit(2);
    }
    let mut ext = Extension::new("hello", "0.1.0");
    ext.tool(
        Tool::new("hello_greet", "Greet someone by name.")
            .schema(json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}))
            .display_name(json!({"en": "Greet", "zh": "打招呼"}))
            .subject("name")
            .clicall(true),
        |args, call| {
            let config = call.core().config();
            let greeting = config.get("hello.greeting").and_then(Value::as_str).unwrap_or("Hello");
            Reply::text(format!("{greeting}, {}!", args["name"].as_str().unwrap_or("there")))
        },
    );
    ext.run();
}
