//! The benchmark integration test: a committed instruction-count baseline plus
//! the "measurably faster" CI gate.
//!
//! For each committed bench program (included from the `k2c` crate's `bench/`
//! directory) this test:
//! * lowers + optimizes it under Debug, ReleaseFast, ReleaseSafe,
//! * runs each on the VM with the deterministic executed-instruction counter,
//! * asserts the optimized release modes produce byte-identical output to Debug
//!   (a divergence here would be a miscompile, but the differential corpus is the
//!   primary guard; this is a second line),
//! * asserts `fast <= debug` for every bench and `fast < debug` (a measurable
//!   reduction) for the loop/fib/slice/struct/branch benches,
//! * asserts ReleaseSafe keeps non-redundant checks where the data is runtime
//!   (slice/loop benches) and `safe <= debug` (Safe is still optimized).

use k2_mir::{lower_program, BuildMode, MirProgram};
use k2_opt::{optimize, OptLevel};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;
use k2_vm::run_metered;

/// One bench program: its display name and source.
struct Bench {
    name: &'static str,
    src: &'static str,
}

/// The committed bench corpus, included from `crates/k2c/bench`.
fn corpus() -> Vec<Bench> {
    vec![
        Bench {
            name: "bench_fib_rec",
            src: include_str!("../../k2c/bench/bench_fib_rec.k2"),
        },
        Bench {
            name: "bench_fib_iter",
            src: include_str!("../../k2c/bench/bench_fib_iter.k2"),
        },
        Bench {
            name: "bench_loop_sum",
            src: include_str!("../../k2c/bench/bench_loop_sum.k2"),
        },
        Bench {
            name: "bench_slice_sum",
            src: include_str!("../../k2c/bench/bench_slice_sum.k2"),
        },
        Bench {
            name: "bench_struct_kernel",
            src: include_str!("../../k2c/bench/bench_struct_kernel.k2"),
        },
        Bench {
            name: "bench_const_branch",
            src: include_str!("../../k2c/bench/bench_const_branch.k2"),
        },
    ]
}

/// Lowers + optimizes `src` under `mode`/`level` (verifying the result), returning
/// the program.
fn lower_opt(src: &str, mode: BuildMode, level: OptLevel) -> MirProgram {
    let pres = parse(src);
    assert!(pres.is_ok(), "parse errors");
    let resolved = resolve_file(&pres.file);
    assert!(resolved.is_ok(), "resolve errors");
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors");
    let mut prog = lower_program(&pres.file, &resolved, typed, mode).expect("lowering");
    optimize(&mut prog, level);
    assert!(
        prog.verify().is_empty(),
        "verify after opt: {:?}",
        prog.verify()
    );
    prog
}

/// Runs a program and returns `(stdout, exit_code, instr_count)`.
fn run(prog: &MirProgram) -> (Vec<u8>, i32, u64) {
    let (_outcome, code, out, _err, count) = run_metered(prog);
    (out, code, count)
}

#[test]
fn benchmarks_are_measurably_faster_and_consistent() {
    // Benches whose cost is dominated by the optimized loop body must show a
    // strict reduction.
    let must_improve = [
        "bench_fib_rec",
        "bench_fib_iter",
        "bench_loop_sum",
        "bench_slice_sum",
        "bench_struct_kernel",
        "bench_const_branch",
    ];

    for b in corpus() {
        let debug = lower_opt(b.src, BuildMode::Debug, OptLevel::None);
        let fast = lower_opt(b.src, BuildMode::ReleaseFast, OptLevel::Fast);
        let safe = lower_opt(b.src, BuildMode::ReleaseSafe, OptLevel::Safe);

        let (out_d, code_d, count_d) = run(&debug);
        let (out_f, code_f, count_f) = run(&fast);
        let (out_s, code_s, count_s) = run(&safe);

        // Behavior identical across modes (second line of defense; the
        // differential corpus is the primary guard).
        assert_eq!(out_f, out_d, "[{}] fast stdout != debug", b.name);
        assert_eq!(code_f, code_d, "[{}] fast exit != debug", b.name);
        assert_eq!(out_s, out_d, "[{}] safe stdout != debug", b.name);
        assert_eq!(code_s, code_d, "[{}] safe exit != debug", b.name);

        // Never slower than Debug.
        assert!(
            count_f <= count_d,
            "[{}] ReleaseFast ({count_f}) must not exceed Debug ({count_d})",
            b.name
        );
        assert!(
            count_s <= count_d,
            "[{}] ReleaseSafe ({count_s}) must not exceed Debug ({count_d})",
            b.name
        );

        // A measurable reduction on the headline benches.
        if must_improve.contains(&b.name) {
            assert!(
                count_f < count_d,
                "[{}] ReleaseFast ({count_f}) must be < Debug ({count_d})",
                b.name
            );
        }
    }
}

#[test]
fn release_safe_keeps_checks_release_fast_strips_them() {
    // The slice/loop benches operate on runtime data, so ReleaseSafe must retain
    // bounds/overflow checks (not provably redundant), while ReleaseFast has none.
    for name in ["bench_slice_sum", "bench_loop_sum"] {
        let b = corpus().into_iter().find(|b| b.name == name).unwrap();

        let safe = lower_opt(b.src, BuildMode::ReleaseSafe, OptLevel::Safe);
        assert!(
            safe.check_count() > 0,
            "[{name}] ReleaseSafe must keep non-redundant checks"
        );

        let fast = lower_opt(b.src, BuildMode::ReleaseFast, OptLevel::Fast);
        assert_eq!(
            fast.check_count(),
            0,
            "[{name}] ReleaseFast must have no checks"
        );
    }
}

/// Asserts the committed baseline file is consistent with freshly-measured counts
/// (within an exact match — the metric is deterministic). If the optimizer
/// changes and the counts move, regenerate with `k2c bench --emit-baseline` and
/// update `bench/baseline.txt`. A mismatch fails loudly so the baseline never
/// silently drifts.
#[test]
fn committed_baseline_matches_measured() {
    let baseline = include_str!("../../k2c/bench/baseline.txt");
    let mut expected: std::collections::HashMap<String, (u64, u64, u64)> =
        std::collections::HashMap::new();
    for line in baseline.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `<name> debug=<n> fast=<n> safe=<n>`
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap().to_string();
        let mut d = 0u64;
        let mut f = 0u64;
        let mut s = 0u64;
        for kv in parts {
            let (k, v) = kv.split_once('=').expect("baseline kv");
            let v: u64 = v.parse().expect("baseline count");
            match k {
                "debug" => d = v,
                "fast" => f = v,
                "safe" => s = v,
                _ => panic!("unknown baseline column {k}"),
            }
        }
        expected.insert(name, (d, f, s));
    }

    for b in corpus() {
        let (_, _, count_d) = run(&lower_opt(b.src, BuildMode::Debug, OptLevel::None));
        let (_, _, count_f) = run(&lower_opt(b.src, BuildMode::ReleaseFast, OptLevel::Fast));
        let (_, _, count_s) = run(&lower_opt(b.src, BuildMode::ReleaseSafe, OptLevel::Safe));
        let (ed, ef, es) = expected
            .get(b.name)
            .copied()
            .unwrap_or_else(|| panic!("no baseline for {}", b.name));
        assert_eq!(
            count_d, ed,
            "[{}] debug count drifted from baseline",
            b.name
        );
        assert_eq!(count_f, ef, "[{}] fast count drifted from baseline", b.name);
        assert_eq!(count_s, es, "[{}] safe count drifted from baseline", b.name);
    }
}
