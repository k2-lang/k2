//! Small constructors for the JSON-RPC envelopes and the LSP `ServerCapabilities`
//! shape, so the server and feature code stay free of literal `JsonValue` plumbing.

use crate::json::JsonValue;

/// JSON-RPC error codes used by the server.
pub mod error_code {
    /// The requested method is not implemented.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// A request arrived before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
}

/// Builds a successful JSON-RPC response `{"jsonrpc":"2.0","id":<id>,"result":..}`.
///
/// The `id` is echoed back **verbatim** — a number stays a number, a string a
/// string — so the int-vs-string ambiguity never bites.
pub fn response(id: JsonValue, result: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("id", id),
        ("result", result),
    ])
}

/// Builds a JSON-RPC error response.
pub fn error_response(id: JsonValue, code: i64, message: &str) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("id", id),
        (
            "error",
            JsonValue::obj(vec![
                ("code", JsonValue::num(code)),
                ("message", JsonValue::str(message)),
            ]),
        ),
    ])
}

/// Builds a server-originated notification `{"jsonrpc":"2.0","method":..,"params":..}`.
pub fn notification(method: &str, params: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("method", JsonValue::str(method)),
        ("params", params),
    ])
}

/// The `result` of an `initialize` request: the capabilities the server
/// advertises plus its name/version.
///
/// `textDocumentSync.change == 2` advertises *incremental* sync; the change
/// applier also accepts full (`Full == 1`) updates, so either client works.
pub fn initialize_result(version: &str) -> JsonValue {
    let capabilities = JsonValue::obj(vec![
        (
            "textDocumentSync",
            JsonValue::obj(vec![
                ("openClose", JsonValue::Bool(true)),
                ("change", JsonValue::num(2)),
            ]),
        ),
        ("hoverProvider", JsonValue::Bool(true)),
        ("definitionProvider", JsonValue::Bool(true)),
        (
            "completionProvider",
            JsonValue::obj(vec![(
                "triggerCharacters",
                JsonValue::arr(vec![JsonValue::str(".")]),
            )]),
        ),
        ("documentFormattingProvider", JsonValue::Bool(true)),
    ]);
    JsonValue::obj(vec![
        ("capabilities", capabilities),
        (
            "serverInfo",
            JsonValue::obj(vec![
                ("name", JsonValue::str("k2c-lsp")),
                ("version", JsonValue::str(version)),
            ]),
        ),
    ])
}
