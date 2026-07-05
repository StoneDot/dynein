/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License").
 * You may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Runtime selection of the executor architecture under benchmark.
//!
//! The candidates are described in `docs/design/benchmark-plan.md` §2:
//! - `pool<N>`: the current fixed worker pool with per-worker queues of
//!   depth N (`pool16` is the baseline A, `pool1` is candidate A′)
//! - `mpmc`: candidate B, a shared MPMC process queue with fixed workers
//! - `task`: candidate C, one tokio task per request with a shared bucket
//!
//! The selection is intentionally an environment variable rather than a CLI
//! option: it exists only for the benchmark phase and disappears once the
//! architecture question is settled.

use crate::algo::congestion::{BoostSlot, CongestionStats, TargetGauge};
use crate::algo::mpmc::MpmcExecutor;
use crate::algo::task_executor::TaskExecutor;
use crate::algo::worker::{ResourceConstraintProcess, ThrottledExecutor};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// Name of the environment variable selecting the executor candidate.
pub const BENCH_EXECUTOR_ENV: &str = "DYNEIN_BENCH_EXECUTOR";

/// Unified error type across executor candidates. The concrete failure modes
/// differ per architecture (e.g. worker channel send failures for the pool),
/// so they are carried as a message.
#[derive(Debug, thiserror::Error)]
#[error("executor error: {0}")]
pub struct ExecutorError(pub String);

/// Executor architecture candidates selectable at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorKind {
    /// Fixed worker pool, per-worker queue of the given depth (A / A′).
    Pool { queue_depth: usize },
    /// Shared MPMC process queue, fixed workers pulling from it (B).
    Mpmc,
    /// One tokio task per request, shared bucket (C). `max_in_flight`
    /// overrides the in-flight request cap (`None` = the candidate default).
    Task { max_in_flight: Option<usize> },
}

impl ExecutorKind {
    /// The production default: the current architecture (candidate A).
    pub const fn default_kind() -> ExecutorKind {
        ExecutorKind::Pool { queue_depth: 16 }
    }

    /// Parses a candidate name: `pool<N>` / `mpmc` / `task[<N>]`.
    pub fn parse(s: &str) -> Option<ExecutorKind> {
        fn positive(s: &str) -> Option<usize> {
            let n: usize = s.parse().ok()?;
            if n == 0 {
                None
            } else {
                Some(n)
            }
        }
        match s {
            "mpmc" => Some(ExecutorKind::Mpmc),
            "task" => Some(ExecutorKind::Task {
                max_in_flight: None,
            }),
            _ => {
                if let Some(rest) = s.strip_prefix("task") {
                    return Some(ExecutorKind::Task {
                        max_in_flight: Some(positive(rest)?),
                    });
                }
                Some(ExecutorKind::Pool {
                    queue_depth: positive(s.strip_prefix("pool")?)?,
                })
            }
        }
    }

    /// Reads the selection from `DYNEIN_BENCH_EXECUTOR`. Returns the default
    /// when the variable is unset; panics on an invalid value so a typo in a
    /// benchmark cell fails loudly instead of silently measuring the default.
    pub fn from_env() -> ExecutorKind {
        match std::env::var(BENCH_EXECUTOR_ENV) {
            Err(_) => ExecutorKind::default_kind(),
            Ok(v) => ExecutorKind::parse(&v).unwrap_or_else(|| {
                panic!(
                    "Invalid {} value '{}': expected pool<N>, mpmc or task",
                    BENCH_EXECUTOR_ENV, v
                )
            }),
        }
    }
}

/// An executor of any candidate architecture, exposing the interface the
/// transfer pipeline needs. This is deliberately an enum rather than a trait
/// object: the pipeline calls a handful of methods once, and the candidates
/// share no other behavior worth abstracting during the benchmark phase.
pub enum AnyExecutor<T: ResourceConstraintProcess + Clone> {
    Pool(ThrottledExecutor<T>),
    Mpmc(MpmcExecutor<T>),
    Task(TaskExecutor<T>),
}

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> AnyExecutor<T> {
    pub fn new(
        kind: ExecutorKind,
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
    ) -> AnyExecutor<T> {
        match kind {
            ExecutorKind::Pool { queue_depth } => {
                AnyExecutor::Pool(ThrottledExecutor::with_queue_depth(
                    recv,
                    target_limit,
                    initial_target,
                    queue_depth,
                ))
            }
            ExecutorKind::Mpmc => {
                AnyExecutor::Mpmc(MpmcExecutor::new(recv, target_limit, initial_target))
            }
            ExecutorKind::Task { max_in_flight } => AnyExecutor::Task(match max_in_flight {
                None => TaskExecutor::new(recv, target_limit, initial_target),
                Some(cap) => {
                    TaskExecutor::with_max_concurrency(recv, target_limit, initial_target, cap)
                }
            }),
        }
    }

    pub async fn run(&mut self) -> Result<(), ExecutorError> {
        match self {
            AnyExecutor::Pool(e) => e
                .run()
                .await
                .map_err(|errs| ExecutorError(format!("Worker send failures: {:?}", errs))),
            AnyExecutor::Mpmc(e) => e.run().await,
            AnyExecutor::Task(e) => e.run().await,
        }
    }

    pub fn congestion_stats(&self) -> Arc<CongestionStats> {
        match self {
            AnyExecutor::Pool(e) => e.congestion_stats(),
            AnyExecutor::Mpmc(e) => e.congestion_stats(),
            AnyExecutor::Task(e) => e.congestion_stats(),
        }
    }

    pub fn boost_slot(&self) -> Arc<BoostSlot> {
        match self {
            AnyExecutor::Pool(e) => e.boost_slot(),
            AnyExecutor::Mpmc(e) => e.boost_slot(),
            AnyExecutor::Task(e) => e.boost_slot(),
        }
    }

    pub fn target_gauge(&self) -> Arc<TargetGauge> {
        match self {
            AnyExecutor::Pool(e) => e.target_gauge(),
            AnyExecutor::Mpmc(e) => e.target_gauge(),
            AnyExecutor::Task(e) => e.target_gauge(),
        }
    }

    /// Monitor instrumenting the candidate's request-executing tasks
    /// (workers for the pool architectures, request tasks for `task`).
    pub fn task_monitor(&self) -> tokio_metrics::TaskMonitor {
        match self {
            AnyExecutor::Pool(e) => e.task_monitor(),
            AnyExecutor::Mpmc(e) => e.task_monitor(),
            AnyExecutor::Task(e) => e.task_monitor(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::sim::SimRecorder;
    use std::time::Duration;
    use tokio::sync::mpsc::channel;

    #[tokio::test(start_paused = true)]
    async fn test_any_executor_runs_every_candidate() {
        for kind in [
            ExecutorKind::Pool { queue_depth: 16 },
            ExecutorKind::Pool { queue_depth: 1 },
            ExecutorKind::Mpmc,
            ExecutorKind::Task {
                max_in_flight: None,
            },
        ] {
            let recorder = SimRecorder::new();
            let (tx, rx) = channel(16);
            let mut executor = AnyExecutor::new(kind, rx, 1000.0, Some(1000.0));
            for _ in 0..5 {
                tx.send(recorder.process(1.0, Duration::from_millis(10)))
                    .await
                    .unwrap();
            }
            drop(tx);

            executor.run().await.unwrap();

            assert_eq!(recorder.records().len(), 5, "candidate {:?}", kind);
            assert_eq!(executor.congestion_stats().snapshot().0, 5);
            assert_eq!(executor.target_gauge().get(), 1000.0);
            // Every candidate instruments its request-executing tasks
            // (workers for the pools, spawned request tasks for `task`).
            assert!(
                executor.task_monitor().cumulative().instrumented_count > 0,
                "candidate {:?} did not instrument its tasks",
                kind
            );
        }
    }

    #[test]
    fn test_parse_pool_variants() {
        assert_eq!(
            ExecutorKind::parse("pool16"),
            Some(ExecutorKind::Pool { queue_depth: 16 })
        );
        assert_eq!(
            ExecutorKind::parse("pool1"),
            Some(ExecutorKind::Pool { queue_depth: 1 })
        );
        assert_eq!(
            ExecutorKind::parse("pool4"),
            Some(ExecutorKind::Pool { queue_depth: 4 })
        );
    }

    #[test]
    fn test_parse_mpmc_and_task() {
        assert_eq!(ExecutorKind::parse("mpmc"), Some(ExecutorKind::Mpmc));
        assert_eq!(
            ExecutorKind::parse("task"),
            Some(ExecutorKind::Task {
                max_in_flight: None
            })
        );
        // task<N> overrides the in-flight request cap (a benchmark axis).
        assert_eq!(
            ExecutorKind::parse("task64"),
            Some(ExecutorKind::Task {
                max_in_flight: Some(64)
            })
        );
    }

    #[test]
    fn test_parse_rejects_invalid_values() {
        assert_eq!(ExecutorKind::parse(""), None);
        assert_eq!(ExecutorKind::parse("bogus"), None);
        assert_eq!(ExecutorKind::parse("pool"), None);
        assert_eq!(ExecutorKind::parse("pool0"), None);
        assert_eq!(ExecutorKind::parse("pool-1"), None);
        assert_eq!(ExecutorKind::parse("POOL16"), None);
        assert_eq!(ExecutorKind::parse("task0"), None);
        assert_eq!(ExecutorKind::parse("task-1"), None);
    }

    #[test]
    fn test_default_is_current_architecture() {
        assert_eq!(
            ExecutorKind::default_kind(),
            ExecutorKind::Pool { queue_depth: 16 }
        );
    }
}
