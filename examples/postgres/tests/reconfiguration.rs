use postgres_replicated::testing::{PgGroup, run_pg_test};

#[test_log::test]
fn sequential_scale_up_and_secondary_removal_preserve_complete_sql_contents() {
    run_pg_test(|| async {
        let mut group = PgGroup::singleton().await;
        group.add(2).await;
        group.write("two replicas").await;
        group.add(3).await;
        group.write("three replicas").await;
        group.assert_contents().await;
        let removed = group.session(3, false).await;
        let removed_admin = group.session(3, true).await;
        let probe = group.next_probe();
        group.remove(3).await;
        removed.rejected(probe).await;
        removed.disconnected().await;
        removed_admin.disconnected().await;
        group.write("reduced").await;
        group.assert_contents().await;
        let last_secondary = group.session(2, false).await;
        let singleton_probe = group.next_probe();
        group.remove(2).await;
        last_secondary.rejected(singleton_probe).await;
        group.write("reduced to singleton").await;
        group.assert_contents().await;
        group.restart_peer(1).await;
        group.write("singleton restarted after reductions").await;
        group.assert_contents().await;
        group.shutdown().await;
    });
}

#[test_log::test]
fn quorum_loss_keeps_secondary_reads_and_fresh_sessions_restore_writes() {
    run_pg_test(|| async {
        let mut group = PgGroup::singleton().await;
        group.add(2).await;
        group.add(3).await;
        group.write("last acknowledged before quorum loss").await;
        group.assert_contents().await;
        let ordinary = group.session(1, false).await;
        let admin = group.session(1, true).await;
        let one = group.next_probe();
        let two = group.next_probe();
        group.fence_quorum_loss().await;
        ordinary.rejected(one).await;
        admin.rejected(two).await;
        for id in [2, 3] {
            let reader = group.session(id, false).await;
            assert_eq!(
                reader
                    .client()
                    .query("SELECT * FROM phase6_rows", &[])
                    .await
                    .unwrap()
                    .len(),
                group.expected.len()
            );
            reader.rejected(group.next_probe()).await;
        }
        group.restart_peer(2).await;
        group.restart_peer(3).await;
        group.write("fresh sessions restored").await;
        group.assert_contents().await;
        group.shutdown().await;
    });
}

#[test_log::test]
fn application_restart_and_agent_metadata_reopen_preserve_committed_contents() {
    run_pg_test(|| async {
        let mut group = PgGroup::singleton().await;
        group.add(2).await;
        group.add(3).await;
        let session = group.pod(1).session.clone();
        let sql = group.session(1, false).await;
        let probe = group.next_probe();
        group.pod(1).restart_application().await.unwrap();
        sql.rejected(probe).await;
        assert_eq!(group.pod(1).session, session);
        group.refresh_configuration().await;
        group.reopen_agent_metadata(1).await;
        group.write("metadata connection reopened").await;
        let retained = group.session(1, false).await;
        let agent_probe = group.next_probe();
        group.restart_peer(1).await;
        retained.rejected(agent_probe).await;
        assert_ne!(group.pod(1).session, session);
        group.write("whole agent reopened").await;
        group.assert_contents().await;
        group.shutdown().await;
    });
}

#[test_log::test]
fn scaling_reopens_application_and_agent_state_at_each_durable_admission_boundary() {
    run_pg_test(|| async {
        use postgres_replicated::testing::{AdmissionCut, RestartPart};
        for cut in [
            AdmissionCut::Built,
            AdmissionCut::PreviousCurrent,
            AdmissionCut::CurrentOnly,
        ] {
            for part in [
                RestartPart::Application,
                RestartPart::AgentMetadata,
                RestartPart::AgentHost,
            ] {
                tracing::info!(?cut, ?part, "restarting scale-up at a durable boundary");
                let mut group = PgGroup::singleton().await;
                group.add_with_restart(2, Some((cut, part))).await;
                group.write("after admission recovery").await;
                group.assert_contents().await;
                group.shutdown().await;
            }
        }
    });
}
