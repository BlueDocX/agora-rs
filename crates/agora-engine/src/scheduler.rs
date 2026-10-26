//! Runs a [`Plan`]'s tasks on async workers.
//!
//! [`run_ready_queue`] is the Agora scheduler: a task starts the moment its own
//! dependencies finish, and when more tasks are ready than there are worker slots,
//! the one at the head of the heaviest remaining chain goes first.
//!
//! [`run_waves`] reproduces the old engine (`dependencyWaves` + `Promise.all`): a whole
//! wave must finish before the next starts. It exists only as a benchmark baseline.
//!
//! Both share one failure rule: when a task fails, everything that depends on it,
//! directly or not, is skipped, and every other task still runs.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::future::Future;

use agora_core::Plan;
use tokio::task::JoinSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T, E> {
    Done(T),
    Failed(E),
    /// Not run, because a task it depends on failed or was skipped.
    Skipped,
}

impl<T, E> Outcome<T, E> {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done(_))
    }
}

/// A ready task, ordered by remaining critical path, ties broken by plan order.
struct Ready {
    priority: f64,
    task: usize,
}

impl PartialEq for Ready {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ready {}

impl PartialOrd for Ready {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ready {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.total_cmp(&other.priority).then_with(|| other.task.cmp(&self.task))
    }
}

/// Mark every task downstream of `failed` as skipped.
fn skip_descendants<T, E>(plan: &Plan, failed: usize, outcomes: &mut [Option<Outcome<T, E>>]) {
    let mut stack: Vec<usize> = plan.dependents(failed).to_vec();
    while let Some(task) = stack.pop() {
        if outcomes[task].is_none() {
            outcomes[task] = Some(Outcome::Skipped);
            stack.extend_from_slice(plan.dependents(task));
        }
    }
}

fn collect<T, E>(outcomes: Vec<Option<Outcome<T, E>>>) -> Vec<Outcome<T, E>> {
    outcomes.into_iter().map(|o| o.expect("every task ends done, failed or skipped")).collect()
}

/// Run every task, starting each one as soon as its dependencies are done.
///
/// `priority[i]` should be task `i`'s remaining critical path (see
/// [`Plan::remaining_path`]). `max_parallel` caps how many workers run at once.
///
/// # Panics
/// Panics if `priority` does not have one entry per task, if `max_parallel` is zero,
/// or if a worker panics.
pub async fn run_ready_queue<T, E, F, Fut>(
    plan: &Plan,
    priority: &[f64],
    max_parallel: usize,
    work: F,
) -> Vec<Outcome<T, E>>
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    assert_eq!(priority.len(), plan.len(), "one priority per task");
    assert!(max_parallel > 0, "max_parallel must be at least 1");

    let mut waiting_on: Vec<usize> = (0..plan.len()).map(|i| plan.deps(i).len()).collect();
    let mut outcomes: Vec<Option<Outcome<T, E>>> = (0..plan.len()).map(|_| None).collect();
    let mut ready: BinaryHeap<Ready> = (0..plan.len())
        .filter(|&i| waiting_on[i] == 0)
        .map(|task| Ready { priority: priority[task], task })
        .collect();
    let mut running = JoinSet::new();

    loop {
        while running.len() < max_parallel {
            let Some(Ready { task, .. }) = ready.pop() else { break };
            let job = work(task);
            running.spawn(async move { (task, job.await) });
        }
        let Some(joined) = running.join_next().await else { break };
        let (task, result) = joined.expect("worker task panicked");
        match result {
            Ok(value) => {
                outcomes[task] = Some(Outcome::Done(value));
                for &next in plan.dependents(task) {
                    waiting_on[next] -= 1;
                    if waiting_on[next] == 0 && outcomes[next].is_none() {
                        ready.push(Ready { priority: priority[next], task: next });
                    }
                }
            }
            Err(error) => {
                outcomes[task] = Some(Outcome::Failed(error));
                skip_descendants(plan, task, &mut outcomes);
            }
        }
    }
    collect(outcomes)
}

/// The old engine's schedule: run each wave to completion before starting the next.
/// Within a wave, tasks start in plan order as worker slots free up.
///
/// # Panics
/// Panics if `max_parallel` is zero or a worker panics.
pub async fn run_waves<T, E, F, Fut>(plan: &Plan, max_parallel: usize, work: F) -> Vec<Outcome<T, E>>
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    assert!(max_parallel > 0, "max_parallel must be at least 1");

    let mut outcomes: Vec<Option<Outcome<T, E>>> = (0..plan.len()).map(|_| None).collect();
    for wave in plan.waves() {
        // Tasks already skipped by an earlier failure never start.
        let runnable: Vec<usize> = wave.into_iter().filter(|&t| outcomes[t].is_none()).collect();
        let mut queue = runnable.into_iter();
        let mut running = JoinSet::new();
        loop {
            while running.len() < max_parallel {
                let Some(task) = queue.next() else { break };
                let job = work(task);
                running.spawn(async move { (task, job.await) });
            }
            let Some(joined) = running.join_next().await else { break };
            let (task, result) = joined.expect("worker task panicked");
            match result {
                Ok(value) => outcomes[task] = Some(Outcome::Done(value)),
                Err(error) => {
                    outcomes[task] = Some(Outcome::Failed(error));
                    skip_descendants(plan, task, &mut outcomes);
                }
            }
        }
    }
    collect(outcomes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use agora_core::TaskSpec;
    use proptest::prelude::*;
    use tokio::time::Instant;

    use super::*;

    /// When a task started and finished, in virtual milliseconds since the run began.
    #[derive(Debug, Default, Clone, Copy)]
    struct Span {
        start: u64,
        end: u64,
    }

    #[derive(Debug)]
    struct Run {
        outcomes: Vec<Outcome<usize, String>>,
        spans: Vec<Span>,
        peak_parallel: usize,
    }

    impl Run {
        fn makespan(&self) -> u64 {
            self.spans.iter().map(|s| s.end).max().unwrap_or(0)
        }
    }

    #[derive(Clone, Copy)]
    enum Mode {
        ReadyQueue,
        Waves,
    }

    /// Run `plan` on a paused tokio clock: task `i` takes `latency[i]` ms of virtual
    /// time and fails if `fails[i]`. Deterministic and instant.
    fn simulate(plan: &Plan, latency: &[u64], fails: &[bool], max_parallel: usize, mode: Mode) -> Run {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        rt.block_on(async {
            let origin = Instant::now();
            let spans = Arc::new(Mutex::new(vec![Span::default(); plan.len()]));
            let running = Arc::new(Mutex::new((0usize, 0usize))); // (now, peak)
            let work = |task: usize| {
                let (spans, running) = (Arc::clone(&spans), Arc::clone(&running));
                let (ms, fail) = (latency[task], fails[task]);
                async move {
                    {
                        let mut r = running.lock().unwrap();
                        r.0 += 1;
                        r.1 = r.1.max(r.0);
                    }
                    let start = u64::try_from(origin.elapsed().as_millis()).unwrap();
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    let end = u64::try_from(origin.elapsed().as_millis()).unwrap();
                    spans.lock().unwrap()[task] = Span { start, end };
                    running.lock().unwrap().0 -= 1;
                    if fail { Err(format!("task {task} failed")) } else { Ok(task) }
                }
            };
            let priority = plan.remaining_path(&vec![1.0; plan.len()]);
            let outcomes = match mode {
                Mode::ReadyQueue => run_ready_queue(plan, &priority, max_parallel, work).await,
                Mode::Waves => run_waves(plan, max_parallel, work).await,
            };
            let spans = spans.lock().unwrap().clone();
            let peak_parallel = running.lock().unwrap().1;
            Run { outcomes, spans, peak_parallel }
        })
    }

    /// Random DAGs shaped like LLM plans: 1-14 tasks, each depending on up to three
    /// earlier ones, with per-task latencies of 1-60 s.
    fn arb_case() -> impl Strategy<Value = (Plan, Vec<u64>)> {
        (1usize..15)
            .prop_flat_map(|n| {
                let deps = (0..n).map(|i| proptest::collection::vec(0..i.max(1), 0..=3.min(i))).collect::<Vec<_>>();
                (deps, proptest::collection::vec(1_000u64..60_000, n))
            })
            .prop_map(|(deps, latency)| {
                let specs: Vec<TaskSpec> = deps
                    .iter()
                    .enumerate()
                    .map(|(i, ds)| TaskSpec {
                        id: i.to_string(),
                        deps: ds.iter().filter(|&&d| d < i).map(ToString::to_string).collect(),
                    })
                    .collect();
                (Plan::new(&specs).unwrap(), latency)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn ready_queue_respects_deps_and_the_parallel_cap((plan, latency) in arb_case(), max_parallel in 1usize..6) {
            let run = simulate(&plan, &latency, &vec![false; plan.len()], max_parallel, Mode::ReadyQueue);
            prop_assert!(run.outcomes.iter().all(Outcome::is_done));
            for task in 0..plan.len() {
                for &dep in plan.deps(task) {
                    prop_assert!(run.spans[task].start >= run.spans[dep].end, "{task} started before {dep} finished");
                }
            }
            prop_assert!(run.peak_parallel <= max_parallel);
        }

        #[test]
        fn unlimited_ready_queue_finishes_on_the_critical_path((plan, latency) in arb_case()) {
            let run = simulate(&plan, &latency, &vec![false; plan.len()], usize::MAX, Mode::ReadyQueue);
            #[allow(clippy::cast_precision_loss)]
            let weights: Vec<f64> = latency.iter().map(|&ms| ms as f64).collect();
            #[allow(clippy::cast_precision_loss)]
            let makespan = run.makespan() as f64;
            prop_assert!((makespan - plan.critical_path(&weights)).abs() < 1.0);
        }

        #[test]
        fn unlimited_ready_queue_is_never_slower_than_waves((plan, latency) in arb_case()) {
            let fails = vec![false; plan.len()];
            let rq = simulate(&plan, &latency, &fails, usize::MAX, Mode::ReadyQueue);
            let waves = simulate(&plan, &latency, &fails, usize::MAX, Mode::Waves);
            prop_assert!(rq.makespan() <= waves.makespan());
        }

        #[test]
        fn a_failure_skips_exactly_its_descendants(
            (plan, latency) in arb_case(),
            fail_at in any::<proptest::sample::Index>(),
        ) {
            let failed = fail_at.index(plan.len());
            let mut fails = vec![false; plan.len()];
            fails[failed] = true;

            let mut downstream = vec![false; plan.len()];
            let mut stack = plan.dependents(failed).to_vec();
            while let Some(t) = stack.pop() {
                if !downstream[t] {
                    downstream[t] = true;
                    stack.extend_from_slice(plan.dependents(t));
                }
            }

            for mode in [Mode::ReadyQueue, Mode::Waves] {
                let out = simulate(&plan, &latency, &fails, 3, mode).outcomes;
                for task in 0..plan.len() {
                    let ok = if task == failed {
                        matches!(out[task], Outcome::Failed(_))
                    } else if downstream[task] {
                        out[task] == Outcome::Skipped
                    } else {
                        out[task].is_done()
                    };
                    prop_assert!(ok, "task {}: {:?}", task, out[task]);
                }
            }
        }
    }

    #[test]
    fn ready_queue_beats_waves_when_a_wave_has_a_straggler() {
        // a(30s) and b(1s) are wave 1; c depends only on b. The old engine makes c
        // wait 30 s for a; the ready-queue starts c after 1 s.
        let plan = Plan::new(&[TaskSpec::new("a", &[]), TaskSpec::new("b", &[]), TaskSpec::new("c", &["b"])]).unwrap();
        let (latency, fails) = ([30_000, 1_000, 20_000], [false; 3]);
        assert_eq!(simulate(&plan, &latency, &fails, 4, Mode::ReadyQueue).spans[2].start, 1_000);
        assert_eq!(simulate(&plan, &latency, &fails, 4, Mode::Waves).spans[2].start, 30_000);
    }
}
