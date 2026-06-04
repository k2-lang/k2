//! The LSP server: the dispatch loop, lifecycle, and document-sync wiring.
//!
//! The server is single-threaded and synchronous — pure `std`, no async runtime.
//! It reads one `Content-Length`-framed message at a time, dispatches on the
//! method, and writes any response/notification back. All human-readable logging
//! goes to **stderr** so stdout stays a clean RPC channel.
//!
//! ## Debounce
//!
//! "Debounced on change" is realized as *coalescing*: each `didChange`
//! notification already batches every content change in one message, and the
//! server applies them all and republishes diagnostics exactly once per
//! notification. With a synchronous loop and no threads this is the simplest
//! correct strategy; a timer-based debounce would need a runtime we deliberately
//! avoid.

use std::io::{self, BufRead, Write};

use crate::document::DocumentStore;
use crate::features;
use crate::json::JsonValue;
use crate::protocol::{self, error_code};
use crate::rpc::{read_message, write_message};

/// The server version, surfaced in `initialize`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The mutable server state.
pub struct Server {
    /// The open-document store.
    store: DocumentStore,
    /// Set once the client's `initialized` notification arrives.
    initialized: bool,
    /// Set once a `shutdown` request has been honored, so `exit` chooses the
    /// right process code.
    shutdown_requested: bool,
}

/// What the dispatch loop should do after handling a message.
enum Flow {
    /// Keep serving.
    Continue,
    /// Exit the process with the given code.
    Exit(i32),
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}

impl Server {
    /// Builds a fresh, uninitialized server.
    pub fn new() -> Server {
        Server {
            store: DocumentStore::new(),
            initialized: false,
            shutdown_requested: false,
        }
    }

    /// Runs the server over arbitrary framed-message reader/writer streams. This
    /// is the testable core; [`crate::run_stdio`] wires it to stdin/stdout.
    ///
    /// Returns the process exit code (`0` for a clean shutdown/EOF).
    pub fn serve(&mut self, reader: &mut impl BufRead, writer: &mut impl Write) -> io::Result<i32> {
        loop {
            let msg = match read_message(reader) {
                Ok(Some(msg)) => msg,
                Ok(None) => return Ok(0), // clean EOF
                Err(e) => {
                    // A malformed frame is logged but does not crash the server.
                    let _ = writeln!(io::stderr(), "k2c-lsp: bad message: {e}");
                    return Ok(1);
                }
            };
            match self.handle(&msg, writer)? {
                Flow::Continue => {}
                Flow::Exit(code) => return Ok(code),
            }
        }
    }

    /// Dispatches one parsed message, writing any response/notification.
    fn handle(&mut self, msg: &JsonValue, writer: &mut impl Write) -> io::Result<Flow> {
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").cloned();
        let params = msg.get("params");

        let method = match method {
            Some(m) => m,
            // A response to a server request, or a malformed message: ignore.
            None => return Ok(Flow::Continue),
        };

        // Before `initialize`, only `initialize`/`exit` are valid; other
        // *requests* get ServerNotInitialized.
        if !self.initialized && id.is_some() && method != "initialize" && method != "shutdown" {
            if let Some(id) = id {
                write_message(
                    writer,
                    &protocol::error_response(
                        id,
                        error_code::SERVER_NOT_INITIALIZED,
                        "server not initialized",
                    ),
                )?;
            }
            return Ok(Flow::Continue);
        }

        match method {
            "initialize" => {
                if let Some(id) = id {
                    write_message(
                        writer,
                        &protocol::response(id, protocol::initialize_result(VERSION)),
                    )?;
                }
                Ok(Flow::Continue)
            }
            "initialized" => {
                self.initialized = true;
                Ok(Flow::Continue)
            }
            "shutdown" => {
                self.shutdown_requested = true;
                if let Some(id) = id {
                    write_message(writer, &protocol::response(id, JsonValue::Null))?;
                }
                Ok(Flow::Continue)
            }
            "exit" => Ok(Flow::Exit(if self.shutdown_requested { 0 } else { 1 })),

            "textDocument/didOpen" => {
                self.did_open(params, writer)?;
                Ok(Flow::Continue)
            }
            "textDocument/didChange" => {
                self.did_change(params, writer)?;
                Ok(Flow::Continue)
            }
            "textDocument/didClose" => {
                self.did_close(params, writer)?;
                Ok(Flow::Continue)
            }

            "textDocument/hover" => self.request(id, params, writer, features::hover::compute),
            "textDocument/definition" => self.definition(id, params, writer),
            "textDocument/completion" => {
                self.request(id, params, writer, features::completion::compute)
            }
            "textDocument/formatting" => self.formatting(id, params, writer),

            // Unknown request → MethodNotFound; unknown notification → ignore.
            _ => {
                if let Some(id) = id {
                    write_message(
                        writer,
                        &protocol::error_response(
                            id,
                            error_code::METHOD_NOT_FOUND,
                            &format!("method not found: {method}"),
                        ),
                    )?;
                }
                Ok(Flow::Continue)
            }
        }
    }

    // ---- document sync ---------------------------------------------------

    /// Handles `textDocument/didOpen` and publishes diagnostics.
    fn did_open(&mut self, params: Option<&JsonValue>, writer: &mut impl Write) -> io::Result<()> {
        let params = match params {
            Some(p) => p,
            None => return Ok(()),
        };
        let doc = match params.get("textDocument") {
            Some(d) => d,
            None => return Ok(()),
        };
        let uri = doc
            .get("uri")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        let version = doc.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
        let text = doc
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        self.store.open(uri.clone(), version, text);
        self.publish(&uri, writer)
    }

    /// Handles `textDocument/didChange` and republishes diagnostics.
    fn did_change(
        &mut self,
        params: Option<&JsonValue>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        let params = match params {
            Some(p) => p,
            None => return Ok(()),
        };
        let doc = match params.get("textDocument") {
            Some(d) => d,
            None => return Ok(()),
        };
        let uri = doc
            .get("uri")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        let version = doc.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
        let empty: Vec<JsonValue> = Vec::new();
        let changes = params
            .get("contentChanges")
            .and_then(|c| c.as_array())
            .unwrap_or(&empty);
        self.store.apply_changes(&uri, version, changes);
        self.publish(&uri, writer)
    }

    /// Handles `textDocument/didClose` and clears the document's diagnostics.
    fn did_close(&mut self, params: Option<&JsonValue>, writer: &mut impl Write) -> io::Result<()> {
        let params = match params {
            Some(p) => p,
            None => return Ok(()),
        };
        let uri = params
            .get("textDocument")
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        self.store.close(&uri);
        // Publish an empty diagnostics list to clear the editor's markers.
        let notif = protocol::notification(
            "textDocument/publishDiagnostics",
            JsonValue::obj(vec![
                ("uri", JsonValue::str(uri)),
                ("diagnostics", JsonValue::arr(Vec::new())),
            ]),
        );
        write_message(writer, &notif)
    }

    /// Computes and sends `publishDiagnostics` for one document.
    fn publish(&mut self, uri: &str, writer: &mut impl Write) -> io::Result<()> {
        let (version, diagnostics) = match self.store.get_mut(uri) {
            Some(doc) => {
                let version = doc.version;
                let diags = features::diagnostics::compute(doc.analysis());
                (version, diags)
            }
            None => return Ok(()),
        };
        let notif = protocol::notification(
            "textDocument/publishDiagnostics",
            JsonValue::obj(vec![
                ("uri", JsonValue::str(uri)),
                ("version", JsonValue::num(version)),
                ("diagnostics", diagnostics),
            ]),
        );
        write_message(writer, &notif)
    }

    // ---- feature requests ------------------------------------------------

    /// Shared plumbing for a position-based request: extract the URI and
    /// position, compute the analysis, convert the position to a scalar offset,
    /// and reply with `f`'s result (or `null` if the doc is unknown).
    fn request(
        &mut self,
        id: Option<JsonValue>,
        params: Option<&JsonValue>,
        writer: &mut impl Write,
        f: fn(&crate::analysis::Analysis, u32) -> JsonValue,
    ) -> io::Result<Flow> {
        let id = match id {
            Some(id) => id,
            None => return Ok(Flow::Continue),
        };
        let result = self
            .offset_and_doc(params)
            .map(|(uri, offset)| {
                let doc = self.store.get_mut(&uri).expect("doc present");
                f(doc.analysis(), offset)
            })
            .unwrap_or(JsonValue::Null);
        write_message(writer, &protocol::response(id, result))?;
        Ok(Flow::Continue)
    }

    /// `textDocument/definition` (needs the URI for the resulting `Location`).
    fn definition(
        &mut self,
        id: Option<JsonValue>,
        params: Option<&JsonValue>,
        writer: &mut impl Write,
    ) -> io::Result<Flow> {
        let id = match id {
            Some(id) => id,
            None => return Ok(Flow::Continue),
        };
        let result = self
            .offset_and_doc(params)
            .map(|(uri, offset)| {
                let doc = self.store.get_mut(&uri).expect("doc present");
                features::definition::compute(doc.analysis(), &uri, offset)
            })
            .unwrap_or(JsonValue::Null);
        write_message(writer, &protocol::response(id, result))?;
        Ok(Flow::Continue)
    }

    /// `textDocument/formatting`.
    fn formatting(
        &mut self,
        id: Option<JsonValue>,
        params: Option<&JsonValue>,
        writer: &mut impl Write,
    ) -> io::Result<Flow> {
        let id = match id {
            Some(id) => id,
            None => return Ok(Flow::Continue),
        };
        let uri = params
            .and_then(|p| p.get("textDocument"))
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let result = match uri {
            Some(u) => match self.store.get_mut(&u) {
                Some(doc) => features::formatting::compute(doc.analysis()),
                None => JsonValue::Null,
            },
            None => JsonValue::Null,
        };
        write_message(writer, &protocol::response(id, result))?;
        Ok(Flow::Continue)
    }

    /// Extracts `(uri, scalar offset)` from a `textDocument/{position}` request,
    /// returning `None` if the document is not open.
    fn offset_and_doc(&mut self, params: Option<&JsonValue>) -> Option<(String, u32)> {
        let params = params?;
        let uri = params
            .get("textDocument")
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())?
            .to_string();
        let pos = params.get("position")?;
        let line = pos.get("line").and_then(|x| x.as_u32()).unwrap_or(0);
        let character = pos.get("character").and_then(|x| x.as_u32()).unwrap_or(0);
        let doc = self.store.get_mut(&uri)?;
        let offset = doc.analysis().posmap.position_to_offset(line, character);
        Some((uri, offset))
    }
}
