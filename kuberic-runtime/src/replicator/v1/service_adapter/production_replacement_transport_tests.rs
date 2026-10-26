use super::super::engine_session::replacement_tests::{description, fixture};
use super::*;

fn joint(binding: &Binding, source: bool) -> ReplicaPublication {
    let mut now = if source {
        binding.source.clone()
    } else {
        binding.target.clone()
    };
    let mut old = binding.source.scope.configuration.current.clone();
    old.epoch.configuration_number -= 1;
    old.configuration_id = old.expected_id();
    let mut current = binding.source.scope.configuration.current.clone();
    let predecessor = binding.replacement().unwrap();
    current
        .members
        .iter_mut()
        .find(|member| member.identity == predecessor.identity)
        .unwrap()
        .identity = binding.secondary().identity;
    current.configuration_id = current.expected_id();
    now.scope.configuration.current = current;
    now.scope.configuration.previous = Some(old);
    if source {
        now.scope
            .configuration
            .replicas
            .push(description(&binding.target, ReplicaRole::ActiveSecondary));
    } else {
        now.role = ReplicaRole::ActiveSecondary;
    }
    now
}

#[test]
fn m3_transport_replacement_allows_only_exact_same_epoch_joint_and_current_scope() {
    let binding = fixture();
    for source in [true, false] {
        let origin = if source {
            &binding.source
        } else {
            &binding.target
        };
        assert!(publication_admitted(origin, origin, &binding, source, true));
        let mut admitted = joint(&binding, source);
        assert!(publication_admitted(
            &admitted, origin, &binding, source, false
        ));
        admitted.stage = ReplicaPublicationStage::DiscoveryPending;
        admitted.role = ReplicaRole::None;
        assert!(publication_admitted(
            &admitted, origin, &binding, source, false
        ));
        assert!(!publication_admitted(
            &admitted, origin, &binding, source, true
        ));
        admitted = joint(&binding, source);
        admitted.scope.configuration.previous = None;
        admitted
            .scope
            .configuration
            .replicas
            .retain(|peer| peer.identity != binding.replacement().unwrap().identity);
        assert!(publication_admitted(
            &admitted, origin, &binding, source, true
        ));
    }
    let mut unrelated = joint(&binding, true);
    unrelated.scope.configuration.current.write_quorum = 1;
    unrelated.scope.configuration.current.configuration_id =
        unrelated.scope.configuration.current.expected_id();
    assert!(!replacement_scope(&unrelated, &binding, true));
    let mut wrong_pc = joint(&binding, true);
    wrong_pc
        .scope
        .configuration
        .previous
        .as_mut()
        .unwrap()
        .epoch = binding.epoch();
    assert!(!replacement_scope(&wrong_pc, &binding, true));
    let mut wrong_epoch = joint(&binding, true);
    wrong_epoch.scope.epoch.configuration_number += 1;
    assert!(!replacement_scope(&wrong_epoch, &binding, true));
    let mut crossed = joint(&binding, true);
    crossed
        .scope
        .configuration
        .replicas
        .last_mut()
        .unwrap()
        .process_session_id =
        crate::protocol::types::ProcessSessionId::new("not-the-selected-fresh-process");
    assert!(!replacement_scope(&crossed, &binding, true));
    let mut old_credit = joint(&binding, true);
    old_credit.scope.configuration.replicas[0].current_progress = 100;
    assert!(!replacement_scope(&old_credit, &binding, true));
}
