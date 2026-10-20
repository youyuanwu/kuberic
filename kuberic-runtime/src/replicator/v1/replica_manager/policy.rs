// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided ReplicaManager.cpp:173-284,716-778,1832-2076,
// standarddeviation.h/.cpp and REConfigMacros.h:166-177 at
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

use super::*;

#[derive(Clone, Copy, Debug)]
pub(in crate::replicator::v1) struct QueuePolicyConfig {
    pub(in crate::replicator::v1) trace_interval: Duration,
    pub(in crate::replicator::v1) idle_max_lag: Duration,
    pub(in crate::replicator::v1) restart_idle: bool,
    pub(in crate::replicator::v1) restart_active: bool,
    pub(in crate::replicator::v1) idle_usage_percent: u32,
    pub(in crate::replicator::v1) active_usage_percent: u32,
    pub(in crate::replicator::v1) active_min_age: Duration,
    pub(in crate::replicator::v1) extra_active: usize,
}

impl QueuePolicyConfig {
    pub(in crate::replicator::v1) fn for_persisted(persisted: bool) -> Self {
        Self {
            trace_interval: Duration::from_secs(30),
            idle_max_lag: Duration::from_secs(60),
            restart_idle: persisted,
            restart_active: persisted,
            idle_usage_percent: 85,
            active_usage_percent: 90,
            active_min_age: Duration::from_secs(120),
            extra_active: 0,
        }
    }

    pub(in crate::replicator::v1) fn is_valid(self) -> bool {
        !self.trace_interval.is_zero() && !self.idle_max_lag.is_zero()
    }

    pub(in crate::replicator::v1) fn interval(self) -> Duration {
        self.trace_interval.min(self.idle_max_lag)
    }
}

impl ReplicaManager {
    fn retarget_idles(&self, target: Lsn) -> Result<bool, ManagerError> {
        let mut changed = false;
        for idle in &self.idle {
            if idle.session.idle_faulted() {
                continue;
            }
            let build = self.builds.get(&idle.replica_id).ok_or_else(|| {
                ManagerError::BuildProgress(format!("idle {} has no copy owner", idle.replica_id))
            })?;
            changed |= build
                .retarget_replication(target)
                .map_err(|error| ManagerError::BuildProgress(error.to_string()))?;
        }
        Ok(changed)
    }

    pub(in crate::replicator::v1) fn refresh_idle_targets(&self) -> Result<(), ManagerError> {
        self.ensure_active()?;
        self.retarget_idles(self.queue.current_progress())?;
        Ok(())
    }

    pub(in crate::replicator::v1) fn apply_queue_pressure(
        &mut self,
        config: QueuePolicyConfig,
        now: Duration,
    ) -> (ManagerEffects, Result<(), ManagerError>) {
        let mut effects = ManagerEffects::default();
        let result = self.apply_queue_pressure_inner(config, now, &mut effects);
        (effects, result)
    }

    fn apply_queue_pressure_inner(
        &mut self,
        config: QueuePolicyConfig,
        now: Duration,
        effects: &mut ManagerEffects,
    ) -> Result<(), ManagerError> {
        self.ensure_active()?;
        let initial_usage = self.queue.utilization_percent();
        if config.restart_idle && initial_usage >= config.idle_usage_percent {
            if self.retarget_idles(self.queue.last_sequence_number())? {
                let observation = self.observe()?;
                self.advance(observation, effects);
            }
            if self.queue.utilization_percent() >= config.idle_usage_percent {
                let first = self.queue.first_lsn_in_replication_queue();
                let last = self.queue.last_sequence_number();
                let mut faulted = false;
                for idle in &self.idle {
                    if idle.session.idle_faulted() {
                        continue;
                    }
                    let build = self.builds.get(&idle.replica_id).ok_or_else(|| {
                        ManagerError::BuildProgress(format!(
                            "idle {} has no copy owner",
                            idle.replica_id
                        ))
                    })?;
                    let target = build
                        .finish_copy_target()
                        .map_err(|error| ManagerError::BuildProgress(error.to_string()))?;
                    let progress = self
                        .sample_progress(idle.replica_id, &idle.session)?
                        .idle_progress(target);
                    if progress <= first {
                        faulted |= build.try_fault_slow(
                            format!("Replica {} is causing high primary queue usage. First replication operation = {first}, last = {last}, replica progress = {progress}", idle.replica_id),
                            now,
                        ).map_err(|error| ManagerError::BuildProgress(error.to_string()))?;
                    }
                }
                if faulted {
                    let observation = self.observe()?;
                    self.advance(observation, effects);
                }
            }
        }
        if config.restart_active && initial_usage >= config.active_usage_percent {
            self.fault_slow_active(config, now, effects)?;
        }
        Ok(())
    }

    fn fault_slow_active(
        &mut self,
        config: QueuePolicyConfig,
        now: Duration,
        effects: &mut ManagerEffects,
    ) -> Result<(), ManagerError> {
        let Some(current) = self
            .current
            .as_ref()
            .filter(|set| set.write_quorum > 1 && Self::has_enough_replicas(set))
        else {
            return Ok(());
        };
        let mut candidates: Vec<_> = current
            .members
            .iter()
            .filter(|member| !member.session.active_faulted())
            .collect();
        if candidates.len() < current.write_quorum
            || candidates.len() - current.write_quorum < config.extra_active
            || self
                .queue
                .oldest_operation_age()
                .is_none_or(|age| age < config.active_min_age)
            || self
                .cached_current
                .and_then(|progress| progress.quorum)
                .is_none_or(|quorum| quorum <= self.queue.first_lsn_in_replication_queue())
        {
            return Ok(());
        }
        candidates.sort_by_key(|member| member.session.averages().0);
        let samples: Vec<_> = candidates
            .iter()
            .take(current.write_quorum - 1)
            .map(|member| member.session.averages().0.as_secs_f64() * 1000.0)
            .collect();
        let mean_ms = samples.iter().sum::<f64>() / samples.len() as f64;
        // Native TimeSpan truncates mean and deviation separately to 100ns.
        let mean_ticks = (mean_ms * 10_000.0) as u128;
        let native_mean_ms = mean_ticks as f64 / 10_000.0;
        let variance = samples.iter().map(|value| value * value).sum::<f64>()
            / samples.len() as f64
            - native_mean_ms * native_mean_ms;
        let deviation_ticks = (variance.max(0.0).sqrt() * 10_000.0) as u128;
        let slowest = candidates.last().unwrap();
        let least_progress_id = current
            .members
            .iter()
            .filter(|member| !member.session.active_faulted())
            .min_by_key(|member| member.session.receive_lsn(0))
            .map(|member| member.replica_id);
        if slowest.session.averages().0.as_nanos() / 100 <= mean_ticks + 2 * deviation_ticks
            || least_progress_id != Some(slowest.replica_id)
            || self.queue.utilization_percent() < config.active_usage_percent
        {
            return Ok(());
        }
        if slowest.session.try_fault_active(
            format!("Replica {} is causing high primary queue usage. First replication operation = {}, last = {}; quorum receive mean = {mean_ms}ms, deviation = {}ms",
                slowest.replica_id, self.queue.first_lsn_in_replication_queue(),
                self.queue.last_sequence_number(), deviation_ticks as f64 / 10_000.0),
            now,
        ) {
            let observation = self.observe()?;
            self.advance(observation, effects);
        }
        Ok(())
    }
}
