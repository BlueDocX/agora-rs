//! A plan is a set of tasks and the dependencies between them.
//!
//! [`Plan::new`] rejects anything that is not a valid DAG, so the scheduler never has
//! to guess. The error says exactly what is wrong, so it can go straight back to the
//! planner model.

use std::collections::{HashMap, VecDeque};
use std::fmt;

/// A task as the planner describes it: an id and the ids it depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub id: String,
    pub deps: Vec<String>,
}

impl TaskSpec {
    pub fn new(id: impl Into<String>, deps: &[&str]) -> Self {
        Self {
            id: id.into(),
            deps: deps.iter().map(|d| (*d).to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Empty,
    DuplicateId(String),
    SelfDependency(String),
    UnknownDependency { task: String, dep: String },
    /// The ids of every task that sits on or behind a cycle.
    Cycle(Vec<String>),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the plan has no tasks"),
            Self::DuplicateId(id) => write!(f, "task id \"{id}\" is used more than once"),
            Self::SelfDependency(id) => write!(f, "task \"{id}\" depends on itself"),
            Self::UnknownDependency { task, dep } => {
                write!(f, "task \"{task}\" depends on \"{dep}\", which is not in the plan")
            }
            Self::Cycle(ids) => write!(f, "these tasks form a dependency cycle: {}", ids.join(", ")),
        }
    }
}

impl std::error::Error for PlanError {}

/// A validated dependency graph. Tasks are addressed by index (their position in the
/// input), which keeps the scheduler free of string lookups.
#[derive(Debug, Clone)]
pub struct Plan {
    ids: Vec<String>,
    deps: Vec<Vec<usize>>,
    dependents: Vec<Vec<usize>>,
    topo: Vec<usize>,
}

impl Plan {
    /// Validate a planner's output.
    ///
    /// # Errors
    /// Returns a [`PlanError`] for an empty plan, a duplicate id, a self-dependency,
    /// a dependency on an unknown id, or a cycle.
    pub fn new(tasks: &[TaskSpec]) -> Result<Self, PlanError> {
        if tasks.is_empty() {
            return Err(PlanError::Empty);
        }
        let mut index = HashMap::with_capacity(tasks.len());
        for (i, task) in tasks.iter().enumerate() {
            if index.insert(task.id.as_str(), i).is_some() {
                return Err(PlanError::DuplicateId(task.id.clone()));
            }
        }

        let mut deps = vec![Vec::new(); tasks.len()];
        let mut dependents = vec![Vec::new(); tasks.len()];
        for (i, task) in tasks.iter().enumerate() {
            for dep in &task.deps {
                if *dep == task.id {
                    return Err(PlanError::SelfDependency(task.id.clone()));
                }
                let Some(&d) = index.get(dep.as_str()) else {
                    return Err(PlanError::UnknownDependency { task: task.id.clone(), dep: dep.clone() });
                };
                // A dependency listed twice is harmless; count it once.
                if !deps[i].contains(&d) {
                    deps[i].push(d);
                    dependents[d].push(i);
                }
            }
        }

        // Kahn's algorithm. Whatever is left unvisited is on or behind a cycle.
        let mut indegree: Vec<usize> = deps.iter().map(Vec::len).collect();
        let mut queue: VecDeque<usize> = (0..tasks.len()).filter(|&i| indegree[i] == 0).collect();
        let mut topo = Vec::with_capacity(tasks.len());
        while let Some(i) = queue.pop_front() {
            topo.push(i);
            for &next in &dependents[i] {
                indegree[next] -= 1;
                if indegree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }
        if topo.len() < tasks.len() {
            let stuck = (0..tasks.len()).filter(|&i| indegree[i] > 0).map(|i| tasks[i].id.clone()).collect();
            return Err(PlanError::Cycle(stuck));
        }

        Ok(Self { ids: tasks.iter().map(|t| t.id.clone()).collect(), deps, dependents, topo })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    /// Always false: [`Plan::new`] rejects empty plans.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn id(&self, task: usize) -> &str {
        &self.ids[task]
    }

    #[must_use]
    pub fn deps(&self, task: usize) -> &[usize] {
        &self.deps[task]
    }

    #[must_use]
    pub fn dependents(&self, task: usize) -> &[usize] {
        &self.dependents[task]
    }

    #[must_use]
    /// Every task, ordered so each comes after all of its dependencies.
    pub fn topo_order(&self) -> &[usize] {
        &self.topo
    }

    #[must_use]
    /// For each task, the total weight of the heaviest chain that starts at it
    /// (its own weight included). The scheduler runs the task with the largest value
    /// first, because that chain decides when the whole plan can finish.
    ///
    /// # Panics
    /// Panics if `weights.len()` differs from the number of tasks.
    pub fn remaining_path(&self, weights: &[f64]) -> Vec<f64> {
        assert_eq!(weights.len(), self.len(), "one weight per task");
        let mut remaining = weights.to_vec();
        for &i in self.topo.iter().rev() {
            let tail = self.dependents[i].iter().map(|&d| remaining[d]).fold(0.0, f64::max);
            remaining[i] = weights[i] + tail;
        }
        remaining
    }

    #[must_use]
    /// Length of the heaviest chain in the plan: the fastest the plan can possibly
    /// finish with unlimited parallelism.
    pub fn critical_path(&self, weights: &[f64]) -> f64 {
        self.remaining_path(weights).into_iter().fold(0.0, f64::max)
    }

    #[must_use]
    /// The grouping the old TypeScript engine used (`dependencyWaves`): each wave is
    /// every task whose dependencies all sit in earlier waves. Kept only as the
    /// baseline for benchmarks.
    pub fn waves(&self) -> Vec<Vec<usize>> {
        let mut level = vec![0usize; self.len()];
        for &i in &self.topo {
            level[i] = self.deps[i].iter().map(|&d| level[d] + 1).max().unwrap_or(0);
        }
        let depth = level.iter().max().map_or(0, |m| m + 1);
        let mut waves = vec![Vec::new(); depth];
        for (i, &l) in level.iter().enumerate() {
            waves[l].push(i);
        }
        waves
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn plan(tasks: &[(&str, &[&str])]) -> Result<Plan, PlanError> {
        let specs: Vec<TaskSpec> = tasks.iter().map(|(id, deps)| TaskSpec::new(*id, deps)).collect();
        Plan::new(&specs)
    }

    #[test]
    fn rejects_bad_plans_with_a_specific_reason() {
        assert_eq!(plan(&[]).unwrap_err(), PlanError::Empty);
        assert_eq!(plan(&[("a", &[]), ("a", &[])]).unwrap_err(), PlanError::DuplicateId("a".into()));
        assert_eq!(plan(&[("a", &["a"])]).unwrap_err(), PlanError::SelfDependency("a".into()));
        assert_eq!(
            plan(&[("a", &["ghost"])]).unwrap_err(),
            PlanError::UnknownDependency { task: "a".into(), dep: "ghost".into() }
        );
        assert_eq!(
            plan(&[("a", &[]), ("b", &["c"]), ("c", &["b"])]).unwrap_err(),
            PlanError::Cycle(vec!["b".into(), "c".into()])
        );
    }

    #[test]
    fn waves_match_the_old_engine() {
        // a → c, b → d, c → e: the old engine ran {a,b}, then {c,d}, then {e}.
        let p = plan(&[("a", &[]), ("b", &[]), ("c", &["a"]), ("d", &["b"]), ("e", &["c"])]).unwrap();
        assert_eq!(p.waves(), vec![vec![0, 1], vec![2, 3], vec![4]]);
    }

    #[test]
    fn remaining_path_follows_the_heaviest_chain() {
        // a(1) → b(5) → d(1); a → c(1) → d
        let p = plan(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])]).unwrap();
        let w = [1.0, 5.0, 1.0, 1.0];
        assert_eq!(p.remaining_path(&w), vec![7.0, 6.0, 2.0, 1.0]);
        assert!((p.critical_path(&w) - 7.0).abs() < f64::EPSILON);
    }
}
