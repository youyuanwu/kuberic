#![allow(dead_code)]

use crate::crd::{
    DURABLE_OPERATION_VERSION, DurableOperationKind, DurableOperationPhase, DurableOperationStatus,
    EpochStatus, StablePartitionSnapshotStatus, StableReplicaRoleStatus,
    StableReplicaSnapshotStatus,
};
use crate::durable::ACTION_DEADLINE_SECONDS;

#[derive(Clone, Debug, PartialEq)]
pub struct DirectSwitchoverDefinition {
    pub execution_id: String,
    pub operation_id: String,
    pub previous_snapshot: StablePartitionSnapshotStatus,
    pub target_snapshot: StablePartitionSnapshotStatus,
    pub old_primary_id: i64,
    pub target_primary_id: i64,
    pub initial_deadline_unix_seconds: i64,
}

impl DirectSwitchoverDefinition {
    pub fn from_initial(operation: &DurableOperationStatus) -> Result<Self, String> {
        if operation.version != DURABLE_OPERATION_VERSION {
            return Err(format!(
                "unsupported direct switchover operation version {}",
                operation.version
            ));
        }
        if operation.kind != DurableOperationKind::Switchover {
            return Err("direct switchover input is not a switchover operation".to_string());
        }
        if operation.phase != DurableOperationPhase::Revoke
            || operation.pending_action.is_some()
            || operation.frozen_lsn.is_some()
            || operation.next_secondary_index != 0
        {
            return Err("direct switchover input is not immutable admission state".to_string());
        }
        let previous_snapshot = operation
            .previous_snapshot
            .cloned()
            .ok_or_else(|| "direct switchover input has no previous snapshot".to_string())?;
        validate_snapshot(&previous_snapshot)?;
        validate_snapshot(&operation.target_snapshot)?;
        if previous_snapshot.primary_id != operation.old_primary_id {
            return Err(
                "direct switchover old primary does not match admission snapshot".to_string(),
            );
        }
        if operation.target_snapshot.primary_id != operation.target_primary_id {
            return Err("direct switchover target does not match target snapshot".to_string());
        }
        if operation.old_primary_id == operation.target_primary_id {
            return Err("direct switchover target is already primary".to_string());
        }
        if previous_snapshot.members.len() != operation.target_snapshot.members.len() {
            return Err("direct switchover membership changed at admission".to_string());
        }
        for previous in &previous_snapshot.members {
            let target = member(&operation.target_snapshot, previous.id)?;
            if target.instance_id != previous.instance_id {
                return Err(format!(
                    "direct switchover replica {} incarnation changed at admission",
                    previous.id
                ));
            }
        }

        Ok(Self {
            execution_id: operation.execution_id.clone(),
            operation_id: operation.operation_id.clone(),
            previous_snapshot,
            target_snapshot: operation.target_snapshot.clone(),
            old_primary_id: operation.old_primary_id,
            target_primary_id: operation.target_primary_id,
            initial_deadline_unix_seconds: operation.phase_deadline_unix_seconds,
        })
    }

    pub fn member(&self, id: i64) -> Result<&StableReplicaSnapshotStatus, String> {
        member(&self.previous_snapshot, id)
    }

    pub fn normal_epoch_distribution_ids(&self) -> Vec<i64> {
        sorted_ids_excluding(
            &self.previous_snapshot,
            &[self.old_primary_id, self.target_primary_id],
        )
    }

    pub fn compensation_epoch_distribution_ids(&self) -> Vec<i64> {
        sorted_ids_excluding(&self.previous_snapshot, &[self.old_primary_id])
    }

    pub fn compensation_snapshot(&self) -> StablePartitionSnapshotStatus {
        let mut snapshot = self.previous_snapshot.clone();
        snapshot.epoch = self.target_snapshot.epoch.clone();
        snapshot.primary_id = self.old_primary_id;
        for member in &mut snapshot.members {
            member.role = if member.id == self.old_primary_id {
                StableReplicaRoleStatus::Primary
            } else {
                StableReplicaRoleStatus::ActiveSecondary
            };
        }
        snapshot
    }
}

pub fn next_deadline(observed_at_unix_seconds: i64) -> Result<i64, String> {
    observed_at_unix_seconds
        .checked_add(ACTION_DEADLINE_SECONDS)
        .ok_or_else(|| "direct switchover action deadline overflows unix time".to_string())
}

pub fn expected_epoch_for_previous(snapshot: &StablePartitionSnapshotStatus) -> EpochStatus {
    snapshot.epoch.clone()
}

fn member(
    snapshot: &StablePartitionSnapshotStatus,
    id: i64,
) -> Result<&StableReplicaSnapshotStatus, String> {
    snapshot
        .members
        .iter()
        .find(|member| member.id == id)
        .ok_or_else(|| format!("direct switchover replica {id} is not in the snapshot"))
}

fn sorted_ids_excluding(snapshot: &StablePartitionSnapshotStatus, excluded: &[i64]) -> Vec<i64> {
    let mut ids = snapshot
        .members
        .iter()
        .filter_map(|member| (!excluded.contains(&member.id)).then_some(member.id))
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn validate_snapshot(snapshot: &StablePartitionSnapshotStatus) -> Result<(), String> {
    if snapshot.members.is_empty() {
        return Err("direct switchover snapshot has no members".to_string());
    }
    let expected_quorum = u32::try_from(snapshot.members.len() / 2 + 1)
        .map_err(|_| "direct switchover snapshot quorum overflows u32".to_string())?;
    if snapshot.write_quorum != expected_quorum {
        return Err("direct switchover snapshot write quorum is not a majority".to_string());
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut instances = std::collections::BTreeSet::new();
    let mut primary_count = 0;
    for member in &snapshot.members {
        if !ids.insert(member.id) {
            return Err(format!(
                "direct switchover snapshot repeats replica {}",
                member.id
            ));
        }
        if member.instance_id.is_empty() || !instances.insert(member.instance_id.as_str()) {
            return Err(
                "direct switchover snapshot has an empty or repeated incarnation".to_string(),
            );
        }
        if member.role == StableReplicaRoleStatus::Primary {
            primary_count += 1;
            if member.id != snapshot.primary_id {
                return Err("direct switchover snapshot role disagrees with primary id".to_string());
            }
        } else if member.id == snapshot.primary_id {
            return Err("direct switchover primary id is not marked primary".to_string());
        }
    }
    if primary_count != 1 {
        return Err("direct switchover snapshot must contain exactly one primary".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        primary_id: i64,
        count: i64,
        configuration_number: i64,
    ) -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 1,
                configuration_number,
            },
            primary_id,
            members: (1..=count)
                .map(|id| StableReplicaSnapshotStatus {
                    id,
                    instance_id: format!("instance-{id}"),
                    role: if id == primary_id {
                        StableReplicaRoleStatus::Primary
                    } else {
                        StableReplicaRoleStatus::ActiveSecondary
                    },
                    election_metadata: None,
                })
                .collect(),
            write_quorum: u32::try_from(count / 2 + 1).unwrap(),
        }
    }

    #[test]
    fn distribution_orders_are_deterministic() {
        let previous = snapshot(1, 4, 7);
        let target = snapshot(3, 4, 8);
        let definition = DirectSwitchoverDefinition {
            execution_id: "execution".to_string(),
            operation_id: "operation".to_string(),
            previous_snapshot: previous,
            target_snapshot: target,
            old_primary_id: 1,
            target_primary_id: 3,
            initial_deadline_unix_seconds: 10,
        };
        assert_eq!(definition.normal_epoch_distribution_ids(), vec![2, 4]);
        assert_eq!(
            definition.compensation_epoch_distribution_ids(),
            vec![2, 3, 4]
        );
        let compensated = definition.compensation_snapshot();
        assert_eq!(compensated.primary_id, 1);
        assert_eq!(compensated.epoch.configuration_number, 8);
    }

    #[test]
    fn direct_switchover_rejects_old_operation_versions() {
        let mut operation =
            crate::durable::start_switchover("set-uid", snapshot(1, 2, 7), 2, 100).unwrap();
        operation.version = DURABLE_OPERATION_VERSION + 1;
        assert!(DirectSwitchoverDefinition::from_initial(&operation).is_err());
    }
}
