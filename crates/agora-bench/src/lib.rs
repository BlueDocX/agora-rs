//! Shared pieces for Agora benchmarks: a seeded RNG, plan generators, and a simulator
//! that runs a plan on tokio's paused clock so results are exact and instant.

use std::convert::Infallible;
use std::time::Duration;

use agora_core::{Plan, TaskSpec};
use agora_engine::{run_ready_queue, run_waves};
use tokio::time::Instant;

/// `SplitMix64`: small, fast, and the same sequence on every machine.
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        const TWO_POW_53: f64 = 9_007_199_254_740_992.0;
        #[allow(clippy::cast_precision_loss)] // 53 bits fit exactly in an f64 mantissa
        let x = (self.next_u64() >> 11) as f64;
        x / TWO_POW_53
    }

    /// Uniform in `lo..=hi`.
    ///
    /// # Panics
    /// Panics if `hi < lo`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        let span = u64::try_from(hi - lo + 1).expect("small range");
        lo + usize::try_from(self.next_u64() % span).expect("fits in usize")
    }

    /// Log-normal with the given median and log-space spread.
    pub fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
        let (u1, u2) = (self.unit().max(f64::MIN_POSITIVE), self.unit());
        let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
        median * (sigma * z).exp()
    }
}

/// A plan shaped like the ones an LLM planner writes: 3-12 tasks, where each task
/// depends on each earlier task with probability `edge_p`, at most 3 dependencies.
///
/// # Panics
/// Never in practice: dependencies only point backwards, so the plan is always a DAG.
pub fn random_plan(rng: &mut Rng, edge_p: f64) -> Plan {
    let n = rng.range(3, 12);
    let specs: Vec<TaskSpec> = (0..n)
        .map(|i| {
            let mut deps = Vec::new();
            for j in 0..i {
                if deps.len() < 3 && rng.unit() < edge_p {
                    deps.push(j.to_string());
                }
            }
            TaskSpec { id: i.to_string(), deps }
        })
        .collect();
    Plan::new(&specs).expect("backward-only edges always form a DAG")
}

/// Per-task model-call latency in ms: log-normal around `median_ms`, clamped to
/// 0.5 s - 3 min.
#[must_use]
pub fn latencies(rng: &mut Rng, n: usize, median_ms: f64, sigma: f64) -> Vec<u64> {
    (0..n)
        .map(|_| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = rng.log_normal(median_ms, sigma).clamp(500.0, 180_000.0) as u64;
            ms
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheduler {
    /// The old engine's `dependencyWaves` + `Promise.all`.
    Waves,
    /// Agora's ready-queue, prioritised by remaining chain length in tasks (the
    /// scheduler does not know real latencies in advance).
    ReadyQueue,
}

/// Run `plan` with each task sleeping `latency[i]` ms of virtual time, and return
/// the makespan in ms.
///
/// # Panics
/// Panics if the tokio runtime cannot be built.
#[must_use]
pub fn makespan_ms(plan: &Plan, latency: &[u64], max_parallel: usize, scheduler: Scheduler) -> u64 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        let origin = Instant::now();
        let work = |task: usize| {
            let ms = latency[task];
            async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok::<(), Infallible>(())
            }
        };
        match scheduler {
            Scheduler::Waves => {
                run_waves(plan, max_parallel, work).await;
            }
            Scheduler::ReadyQueue => {
                let priority = plan.remaining_path(&vec![1.0; plan.len()]);
                run_ready_queue(plan, &priority, max_parallel, work).await;
            }
        }
        u64::try_from(origin.elapsed().as_millis()).expect("fits in u64")
    })
}

/// The value at quantile `q` (0..=1) of an already-sorted slice.
#[must_use]
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}
