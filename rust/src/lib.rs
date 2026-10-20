//! Write a Miyu extension in Rust: the handshake, line-by-line JSON-RPC, concurrent calls, cancelling and config pushes are
//! handled here, so an extension is a few registered closures. Synchronous, threads only, `serde_json` only; MIT licensed.
//! The protocol itself is in Miyu's `docs/reference/extensions.md`. The Python `miyu_ext.py` has the same concepts and names.
//!
//! ```no_run
//! use miyu_ext::{Extension, Reply, Tool};
//! use serde_json::json;
//!
//! let mut ext = Extension::new("hello", "0.1.0");
//! ext.tool(
//!     Tool::new("hello_greet", "Greet someone by name.")
//!         .schema(json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}))
//!         .access("read")
//!         .subject("name"),
//!     |args, call| {
//!         let greeting = call.core().config().get("hello.greeting").and_then(|v| v.as_str().map(String::from));
//!         let name = args["name"].as_str().unwrap_or("there");
//!         Reply::text(format!("{}, {name}!", greeting.as_deref().unwrap_or("Hello")))
//!     },
//! );
//! ext.run();
//! ```

mod extension;
mod fake;
mod reply;
mod wire;

pub use extension::Extension;
pub use fake::FakeCore;
pub use reply::{Call, Core, CoreError, Reply, Tool};

/// The protocol major versions this crate speaks.
pub const PROTOCOL: [u64; 2] = [1, 1];

/// Exit code for "restarting won't help" (bad config, port taken, protocol mismatch): the core stops instead of retrying.
pub const EXIT_CONFIG: i32 = 1;
