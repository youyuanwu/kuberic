#[test]
fn same_epoch_built_peer_admission_uses_fresh_current_sync_not_joint_regression() {
    use kuberic_runtime::protocol::public_operations::PublicV1InstalledConfiguration;
    let mut replicas = vec![
        replica(1, ReplicaRole::Primary),
        replica(2, ReplicaRole::ActiveSecondary),
    ];
    let epoch = Epoch::new(0, 2);
    let retained = ConfigurationDescriptor::new(
        epoch,
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: replicas[0].report.replica.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    replicas[0].report.configuration = Some(PublicV1InstalledConfiguration {
        voting: native_public_configuration(
            &replicas[0].report.replica,
            &replicas,
            &retained,
            None,
            None,
        )
        .unwrap(),
    });
    let desired = ConfigurationDescriptor::new(
        epoch,
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: replicas[0].report.replica.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replicas[1].report.replica.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let mut authority = accepted(&replicas[0]);
    let mut synchronized = false;
    let mut stable = false;
    for step in 0..12 {
        let Some((identity, program)) = native_configuration_step(&replicas, &desired).unwrap()
        else {
            stable = true;
            break;
        };
        if identity == replicas[0].report.replica
            && let PublicOperationProgram::CatchUp { configuration, .. } = &program
        {
            assert_eq!(configuration.current, desired);
            assert!(configuration.previous.is_none());
            synchronized = true;
        }
        let replica = replicas
            .iter()
            .find(|replica| replica.report.replica == identity)
            .unwrap();
        let (_, command) = begin_operation(
            &authority,
            replica,
            program,
            OperationId::new(format!("sync-{step}")),
            "rv",
        )
        .unwrap();
        accept_command(&mut authority, &mut replicas, &command);
    }
    assert!(stable && synchronized);
    assert_eq!(
        replicas[0]
            .report
            .configuration
            .as_ref()
            .unwrap()
            .voting
            .current,
        desired
    );
}
