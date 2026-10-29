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
  - **Budget.** Every call's cost is counted and shown live. Runs are uncapped by default; `AGORA_CAP_USD` sets a per-run cap (do set one before opening the hub to others).
  - **Code checks.** The planner sets word limits and required headings per task. Plain code checks them before any paid review.
  - **Resume.** Every completed reply is saved to a SQLite file (`agora-journal.db`) as it arrives. `agora-run --resume <run-id>` replays those replies for free and continues from the first call that never finished. Tested live by force-killing a run mid-way.
- **`agora-server`:** the room hub and office board.
  - Start a brief in the browser and watch each team member's card move: working, drafted, sent back with the unmet criteria, then accepted or needs a person.
  - The cost updates live, and the deliverable appears at the end.
  - Runs stream as Server-Sent Events. A link like `/#run-…` replays a run from the start.
  - **3D office:** Three.js with soft shadows, filmic tone mapping and environment light. Each agent has a desk in a pod; they stand in the aisle while work waits, walk to their chair and sit to work, cheer when accepted, and the reviewer walks the aisles to each draft (skeletal animation with crossfades). Speech bubbles show the chat; Enter VR appears on headsets. Without WebGL2 the kanban remains. Characters and furniture are Kenney's CC0 packs (see Credits).
  - It listens on localhost unless `AGORA_TOKEN` is set, and every `/api` call then needs the token. At most 2 runs are active at once.
- **Open-weights reviewer:** `AGORA_REVIEWER=baseten` reviews on Muse Spark Open via Baseten (Chat Completions), while planning and writing stay on Meta's Model API.
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
MODEL_API_KEY=... cargo run -p agora-server   # then open http://127.0.0.1:8080

# after editing web/src (the 3D office); the built files are committed
cd web && npm ci --ignore-scripts && npm run build
```

## Principles

- Our code is 100% safe Rust (`unsafe_code = "forbid"` in every crate). Dependencies are pinned exactly.
- Every performance claim comes with the command that reproduces it.

## Credits

- Characters and furniture in the 3D office: [Mini Characters](https://kenney.nl/assets/mini-characters) and [Furniture Kit](https://kenney.nl/assets/furniture-kit) by Kenney (www.kenney.nl), CC0. They are trimmed into two files by `web/models/build.mjs`; see [assets/kenney-src/README.md](assets/kenney-src/README.md). Everything else, including the scene, its animation logic and lighting, is this project's own code.

## License

MIT for the code. See [LICENSE](LICENSE). The Kenney assets are CC0 (public domain).
