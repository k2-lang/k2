# k2 Language Specification — 01. Lexical Structure

> Part of the official k2 specification.
> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

This chapter defines how raw `.k2` source text is decomposed into a stream of
**tokens**. It is the foundation the [grammar](../grammar.ebnf) builds on: the
grammar's terminals are exactly the token kinds named here, and the two
documents are kept deliberately in agreement. Where this chapter says a lexeme
exists, the grammar consumes it; where the grammar references a terminal, this
chapter produces it.

Lexing is the first stage of the [compiler pipeline](../../README.md): *“Lex and
parse `.k2` source into an AST.”* It is purely textual — no type information, no
`comptime` evaluation, and no I/O participate. Consistent with k2's
**no ambient authority** pillar, even the lexer is a pure function from bytes to
tokens.

---

## 1. Source files and encoding

### 1.1 File extension

k2 source files use the extension **`.k2`**. The compiler's entry module, the
standard library, and the build script (`build.k2`) all share this single
extension. There is no separate header, interface, or configuration file format:
a project's build is itself ordinary k2 (see the **build.k2** distinctive
feature).

### 1.2 Encoding

A `.k2` file is a sequence of bytes interpreted as **UTF-8**. The lexer operates
on Unicode scalar values decoded from that UTF-8 stream.

- The byte order mark `U+FEFF`, if present, is permitted only as the very first
  scalar of the file and is discarded before tokenizing.
- Outside of comments, character literals, and string literals, only ASCII is
  permitted. The structural syntax of k2 — keywords, identifiers, operators,
  punctuation — is ASCII-only.
- Inside comments and string/character literals, any valid UTF-8 is allowed.
- A `NUL` (`U+0000`) byte anywhere in the source is a lexical error.

### 1.3 Line terminators

A line terminator is either a single line feed `U+000A` (`\n`) or a carriage
return followed by a line feed `U+000D U+000A` (`\r\n`). A lone `\r` not
followed by `\n` is treated as **whitespace** — the lenient, common choice — so a
file with old-style Mac line endings still lexes. (Earlier drafts called a lone
`\r` a lexical error; the rule was relaxed to match the reference lexer.) Line
terminators delimit line comments and are otherwise treated as whitespace.

---

## 2. Whitespace

The following scalars are **whitespace** and serve only to separate tokens:

| Scalar     | Name            |
| ---------- | --------------- |
| `U+0009`   | horizontal tab  |
| `U+000A`   | line feed       |
| `U+000D`   | carriage return |
| `U+0020`   | space           |

k2 is **not** whitespace-sensitive: indentation and line breaks carry no
semantic meaning, and statements are terminated explicitly with `;` while blocks
are delimited by `{` and `}`. Whitespace is required only where two adjacent
tokens would otherwise merge into one (e.g. `const x` cannot be written
`constx`). Any run of whitespace is equivalent to a single space. Form feed,
vertical tab, and other control characters are not whitespace and are lexical
errors outside literals and comments.

---

## 3. Comments

k2 has exactly two comment forms, both line-based. There is **no block /
nestable comment syntax** — a deliberate *one obvious way, small surface*
decision that keeps the lexer trivial and avoids the classic “unterminated block
comment ate my file” failure mode.

### 3.1 Line comments

A line comment begins with `//` and extends to (but does not include) the next
line terminator or end of file. It is discarded entirely by the lexer.

```k2
// This is a line comment.
const x = 1; // Trailing comments are fine.
```

### 3.2 Doc comments

A doc comment begins with `///` and likewise runs to end of line, but is
**retained** by the lexer as an attached token so that documentation tooling and
`comptime` reflection can read it. Doc comments document the declaration that
immediately follows them; consecutive `///` lines are concatenated into one
documentation block.

```k2
/// A growable list. `List` is a function from a type to a type,
/// evaluated at comptime — k2 has no separate generics syntax.
pub fn List(comptime T: type) type {
    // ...
}
```

A `///` that does not precede a declaration is permitted but carries no
attachment. Note that `////` (four or more slashes) is an ordinary line comment,
not a doc comment: the lexer matches exactly three slashes for `///`, with the
fourth slash being part of the comment body.

There is no separate “module doc comment” token; a doc comment block at the top
of a file simply documents the first declaration, and tooling may treat a
leading block specially by convention.

---

## 4. Identifiers

An ordinary identifier matches:

```
identifier      = ( letter | "_" ) , { letter | digit | "_" } ;
letter          = "A" … "Z" | "a" … "z" ;
digit           = "0" … "9" ;
```

- Identifiers are ASCII-only and **case-sensitive**: `Point`, `point`, and
  `POINT` are three distinct names.
- A bare underscore `_` is a legal identifier and, by convention, the
  **discard** binding — a write-only placeholder for a value you do not intend
  to read (e.g. `for (items) |_| { ... }`). It is the only identifier the
  language treats specially, and only at binding sites.
- Identifiers that collide with a [keyword](#5-keywords) are not identifiers;
  see §4.2 for the escape hatch.

By convention (not enforced by the lexer): types and the `type`-returning
generic functions use `PascalCase`; functions, variables, and fields use
`snake_case` or `camelCase`; and `comptime` constants follow the same rules as
their runtime counterparts. These are style guidelines, not lexical rules.

### 4.1 `@`-builtins

A name beginning with `@` immediately followed by an identifier is a
**builtin** token (a compile-time builtin function), e.g. `@import`, `@TypeOf`,
`@sizeOf`. The `@` sigil is reserved exclusively for builtins:

```
builtin         = "@" , identifier ;
```

There is no whitespace between `@` and the name. The complete, closed set of
builtins recognized by the compiler is:

`@import`, `@TypeOf`, `@typeInfo`, `@Type`, `@sizeOf`, `@alignOf`,
`@compileError`, `@compileLog`, `@hasField`, `@field`, `@as`, `@intCast`,
`@ptrCast`, `@embedFile`, `@panic`.

A `@name` that is not in this set is a compile error reported during semantic
analysis; lexically it is still a `builtin` token. (Diagnostic-only names such as
`@This`, `@typeName`, and `@errorName` used in the canonical examples are
provided by the standard prologue as builtins of the same lexical form; the list
above is the charter's core set.)

### 4.2 Escaped identifiers

To use a keyword or an otherwise non-conforming name as an identifier — most
often when binding to a symbol coming from C via `extern` — k2 provides the
`@"..."` escaped-identifier form:

```
escaped_identifier = "@" , string_literal ;
```

```k2
const @"const" = 1;            // bind the keyword `const` as a name
extern fn @"weird-c-name"() void;
```

The string's contents become the identifier verbatim. This is the only place a
keyword may appear in identifier position.

---

## 5. Keywords

The following **35** words are reserved and may never be used as ordinary
identifiers (use the `@"..."` form of §4.2 if you must). This is the complete,
locked keyword set:

```
const      var       pub         fn         comptime
return     struct    enum        union      error
if         else      while       for        switch
break      continue  defer       errdefer   try
catch      orelse    and         or         not
unreachable test      extern      export     inline
align      true      false       null       undefined
```

That is exactly 35 words (7 rows × 5 columns).

Grouped by role:

- **Declarations & visibility:** `const`, `var`, `pub`, `fn`, `comptime`,
  `extern`, `export`, `inline`, `align`, `test`.
- **Aggregates & errors:** `struct`, `enum`, `union`, `error`.
- **Control flow:** `if`, `else`, `while`, `for`, `switch`, `break`, `continue`,
  `return`, `defer`, `errdefer`, `unreachable`.
- **Error / optional operators:** `try`, `catch`, `orelse`.
- **Word-based boolean operators:** `and`, `or`, `not`.
- **Literal keywords:** `true`, `false`, `null`, `undefined`.

### 5.1 Word-based boolean operators

k2 uses **words, not symbols**, for the entire boolean operator set: `and`,
`or`, and `not`. This is a deliberate difference from Zig: in k2 the symbolic
`!` is reserved **strictly** for error-union types (`!T`, `E!T`) and is *never*
boolean negation. Likewise there are no `&&` / `||` tokens.

```k2
if (a and not b or c) { ... }   // boolean logic reads as words
const u: !void = ...;            // `!` means "error union", never "not"
```

`and` and `or` short-circuit and are part of the precedence table (§7).

### 5.2 Literal keywords

`true` and `false` are the two `bool` values. `null` is the absence value
inhabiting any optional `?T`. `undefined` is the explicit “uninitialized”
placeholder; reading a value that is still `undefined` is detectable in safe
builds. These four are keywords, not identifiers, even though they appear in
expression position.

### 5.3 Primitive type names

The primitive type names are **not** keywords — they are predeclared
identifiers, visible everywhere unless shadowed:

```
i8  i16  i32  i64  i128  isize
u8  u16  u32  u64  u128  usize
f32 f64
bool void type noreturn anyerror
comptime_int comptime_float
```

Because they are identifiers rather than reserved words, the grammar treats a
type reference as an ordinary `identifier`; the predeclared meaning is assigned
during name resolution, not lexing.

---

## 6. Literals

### 6.1 Integer literals

An integer literal denotes a `comptime_int` until it is coerced to a concrete
integer type. Four bases are supported, each with an optional underscore digit
separator for readability. Underscores may appear between digits but not
leading, trailing, or adjacent to the base prefix.

```
int_literal     = dec_int | hex_int | oct_int | bin_int ;
dec_int         = dec_digit , { dec_digit | "_" , dec_digit } ;
hex_int         = "0x" , hex_digit , { hex_digit | "_" , hex_digit } ;
oct_int         = "0o" , oct_digit , { oct_digit | "_" , oct_digit } ;
bin_int         = "0b" , bin_digit , { bin_digit | "_" , bin_digit } ;

dec_digit       = "0" … "9" ;
hex_digit       = "0" … "9" | "a" … "f" | "A" … "F" ;
oct_digit       = "0" … "7" ;
bin_digit       = "0" | "1" ;
```

```k2
const a = 1_000_000;     // decimal with separators
const b = 0xFF_FF;       // hex
const c = 0o755;         // octal
const d = 0b1010_0101;   // binary
```

Integer literals are unsigned in their lexical form; a leading `-` is the unary
negation **operator** (§7), not part of the literal. There is no `l`/`u`/etc.
suffix — width and signedness come from the target type via coercion or `@as`.

### 6.2 Float literals

A float literal denotes a `comptime_float` until coerced to `f32`/`f64`. Both
decimal and hexadecimal floats are supported; an exponent is required for hex
floats. Underscore separators are allowed between digits.

```
float_literal   = dec_float | hex_float ;
dec_float       = dec_int , "." , dec_digits , [ dec_exp ]
                | dec_int , dec_exp ;
dec_exp         = ( "e" | "E" ) , [ "+" | "-" ] , dec_digits ;
hex_float       = "0x" , hex_digits , "." , hex_digits , hex_exp
                | "0x" , hex_digits , hex_exp ;
hex_exp         = ( "p" | "P" ) , [ "+" | "-" ] , dec_digits ;

dec_digits      = dec_digit , { dec_digit | "_" , dec_digit } ;
hex_digits      = hex_digit , { hex_digit | "_" , hex_digit } ;
```

```k2
const pi   = 3.141_592_653;
const avo  = 6.022e23;
const tiny = 1.5e-9;
const hx   = 0x1.8p3;     // hex float = 12.0
```

A digit must appear on **both** sides of the `.` in a decimal float: `1.` and
`.5` are not valid float literals (write `1.0` and `0.5`). This keeps `x.0`
field/method access unambiguous from a malformed float.

### 6.3 Character literals

A character literal is a single Unicode scalar value in single quotes, of type
`comptime_int` (it coerces to any integer type wide enough to hold it, typically
`u8` or `u21`).

```
char_literal    = "'" , ( char_scalar | escape_seq ) , "'" ;
char_scalar     = ? any Unicode scalar except "'", "\", or a line terminator ? ;
```

```k2
const newline = '\n';
const a       = 'a';        // 97
const heart   = '❤';        // a Unicode scalar value
const quote   = '\'';
```

The recognized escape sequences (`escape_seq`) are shared with string literals
and listed in §6.6.

### 6.4 String literals

A normal string literal is a sequence of scalars and escapes in double quotes.
Its type is `[]const u8` (a UTF-8 byte slice); it does **not** allocate and
carries no implicit `NUL` terminator. A string literal may not span a line
terminator — use the multiline form (§6.5) for that.

```
string_literal  = '"' , { str_scalar | escape_seq } , '"' ;
str_scalar      = ? any Unicode scalar except '"', "\", or a line terminator ? ;
```

```k2
const greeting = "Hello, k2!\n";
const path     = "C:\\tmp\\out.bin";
const unicode  = "caf\u{00E9}";
```

### 6.5 Multiline string literals

For text spanning many lines, k2 uses a **line-prefixed** multiline form: each
physical line of the string starts with `\\`. There are no escape sequences
inside a multiline string (it is raw), and the newline between consecutive
`\\` lines is part of the value. The literal ends at the first line that does
not begin (after optional leading whitespace) with `\\`.

```
multiline_string = ml_line , { line_terminator , ml_line } ;
ml_line          = "\\" , { ? any scalar except a line terminator ? } ;
```

```k2
const usage =
    \\Usage: k2 build [options]
    \\
    \\  --target <triple>   Cross-compile target.
    \\  --release-fast      Strip safety checks for raw throughput.
    ;
```

The value above is exactly the four lines of text (with the blank middle line),
joined by `\n`, with **no** trailing newline after the last line. Because the
form is raw, backslashes and quotes inside need no escaping — ideal for paths,
regexes, and embedded source. This is the single multiline string spelling in
the language (*one obvious way*).

### 6.6 Escape sequences

The following escape sequences are valid inside character literals (§6.3) and
normal string literals (§6.4). They are **not** interpreted inside multiline
strings.

| Escape       | Meaning                                  |
| ------------ | ---------------------------------------- |
| `\n`         | line feed (`U+000A`)                     |
| `\r`         | carriage return (`U+000D`)               |
| `\t`         | horizontal tab (`U+0009`)                |
| `\\`         | backslash (`U+005C`)                     |
| `\'`         | single quote (`U+0027`)                  |
| `\"`         | double quote (`U+0022`)                  |
| `\x` *HH*    | one byte from exactly two hex digits     |
| `\u{` *H…* `}` | one Unicode scalar from 1–6 hex digits |

```
escape_seq      = "\" , ( "n" | "r" | "t" | "\" | "'" | '"'
                        | "x" , hex_digit , hex_digit
                        | "u" , "{" , hex_digit , { hex_digit } , "}" ) ;
```

`\x` produces a single byte (it may form part of a multi-byte UTF-8 sequence the
programmer is assembling). `\u{...}` produces the UTF-8 encoding of the named
Unicode scalar and must name a value `≤ U+10FFFF` that is not a surrogate. Any
other backslash sequence is a lexical error — there is no implicit pass-through.

### 6.7 The literal keywords as values

`true`, `false`, `null`, and `undefined` (§5.2) are lexed as keywords, but
semantically they are literals: the two `bool` values, the optional-absence
value, and the uninitialized placeholder, respectively. The grammar admits them
wherever a primary expression is expected.

---

## 7. Operators and punctuation

### 7.1 Operator and punctuator tokens

k2's symbolic tokens are listed below. Recall that boolean logic uses the
**keywords** `and`/`or`/`not` (§5.1), and `!` is **only** the error-union type
constructor — neither appears in this table as a boolean operator.

| Category          | Tokens                                                |
| ----------------- | ----------------------------------------------------- |
| Arithmetic        | `+`  `-`  `*`  `/`  `%`                                |
| String/list concat | `++`                                                 |
| Bitwise           | `&`  `\|`  `^`  `~`  `<<`  `>>`                        |
| Comparison        | `==`  `!=`  `<`  `<=`  `>`  `>=`                       |
| Assignment        | `=`  `+=`  `-=`  `*=`  `/=`  `%=`  `&=`  `\|=`  `^=`  `<<=`  `>>=` |
| Member / postfix  | `.` (field)   `.*` (deref)   `.?` (optional unwrap)   |
| Type sigils       | `?` (optional `?T`)   `!` (error-union `E!T` / `!T`)  |
| Punctuation       | `(` `)`  `{` `}`  `[` `]`  `,`  `;`  `:`  `=>`  `..`  `...` |
| Name sigil        | `@` (builtin `@name` / escaped identifier `@"..."`)   |

These are **all** of k2's symbolic tokens; there is no `&&`, `||`, `!` (boolean),
`->`, increment `++x`/decrement `--`, or block-comment delimiter. The single `&`
glyph serves as both prefix address-of and infix bitwise-AND, the single `|`
glyph serves as both infix bitwise-OR and capture delimiter `|x|`, and the single
`-` glyph serves as both prefix negation and infix subtraction; the grammar
distinguishes each pair by position.

Notes on the cluster of `.`-prefixed tokens, which the lexer disambiguates by
maximal munch:

- `.x` — field / member access (the `.` is the binary access operator).
- `.*` — pointer dereference, written postfix: `ptr.*`.
- `.?` — optional unwrap (assert-non-null), written postfix: `opt.?`.
- `.{ ... }` and `.{ .field = v }` — anonymous struct / initializer literal.
- `.Name` — enum/union variant or error literal shorthand in inferred context.
- `..` — range in `for` and slicing (`a..b`); `...` — inclusive range in
  `switch` patterns (`1...9`).

`++` is **compile-time** array/string concatenation (as used in
`@compileError("... " ++ @typeName(T))`); it is not a runtime string-builder and
performs no allocation.

### 7.2 Maximal munch

The lexer is **greedy**: at each position it consumes the longest sequence of
characters that forms a valid token. Thus `<<=` lexes as one token, not `<<`
then `=`; `..` is one token, not two `.`; and `===` lexes as `==` then `=`
(which is then a parse error). Where you intend two tokens you must separate
them (rare in practice).

### 7.3 Precedence and associativity

The table below lists every operator class from **lowest** binding (level 12,
applied last / outermost) to **highest** binding (level 1, applied first /
innermost). Operators in the same row share a precedence level. This numbering
is identical to — and authoritative for — the expression cascade in
[`docs/grammar.ebnf`](../grammar.ebnf) §6, whose **lowest**-precedence rule
(`coalesce_expr`) sits at the **top** of the cascade and whose **highest**-
precedence rule (`postfix_expr`) sits at the bottom. The fourth column names the
grammar rule that encodes each level. k2 has no operator overloading (per
*no hidden control flow*), so these meanings are fixed and total.

| Lvl | Operators                                       | Assoc.        | Grammar rule    |
| --- | ----------------------------------------------- | ------------- | --------------- |
| 12  | `orelse` · `catch` (`catch \|err\|`)            | left          | `coalesce_expr` |
| 11  | `or`                                            | left (short-circuit) | `or_expr`  |
| 10  | `and`                                           | left (short-circuit) | `and_expr` |
| 9   | `==` · `!=` · `<` · `<=` · `>` · `>=`           | **non-assoc** | `compare_expr`  |
| 8   | `\|`                                            | left          | `bitor_expr`    |
| 7   | `^`                                             | left          | `bitxor_expr`   |
| 6   | `&`                                             | left          | `bitand_expr`   |
| 5   | `<<` · `>>`                                      | left          | `shift_expr`    |
| 4   | `+` · `-` · `++`                                 | left          | `add_expr`      |
| 3   | `*` · `/` · `%`                                  | left          | `mul_expr`      |
| 2   | prefix `-` · `~` · `not` · `&` · `try` · `comptime` | right     | `unary_expr`    |
| 1   | `a()` call · `a[i]` index/slice · `a.b` field · `a.*` deref · `a.?` unwrap · `@b()` builtin | left | `postfix_expr` |

Clarifying rules:

- **Assignment is not in this table.** The assignment operators (`=`, `+=`, …)
  are part of the *statement* grammar (`assign_stmt`), never of an expression, so
  they have no precedence relative to other operators. `a = b = c` is a parse
  error and `if (x = 1)` does not parse — this eliminates the `=`/`==` confusion
  class entirely.
- **`!` and `?` are not in this table as operators.** In *type* position `?T`
  and `E!T`/`!T` are type constructors parsed by the type grammar; in *value*
  position the related operators are the postfix `.?` (unwrap, level 1) and the
  prefix `try` (level 2) / infix `catch` (level 12) for error unions.
- **Comparisons are non-associative** to forbid the ambiguous `a < b < c`; write
  `a < b and b < c`.
- **`and` (level 10) binds tighter than `or` (level 11)** but both bind looser
  than comparison (level 9), so `a == b and c == d` groups as
  `(a == b) and (c == d)`. Use parentheses for clarity in mixed expressions.
- **`orelse`/`catch` are the loosest binary operators** (level 12), so
  `const x = maybe orelse default;` and
  `const p = thing() catch |err| { ... };` parse as a single initializer.
- **Prefix `not` (level 2) binds tighter than comparison (level 9)**, so
  `not a == b` parses as `(not a) == b`: `not` binds only to its immediate
  unary/postfix operand (`a`), and the result is then compared with `b`. This
  parses fine but is a **type error** (it compares the `bool` `not a` against
  `b`), surfaced during semantic analysis rather than at parse time. When you
  mean "negate the comparison," parenthesize: write `not (a == b)`.
- **Ternary does not exist.** Use `if (cond) a else b`, which is an ordinary
  expression (`if_expr`) in k2.

### 7.4 The role of `@` and `.`

`@` never stands alone: it forms either a `builtin` (`@name`) or an escaped
identifier (`@"..."`). A leading `.` forms the access operator, the deref/unwrap
postfixes, or — when followed by `{` or an identifier — an anonymous
struct/enum-literal in inferred context. The lexer emits the `.`-tokens by
maximal munch and leaves their disambiguation to the parser.

---

## 8. Token kinds (summary)

The lexer emits exactly these token kinds, which are the terminals of
[`docs/grammar.ebnf`](../grammar.ebnf):

| Token kind            | Produced by                                   |
| --------------------- | --------------------------------------------- |
| `identifier`          | §4 (including `_` and predeclared type names) |
| `builtin`             | §4.1 (`@name`)                                |
| `escaped_identifier`  | §4.2 (`@"..."`)                               |
| `keyword`             | §5 (one of the 35 reserved words)             |
| `int_literal`         | §6.1                                          |
| `float_literal`       | §6.2                                          |
| `char_literal`        | §6.3                                          |
| `string_literal`      | §6.4                                          |
| `multiline_string`    | §6.5                                          |
| `operator`            | §7.1 (symbolic operators)                     |
| `punctuation`         | §7.1 (`(`, `)`, `{`, `}`, `,`, `;`, `:`, `=>`, …) |
| `doc_comment`         | §3.2 (retained)                               |

Line comments (§3.1) and whitespace (§2) are consumed and produce **no** token.
End of file produces an implicit end-of-input marker the parser uses to close
the top-level declaration list.

---

## 9. Worked example

The following valid k2 program exercises most lexical forms. Each annotation
points at the rule that governs the highlighted lexeme.

```k2
const std = @import("std");          // builtin @import + string_literal

/// Doubles every byte; demonstrates literals and operators.   <- doc_comment
pub fn main(sys: *System) !void {     // keywords pub/fn; `!void` error-union type
    const alloc = sys.heap;           // identifier.field via `.`
    const buf: []u8 = try alloc.alloc(u8, 0x10);  // try prefix; hex int 0x10
    defer alloc.free(buf);            // keyword defer

    for (buf, 0..) |*slot, i| {       // `..` range; `|*slot, i|` capture
        slot.* = @intCast(i << 1);    // `.*` deref; `<<` shift; @intCast builtin
    }

    const note =                       // multiline string literal:
        \\done — all bytes doubled.
        \\(no allocation happened behind your back)
        ;

    const out = sys.io.stdout();
    if (buf.len > 0 and not isEmpty(buf)) {   // word operators `and`, `not`
        try out.print("{s}\n", .{note});       // .{...} anonymous tuple literal
    }
}
```

Everything outside comments and string contents is ASCII; the em-dash and the
prose in the multiline string and comment are valid UTF-8 permitted only there.

---

## 10. Conformance notes

- A lexically valid token stream is necessary but not sufficient for a valid
  program; the [grammar](../grammar.ebnf) and later semantic phases impose
  further constraints (e.g. a `@name` must be a known builtin, a `?T` must wrap a
  type, comparisons may not chain).
- The keyword set, builtin set, primitive type names, file extension, and the
  word-based boolean operators (`and`/`or`/`not`) are **locked** by the k2
  charter and identical across the whole toolchain.
- This chapter and `docs/grammar.ebnf` are co-normative: any discrepancy is a
  documentation bug to be fixed in favor of mutual agreement.
