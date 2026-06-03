# Security Policy

k2 is an early-stage language toolchain. Even so, we take security reports
seriously and want them to have a clear home.

## Supported versions

The project has not yet cut a stable release. Until `0.1.0` is tagged, **only
the `main` branch is supported** — fixes land there, and there are no backports.
This table will grow once we publish releases.

| Version    | Supported          |
| ---------- | ------------------ |
| `main`     | :white_check_mark: |
| (released) | not yet applicable |

## What counts as a security issue

Because k2 compiles untrusted-ish input, we are especially interested in:

- **Compiler memory safety** — a `.k2` input that causes the Rust toolchain
  (`k2c` and its crates) to panic on a path that should be a clean diagnostic,
  or, worse, to read/write out of bounds.
- **`comptime` sandbox escapes** — compile-time evaluation reading the
  filesystem, network, or environment beyond what the build graph authorizes
  (this directly violates the "no ambient authority" pillar; see
  `docs/philosophy.md`).
- **Build-system command injection** — `build.k2` logic that can be coerced
  into running unintended commands.
- **Unsound `@`-builtins** — a reflection or type builtin that lets safe code
  construct an invalid value or violate a documented invariant.

Crashes that are merely incorrect diagnostics (no unsafety, no escape) are
ordinary bugs — please file them in the public issue tracker instead.

## Reporting a vulnerability

**Do not open a public issue for a suspected vulnerability.**

Use GitHub's private reporting:

1. Go to the repository's **Security** tab → **Report a vulnerability**
   (GitHub Private Vulnerability Reporting), or
2. Email **security@k2-lang.org** with a description and, ideally, a minimal
   `.k2` reproduction.

Please include the toolchain commit, your platform, and the exact input. A
minimal reproduction is the single most useful thing you can send.

## Our commitment

- We aim to **acknowledge** a report within **3 business days**.
- We aim to give an initial assessment within **10 business days**.
- We will keep you updated on remediation and coordinate a disclosure date with
  you. We are happy to credit you in the changelog and the fix unless you ask us
  not to.

Thank you for helping keep k2 and its users safe.
