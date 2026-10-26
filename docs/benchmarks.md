# Benchmarks

Every number here comes from a command in this repository. Where a result was weaker than we hoped, it stays in.

## B1: scheduling (old waves vs ready-queue)

**Question:** how much time does the old engine lose by running a plan in waves, where each wave waits for its slowest task?

**Setup** (`crates/agora-bench/src/bin/sched_sim.rs`):
- 1,000 random plans shaped like LLM plans: 3-12 tasks, each depending on earlier tasks with probability `edge_p`, at most 3 dependencies.
- Each task's latency is drawn from a log-normal distribution with an 8 s median, with spread `sigma`.
- Both schedulers run the same plans with the same latencies, on tokio's paused virtual clock, so results are exact and repeatable.
- The ready-queue doesn't know the real latencies. It prioritises by the number of tasks left on each chain.
- `waves` reproduces the old engine's `dependencyWaves` + `Promise.all`.

**Reproduce:** `cargo run -p agora-bench --release --bin sched-sim -- 1000 42 0.6 0.3`

**Default run (seed 42, sigma 0.6, edge_p 0.3).** Saving is how much less time the ready-queue took than waves. "Slower" counts plans where the ready-queue lost.

| Parallel slots | Median saving | p90 saving | Mean | Faster | Slower | Worst case |
|---|---|---|---|---|---|---|
| unlimited | 9.8% | 30.7% | 12.4% | 686 / 1000 | 0 | 0.0% |
| 4 | 10.1% | 30.5% | 12.5% | 687 / 1000 | 0 | 0.0% |
| 2 | 11.3% | 24.2% | 11.6% | 771 / 1000 | 13 | −10.1% |

**Sensitivity:**

| Variation | Median saving (4 slots) | p90 saving (4 slots) |
|---|---|---|
| less latency spread (sigma 0.3) | 6.8% | 22.3% |
| more latency spread (sigma 1.0) | 10.5% | 32.5% |
| sparser plans (edge_p 0.15) | 14.3% | 33.9% |
| denser plans (edge_p 0.5) | 5.5% | 23.3% |
| another seed (7) | 10.2% | 28.1% |

**What it means:**
- The win is real but modest on a typical plan (about 5-14% at the median). It's large on plans with a slow straggler (about 20-34% at p90).
- With unlimited slots the ready-queue can't lose: it finishes exactly on the critical path. A property test checks this on random plans.
- With 2 slots, greedy list scheduling can occasionally lose (Graham's anomaly): 7-32 plans per 1,000, at worst about 10% slower.
- Scheduling alone is not why to rebuild the engine. The larger expected gains are in verification, caching and cost control, which B2 measures.

## First live run (2026-10-26, not a benchmark)

One brief on `muse-spark-1.3`, standard pricing. The brief asked for a one-page delivery launch plan for a Pune bakery.

| Tasks | Calls | Wall time | Input tokens (cached) | Output tokens (reasoning) | Cost |
|---|---|---|---|---|---|
| 5 | 19 | 67.2 s | 19,921 (9,119 = 46%) | 33,885 (28,016 = 83%) | $0.159 |

- 4 tasks were accepted. One of them (the WhatsApp message) was accepted on its second draft, after the reviewer listed what was missing.
- The final "assemble" task was rejected twice for exceeding a word limit and left for a person. Word limits should be checked in code, not by a model (next change).
- **Reasoning tokens are 83% of output** even at `low` effort, so effort per role is the biggest cost lever. B2 will measure acceptance rate against effort.

Reproduce: `MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- "<brief>"`. A single run varies; B2 will repeat many briefs.
