//! `refdump` — prints the k2-lexer reference token stream for a source file in
//! the canonical `line:col Kind byteLen` form (one token per line, ending with
//! `… Eof 0`). This is the exact format `selfhost/lexer.k2` emits, so the two can
//! be diffed byte-for-byte. Used by the self-hosting differential tooling.
//!
//! Usage: `cargo run -q -p k2-lexer --example refdump -- <file.k2>`

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: refdump <file.k2>");
            std::process::exit(2);
        }
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("refdump: cannot read {path}: {e}");
            std::process::exit(2);
        }
    };
    let mut out = String::new();
    for t in k2_lexer::tokenize(&src) {
        out.push_str(&format!(
            "{}:{} {:?} {}\n",
            t.line,
            t.col,
            t.kind,
            t.text.len()
        ));
    }
    print!("{out}");
}
