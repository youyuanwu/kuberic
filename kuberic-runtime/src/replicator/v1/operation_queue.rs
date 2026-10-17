// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided Rust translation of selected OperationQueue.h/.cpp algorithms:
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Internal, unselected queue core, not a replication engine.
//!
//! Owners serialize synchronous calls. Callbacks must be fast, staging only;
//! owners perform provider/transport/dispatch effects outside their guards.
//! There is no lock, task or await here. Ordered owned storage replaces the
//! native ring, but capacity still bounds LSN span (holes included), not length.
//! Trace/timing/performance-counter plumbing is not translated.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::application::{Lsn, OperationData};

/// Minimal operation dependency, not the received-operation ACK state machine.
/// The owner supplies numbered data and its current completion-blocking state.
/// Releasing that state is not a provider-persistence or quorum assertion.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue entry seam")
)]
pub(super) struct QueueOperation {
    lsn: Lsn,
    data: OperationData,
    needs_ack: AtomicBool,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "unselected V1 queue entry constructors and owner ACK predicate"
    )
)]
impl QueueOperation {
    pub(super) fn new(lsn: Lsn, data: OperationData, needs_ack: bool) -> Arc<Self> {
        Arc::new(Self {
            lsn,
            data,
            needs_ack: AtomicBool::new(needs_ack),
        })
    }

    pub(super) fn lsn(&self) -> Lsn {
        self.lsn
    }

    pub(super) fn data(&self) -> &OperationData {
        &self.data
    }

    pub(super) fn needs_ack(&self) -> bool {
        self.needs_ack.load(Ordering::Acquire)
    }

    /// Updates the dependency's current predicate, not the full native ACK
    /// callback lifecycle. Reset/removal must not change externally held state.
    pub(super) fn release_ack_requirement(&self) {
        self.needs_ack.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue configuration")
)]
pub(super) struct QueueConfig {
    pub(super) initial_capacity: usize,
    /// Zero disables this bound; capacity measures sequence-number positions.
    pub(super) max_capacity: usize,
    /// Logical payload bytes, excluding map/object/wire overhead; zero disables.
    pub(super) max_bytes: usize,
    pub(super) max_completed_count: usize,
    pub(super) max_completed_bytes: usize,
    pub(super) clean_on_complete: bool,
}

impl QueueConfig {
    fn validate(self) {
        assert!(self.initial_capacity > 1 && self.initial_capacity.is_power_of_two());
        assert!(
            self.max_capacity == 0
                || (self.max_capacity.is_power_of_two()
                    && self.initial_capacity <= self.max_capacity)
        );
        assert!(self.max_capacity != 0 || self.max_bytes != 0);
        // Preserve the native conjunction, not a guessed stronger prohibition.
        assert!(
            !self.clean_on_complete
                || self.max_completed_count == 0
                || self.max_completed_bytes == 0
        );
    }
}

/// All boundaries are exclusive "next" positions; the native invalid/capability
/// sentinel is zero, distinct from the public API's INVALID_LSN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue progress view")
)]
pub(super) struct QueueProgress {
    pub(super) retained_head: Lsn,
    pub(super) complete_head: Lsn,
    pub(super) commit_head: Lsn,
    pub(super) tail: Lsn,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue progress accessors")
)]
impl QueueProgress {
    pub(super) fn first_available_completed(self) -> Lsn {
        if self.retained_head == self.complete_head {
            0
        } else {
            self.retained_head
        }
    }

    pub(super) fn first_committed(self) -> Lsn {
        if self.complete_head == self.commit_head {
            0
        } else {
            self.complete_head
        }
    }

    pub(super) fn has_pending(self) -> bool {
        self.complete_head != self.tail
    }

    pub(super) fn has_committed(self) -> bool {
        self.complete_head != self.commit_head
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue admission result")
)]
pub(super) enum EnqueueError {
    Duplicate,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 queue callback control")
)]
pub(super) enum CallbackControl {
    Keep,
    Disable,
}

/// Borrow-safe self-disable and callback-visible progress, at the *native call
/// point*. Does not promise arbitrary reentrant queue mutation.
type CommitCallback = Box<dyn FnMut(Arc<QueueOperation>, QueueProgress) -> CallbackControl + Send>;

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "unselected V1 OperationQueue state, production compiled but not factory selected"
    )
)]
pub(super) struct OperationQueue {
    config: QueueConfig,
    entries: BTreeMap<Lsn, Arc<QueueOperation>>,
    progress: QueueProgress,
    require_service_ack: bool,
    ignore_commit: bool,
    callback: Option<CommitCallback>,
    capacity: usize,
    expanded_last: bool,
    capacity_sum: usize,
    capacity_changes: usize,
    total_bytes: usize,
    completed_bytes: usize,
    completed_count: usize,
}

// This expectation is restricted to the preparatory queue API, not its module
// or the crate. Tests call the real production implementation; later owner
// integration will remove it. It does not suppress any other compiler lint.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "V1 queue API awaits later roadmap owner integration; no production activation in this slice"
    )
)]
impl OperationQueue {
    pub(super) fn new(
        config: QueueConfig,
        start: Lsn,
        require_service_ack: bool,
        ignore_commit: bool,
    ) -> Self {
        config.validate();
        assert!(start > 0);
        Self {
            config,
            entries: BTreeMap::new(),
            progress: QueueProgress {
                retained_head: start,
                complete_head: start,
                commit_head: start,
                tail: start,
            },
            require_service_ack,
            ignore_commit,
            callback: None,
            capacity: config.initial_capacity,
            expanded_last: false,
            capacity_sum: config.initial_capacity,
            capacity_changes: 1,
            total_bytes: 0,
            completed_bytes: 0,
            completed_count: 0,
        }
    }

    /// Native transfer constructor (cpp 93–180). Takes new limits/cleanup
    /// policy, inherits ACK/ignore-commit/callback and all queue progress.
    /// Owners normalize role state/byte/history limits, not this constructor.
    pub(super) fn transfer(mut self, config: QueueConfig) -> Self {
        config.validate();
        self.config = config;
        if config.max_capacity != 0 {
            assert!(self.entries.len() <= config.max_capacity);
            let previous = self.expanded_last;
            self.expanded_last = true;
            if !self.shrink(self.span(self.progress.tail, self.progress.retained_head)) {
                self.expanded_last = previous;
            }
            self.capacity_sum = self.capacity;
            self.capacity_changes = 1;
        }
        self
    }

    pub(super) fn progress(&self) -> QueueProgress {
        self.progress
    }

    pub(super) fn operation_count(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub(super) fn completed_bytes(&self) -> usize {
        self.completed_bytes
    }

    pub(super) fn completed_count(&self) -> usize {
        self.completed_count
    }

    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(super) fn capacity_change_count(&self) -> usize {
        self.capacity_changes
    }

    pub(super) fn convergent_capacity(&self) -> usize {
        (self.capacity_sum / self.capacity_changes).next_power_of_two()
    }

    pub(super) fn set_ignore_commit(&mut self, value: bool) {
        assert_eq!(self.progress.commit_head, self.progress.complete_head);
        self.ignore_commit = value;
    }

    /// Flag assignment only: the owner must arrange valid history state.
    pub(super) fn set_clean_on_complete(&mut self, value: bool) {
        self.config.clean_on_complete = value;
    }

    /// cpp 214–236: replacement replays only committed, noncompleted entries.
    pub(super) fn set_commit_callback(&mut self, callback: Option<CommitCallback>) {
        self.callback = callback;
        for lsn in self.progress.complete_head..self.progress.commit_head {
            if self.callback.is_none() {
                break;
            }
            self.invoke_callback(lsn);
        }
    }

    fn invoke_callback(&mut self, lsn: Lsn) {
        if let Some(callback) = &mut self.callback
            && callback(self.entries[&lsn].clone(), self.progress) == CallbackControl::Disable
        {
            self.callback = None;
        }
    }

    /// cpp 367–441. Input is already numbered; successful assignment belongs
    /// to ReplicationQueueManager, not OperationQueue.
    pub(super) fn try_enqueue(&mut self, op: Arc<QueueOperation>) -> Result<(), EnqueueError> {
        assert!(op.lsn > 0);
        if op.lsn < self.progress.commit_head || self.entries.contains_key(&op.lsn) {
            return Err(EnqueueError::Duplicate);
        }
        self.make_room(op.lsn, op.data.len())?;
        self.total_bytes += op.data.len();
        self.progress.tail = self.progress.tail.max(op.lsn + 1);
        assert!(self.entries.insert(op.lsn, op).is_none());
        self.check_invariants();
        Ok(())
    }

    pub(super) fn get_operation(&self, lsn: Lsn) -> Option<&Arc<QueueOperation>> {
        // Native callers must stay in the retained interval; ordered storage
        // intentionally does not reproduce ring aliasing for invalid queries.
        self.entries.get(&lsn)
    }

    /// cpp 458–500: a gap returns false without rolling back appended output.
    pub(super) fn get_operations(&self, first: Lsn, out: &mut Vec<Arc<QueueOperation>>) -> bool {
        if first >= self.progress.tail {
            return true;
        }
        assert!(
            first >= self.progress.retained_head,
            "query precedes retained range"
        );
        for lsn in first..self.progress.tail {
            let Some(op) = self.entries.get(&lsn) else {
                return false;
            };
            out.push(op.clone());
        }
        true
    }

    /// cpp 256–359. Byte pressure precedes count/span pressure. In particular,
    /// failure is not universally transactional with respect to history.
    fn make_room(&mut self, lsn: Lsn, bytes: usize) -> Result<(), EnqueueError> {
        if self.config.max_bytes != 0 {
            let mut pending_bytes = self.total_bytes - self.completed_bytes + bytes;
            if pending_bytes > self.config.max_bytes {
                if lsn < self.progress.tail {
                    let mut drop_start = self.progress.tail - 1;
                    while drop_start > lsn && pending_bytes > self.config.max_bytes {
                        pending_bytes -= self.entries[&drop_start].data.len();
                        drop_start -= 1;
                        while drop_start >= self.progress.commit_head
                            && !self.entries.contains_key(&drop_start)
                        {
                            drop_start -= 1;
                        }
                    }
                    if pending_bytes <= self.config.max_bytes {
                        self.discard_from(drop_start + 1);
                    }
                }
                if pending_bytes > self.config.max_bytes {
                    return Err(EnqueueError::Full);
                }
            }
            let mut new_bytes = self.total_bytes + bytes;
            if new_bytes > self.config.max_bytes {
                let mut clear_until = self.progress.retained_head;
                while clear_until < self.progress.complete_head && new_bytes > self.config.max_bytes
                {
                    new_bytes -= self.entries[&clear_until].data.len();
                    clear_until += 1;
                }
                assert!(new_bytes <= self.config.max_bytes);
                self.clear_completed(clear_until);
            }
        }

        let distance = self.span(lsn, self.progress.retained_head);
        if distance >= self.capacity {
            if !self.config.clean_on_complete && self.expand(distance) {
                return Ok(());
            }
            let missing = distance - self.capacity + 1;
            if missing <= self.span(self.progress.complete_head, self.progress.retained_head) {
                self.clear_completed(self.progress.retained_head + missing as Lsn);
                self.shrink(self.span(lsn, self.progress.retained_head));
            } else {
                self.clear_completed(self.progress.complete_head);
                if !self.expand(self.span(lsn, self.progress.complete_head)) {
                    return Err(EnqueueError::Full);
                }
            }
        }
        Ok(())
    }

    // cpp 502–540, 571–619, 1347–1369. Storage layout is independent of this
    // native capacity/convergence policy. active is a distance for expansion
    // (room for the candidate too) and an exclusive span for shrinking.
    fn expand(&mut self, active: usize) -> bool {
        assert!(active >= self.capacity);
        let new_capacity = if self.convergent_capacity() > active {
            self.convergent_capacity()
        } else {
            let mut next = self.capacity * 2;
            while active >= next
                && (self.config.max_capacity == 0 || next < self.config.max_capacity)
            {
                next *= 2;
            }
            if (self.config.max_capacity != 0 && next > self.config.max_capacity) || active >= next
            {
                return false;
            }
            next
        };
        self.update_capacity(new_capacity);
        self.expanded_last = true;
        true
    }

    fn shrink(&mut self, active: usize) -> bool {
        if !self.expanded_last {
            return false;
        }
        let mut next = self.config.initial_capacity;
        while next < self.capacity && next < active {
            next *= 2;
        }
        if next >= self.capacity {
            return false;
        }
        self.update_capacity(next);
        self.expanded_last = false;
        true
    }

    fn update_capacity(&mut self, next: usize) {
        assert!(next > 1 && next != self.capacity);
        self.capacity = next;
        self.capacity_changes += 1;
        self.capacity_sum += next;
    }

    fn span(&self, end: Lsn, start: Lsn) -> usize {
        usize::try_from(end - start).expect("valid nonnegative allocatable LSN span")
    }

    fn clear_completed(&mut self, until: Lsn) {
        if self.progress.retained_head >= until {
            return;
        }
        while self.progress.retained_head < until {
            self.remove_item(self.progress.retained_head);
            self.progress.retained_head += 1;
        }
        self.check_invariants();
    }

    /// Exclusive retained-floor removal, not completion (cpp 621–668).
    pub(super) fn update_complete_head(&mut self, boundary: Lsn) -> bool {
        if boundary <= self.progress.retained_head || boundary > self.progress.complete_head {
            return false;
        }
        self.clear_completed(boundary);
        self.shrink(self.span(self.progress.tail, boundary));
        true
    }

    /// cpp 670–675: reclassify history as committed work without deleting it.
    pub(super) fn reset_completed(&mut self) {
        self.completed_bytes = 0;
        self.completed_count = 0;
        self.progress.complete_head = self.progress.retained_head;
    }

    pub(super) fn commit(&mut self) -> bool {
        let old = self.progress.commit_head;
        while self.progress.commit_head < self.progress.tail {
            if !self.entries.contains_key(&self.progress.commit_head) {
                break;
            }
            self.commit_item();
        }
        if old < self.progress.commit_head {
            self.check_invariants();
            true
        } else {
            false
        }
    }

    /// Inclusive, caller-established contiguous target (cpp 1054–1126).
    pub(super) fn commit_through(&mut self, last: Lsn) -> bool {
        if last <= 0 {
            return false;
        }
        assert!(last >= self.progress.complete_head && last < self.progress.tail);
        let old = self.progress.commit_head;
        while self.progress.commit_head <= last {
            assert!(
                self.entries.contains_key(&self.progress.commit_head),
                "commit target crosses hole"
            );
            self.commit_item();
        }
        if old < self.progress.commit_head {
            self.check_invariants();
            true
        } else {
            false
        }
    }

    fn commit_item(&mut self) {
        // cpp 1187–1200: callback before advancing commit.
        self.invoke_callback(self.progress.commit_head);
        self.progress.commit_head += 1;
    }

    pub(super) fn complete(&mut self) -> bool {
        let last = if self.ignore_commit {
            self.progress.tail
        } else {
            self.progress.commit_head
        };
        let old = self.progress.complete_head;
        while self.progress.complete_head < last {
            let Some(op) = self.entries.get(&self.progress.complete_head) else {
                break;
            };
            if self.require_service_ack && op.needs_ack() {
                break;
            }
            self.complete_item();
        }
        self.finish_complete(old)
    }

    /// Inclusive target: unlike scanning, holes/required ACKs are caller errors.
    /// Native checks tail before the no-op cases (cpp 751–841).
    pub(super) fn complete_through(&mut self, last: Lsn) -> bool {
        assert!(
            last < self.progress.tail,
            "completion target must be below tail"
        );
        if last <= 0 || self.progress.complete_head > last {
            return false;
        }
        let last = if self.ignore_commit {
            last
        } else {
            last.min(self.progress.commit_head - 1)
        };
        let old = self.progress.complete_head;
        while self.progress.complete_head <= last {
            let op = self
                .entries
                .get(&self.progress.complete_head)
                .expect("completion target crosses hole");
            assert!(
                !self.require_service_ack || !op.needs_ack(),
                "completion target requires ACK"
            );
            self.complete_item();
        }
        self.finish_complete(old)
    }

    fn finish_complete(&mut self, old: Lsn) -> bool {
        if old == self.progress.complete_head {
            return false;
        }
        self.check_invariants();
        if self.config.clean_on_complete {
            self.shrink(self.span(self.progress.tail, self.progress.retained_head));
        }
        true
    }

    fn complete_item(&mut self) {
        let lsn = self.progress.complete_head;
        if self.ignore_commit {
            // cpp 901–944: advance commit BEFORE callback, completion AFTER.
            self.progress.commit_head += 1;
            self.invoke_callback(lsn);
        }
        self.progress.complete_head += 1;
        if self.config.clean_on_complete {
            let op = self.entries.remove(&lsn).expect("completed operation");
            self.total_bytes -= op.data.len();
            self.progress.retained_head += 1;
        } else {
            self.completed_bytes += self.entries[&lsn].data.len();
            self.completed_count += 1;
            if self.should_trim() {
                while self.should_trim() {
                    self.remove_item(self.progress.retained_head);
                    self.progress.retained_head += 1;
                }
                self.shrink(self.span(self.progress.tail, self.progress.retained_head));
                self.check_invariants();
            }
        }
    }

    fn should_trim(&self) -> bool {
        (self.config.max_completed_bytes != 0
            && self.completed_bytes > self.config.max_completed_bytes)
            || (self.config.max_completed_count != 0
                && self.completed_count > self.config.max_completed_count)
    }

    /// Inclusive last-completed, clamped to available history on rewind.
    pub(super) fn update_last_completed(&mut self, last: Lsn) -> bool {
        if last < 0 {
            return false;
        }
        let next = last + 1;
        if self.progress.complete_head == next {
            return true;
        }
        if self.progress.complete_head < next {
            return self.complete_through(last);
        }
        if self.progress.complete_head == self.progress.retained_head {
            return false;
        }
        let next = next.max(self.progress.retained_head);
        while self.progress.complete_head > next {
            self.progress.complete_head -= 1;
            self.completed_bytes -= self.entries[&self.progress.complete_head].data.len();
            self.completed_count -= 1;
        }
        self.check_invariants();
        true
    }

    /// Inclusive last-committed. Recommit intentionally may replay callbacks.
    pub(super) fn update_commit_head(&mut self, last: Lsn) -> bool {
        if last < 0 {
            return false;
        }
        let next = last + 1;
        if self.progress.commit_head == next {
            return true;
        }
        if self.progress.commit_head < next {
            return self.commit_through(last);
        }
        if next < self.progress.complete_head {
            return false;
        }
        self.progress.commit_head = next;
        self.check_invariants();
        true
    }

    pub(super) fn discard_noncommitted(&mut self) {
        self.discard_from(self.progress.commit_head);
        self.shrink(self.span(self.progress.tail, self.progress.retained_head));
    }

    pub(super) fn discard_noncompleted(&mut self) {
        self.discard_from(self.progress.complete_head);
        self.shrink(self.span(self.progress.tail, self.progress.retained_head));
    }

    fn discard_from(&mut self, from: Lsn) {
        if from == self.progress.tail {
            return;
        }
        assert!(from >= self.progress.complete_head && from <= self.progress.tail);
        // Holes are harmless. No successful completion or ACK is fabricated.
        for lsn in from..self.progress.tail {
            self.remove_item(lsn);
        }
        self.progress.tail = from;
        self.progress.commit_head = self.progress.commit_head.min(from);
        self.check_invariants();
    }

    fn remove_item(&mut self, lsn: Lsn) {
        if let Some(op) = self.entries.remove(&lsn) {
            self.total_bytes -= op.data.len();
            if lsn < self.progress.complete_head {
                self.completed_bytes -= op.data.len();
                self.completed_count -= 1;
            }
        }
    }

    /// cpp 1202–1230: clear storage/statistics, preserve configuration/callback.
    pub(super) fn reset(&mut self, start: Lsn) {
        assert!(start > 0);
        self.entries.clear();
        self.progress = QueueProgress {
            retained_head: start,
            complete_head: start,
            commit_head: start,
            tail: start,
        };
        self.total_bytes = 0;
        self.completed_bytes = 0;
        self.completed_count = 0;
        self.capacity = self.config.initial_capacity;
        self.expanded_last = false;
        self.capacity_sum = self.capacity;
        self.capacity_changes = 1;
    }

    fn check_invariants(&self) {
        let p = self.progress;
        assert!(
            p.retained_head <= p.complete_head
                && p.complete_head <= p.commit_head
                && p.commit_head <= p.tail
        );
        assert!(!self.config.clean_on_complete || p.retained_head == p.complete_head);
        assert!(self.config.max_capacity == 0 || self.entries.len() <= self.config.max_capacity);
        assert!(self.total_bytes >= self.completed_bytes);
        assert!(self.entries.len() >= self.completed_count);
        assert!(p.retained_head != p.complete_head || self.completed_bytes == 0);
        assert!(!self.should_trim());
    }
}

#[cfg(test)]
mod tests;
