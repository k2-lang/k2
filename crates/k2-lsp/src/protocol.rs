//! Small constructors for the JSON-RPC envelopes and the LSP `ServerCapabilities`
//! shape, so the server and feature code stay free of literal `JsonValue` plumbing.

use crate::json::JsonValue;

/// JSON-RPC error codes used by the server.
pub mod error_code {
    /// The requested method is not implemented.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// A request arrived before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// A request carried invalid parameters (e.g. an invalid rename name).
    pub const INVALID_PARAMS: i64 = -32602;
}

/// The semantic-token *type* legend, in fixed index order. This array IS the
/// index contract used by the delta encoder: the encoder emits the index of a
/// type into this slice, so the legend advertised in `initialize` and the encoder
/// must reference this one constant. Adding a type appends to the end (never
/// reorders) so existing indices stay stable.
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "function",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "keyword",
    "string",
    "number",
    "operator",
    "comment",
];

/// The semantic-token *modifier* legend, in fixed index order (a bitset position
/// each). `declaration` marks a token at a binding site; `readonly` marks a
/// `const`/item.
pub const SEMANTIC_TOKEN_MODIFIERS: &[&str] = &["declaration", "readonly"];

/// The legend index of a token type, panicking only on a programming error (a
/// name not in [`SEMANTIC_TOKEN_TYPES`]). Callers pass string literals checked by
/// the unit tests, so this is infallible in practice.
pub fn token_type_index(name: &str) -> u32 {
    SEMANTIC_TOKEN_TYPES
        .iter()
        .position(|&t| t == name)
        .expect("token type in legend") as u32
}

/// The legend index of a token modifier (its bit position).
pub fn token_modifier_index(name: &str) -> u32 {
    SEMANTIC_TOKEN_MODIFIERS
        .iter()
        .position(|&t| t == name)
        .expect("token modifier in legend") as u32
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
        ("referencesProvider", JsonValue::Bool(true)),
        (
            "renameProvider",
            JsonValue::obj(vec![("prepareProvider", JsonValue::Bool(true))]),
        ),
        (
            "signatureHelpProvider",
            JsonValue::obj(vec![
                (
                    "triggerCharacters",
                    JsonValue::arr(vec![JsonValue::str("("), JsonValue::str(",")]),
                ),
                (
                    "retriggerCharacters",
                    JsonValue::arr(vec![JsonValue::str(",")]),
                ),
            ]),
        ),
        ("inlayHintProvider", JsonValue::Bool(true)),
        (
            "semanticTokensProvider",
            JsonValue::obj(vec![
                (
                    "legend",
                    JsonValue::obj(vec![
                        (
                            "tokenTypes",
                            JsonValue::arr(
                                SEMANTIC_TOKEN_TYPES
                                    .iter()
                                    .map(|&t| JsonValue::str(t))
                                    .collect(),
                            ),
                        ),
                        (
                            "tokenModifiers",
                            JsonValue::arr(
                                SEMANTIC_TOKEN_MODIFIERS
                                    .iter()
                                    .map(|&t| JsonValue::str(t))
                                    .collect(),
                            ),
                        ),
                    ]),
                ),
                ("full", JsonValue::Bool(true)),
            ]),
        ),
        (
            "codeActionProvider",
            JsonValue::obj(vec![(
                "codeActionKinds",
                JsonValue::arr(vec![JsonValue::str("quickfix")]),
            )]),
        ),
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
