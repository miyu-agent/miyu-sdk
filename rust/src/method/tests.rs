use serde_json::json;

use super::{Method, MethodError, Registry};

#[test]
fn later_methods_wait_and_timeouts_are_sent() {
    let mut registry = Registry::default();
    registry.declare(&Method::new("stt.transcribe").timeout_ms(120_000).later());
    registry.declare(&"status".into());
    assert_eq!(registry.params(), json!({"methods": [{"name": "status"}]}));
    assert_eq!(registry.set(&["stt.transcribe"], true), Ok(()));
    assert_eq!(
        registry.params(),
        json!({"methods": [{"name": "status"}, {"name": "stt.transcribe", "timeout_ms": 120_000}]})
    );
    assert_eq!(
        registry.set(&["stt.transcribe", "nope"], false),
        Err(vec!["nope".to_string()]),
        "an unknown name changes nothing"
    );
    assert_eq!(
        registry.params()["methods"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn errors_carry_their_code_in_data() {
    assert_eq!(
        MethodError::new("bad_audio", "Not a WAV file.").into_json(),
        json!({"code": -32000, "message": "Not a WAV file.", "data": {"code": "bad_audio"}})
    );
    assert_eq!(
        MethodError::from("broke").into_json(),
        json!({"code": -32603, "message": "broke"})
    );
}
