//! miyu-ext 经假核心走协议，照 Miyu 的协议参考（`docs/reference/extensions.md`），和 Python 那份的测试一一对应。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use miyu_ext::{Extension, FakeCore, Method, MethodError, Reply, Tool};

fn config(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

fn text(answer: &Value) -> &str {
    answer["blocks"][0]["text"].as_str().unwrap_or_default()
}

#[test]
fn registers_tools_and_takes_config() {
    let mut ext = Extension::new("hello", "0.1.0");
    ext.tool(
        Tool::new("hello_greet", "Greet someone.")
            .schema(json!({"type": "object", "properties": {"name": {"type": "string"}}}))
            .display_name(json!({"en": "Greet"}))
            .subject("name")
            .clicall(true),
        |args, call| {
            let config = call.core().config();
            let greeting = config
                .get("hello.greeting")
                .and_then(Value::as_str)
                .unwrap_or("Hello");
            Reply::text(format!(
                "{greeting}, {}!",
                args["name"].as_str().unwrap_or("there")
            ))
        },
    );
    let core = FakeCore::start(ext, config(&[("hello.greeting", json!("Hi"))]));
    let hello = &core.received()[0];
    assert_eq!(hello["params"]["protocol"], json!([1, 1]));
    assert_eq!(
        hello["params"]["head"],
        json!({"kind": "hello", "version": "0.1.0"})
    );
    assert_eq!(
        core.tools(),
        [
            json!({"name": "hello_greet", "description": "Greet someone.",
                "input_schema": {"type": "object", "properties": {"name": {"type": "string"}}},
                "access": "read", "venues": ["local"], "display_name": {"en": "Greet"}, "subject": "name",
                "clicall": true})
        ]
    );
    assert_eq!(
        text(&core.call_tool("hello_greet", json!({"name": "Ann"}))),
        "Hi, Ann!"
    );
    core.notify(
        "extension.config",
        json!({"keys": {"hello.greeting": null}}),
    );
    assert_eq!(
        text(&core.call_tool("hello_greet", json!({"name": "Bo"}))),
        "Hello, Bo!",
        "unset falls back"
    );
    assert_eq!(core.close(), Ok(()), "stdin ended, run_with returned");
}

#[test]
fn provides_even_without_tools_and_lists_methods() {
    let mut ext = Extension::new("bare", "0.0.0");
    ext.method("status", |_, caller, _| Ok(json!({"caller": caller})));
    let core = FakeCore::start(ext, Map::new());
    assert!(core.tools().is_empty(), "provide once, empty");
    assert_eq!(core.methods(), [json!({"name": "status"})]);
    let answer = core.request(
        "method.call",
        json!({"method": "status", "params": {}, "from": "im"}),
    );
    assert_eq!(answer, json!({"caller": "im"}));
    assert_eq!(
        core.request("method.call", json!({"method": "nope"}))["error"]["code"],
        -32601
    );
    assert_eq!(core.close(), Ok(()));
}

#[test]
fn methods_carry_waits_codes_and_can_wait_to_register() {
    let mut ext = Extension::new("voice", "0.0.0");
    ext.method_with(
        Method::new("stt.transcribe").timeout_ms(120_000).later(),
        |params, _, _| {
            if params["audio"].is_null() {
                return Err(MethodError::new("bad_audio", "No audio."));
            }
            Ok(json!({"text": "hi"}))
        },
    );
    ext.method("status", |_, _, _| Ok(json!({})));
    let (tx, rx) = std::sync::mpsc::channel();
    ext.ready(move |core| {
        tx.send(core.clone()).unwrap_or_default();
    });
    let core = FakeCore::start(ext, Map::new());
    assert_eq!(
        core.methods(),
        [json!({"name": "status"})],
        "later ones wait"
    );
    let ext_core = rx.recv().expect("ready ran");
    assert!(
        ext_core.enable_methods(&["nope"]).is_err(),
        "unknown names are refused"
    );
    ext_core
        .enable_methods(&["stt.transcribe"])
        .expect("registered");
    assert_eq!(
        core.methods(),
        [
            json!({"name": "status"}),
            json!({"name": "stt.transcribe", "timeout_ms": 120_000})
        ]
    );
    let refused = core.request(
        "method.call",
        json!({"method": "stt.transcribe", "params": {}}),
    );
    assert_eq!(refused["error"]["data"]["code"], "bad_audio", "{refused}");
    assert_eq!(refused["error"]["code"], -32000);
    ext_core
        .disable_methods(&["stt.transcribe"])
        .expect("unregistered");
    assert_eq!(core.methods(), [json!({"name": "status"})]);
    assert_eq!(core.close(), Ok(()));
}

#[test]
fn a_config_handler_can_call_the_core() {
    // 处理配置推送时调核心、等它回（照新配置重登记方法这类）：处理函数要是跑在读核心回应的线程上，回应永远读不到，
    // 扩展就卡死了（miyu-voice 2026-11-10 端到端测出来的）。
    let (done, finished) = std::sync::mpsc::channel();
    let done = std::sync::Mutex::new(done);
    let mut ext = Extension::new("cfg", "0.0.0");
    ext.method("status", |_, _, _| Ok(json!({"ok": true})));
    ext.on_config(move |changed, core| {
        let answer = core.call("ping", json!({}));
        done.lock()
            .unwrap()
            .send((changed.clone(), answer.map_err(|e| e.to_string())))
            .unwrap();
    });
    let core = FakeCore::start(ext, Map::new());
    core.answer("ping", |_| Ok(json!({"pong": true})));
    core.notify("extension.config", json!({"keys": {"cfg.voice": "a"}}));
    let (changed, answer) = finished
        .recv_timeout(Duration::from_secs(5))
        .expect("the handler got its answer instead of hanging");
    assert_eq!(changed["cfg.voice"], "a");
    assert_eq!(answer.unwrap(), json!({"pong": true}));
    assert_eq!(
        core.request("method.call", json!({"method": "status", "params": {}})),
        json!({"ok": true}),
        "still answering"
    );
    assert_eq!(core.close(), Ok(()));
}

#[test]
fn a_protocol_mismatch_is_an_error_not_a_hang() {
    for reply in [
        r#"{"jsonrpc":"2.0","id":"hello","error":{"code":-32010,"message":"no","data":{"reason":"protocol_mismatch"}}}"#,
        r#"{"jsonrpc":"2.0","id":"hello","result":{"protocol":2}}"#,
    ] {
        let ext = Extension::new("old", "0.0.0");
        let input = format!("{reply}\n");
        let why = ext
            .run_with(input.as_bytes(), Box::new(std::io::sink()))
            .expect_err("refused");
        assert!(why.contains("protocol"), "{why}");
    }
}

#[test]
fn replies_carry_data_failures_and_panics() {
    let mut ext = Extension::new("t", "0.0.0");
    ext.tool(Tool::new("full", "Full."), |_, _| {
        Reply::text("found 2")
            .data(json!({"hits": [1, 2]}))
            .detail(json!({"kind": "links", "links": []}))
    })
    .tool(Tool::new("bad", "Bad."), |_, _| Reply::failed("bad input"))
    .tool(Tool::new("boom", "Panics."), |_, _| panic!("a bug"));
    let core = FakeCore::start(ext, Map::new());
    assert_eq!(
        core.call_tool("full", json!({})),
        json!({"blocks": [{"type": "text", "text": "found 2"}], "data": {"hits": [1, 2]},
               "detail": {"kind": "links", "links": []}})
    );
    assert_eq!(core.call_tool("bad", json!({}))["error"], true);
    assert_eq!(
        core.call_tool("boom", json!({}))["error"],
        true,
        "a panic is a failed result"
    );
    assert_eq!(
        text(&core.call_tool("full", json!({}))),
        "found 2",
        "still serving"
    );
    assert_eq!(core.call_tool("ghost", json!({}))["error"]["code"], -32601);
    assert_eq!(core.close(), Ok(()));
}

#[test]
fn cancel_reaches_a_running_call_and_others_are_not_held_up() {
    let started = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&started);
    let mut ext = Extension::new("t", "0.0.0");
    ext.tool(Tool::new("patient", "Waits."), move |_, call| {
        seen.store(true, Ordering::Relaxed);
        while !call.cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Reply::text("stopped")
    })
    .tool(Tool::new("quick", "Quick."), |_, _| Reply::text("quick"));
    let core = Arc::new(FakeCore::start(ext, Map::new()));
    let waiting = {
        let core = Arc::clone(&core);
        std::thread::spawn(move || core.call_tool_as("patient", json!({}), "c-p"))
    };
    while !started.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        text(&core.call_tool("quick", json!({}))),
        "quick",
        "the waiting one doesn't hold it up"
    );
    core.notify("tool.cancel", json!({"session": "s1", "call_id": "c-p"}));
    assert_eq!(text(&waiting.join().unwrap()), "stopped");
}

#[test]
fn calling_the_core_from_a_tool() {
    let mut ext = Extension::new("t", "0.0.0");
    ext.tool(Tool::new("asks", "Calls the core."), |_, call| {
        let put = call
            .core()
            .call("blob.put", json!({"data": "aGk="}))
            .unwrap();
        let refused = call.core().call("secret.set", json!({})).unwrap_err();
        Reply::text(format!(
            "{} {}",
            put["blob"].as_str().unwrap_or_default(),
            refused.reason.unwrap_or_default()
        ))
    })
    .command("weather", |params, _| {
        format!("sunny {}", params["argument"].as_str().unwrap_or_default())
    })
    .on(
        "user.append",
        |params, _| json!({"content": [{"kind": "hint", "text": params["text"]}]}),
    );
    let core = FakeCore::start(ext, Map::new());
    core.answer("blob.put", |_| Ok(json!({"blob": "sha256:x"})));
    assert_eq!(
        text(&core.call_tool("asks", json!({}))),
        "sha256:x unknown_method"
    );
    assert_eq!(
        core.request(
            "command.call",
            json!({"command": "weather", "argument": "today"})
        ),
        json!({"said": "sunny today"})
    );
    assert_eq!(
        core.request(
            "user.append",
            json!({"session": "s1", "venue": "local", "text": "hi"})
        ),
        json!({"content": [{"kind": "hint", "text": "hi"}]})
    );
    assert_eq!(
        core.request("search.query", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(core.close(), Ok(()));
}
