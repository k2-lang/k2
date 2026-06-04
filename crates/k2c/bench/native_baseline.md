# k2 native-vs-VM benchmark baseline

Host: x86_64-linux. Wall-clock is **best-of-5** (the minimum elapsed time rejects scheduler noise and is the most reproducible statistic). These numbers are *measured*, regenerated with `k2c bench --emit-report`, and will vary run-to-run; the CI gate is the conservative `>= 5x` speedup floor in the test suite, **not** these exact values. Native time includes a fixed process-spawn/startup cost, so the pure-compute ratio is higher still.

| kernel | native (ms) | vm (ms) | speedup | peephole .text (bytes) | reduction |
|---|---:|---:|---:|---:|---:|
| `bench_fib_rec_native` | 1.182 | 1892.560 | 1601.2x | 1249 -> 1249 | 0.0% |
| `bench_fib_rec` | 0.558 | 718.983 | 1288.5x | 1249 -> 1249 | 0.0% |
| `bench_loop_sum` | 0.193 | 3.884 | 20.1x | 694 -> 689 | 0.7% |
| `bench_slice_sum` | 0.202 | 22.563 | 111.7x | 1015 -> 1005 | 1.0% |

The peephole pass (redundant self-moves, dead stores, `mov r,0`->`xor`, jump-to-next/jump-to-jump) shrinks `.text` while leaving behavior byte-identical (verified differentially: native-opt == native-unopt == VM). The speedup is the headline result: native executes the same program many times faster than the bytecode VM.
