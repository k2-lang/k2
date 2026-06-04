# The k2 Language Server (`k2c lsp`)

> k2 — *Kardashev Type II*: total control over the machine, with zero waste.

`k2c lsp` is a [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for k2. It speaks JSON-RPC over **stdio** and answers editor requests by
**reusing the same front-end crates** the compiler uses — the lexer, parser,
resolver, type checker, and the canonical formatter. There is no second
implementation of the language: the server's diagnostics, hovers, definitions,
completions, and formatting are exactly what `k2c check` and `k2c fmt` produce.

Like the rest of the toolchain it is **pure `std`** and **offline**: the JSON
value/parser/serializer and the LSP message framing are hand-rolled, with zero
third-party crates.

## Launching the server

```sh
k2c lsp            # run the server over stdio (default and only transport)
k2c lsp --stdio    # identical; the conventional spelling is accepted
```

The server reads `Content-Length`-framed JSON-RPC messages from **stdin** and
writes responses/notifications to **stdout**. All human-readable logging goes to
**stderr**, so stdout stays a clean protocol channel. The process exits `0` on a
clean `shutdown`/`exit` handshake or when stdin closes.

## Pointing an editor at it

Any LSP client works. Point it at the `k2c` binary with the `lsp` argument and a
`k2` document selector.

### VS Code (generic LSP client)

```jsonc
{
  "command": "k2c",
  "args": ["lsp"],
  "documentSelector": [{ "language": "k2" }],
  "transport": "stdio"
}
```

### Neovim (`nvim-lspconfig`-style)

```lua
vim.lsp.start({
  name = "k2c-lsp",
  cmd = { "k2c", "lsp" },
  root_dir = vim.fs.dirname(vim.fs.find({ "build.k2" }, { upward = true })[1]),
})
```

### Generic clients

Spawn `k2c lsp`, connect to its stdin/stdout, and drive the standard lifecycle
(`initialize` → `initialized` → … → `shutdown` → `exit`). Associate the `.k2`
extension with a `k2` language id.

## Supported capabilities

The `initialize` response advertises:

| Capability | LSP method | What it does |
|---|---|---|
| Document sync | `textDocument/didOpen` / `didChange` / `didClose` | An in-memory document store; **full *and* incremental** edits are accepted (the server advertises incremental, `change: 2`). |
| Diagnostics | `textDocument/publishDiagnostics` | Live lex + parse + resolve + check; errors and warnings with exact UTF-16 ranges and a `k2c` source. Published on open and after every change; cleared on close. |
| Hover | `textDocument/hover` | The type of the symbol under the cursor (from the type checker's per-occurrence types) plus its definition kind, rendered as a `k2` code block. |
| Go to definition | `textDocument/definition` | The declaration site of the symbol, via the resolver's occurrence→definition map; struct fields and enum/union variants resolve through the type checker's member table. |
| Completion | `textDocument/completion` | **Scope-aware** identifier completion (every binding visible at the cursor — params, locals, file items, and the predeclared types) and **member completion** after `.` (fields, methods, and variants of the base type). The trigger character is `.`. |
| Formatting | `textDocument/formatting` | Runs the canonical formatter (`k2c fmt`) and returns a single full-document `TextEdit`. Returns nothing for input that does not parse, exactly like the CLI. |

### Position mapping

LSP positions are 0-based line/character in **UTF-16 code units**; k2's compiler
keys spans by **scalar (char) offsets** with 1-based line/col for messages. The
server converts between the two with a dedicated, well-tested bidirectional map,
so multi-byte and astral (surrogate-pair) text produces exact ranges. Ranges are
always derived from the byte/scalar `start`/`end` of a span, never from its
human-readable `line`/`col`.

## Not yet supported

These are designed but deliberately out of scope for v0.13 (single open buffer,
no cross-file index):

- find references, rename, signature help, inlay hints, document symbols
- cross-file go-to-definition (definitions are resolved within the open buffer)
- workspace-wide configuration

They remain on the roadmap; the server degrades gracefully (a `null`/empty
result) rather than guessing.

## Design notes

- **Reuses the front end.** Each feature calls `k2_parse::parse`,
  `k2_resolve::resolve_file`, `k2_types::check_file`, and `k2_fmt::format_source`
  on exactly the user's buffer — so editor results cannot diverge from the CLI.
- **Never panics.** A malformed JSON-RPC frame becomes a protocol error and a
  clean shutdown; a syntactically-broken document yields diagnostics and empty
  feature results, never a crashed process.
- **Debounce.** With a single synchronous stdio loop (no async runtime), changes
  are coalesced: each `didChange` notification applies all of its content
  changes and republishes diagnostics once.

## See also

- **`docs/tooling.md`** — the full `k2c` driver surface and the one-front-end
  philosophy the language server embodies.
- **`ROADMAP.md`** — milestone status, including v0.13.
