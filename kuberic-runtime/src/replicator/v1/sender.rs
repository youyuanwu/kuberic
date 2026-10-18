// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of ReliableOperationSender.h/.cpp,
// DecayAverage.h/.cpp and REPerformanceCounters.h's tick mean helper.
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Unselected reliable scheduling core, not a carrier or retention queue.
//! Owners serialize transitions, then dispatch owned batches outside guards.
//! Time is an injected, nondecreasing observation; deadlines are one-shot timer
//! instructions, not sleeping tasks. Timer entry can race disabling/close.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::application::Lsn;
use crate::protocol::types::Epoch;

use super::operation_queue::QueueOperation;

#[derive(Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 admission seam"))]
pub(super) struct SenderOperation {
    pub(super) operation: Arc<QueueOperation>,
    /// Supplied by the owner, never inferred from the current sending epoch.
    pub(super) original_epoch: Epoch,
}

#[derive(Clone, Copy)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 sender settings"))]
pub(super) struct SenderConfig {
    pub(super) initial_window: usize,
    pub(super) max_window: usize,
    pub(super) retry: Duration,
    pub(super) decay_factor: f64,
    pub(super) decay_interval: Duration,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 selected work"))]
pub(super) enum SendAction {
    Operation(SenderOperation),
    RequestAck,
}

/// Every selected operation is retained and stamped before the first callback.
/// Closing a core cannot withdraw this batch. Different batches need not be
/// globally serialized. A false callback means send queue full, not failed ACK.
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 dispatch seam"))]
pub(super) struct SendBatch {
    actions: Vec<SendAction>,
    completed_hint: Lsn,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 dispatch seam"))]
impl SendBatch {
    fn empty(completed_hint: Lsn) -> Self {
        Self {
            actions: Vec::new(),
            completed_hint,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub(super) fn dispatch(self, mut send: impl FnMut(SendAction, Lsn) -> bool) {
        for action in self.actions {
            if !send(action, self.completed_hint) {
                break;
            }
        }
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK result"))]
pub(super) struct AckResult {
    pub(super) advanced: bool,
    pub(super) sends: SendBatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 progress view"))]
pub(super) struct SenderProgress {
    pub(super) received: Lsn,
    pub(super) applied: Lsn,
    pub(super) last_ack_at: Option<Duration>,
    pub(super) not_received: Lsn,
    pub(super) received_not_applied: Lsn,
}

impl SenderProgress {
    pub(super) fn idle_progress(self, finish_copy_target: Option<Lsn>) -> Lsn {
        finish_copy_target
            .map_or(self.received, |target| self.received.min(target))
            .max(self.applied)
            .max(0)
    }
}

struct Pending {
    operation: SenderOperation,
    selected_at: Option<Duration>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected native V1 reliable sender")
)]
pub(super) struct ReliableOperationSender {
    config: SenderConfig,
    active: bool,
    pending: BTreeMap<Lsn, Pending>,
    received: Lsn,
    applied: Lsn,
    highest: Lsn,
    completed_hint: Lsn,
    last_ack_at: Option<Duration>,
    window: usize,
    max_window: usize,
    deadline: Option<Duration>,
    no_progress: bool,
    latencies: Latencies,
    average_collected_at: Duration,
    receive_average: DecayAverage,
    apply_average: DecayAverage,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 sender API"))]
impl ReliableOperationSender {
    pub(super) fn new(config: SenderConfig, last_ack: Lsn, now: Duration) -> Self {
        assert!(config.initial_window > 0 && !config.retry.is_zero());
        let max_window = if config.max_window != 0 {
            config.max_window
        } else if config.initial_window > 1024 {
            config.initial_window.checked_mul(4).unwrap()
        } else {
            1024
        };
        Self {
            config,
            active: false,
            pending: BTreeMap::new(),
            received: last_ack,
            applied: last_ack,
            highest: last_ack,
            completed_hint: 0,
            last_ack_at: None,
            window: config.initial_window,
            max_window,
            deadline: None,
            no_progress: false,
            latencies: Latencies::default(),
            average_collected_at: now,
            receive_average: DecayAverage::new(config.decay_factor, config.decay_interval),
            apply_average: DecayAverage::new(config.decay_factor, config.decay_interval),
        }
    }

    pub(super) fn open(&mut self) {
        assert!(!self.active);
        self.active = true;
    }

    pub(super) fn close(&mut self) {
        if self.active {
            self.active = false;
            self.deadline = None;
            self.pending.clear();
            // Native Clear does not reset the latency list's last apply cursor.
            self.latencies.items.clear();
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    pub(super) fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    pub(super) fn window(&self) -> usize {
        self.window
    }

    pub(super) fn progress(&mut self, completed_hint: Lsn) -> SenderProgress {
        self.update_hint(completed_hint);
        SenderProgress {
            received: self.received,
            applied: self.applied,
            last_ack_at: self.last_ack_at,
            not_received: (self.highest - self.received).max(0),
            received_not_applied: (self.received - self.applied).max(0),
        }
    }

    pub(super) fn averages(&self) -> (Duration, Duration) {
        (self.receive_average.value(), self.apply_average.value())
    }

    pub(super) fn pending_operations(&self) -> impl Iterator<Item = &SenderOperation> {
        self.pending.values().map(|pending| &pending.operation)
    }

    pub(super) fn set_decay_settings(&mut self, factor: f64, interval: Duration) {
        assert!((0.0..=1.0).contains(&factor) && !interval.is_zero());
        self.config.decay_factor = factor;
        self.config.decay_interval = interval;
    }

    pub(super) fn reset_average_statistics(&mut self, now: Duration) {
        self.receive_average.reset(now);
        self.apply_average.reset(now);
        self.average_collected_at = now;
    }

    fn update_hint(&mut self, hint: Lsn) {
        if hint != 0 {
            self.completed_hint = hint;
        }
    }

    fn insert(&mut self, operation: SenderOperation, now: Duration, batch: &mut SendBatch) {
        let lsn = operation.operation.lsn();
        if self.pending.contains_key(&lsn) {
            return;
        }
        let immediate = self.pending.len() < self.window;
        self.highest = self.highest.max(lsn);
        self.latencies.add(lsn, now);
        if immediate {
            batch.actions.push(SendAction::Operation(operation.clone()));
        }
        self.pending.insert(
            lsn,
            Pending {
                operation,
                selected_at: immediate.then_some(now),
            },
        );
    }

    pub(super) fn add(
        &mut self,
        operation: SenderOperation,
        completed_hint: Lsn,
        now: Duration,
    ) -> SendBatch {
        let mut batch = SendBatch::empty(completed_hint);
        if !self.active {
            return batch;
        }
        self.update_hint(completed_hint);
        let lsn = operation.operation.lsn();
        if self.pending.contains_key(&lsn) {
            return batch;
        }
        assert!(lsn > self.received);
        self.insert(operation, now, &mut batch);
        self.deadline.get_or_insert(now + self.config.retry);
        batch
    }

    /// Caller supplies the native sorted, contiguous queue range. Overlap with
    /// single-operation multicast is expected; duplicate identity/time survives.
    pub(super) fn add_replay(
        &mut self,
        operations: Vec<SenderOperation>,
        completed_hint: Lsn,
        now: Duration,
    ) -> SendBatch {
        let mut batch = SendBatch::empty(completed_hint);
        if !self.active || operations.is_empty() {
            return batch;
        }
        assert!(operations.windows(2).all(|pair| {
            pair[0].operation.lsn().checked_add(1) == Some(pair[1].operation.lsn())
        }));
        self.update_hint(completed_hint);
        let previous_count = self.pending.len();
        for operation in operations {
            self.insert(operation, now, &mut batch);
        }
        if let Some(SendAction::Operation(first)) = batch.actions.first() {
            assert!(first.operation.lsn() > self.received);
        }
        if self.pending.len() != previous_count {
            self.deadline.get_or_insert(now + self.config.retry);
        }
        batch
    }

    pub(super) fn on_ack(&mut self, received: Lsn, applied: Lsn, now: Duration) -> AckResult {
        assert!(received >= applied);
        if !self.active {
            return AckResult {
                advanced: false,
                sends: SendBatch::empty(self.completed_hint),
            };
        }
        self.last_ack_at = Some(now);
        let advanced = received > self.received || applied > self.applied;
        if received > self.received {
            self.received = received;
            self.pending.retain(|lsn, _| *lsn > received);
        }
        self.applied = self.applied.max(applied);
        if advanced {
            self.no_progress = false;
            if self.window < self.max_window {
                // Native tests the old window, then doubles WITHOUT clamping.
                self.window = self.window.checked_mul(2).unwrap();
            }
            self.latencies.on_ack(self.received, self.applied, now);
        }
        AckResult {
            advanced,
            // Suppress ACK -> RequestAck -> duplicate ACK feedback loops.
            sends: self.select(now, false),
        }
    }

    /// Entry of an already-dispatched timer callback, not a deadline poll.
    pub(super) fn on_timer(&mut self, now: Duration) -> SendBatch {
        if !self.active {
            return SendBatch::empty(self.completed_hint);
        }
        if self.no_progress && self.window > 1 {
            self.window /= 2;
        }
        self.no_progress = true;
        self.select(now, true)
    }

    fn select(&mut self, now: Duration, allow_request_ack: bool) -> SendBatch {
        for average in [&mut self.receive_average, &mut self.apply_average] {
            average.settings(self.config.decay_factor, self.config.decay_interval);
        }
        let observation = if now - self.average_collected_at > self.config.retry {
            self.average_collected_at = now;
            Some(self.latencies.compute(now))
        } else {
            None
        };
        let mut batch = SendBatch::empty(self.completed_hint);
        if self.pending.is_empty() && self.applied >= self.received {
            self.deadline = None;
            return batch;
        }
        if let Some((receive_ticks, apply_ticks)) = observation {
            if !self.pending.is_empty() {
                self.receive_average.update(receive_ticks, now);
            }
            self.apply_average.update(apply_ticks, now);
        }
        if self.pending.is_empty() {
            if allow_request_ack {
                batch.actions.push(SendAction::RequestAck);
            }
        } else {
            for pending in self.pending.values_mut() {
                if pending
                    .selected_at
                    .is_none_or(|at| at + self.config.retry <= now)
                {
                    pending.selected_at = Some(now);
                    batch
                        .actions
                        .push(SendAction::Operation(pending.operation.clone()));
                    if batch.actions.len() == self.window {
                        break;
                    }
                }
            }
        }
        self.deadline = Some(now + self.config.retry);
        batch
    }
}

struct Watch {
    started: Duration,
    stopped: Option<Duration>,
}

impl Watch {
    fn ticks(&self, now: Duration) -> i128 {
        ((self.stopped.unwrap_or(now) - self.started).as_nanos() / 100) as i128
    }
}

struct Latency {
    received: Watch,
    applied: Watch,
}

#[derive(Default)]
struct Latencies {
    items: BTreeMap<Lsn, Latency>,
    last_apply: Lsn,
}

impl Latencies {
    fn add(&mut self, lsn: Lsn, now: Duration) {
        self.items.entry(lsn).or_insert_with(|| Latency {
            received: Watch {
                started: now,
                stopped: None,
            },
            applied: Watch {
                started: now,
                stopped: None,
            },
        });
    }

    fn on_ack(&mut self, received: Lsn, applied: Lsn, now: Duration) {
        let Some(first) = self.items.keys().next().copied() else {
            return;
        };
        // Preserve native positional traversal in its dense valid-input domain.
        // Safe skip avoids porting upstream iterator UB outside that domain;
        // arbitrary sparse latency-cursor equivalence is NOT promised.
        let skip = (self.last_apply - first + 1).max(0) as usize;
        for (&lsn, item) in self.items.iter_mut().skip(skip) {
            if lsn > received {
                break;
            }
            item.received.stopped.get_or_insert(now);
            if lsn <= applied {
                item.applied.stopped.get_or_insert(now);
            }
        }
        self.last_apply = applied;
    }

    fn compute(&mut self, now: Duration) -> (i128, i128) {
        let (mut receive, mut apply) = (0, 0);
        for (index, item) in self.items.values().enumerate() {
            let count = (index + 1) as i128;
            // Signed integer division toward zero EACH iteration, in 100ns ticks.
            receive += (item.received.ticks(now) - receive) / count;
            apply += (item.applied.ticks(now) - apply) / count;
        }
        self.items
            .retain(|_, item| item.received.stopped.is_none() || item.applied.stopped.is_none());
        (receive, apply)
    }
}

struct DecayAverage {
    factor: f64,
    interval: Duration,
    last_updated: Duration,
    last_ms: u32,
    sum: f64,
    weight: f64,
}

impl DecayAverage {
    fn new(factor: f64, interval: Duration) -> Self {
        let mut average = Self {
            factor,
            interval,
            last_updated: Duration::ZERO,
            last_ms: 0,
            sum: 0.0,
            weight: 0.0,
        };
        average.settings(factor, interval);
        average
    }

    fn settings(&mut self, factor: f64, interval: Duration) {
        assert!((0.0..=1.0).contains(&factor) && !interval.is_zero());
        self.factor = factor;
        self.interval = interval;
    }

    fn update(&mut self, ticks: i128, now: Duration) {
        assert!(ticks >= 0);
        let milliseconds = (ticks / 10_000).min(u32::MAX as i128) as u32;
        let value = f64::from(milliseconds);
        if self.factor == 0.0 {
            self.sum = value;
            self.weight = 1.0;
        } else {
            let coefficient = self
                .factor
                .powf((now - self.last_updated).as_secs_f64() / self.interval.as_secs_f64());
            if coefficient > 0.001 && f64::MAX - self.sum > value {
                self.sum = self.sum * coefficient + value;
                self.weight = self.weight * coefficient + 1.0;
            } else {
                self.sum = value;
                self.weight = 1.0;
            }
        }
        self.last_ms = milliseconds;
        self.last_updated = now;
    }

    fn value(&self) -> Duration {
        let ms = if self.factor == 0.0 {
            f64::from(self.last_ms)
        } else if self.sum == 0.0 {
            0.0
        } else {
            self.sum / self.weight
        };
        Duration::from_nanos((ms * 10_000.0) as u64 * 100)
    }

    fn reset(&mut self, now: Duration) {
        self.last_updated = now;
        self.last_ms = 0;
        self.sum = 0.0;
        self.weight = 0.0;
    }
}

#[cfg(test)]
mod tests;
