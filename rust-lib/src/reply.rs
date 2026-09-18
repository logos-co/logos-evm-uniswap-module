//! The error reply every method answers with. A refusal a dependency already shaped, like
//! eth_rpc's `{ ok:false, code:"verified_blocked", verifiedProxy, … }`, is relayed verbatim.

use serde_json::{json, Value};

/// `e` unchanged when it is a JSON object with `ok:false`, else `{ ok:false, error: e }`.
pub fn err(e: impl std::fmt::Display) -> String {
    let message = e.to_string();
    if let Ok(v) = serde_json::from_str::<Value>(&message) {
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return message;
        }
    }
    json!({ "ok": false, "error": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // eth_rpc's verified_blocked in its source key order, which a re-serializing relay would
    // sort; the verdict inside carries an `ok` of its own.
    const BLOCKED: &str = concat!(
        r#"{"ok":false,"code":"verified_blocked","blocked":true,"chainId":1,"#,
        r#""error":"The verified proxy is not started. (state is 'stopped')","#,
        r#""verifiedProxy":{"ok":true,"chainId":1,"mode":"required","state":"stopped","usable":false,"#,
        r#""blocking":true,"message":"The verified proxy is not started.","#,
        r#""action":"open_verified_proxy","detail":"state is 'stopped'"}}"#,
    );

    fn wrapped(s: &str) -> String {
        json!({ "ok": false, "error": s }).to_string()
    }

    #[test]
    fn an_eth_rpc_refusal_is_relayed_byte_for_byte() {
        assert_eq!(err(BLOCKED), BLOCKED);
    }

    #[test]
    fn a_sentence_is_wrapped() {
        assert_eq!(err("no route found"), r#"{"error":"no route found","ok":false}"#);
    }

    #[test]
    fn an_ok_reply_or_a_non_object_is_wrapped_as_text() {
        for s in [r#"{"ok":true,"result":"0x"}"#, r#"[{"ok":false}]"#, r#""no route""#, "false", "null"] {
            assert_eq!(err(s), wrapped(s), "{s}");
        }
    }

    #[test]
    fn garbage_is_wrapped() {
        for s in ["", r#"{"ok":false,"code":"verified_blocked""#, "ChannelClosed"] {
            assert_eq!(err(s), wrapped(s), "{s}");
        }
    }
}
