// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;
use crate::replicator::v1::build_idle::BuildError;
use crate::replicator::v1::copy_establishment::EstablishError;
use crate::replicator::v1::replica_manager::CatchupMode;

fn remote_error(id: i64) -> wire::Envelope {
    let mut envelope = ack(id, 0, 0, 0);
    let Some(wire::envelope::Body::ReplicationAck(body)) = &mut envelope.body else {
        unreachable!()
    };
    body.error = -7;
    envelope
}

async fn idle_rejection_order(reject_first: bool) {
    let mut rig = Rig::new();
    let healthy = rig.active(&[20], 2).remove(0);
    let mut write = rig.enqueue();
    assert_eq!(write.lsn, 1);
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    members: vec![SessionMember {
                        replica_id: 20,
                        session: healthy,
                        must_catchup: true,
                    }],
                    write_quorum: 2,
                },
                previous: None,
                catchup_configuration: true,
            },
            vec![route(20)],
        )
        .unwrap();
    let mut wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    assert_eq!(wait.try_recv(), Err(TryRecvError::Empty));
    let provider = Arc::new(Provider::new(1));
    let build = rig.begin(30, true, provider.clone());
    if reject_first {
        rig.handle.receive_ack(&remote_error(30)).unwrap();
        rig.pending();
        assert!(build.result().unwrap().result.is_err());
        assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(wait.try_recv(), Err(TryRecvError::Empty));
    }
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    assert_eq!(wait.await.unwrap(), Ok(()));
    if !reject_first {
        rig.handle.receive_ack(&remote_error(30)).unwrap();
        rig.pending();
    }
    let end = build.result().unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Remote(-7)))
    ));
    assert!(end.fault.is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .build_fault()
            .is_none()
    );
    let next = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 2, 2, -1)).unwrap();
    assert_eq!(next.completion.await.unwrap(), Ok(()));
    rig.handle.close(Duration::ZERO).unwrap();
    rig.primary.run().await.unwrap();
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test]
async fn dispensable_idle_rejection_before_healthy_ack_preserves_real_quorum() {
    idle_rejection_order(true).await;
}

#[tokio::test]
async fn dispensable_idle_rejection_after_healthy_ack_preserves_real_quorum() {
    idle_rejection_order(false).await;
}

#[tokio::test]
async fn active_copy_error_cannot_replace_required_current_previous_or_marked_progress() {
    for required in ["current", "previous", "marked", "all"] {
        let mut rig = Rig::new();
        let sessions = rig.active(&[20, 30], 2);
        let members: Vec<_> = [20, 30]
            .into_iter()
            .zip(sessions)
            .map(|(replica_id, session)| SessionMember {
                replica_id,
                session,
                must_catchup: required == "marked" && replica_id == 30,
            })
            .collect();
        let mut write = rig.enqueue();
        rig.handle
            .configure_established(
                EstablishedConfiguration {
                    current: ReplicaSet {
                        write_quorum: if required == "current" { 3 } else { 2 },
                        members: members.clone(),
                    },
                    previous: (required == "previous").then(|| ReplicaSet {
                        write_quorum: 2,
                        members: vec![members[1].clone()],
                    }),
                    catchup_configuration: true,
                },
                vec![route(20), route(30)],
            )
            .unwrap();
        let mode = if required == "all" {
            CatchupMode::All
        } else {
            CatchupMode::WriteQuorum
        };
        let mut wait = rig.handle.begin_catchup(mode).unwrap();
        rig.handle.receive_ack(&remote_error(30)).unwrap();
        rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
        // An active copy error is not a replication NACK, but also supplies
        // no invented replication apply cursor for C.
        if matches!(required, "current" | "previous") {
            assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
        } else {
            assert_eq!(write.completion.try_recv().unwrap(), Ok(()));
        }
        if required == "previous" {
            // Catch-up is current-only; previous still constrains the write.
            assert_eq!(wait.try_recv().unwrap(), Ok(()));
        } else {
            assert_eq!(wait.try_recv(), Err(TryRecvError::Empty));
        }
        rig.handle.receive_ack(&ack(30, 1, 1, -1)).unwrap();
        if matches!(required, "current" | "previous") {
            assert_eq!(write.completion.await.unwrap(), Ok(()));
        }
        if required != "previous" {
            assert_eq!(wait.await.unwrap(), Ok(()));
        }
        rig.handle.close(Duration::ZERO).unwrap();
        rig.primary.run().await.unwrap();
        rig.primary.drain_builds().await.unwrap();
    }
}

async fn local_fault_blocks_new_entries(enumeration: bool) {
    let mut rig = Rig::new();
    rig.configure(Vec::new(), 1);
    assert_eq!(rig.enqueue().completion.await.unwrap(), Ok(()));
    assert_eq!(
        rig.handle
            .begin_catchup(CatchupMode::WriteQuorum)
            .unwrap()
            .await
            .unwrap(),
        Ok(())
    );
    let mut provider = Provider::new(1);
    provider.failure = !enumeration;
    provider.enumeration_failure = enumeration;
    let build = rig.begin(30, false, Arc::new(provider));
    rig.handle.receive_ack(&ack(30, 1, 1, 0)).unwrap();
    rig.pending();
    let end = build.result().unwrap();
    assert!(end.result.is_err());
    assert_eq!(
        end.fault.unwrap().description,
        if enumeration {
            "EndGetNext"
        } else {
            "GetCopyState"
        }
    );
    let tail = rig
        .primary
        .root
        .manager
        .lock()
        .unwrap()
        .queue()
        .last_sequence_number();
    let mut catchup = rig.handle.begin_catchup(CatchupMode::WriteQuorum);
    let assignment = rig
        .handle
        .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits());
    let catchup_rejected = matches!(catchup, Err(PrimaryError::Manager(ref error))
        if matches!(error.as_ref(), ManagerError::ReportedFault { .. }));
    let assignment_rejected = matches!(assignment, Err(PrimaryError::Manager(ref error))
        if matches!(error.as_ref(), ManagerError::ReportedFault { .. }));
    let admitted_lsn = assignment.as_ref().ok().map(|admission| admission.lsn);
    let cached_completion = catchup.as_mut().ok().map(|receiver| receiver.try_recv());
    assert!(
        catchup_rejected && assignment_rejected,
        "local fault entry: catchup rejected={catchup_rejected}, assignment rejected={assignment_rejected}, cached completion={cached_completion:?}, admitted LSN={admitted_lsn:?}"
    );
    assert_eq!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .queue()
            .last_sequence_number(),
        tail
    );
    rig.handle.close(Duration::ZERO).unwrap();
    rig.primary.run().await.unwrap();
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test]
async fn local_acquisition_fault_rejects_new_assignment_and_cached_ready_catchup() {
    local_fault_blocks_new_entries(false).await;
}

#[tokio::test]
async fn local_enumeration_fault_rejects_new_assignment_and_cached_ready_catchup() {
    local_fault_blocks_new_entries(true).await;
}
