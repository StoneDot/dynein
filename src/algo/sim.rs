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

//! Deterministic in-process simulation of the executor candidates
//! (`docs/design/benchmark-plan.md` §2.5).
//!
//! Everything here runs under tokio virtual time
//! (`#[tokio::test(start_paused = true)]`): the algo layer uses
//! `tokio::time::Instant` throughout, so bucket refills and pacing run in
//! milliseconds of real time regardless of the simulated duration.
//!
//! The simulated "server" only adds latency and always consumes exactly the
//! estimate (phase 1: infinite capacity, never throttled). This deliberately
//! isolates the client-side scheduling questions; see the design document.

use crate::algo::worker::{ProcessResult, ResourceConstraintProcess};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// One completed simulated request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompletionRecord {
    pub finished_at: Instant,
    pub cost: f64,
}

/// Collects completion records from all [`SimProcess`]es of one scenario run.
#[derive(Debug, Clone, Default)]
pub struct SimRecorder(Arc<Mutex<Vec<CompletionRecord>>>);

impl SimRecorder {
    pub fn new() -> SimRecorder {
        SimRecorder::default()
    }

    /// Creates a simulated request with the given capacity cost and
    /// service latency, reporting its completion into this recorder.
    pub fn process(&self, cost: f64, latency: Duration) -> SimProcess {
        SimProcess {
            cost,
            latency,
            recorder: self.clone(),
        }
    }

    pub fn records(&self) -> Vec<CompletionRecord> {
        self.0.lock().unwrap().clone()
    }
}

/// A [`ResourceConstraintProcess`] whose "server" is pure latency: it always
/// consumes exactly its estimate and never reports throttling.
#[derive(Debug, Clone)]
pub struct SimProcess {
    cost: f64,
    latency: Duration,
    recorder: SimRecorder,
}

impl ResourceConstraintProcess for SimProcess {
    fn estimate_resource(&self) -> f64 {
        self.cost
    }

    fn process_and_consume_resource(&self) -> impl Future<Output = ProcessResult> + Send {
        let cost = self.cost;
        let latency = self.latency;
        let recorder = self.recorder.clone();
        async move {
            tokio::time::sleep(latency).await;
            recorder.0.lock().unwrap().push(CompletionRecord {
                finished_at: Instant::now(),
                cost,
            });
            ProcessResult {
                consumed: cost,
                throttled: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_sim_process_consumes_estimate_after_latency() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let process = recorder.process(2.0, Duration::from_millis(5));

        assert_eq!(process.estimate_resource(), 2.0);
        let result = process.process_and_consume_resource().await;

        assert_eq!(result.consumed, 2.0);
        assert!(!result.throttled);
        let records = recorder.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost, 2.0);
        assert_eq!(records[0].finished_at, start + Duration::from_millis(5));
    }
}
