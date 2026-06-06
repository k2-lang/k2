# k2 conformance corpus

> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

A spec-conformance suite for the k2 language: small, deterministic programs that
exercise the language surface chapter by chapter, from lexical structure to the
standard library. It is the executable companion to [`docs/spec/`](../docs/spec)
and the backbone of the v0.30 "1.0 readiness" milestone.

## Layout

```
conformance/<NN-chapter>/<case>.k2          # a runnable k2 program
conformance/<NN-chapter>/<case>.expected    # its exact captured stdout
conformance/<NN-chapter>/<case>.native      # (optional) marker: native ≡ VM
```

The chapters mirror [`docs/spec/01..10`](../docs/spec): lexical structure, types,
expressions & statements, functions, memory & allocators, error handling,
comptime & generics, modules, concurrency, and the standard library.

## What is guaranteed

Each case is run by `crates/k2c/tests/conformance.rs`:

- On the **VM** (`k2c run`, the semantic reference): exits 0 and reproduces its
  `.expected` output byte-for-byte. Every case is **deterministic** — no clock,
  randomness, addresses, or unordered iteration appears in the asserted output.
- On the **native** x86-64 backend (`k2c run-native`): for every case carrying a
  `.native` marker, the native binary's output is byte-identical to the VM's.
  Cases without the marker use capabilities outside the current native subset and
  run on the VM only; the native backend *cleanly refuses* them rather than
  miscompiling.

To regenerate a case's expected output after an intentional change:

```sh
k2c run conformance/<chapter>/<case>.k2 > conformance/<chapter>/<case>.expected
```
