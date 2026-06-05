//! # k2-lsp — the language server for the k2 programming language
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the developer-tooling layer (v0.13): a Language Server Protocol
//! server, `k2c lsp`, that speaks JSON-RPC over stdio and answers editor requests
//! by **reusing the existing front-end crates** — the lexer, parser, resolver,
//! type checker, and canonical formatter — with zero re-implementation of
//! compiler logic. Its diagnostics, hovers, definitions, completions, and
//! formatting therefore cannot drift from `k2c check`/`k2c fmt`.
//!
//! Consistent with the rest of the toolchain it is **pure `std`**: the JSON
//! value/parser/serializer ([`json`]) and the LSP message framing ([`rpc`]) are
//! hand-rolled, so the server builds and runs fully offline.
//!
//! ## Architecture
//!
//! * [`json`] — a minimal JSON value, recursive-descent parser, and serializer.
//! * [`rpc`] — `Content-Length`-framed JSON-RPC reading/writing over a stream.
//! * [`position`] — the bidirectional LSP-UTF-16 ↔ compiler-scalar-offset map,
//!   the load-bearing correctness piece for exact ranges.
//! * [`analysis`] / [`document`] — the in-memory document store and the cached
//!   per-document parse+resolve+check bundle.
//! * [`server`] — the synchronous dispatch loop, lifecycle, and document sync.
//! * [`features`] — the providers: diagnostics, hover, definition, completion,
//!   formatting, references, rename (+ prepareRename), signatureHelp, inlayHint,
//!   semanticTokens (full), and codeAction — each driven by the front-end side
//!   tables (the resolver Uses/Def tables, the type checker's per-occurrence types
//!   and function signatures, and the lexer's token kinds + trivia).
//! * [`workspace`] — the cross-file module-graph layer.
//!
//! ## Cross-file boundary (v0.26)
//!
//! Within-file, all eleven features are exhaustive and exact. **Cross-file** is
//! scoped — per the milestone's documented boundary — to **definition** and
//! **references** over a *path import* (`@import("./b.k2")`): a `b.foo` access
//! jumps to (and lists uses of) the imported file's top-level item, computed in
//! each file's own [`position::PositionMap`] (no merged-buffer re-keying).
//! signatureHelp, inlayHint, semanticTokens, codeAction, and rename remain
//! within-file; cross-file *rename* of a `pub` symbol is a known limitation.
//! Named-module imports (`std`) stay opaque, exactly as `definition` already does.
//!
//! ## Never panics
//!
//! Like the lexer and parser, the server never aborts the host process on a
//! malformed message or a syntactically-broken document: framing/JSON errors
//! become protocol-level errors or a logged shutdown, and a document that does
//! not parse simply yields diagnostics and empty feature results.

pub mod analysis;
pub mod document;
pub mod features;
pub mod json;
pub mod position;
pub mod protocol;
pub mod rpc;
pub mod server;
pub mod workspace;

use std::io::{self, BufReader, BufWriter};

use server::Server;

/// Runs the language server over stdio until stdin closes (or `exit`).
///
/// Reads `Content-Length`-framed JSON-RPC from stdin and writes responses /
/// notifications to stdout; all human-readable logging goes to stderr. Returns
/// the process exit code, which the `k2c lsp` driver maps to its `ExitCode`.
pub fn run_stdio() -> io::Result<i32> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut server = Server::new();
    server.serve(&mut reader, &mut writer)
}

#[cfg(test)]
mod tests;
