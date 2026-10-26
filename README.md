# Agora (Rust rebuild)

A virtual office where Muse agents and people work as one team. The Rust engine schedules the work, checks every deliverable, remembers across sessions, and reports what each run cost.

Built for the Meta Global AI Developer Hackathon (Oct 26 - Nov 4, 2026), on Meta's Muse models.

> **Status:** day 1. The plan model and scheduler exist and are tested. Everything else is under construction.

## Built during the hackathon

All code in this repository was written during the hackathon, starting after kickoff on 26 Oct 2026. The git history is the record.

This is a from-scratch rebuild of an earlier TypeScript project, [Ritesh-Root/agora](https://github.com/Ritesh-Root/agora). The old project is used only as a reference and as a benchmark baseline: no code or assets are copied from it. Its 3D models are CC BY-NC and are not used here.

## What works today

- **`agora-core`:** validates a planner's task graph. Duplicate ids, self-dependencies, unknown dependencies and cycles are rejected, each with a message the planner model can act on.
- **`agora-engine`:** the ready-queue scheduler. A task starts the moment its own dependencies finish, the longest remaining chain goes first, and a failure skips only what depends on it. Property tests check dependency order, the parallel cap, critical-path optimality, and failure isolation on random plans.
- **`agora-bench`:** B1, the old wave scheduler vs the ready-queue. Results and caveats are in [docs/benchmarks.md](docs/benchmarks.md).

## Run it

Requires Rust 1.99 or newer.

```bash
cargo test --workspace
cargo run -p agora-bench --release --bin sched-sim
```

## Principles

- Our code is 100% safe Rust (`unsafe_code = "forbid"` in every crate). Dependencies are pinned exactly.
- Every performance claim comes with the command that reproduces it.

## License

MIT. See [LICENSE](LICENSE).
