//! B1: the old wave scheduler vs Agora's ready-queue on the same plans and latencies.
//!
//! Usage: `cargo run -p agora-bench --release --bin sched-sim -- [plans] [seed] [sigma] [edge_p]`
//! `sigma` is the spread of model-call latency (log-normal); `edge_p` is how densely
//! tasks depend on each other.
//! Writes one CSV row per plan and setting to `bench-out/b1_sched.csv`.

use std::fmt::Write as _;
use std::fs;

use agora_bench::{Rng, Scheduler, latencies, makespan_ms, quantile, random_plan};

const MEDIAN_MS: f64 = 8_000.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let plans: usize = args.next().map_or(1_000, |a| a.parse().expect("plans must be a number"));
    let seed: u64 = args.next().map_or(42, |a| a.parse().expect("seed must be a number"));
    let sigma: f64 = args.next().map_or(0.6, |a| a.parse().expect("sigma must be a number"));
    let edge_p: f64 = args.next().map_or(0.3, |a| a.parse().expect("edge_p must be a number"));

    let mut rng = Rng::new(seed);
    let cases: Vec<_> = (0..plans)
        .map(|_| {
            let plan = random_plan(&mut rng, edge_p);
            let latency = latencies(&mut rng, plan.len(), MEDIAN_MS, sigma);
            (plan, latency)
        })
        .collect();

    println!("B1 scheduler simulation: {plans} plans, seed {seed}, latency median {MEDIAN_MS} ms, sigma {sigma}, edge_p {edge_p}");
    println!("Saving = (waves - ready_queue) / waves. Positive means the ready-queue finished sooner.\n");
    println!("{:>12}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}", "max_parallel", "median", "p90", "mean", "faster", "slower", "worst");

    let mut csv = String::from("plan,tasks,max_parallel,waves_ms,ready_queue_ms\n");
    for (label, max_parallel) in [("unlimited", usize::MAX), ("4", 4), ("2", 2)] {
        let mut savings = Vec::with_capacity(plans);
        let (mut faster, mut slower) = (0, 0);
        for (i, (plan, latency)) in cases.iter().enumerate() {
            let waves = makespan_ms(plan, latency, max_parallel, Scheduler::Waves);
            let rq = makespan_ms(plan, latency, max_parallel, Scheduler::ReadyQueue);
            writeln!(csv, "{i},{},{label},{waves},{rq}", plan.len()).expect("write to String");
            if rq < waves {
                faster += 1;
            } else if rq > waves {
                slower += 1;
            }
            #[allow(clippy::cast_precision_loss)]
            savings.push((waves as f64 - rq as f64) / waves as f64);
        }
        savings.sort_by(f64::total_cmp);
        #[allow(clippy::cast_precision_loss)]
        let mean = savings.iter().sum::<f64>() / savings.len() as f64;
        let pct = |x: f64| format!("{:.1}%", x * 100.0);
        println!(
            "{label:>12}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}",
            pct(quantile(&savings, 0.5)),
            pct(quantile(&savings, 0.9)),
            pct(mean),
            format!("{faster}/{plans}"),
            format!("{slower}/{plans}"),
            pct(savings[0]),
        );
    }

    fs::create_dir_all("bench-out").expect("create bench-out/");
    fs::write("bench-out/b1_sched.csv", csv).expect("write bench-out/b1_sched.csv");
    println!("\nPer-plan rows: bench-out/b1_sched.csv");
}
