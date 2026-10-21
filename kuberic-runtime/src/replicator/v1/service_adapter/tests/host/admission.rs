use super::*;
use crate::application::OpenMode;
use crate::host::hosting::PodRuntime;
use crate::protocol::public_operations::{
    PublicV1Identity, PublicV1LifecycleBinding, StatePersistence,
};
use crate::protocol::types::ResourceUid;
use std::sync::mpsc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_approved_assignment_shares_actual_host_revoke_and_final_catchup_cut() {
    let mut node = lone();
    let registry = node.registry.clone();
    let local = config(1, 1, &[]).configuration.members[0].identity.clone();
    let runtime = PodRuntime::new_replacement(
        local,
        Application::new(),
        Arc::new(()),
        PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: ResourceUid::new("admission-cut"),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        },
        "127.0.0.1:0".parse().unwrap(),
    );
    let context = runtime.public_open_context(OpenMode::Existing).await;
    let gate = context.partition.context.native_write_admission().unwrap();
    node.adapter.service.write_admission = Some(gate.clone());
    let revoke = runtime.public_lifecycle_callbacks().unwrap().revoke_access;
    let (entered, observed) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let held = Mutex::new(held);
    node.adapter.service.before_assignment = Some(Arc::new(move || {
        entered.send(()).unwrap();
        held.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }));
    let once = AtomicBool::new(true);
    let configuration = facade_config(&node, &[(2, true), (3, true)]);
    let facade = FacadeSetup::new(node.adapter, configuration)
        .start(
            Arc::new(move || {
                if once.swap(false, Ordering::SeqCst) {
                    AccessStatus::Granted
                } else {
                    context.partition.get_write_status_now()
                }
            }),
            Arc::new(|_| {}),
        )
        .unwrap();
    facade.open().await.unwrap();
    facade
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    let final_config = config(1, 2, &[(2, 0, true), (3, 0, false)]);
    let mut empty = config(1, 0, &[]);
    empty.configuration.members.clear();
    empty.configuration.configuration_id = empty.configuration.expected_id();
    facade
        .update_catch_up_replica_set_configuration(final_config.clone(), empty.clone())
        .await
        .unwrap();
    let writer = facade.service();
    let writer = std::thread::spawn(move || writer.begin_replicate(Bytes::from_static(b"overlap")));
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let (attempted, attempt) = mpsc::channel();
    let (fenced, fence) = mpsc::channel();
    let (captured, capture) = mpsc::channel();
    let (finished, finish) = mpsc::channel();
    let handoff = facade.clone();
    let executor = tokio::runtime::Handle::current();
    let revoker = std::thread::spawn(move || {
        attempted.send(()).unwrap();
        revoke().unwrap();
        fenced.send(()).unwrap();
        executor.block_on(async move {
            handoff
                .update_catch_up_replica_set_configuration(final_config, empty)
                .await
                .unwrap();
            captured.send(()).unwrap();
            handoff
                .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                .await
                .unwrap();
            finished.send(()).unwrap();
        });
    });
    attempt.recv_timeout(Duration::from_secs(5)).unwrap();
    // Revoke/capture run independently, not after writer.join(). While the
    // actual writer is held, a missing gate on EITHER side permits the whole
    // final barrier to return at zero. The bounded negative wait also gives
    // the announced revoker a chance to expose that forbidden schedule.
    let barrier_finished_before_assignment = match fence.recv_timeout(Duration::from_secs(1)) {
        Ok(()) => {
            capture.recv_timeout(Duration::from_secs(5)).unwrap();
            finish.recv_timeout(Duration::from_secs(5)).unwrap();
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(error) => panic!("revoker disconnected: {error}"),
    };
    release.send(()).unwrap();
    if !barrier_finished_before_assignment {
        fence.recv_timeout(Duration::from_secs(5)).unwrap();
        capture.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let mut write = writer.join().unwrap().unwrap();
    let ack = |peer, applied| {
        registry
            .dispatch(
                &route(peer, 1).envelope(wire::envelope::Body::ReplicationAck(
                    wire::ReplicationAck {
                        replication_received: write.lsn,
                        replication_applied: applied,
                        copy_received: -1,
                        copy_applied: -1,
                        error: 0,
                    },
                )),
            )
            .unwrap();
    };
    // A+C completes normal quorum; successor B is still required by final wait.
    ack(3, write.lsn);
    write.completion.wait().await.unwrap();
    if !barrier_finished_before_assignment {
        assert!(matches!(finish.try_recv(), Err(mpsc::TryRecvError::Empty)));
        ack(2, write.lsn);
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    revoker.join().unwrap();
    assert!(
        !barrier_finished_before_assignment,
        "access-approved LSN escaped final catch-up"
    );
    assert!(facade.service().begin_replicate(Bytes::new()).is_err());
    facade.abort();
    facade.join().await.unwrap();
}
