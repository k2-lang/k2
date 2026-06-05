//! v0.29 self-hosting groundwork: the **k2-written lexer** (`selfhost/lexer.k2`),
//! compiled and run by the k2 toolchain, must produce the *exact* same token
//! stream as the Rust reference lexer (`k2_lexer`) for every corpus input — on
//! BOTH the bytecode VM (`k2c run`) and the native x86-64 backend
//! (`k2c run-native`). This is the differential proof that k2 can lex itself.
//!
//! Mechanism: `selfhost/lexer.k2` lexes an embedded `SAMPLE` byte array and
//! prints one `line:col Kind byteLen` line per token (ending with `… Eof 0`).
//! For each corpus string we (1) render the reference stream from `k2_lexer`,
//! (2) rewrite `SAMPLE` to the corpus bytes, run the program, and (3) assert the
//! two streams are byte-identical. `(line, col, byteLen)` pins each lexeme's
//! exact span in the shared source, so matching signatures imply identical text.
//!
//! The harness is pure-std and offline: it shells out to the in-tree `k2c`
//! binary, writes self-cleaning temp files, and uses no network.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Repository root (two levels up from this crate's manifest dir, `crates/k2c`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

/// The reference token stream: one `line:col Kind byteLen` per token, where
/// `Kind` is the `TokenKind` `Debug` name and `byteLen` is `text.len()`. This is
/// exactly the format `selfhost/lexer.k2` prints, so the two are comparable
/// byte-for-byte.
fn reference(src: &str) -> String {
    let mut out = String::new();
    for t in k2_lexer::tokenize(src) {
        out.push_str(&format!(
            "{}:{} {:?} {}\n",
            t.line,
            t.col,
            t.kind,
            t.text.len()
        ));
    }
    out
}

/// Builds a runnable k2 program by rewriting the `SAMPLE` byte array in
/// `selfhost/lexer.k2` to the bytes of `src` (between the unique marker lines).
fn build_program(src: &str) -> String {
    let lexer = std::fs::read_to_string(repo_root().join("selfhost").join("lexer.k2"))
        .expect("read selfhost/lexer.k2");
    let begin_marker = "//k2-selfhost-sample-begin";
    let end_marker = "//k2-selfhost-sample-end";
    let b = lexer.find(begin_marker).expect("begin marker present");
    let e = lexer.find(end_marker).expect("end marker present");
    let after = e + end_marker.len();

    let mut elems = String::new();
    for (i, byte) in src.bytes().enumerate() {
        if i > 0 {
            elems.push_str(", ");
        }
        elems.push_str(&format!("0x{byte:02x}"));
    }
    // `[_]u8{}` is a valid empty array; otherwise space the elements out.
    let array = if elems.is_empty() {
        "[_]u8{}".to_string()
    } else {
        format!("[_]u8{{ {elems} }}")
    };
    let sample = format!("{begin_marker}\nconst SAMPLE = {array};\n{end_marker}");
    format!("{}{}{}", &lexer[..b], sample, &lexer[after..])
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Runs `selfhost/lexer.k2` over `src` via the given `k2c` subcommand
/// (`run` = VM, `run-native` = native x86-64), returning the program's stdout.
fn run_lexer(subcmd: &str, src: &str) -> String {
    let program = build_program(src);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "k2_selfhost_{}_{}_{}.k2",
        subcmd.replace('-', "_"),
        std::process::id(),
        n
    ));
    std::fs::write(&path, program).expect("write temp program");
    let k2c = env!("CARGO_BIN_EXE_k2c");

    // Retry only on a transient native-exec failure (ETXTBSY) — a freshly
    // written ELF can briefly be "Text file busy". A real compile/run error
    // fails fast with its diagnostics.
    let mut attempt = 0;
    let output = loop {
        let o = Command::new(k2c)
            .arg(subcmd)
            .arg(&path)
            .output()
            .expect("spawn k2c");
        if o.status.success() {
            break o;
        }
        let stderr = String::from_utf8_lossy(&o.stderr);
        let transient = stderr.contains("Text file busy") || stderr.contains("ETXTBSY");
        if transient && attempt < 50 {
            attempt += 1;
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let _ = std::fs::remove_file(&path);
        panic!(
            "k2c {subcmd} failed (status {:?}) for program:\n{}\n--- stderr ---\n{}",
            o.status.code(),
            build_program(src),
            stderr
        );
    };
    let _ = std::fs::remove_file(&path);
    String::from_utf8(output.stdout).expect("k2c stdout is utf-8")
}

/// Hand-crafted snippets exercising every lexical rule and edge: maximal munch,
/// all literal forms, comment/doc/multiline boundaries, multibyte column
/// counting, NUL/BOM, and unterminated literals.
fn edge_corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("empty", ""),
        ("ws_only", "   \t\n\r\n  "),
        (
            "all_keywords",
            "const var pub fn comptime return struct enum union error if else \
             while for switch break continue defer errdefer try catch orelse \
             and or not unreachable test extern export inline align true false \
             null undefined",
        ),
        (
            "predeclared_idents",
            "i32 usize f64 bool void type anyerror comptime_int u0 i7 u255 _ _x x_9",
        ),
        (
            "operators_maximal_munch",
            "+ ++ += - -= -> * *= / /= % %= & &= | |= ^ ^= ~ << <<= >> >>= == != \
             < <= > >= = => ! != ? .* .? . .. ... ( ) { } [ ] , ; :",
        ),
        (
            "error_union_bang",
            "fn f() !void {} const E = A!B; const x = !y;",
        ),
        (
            "numbers",
            "0 123 1_000 0xFF 0xff_ff 0Xab 0o755 0O17 0b1010 0B1 3.14 6.022e23 \
             1e10 1E-5 2E+3 0x1.8p3 0xABCp+2 0X1.0P-1",
        ),
        ("number_dot_boundaries", "5. .5 1.0 x.0 a.b 1..2 0..n"),
        (
            "strings",
            "\"hello\" \"esc \\\" quote\" \"tab\\tnl\\n\" \"back\\\\slash\"",
        ),
        ("string_unterminated_eol", "x = \"open\ny = 1;"),
        ("string_unterminated_eof", "\"never closed"),
        ("chars", "'a' '\\n' '\\'' '\\\\' '0'"),
        ("char_unterminated", "'a"),
        (
            "comments",
            "// line comment\n//// quad slash line\ncode // trailing\nmore",
        ),
        ("line_comment_eof", "x // no newline at eof"),
        (
            "doc_comments",
            "/// doc one\n/// doc two\npub fn f() void {}",
        ),
        ("doc_vs_line", "///\n////\n/////\n///x\n//y"),
        (
            "multiline_string",
            "const s =\n    \\\\line one\n    \\\\line two\n;\nconst t = 1;",
        ),
        ("multiline_then_code", "\\\\only line\ncode after"),
        (
            "builtins_and_escaped",
            "@import @TypeOf @\"escaped name\" @\"const\" @sizeOf(@This())",
        ),
        ("bare_at_error", "@ @123 @+"),
        ("escaped_ident_unterminated", "@\"open"),
        ("nul_byte", "a\u{0}b"),
        ("bom_prefix", "\u{FEFF}const x = 1;"),
        (
            "multibyte_in_literals",
            "const s = \"café ☃ €\"; // comment é — dash\nconst c = 'é';",
        ),
        ("multibyte_top_level_error", "x ☃ y → z"),
        ("crlf_lines", "a\r\nb\r\n  c"),
        (
            "mixed_program",
            "pub fn parse(sys: *System) !u32 {\n    var v: u32 = 0;\n    \
             for (\"123\") |ch| {\n        if (ch < '0' or ch > '9') return error.Bad;\n        \
             v = v * 10 + @as(u32, ch - '0');\n    }\n    return v;\n}\n",
        ),
        (
            "switch_and_ranges",
            "const x = switch (n) { 0 => 'a', 1...9 => 'b', else => 'c', };",
        ),
    ]
}

/// A few real example programs, for breadth (they carry em-dashes in comments,
/// doc comments, escaped strings, generics syntax, etc.).
fn example_corpus() -> Vec<(&'static str, String)> {
    let root = repo_root();
    [
        "hello.k2",
        "errors.k2",
        "generic_list.k2",
        "data_structures.k2",
    ]
    .iter()
    .map(|name| {
        let src = std::fs::read_to_string(root.join("examples").join(name))
            .unwrap_or_else(|e| panic!("read examples/{name}: {e}"));
        (*name, src)
    })
    .collect()
}

#[test]
fn selfhost_lexer_vm_matches_reference_on_edges() {
    for (name, src) in edge_corpus() {
        let expected = reference(src);
        let actual = run_lexer("run", src);
        assert_eq!(
            actual, expected,
            "VM token stream diverges from the Rust lexer on `{name}`"
        );
    }
}

#[test]
fn selfhost_lexer_vm_matches_reference_on_examples() {
    for (name, src) in example_corpus() {
        let expected = reference(&src);
        let actual = run_lexer("run", &src);
        assert_eq!(
            actual, expected,
            "VM token stream diverges from the Rust lexer on example `{name}`"
        );
    }
}

#[test]
fn selfhost_lexer_native_matches_reference() {
    // A curated subset proving the *native* backend lexes identically — covering
    // the byte/scalar/widening paths (numbers, operators, multibyte, strings)
    // and a real program. (The full lexer compiles and runs natively; the subset
    // keeps the test bounded.)
    let subset = [
        "operators_maximal_munch",
        "numbers",
        "multibyte_in_literals",
        "strings",
        "doc_vs_line",
        "mixed_program",
    ];
    let edges = edge_corpus();
    for key in subset {
        let (name, src) = edges
            .iter()
            .find(|(n, _)| *n == key)
            .unwrap_or_else(|| panic!("missing corpus entry {key}"));
        let expected = reference(src);
        let actual = run_lexer("run-native", src);
        assert_eq!(
            &actual, &expected,
            "native token stream diverges from the Rust lexer on `{name}`"
        );
    }
    // And a whole example, end to end, on native.
    let hello = std::fs::read_to_string(repo_root().join("examples").join("hello.k2"))
        .expect("read hello.k2");
    assert_eq!(
        run_lexer("run-native", &hello),
        reference(&hello),
        "native token stream diverges on examples/hello.k2"
    );
}

/// The committed `selfhost/lexer.k2` runs as-is and its built-in sample matches
/// the reference for that sample's source — guarding against the file rotting.
#[test]
fn selfhost_lexer_default_sample_is_self_consistent() {
    // The default sample is `/// doc\npub fn main() !void {\n    const x = 0xFF_FF;\n    return;\n}\n`.
    let sample = "/// doc\npub fn main() !void {\n    const x = 0xFF_FF;\n    return;\n}\n";
    let k2c = env!("CARGO_BIN_EXE_k2c");
    let out = Command::new(k2c)
        .arg("run")
        .arg(repo_root().join("selfhost").join("lexer.k2"))
        .output()
        .expect("spawn k2c on committed lexer.k2");
    assert!(
        out.status.success(),
        "committed selfhost/lexer.k2 must run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        reference(sample),
        "committed lexer.k2 default sample diverges from the reference"
    );
}
