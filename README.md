# Agora (Rust rebuild)

A virtual office where Muse agents and people work as one team. The Rust engine schedules the work, checks every deliverable, remembers across sessions, and reports what each run cost.

Built for the Meta Global AI Developer Hackathon (Oct 26 - Nov 4, 2026), on Meta's Muse models.

> **Status:** day 1. A brief runs end to end from the command line on live Muse: plan, schedule, work, review, assemble. The office UI, memory and device surfaces are under construction.

## Built during the hackathon

All code in this repository was written during the hackathon, starting after kickoff on 26 Oct 2026. The git history is the record.

This is a from-scratch rebuild of an earlier TypeScript project, [Ritesh-Root/agora](https://github.com/Ritesh-Root/agora). The old project is used only as a reference and as a benchmark baseline: no code or assets are copied from it. Its 3D models are CC BY-NC and are not used here.

## What works today

- **`agora-core`:** validates a planner's task graph. Duplicate ids, self-dependencies, unknown dependencies and cycles are rejected, each with a message the planner model can act on.
- **`agora-engine`:** one run, end to end.
  - **Plan.** Muse returns a plan in a fixed JSON schema, and an invalid graph goes back to it with the exact error.
  - **Schedule.** See the next bullet.
  - **Work and review.** Each task's result is reviewed against its acceptance criteria. If the reviewer is unsure, two more reviews vote. A rejected draft gets one retry with the exact unmet criteria, and is then left for a person, without blocking unrelated tasks.
  - **Budget.** Every call is checked against a per-run budget cap.
  - **Code checks.** The planner sets word limits and required headings per task. Plain code checks them before any paid review.
  - **Resume.** Every completed reply is saved to a SQLite file (`agora-journal.db`) as it arrives. `agora-run --resume <run-id>` replays those replies for free and continues from the first call that never finished. Tested live by force-killing a run mid-way.
- **Scheduler (in `agora-engine`):** the ready-queue scheduler. A task starts the moment its own dependencies finish, the longest remaining chain goes first, and a failure skips only what depends on it. Property tests check dependency order, the parallel cap, critical-path optimality, and failure isolation on random plans.
- **`agora-muse`:** a typed client for Meta's Model API (Responses endpoint). It supports structured output and prompt-cache keys. It retries 429 and 5xx responses up to 4 times, waiting at least as long as `Retry-After` asks. It reads the rate-limit headers and computes the exact cost of every call, billing cached tokens at the cached rate. The API key can't appear in debug output. Tested against a local mock server; `examples/smoke.rs` checks it against the live API.
- **`agora-bench`:** B1, the old wave scheduler vs the ready-queue. Results and caveats are in [docs/benchmarks.md](docs/benchmarks.md).

## Run it

Requires Rust 1.99 or newer.

```bash
cargo test --workspace
cargo run -p agora-bench --release --bin sched-sim

# needs a Model API key
MODEL_API_KEY=... cargo run -p agora-muse --example smoke
MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- "your brief" > deliverable.md
MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- --resume <run-id>   # after a crash
```

## Principles

- Our code is 100% safe Rust (`unsafe_code = "forbid"` in every crate). Dependencies are pinned exactly.
- Every performance claim comes with the command that reproduces it.

## License

MIT. See [LICENSE](LICENSE).
