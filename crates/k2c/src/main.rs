//! # k2c — the k2 compiler driver (front-end CLI)
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This binary is the entry point of the k2 toolchain front-end. At this stage
//! it wires up the [`k2_lexer`] (and links [`k2_syntax`] for the AST types that
//! a future parser will populate). It exposes a single working subcommand,
//! `tokenize` (alias `lex`), that reads a `.k2` file — or standard input — and
//! prints the token stream.
//!
//! Argument parsing is done by hand with `std::env::args`: no third-party CLI
//! crate, so the toolchain builds and runs fully offline.
//!
//! ## Usage
//!
//! ```text
//! k2c tokenize <file.k2>     # lex a file and print its tokens
//! k2c tokenize -             # lex from standard input
//! k2c lex <file.k2>          # `lex` is an alias for `tokenize`
//! k2c help                   # print usage
//! k2c version                # print the version
//! ```
//!
//! The process exits `0` on success and a nonzero status on a usage or I/O
//! error (lexical *errors* in the source are reported as `Error` tokens in the
//! stream, not as a process failure — recovery is the lexer's job).

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use k2_lexer::{tokenize, Token, TokenKind};
// Linked to keep the AST crate in the dependency graph and demonstrate the
// span helper the future parser will use; not yet exercised by a real parse.
use k2_syntax::Span;

/// Program name used in diagnostics and the usage text.
const PROG: &str = "k2c";
/// Version string, kept in sync with the crate version by hand for now.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    // Skip argv[0] (the executable path) and dispatch on the subcommand.
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // All diagnostics go to stderr so stdout carries only real output.
            let _ = writeln!(io::stderr(), "{PROG}: error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Parses the subcommand and arguments and runs the requested action. Returns a
/// human-readable error string on failure.
fn run(args: &[String]) -> Result<(), String> {
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => {
            print_usage();
            return Ok(());
        }
    };

    match cmd {
        "tokenize" | "lex" => cmd_tokenize(rest),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("{PROG} {VERSION}");
            Ok(())
        }
        other => Err(format!("unknown subcommand `{other}` (try `{PROG} help`)")),
    }
}

/// The `tokenize` / `lex` subcommand: read source from a file path or stdin,
/// lex it, and print the token stream.
fn cmd_tokenize(args: &[String]) -> Result<(), String> {
    let path = match args {
        [] => {
            return Err("`tokenize` needs a <file.k2> argument (or `-` for stdin)".to_string());
        }
        [p] => p.as_str(),
        _ => {
            return Err(format!(
                "`tokenize` takes exactly one argument; got {}",
                args.len()
            ));
        }
    };

    let (source, label) = read_source(path)?;
    let tokens = tokenize(&source);
    print_tokens(&label, &tokens);
    Ok(())
}

/// Reads the source either from `-` (standard input) or from a file path.
/// Returns the source text together with a human-readable label for headers.
fn read_source(path: &str) -> Result<(String, String), String> {
    if path == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        Ok((buf, "<stdin>".to_string()))
    } else {
        if !path.ends_with(".k2") {
            // A soft warning, not an error: the lexer is content-agnostic, but
            // the .k2 extension is the single canonical one (spec §1.1).
            let _ = writeln!(
                io::stderr(),
                "{PROG}: warning: `{path}` does not end in `.k2`"
            );
        }
        let text = fs::read_to_string(path).map_err(|e| format!("reading `{path}`: {e}"))?;
        Ok((text, path.to_string()))
    }
}

/// Prints the token stream in a stable, greppable, column-aligned format, with
/// a trailing summary of how many tokens (and how many lexical errors) were
/// produced.
fn print_tokens(label: &str, tokens: &[Token]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let _ = writeln!(out, "# tokens for {label}");
    let _ = writeln!(out, "# {:>4} {:<3}  {:<16} text", "line", "col", "kind");

    let mut offset: u32 = 0;
    let mut error_count = 0usize;
    for tok in tokens {
        if tok.kind == TokenKind::Error {
            error_count += 1;
        }
        // Demonstrate the syntax-crate span helper on each token. The span is
        // computed but only its width is shown, to keep the output compact.
        let span: Span = Span::of_token(tok, offset);
        offset = span.end;

        let _ = writeln!(
            out,
            "{:>6}:{:<3} {:<16} {}",
            tok.line,
            tok.col,
            format!("{:?}", tok.kind),
            render_text(&tok.text),
        );
    }

    let _ = writeln!(
        out,
        "# {} token(s), {} lexical error(s)",
        tokens.len(),
        error_count
    );
}

/// Renders a token's source text for display, escaping newlines/tabs and
/// quoting so multi-line lexemes (multiline strings, doc comments) stay on one
/// output line.
fn render_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let escaped = text
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    format!("{escaped:?}")
}

/// Prints the usage banner to standard output.
fn print_usage() {
    // Plain `println!` is fine here; usage is normal program output.
    println!(
        "{PROG} {VERSION} — the k2 compiler front-end\n\
         \n\
         USAGE:\n\
         \x20   {PROG} <subcommand> [args]\n\
         \n\
         SUBCOMMANDS:\n\
         \x20   tokenize <file.k2>   Lex a source file and print its token stream.\n\
         \x20   tokenize -           Lex from standard input.\n\
         \x20   lex <file.k2>        Alias for `tokenize`.\n\
         \x20   help                 Show this help.\n\
         \x20   version              Print the version.\n"
    );
}
