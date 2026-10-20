// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of AckSender.h/.cpp, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.
// See sf-v1-port-provenance.md for selected source/test ancestry.

//! Receiver ACK timing only. Progress, payload snapshot and pending-count
//! thresholds belong to the receiver. No timer runtime or wire ACK is implied.

use std::sync::Arc;
use std::time::Duration;

type AckCallback = Arc<dyn Fn(bool) -> bool + Send + Sync>;

/// Owns the callback/owner, not a stale progress snapshot. Dispatch outside the
/// caller's guard. `should_trace` is NOT a final/force bit in an ACK message.
pub(super) struct AckDispatch {
    callback: AckCallback,
    should_trace: bool,
}

impl AckDispatch {
    pub(super) fn dispatch(self) {
        // Issuance, not acceptance. Native ignores false and does not requeue.
        (self.callback)(self.should_trace);
    }
}

pub(super) struct AckSender {
    batch: Duration,
    retry: Duration,
    trace_interval: Duration,
    trace_started: Option<Duration>,
    force_on_pending_limit: bool,
    active: bool,
    callback: Option<AckCallback>,
    deadline: Option<Duration>,
    last_issued: Option<Duration>,
}

impl AckSender {
    pub(super) fn new(
        batch: Duration,
        retry: Duration,
        max_pending: usize,
        trace_interval: Duration,
    ) -> Self {
        Self {
            batch,
            retry,
            trace_interval,
            trace_started: None,
            force_on_pending_limit: !batch.is_zero() && max_pending > 0,
            active: false,
            callback: None,
            deadline: None,
            last_issued: None,
        }
    }

    pub(super) fn open(&mut self, callback: AckCallback, now: Duration) {
        assert!(!self.active && self.batch < self.retry);
        self.callback = Some(callback);
        // Native Stopwatch::Start is not Restart; Close does not stop it.
        self.trace_started.get_or_insert(now);
        self.active = true;
    }

    pub(super) fn force_on_pending_limit(&self) -> bool {
        self.force_on_pending_limit
    }

    pub(super) fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    pub(super) fn last_issued(&self) -> Option<Duration> {
        self.last_issued
    }

    pub(super) fn set_trace_interval(&mut self, interval: Duration) {
        self.trace_interval = interval;
    }

    fn should_trace(&mut self, now: Duration) -> bool {
        if !self.trace_interval.is_zero() && now - self.trace_started.unwrap() > self.trace_interval
        {
            self.trace_started = Some(now);
            true
        } else {
            false
        }
    }

    fn selected(&mut self, trace: bool, now: Duration) -> AckDispatch {
        self.last_issued = Some(now);
        AckDispatch {
            callback: self.callback.as_ref().unwrap().clone(),
            should_trace: trace,
        }
    }

    pub(super) fn schedule(&mut self, force: bool, now: Duration) -> Option<AckDispatch> {
        if !self.active {
            return None;
        }
        let immediate = self.batch.is_zero() || force;
        if immediate {
            self.deadline = None;
        } else {
            // Coalescing must not postpone the first outstanding deadline.
            self.deadline.get_or_insert(now + self.batch);
        }
        // Even a coalesced request consumes the trace interval.
        let trace = self.should_trace(now);
        immediate.then(|| self.selected(trace, now))
    }

    /// Entry of a previously dispatched timer callback. In particular, do NOT
    /// reject solely because a concurrent force already disabled the deadline.
    pub(super) fn on_timer(&mut self, now: Duration) -> Option<AckDispatch> {
        if !self.active || self.batch.is_zero() {
            return None;
        }
        self.deadline = None;
        let trace = self.should_trace(now);
        Some(self.selected(trace, now))
    }

    pub(super) fn close(&mut self, now: Duration) -> Option<AckDispatch> {
        if !self.active {
            return None;
        }
        self.active = false;
        self.deadline.take().map(|_| self.selected(true, now))
    }
}

#[cfg(test)]
mod tests;
