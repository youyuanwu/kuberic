// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

pub(super) fn same_route(left: &Route, right: &Route) -> bool {
    left.source == right.source
        && left.destination == right.destination
        && left.sender_address == right.sender_address
        && left.remote_address == right.remote_address
}

impl PrimaryHandle {
    pub(in crate::replicator::v1) fn verify_replacement_metadata(
        &self,
        route: &Route,
        identity: &super::super::session::SessionIdentity,
        epoch: Epoch,
    ) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        let mut matching = state.all_bindings().filter(|binding| {
            !binding.idle
                && same_route(&binding.route, route)
                && binding.session.identity() == *identity
        });
        let binding = matching
            .next()
            .ok_or(PrimaryError::InvalidBinding(route.destination.replica))?;
        let manager = root.manager.lock().unwrap();
        if matching.next().is_some()
            || manager
                .build_identity(route, identity.remote_incarnation)?
                .1
                != epoch
            || !manager.contains_root(&binding.session)
        {
            return Err(PrimaryError::InvalidBinding(route.destination.replica));
        }
        manager.check_reported_fault()?;
        Ok(())
    }

    /// A fresh physical transfer, not same-storage recovery. The predecessor
    /// must be the actual configured sender; no completed Build is required.
    pub(in crate::replicator::v1) fn begin_replacement(
        &self,
        predecessor: ReplicaDescriptor,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
    ) -> Result<BuildIdleHandle, PrimaryError> {
        let root = self.root()?;
        let (handle, pending, start, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            root.check_route(&predecessor.route)?;
            root.check_route(&config.route)?;
            let id = config.route.destination.replica;
            let old = state
                .bindings
                .get(&id)
                .ok_or(PrimaryError::UnknownSession(id))?;
            let current = root.manager.lock().unwrap().current_root(id);
            if state.replacement_predecessor.is_some()
                || old.idle
                || state.bindings.values().any(|binding| {
                    matches!(&binding.owner, SessionOwner::Build(build)
                        if !build.result().is_some_and(|end| end.result.is_ok()))
                })
                || predecessor.must_catchup
                || predecessor.route.destination.replica != id
                || !same_route(&old.route, &predecessor.route)
                || old.session.identity().remote_incarnation != predecessor.remote_incarnation
                || !current.is_some_and(|session| session.same_instance(&old.session))
                || same_route(&old.route, &config.route)
                || old.route.destination == config.route.destination
                || old.session.identity().remote_incarnation == config.remote_incarnation
                || config.remote_incarnation <= 0
            {
                return Err(PrimaryError::InvalidBinding(id));
            }
            let old = old.clone();
            let route = config.route.clone();
            let (build, pending, start) = BuildIdle::prepare_replacement(
                &root.manager,
                old.session.clone(),
                config,
                provider,
                callbacks,
                root.clock.clone(),
            )?;
            let handle = build.handle();
            state.replacement_predecessor = Some(old);
            state.bindings.insert(
                id,
                Binding {
                    route,
                    session: handle.session(),
                    owner: SessionOwner::Build(handle.clone()),
                    idle: false,
                },
            );
            state.queued.push(build);
            (handle, pending, start, root.activity(&mut state))
        };
        start.dispatch();
        pending.dispatch(root.clock.now())?;
        Ok(handle)
    }

    /// Verify an exact hot current set without replacing roots or ACK state,
    /// or initialize denied cold metadata before membership is installed.
    pub(in crate::replicator::v1) fn restore_replacement_source(
        &self,
        current: ReplicaConfiguration,
        predecessor: ReplicaDescriptor,
    ) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let (effects, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            let id = predecessor.route.destination.replica;
            root.check_route(&predecessor.route)?;
            if !state.bindings.is_empty() {
                if predecessor.progress.is_some()
                    || predecessor.must_catchup
                    || current.write_quorum <= 1
                    || state.replacement_predecessor.is_some()
                {
                    return Err(PrimaryError::InvalidBinding(id));
                }
                let mut members = Vec::new();
                let mut ids = std::collections::BTreeSet::new();
                let mut found = false;
                for replica in &current.replicas {
                    root.check_route(&replica.route)?;
                    let replica_id = replica.route.destination.replica;
                    let actual = state
                        .bindings
                        .get(&replica_id)
                        .filter(|actual| {
                            !actual.idle
                                && same_route(&actual.route, &replica.route)
                                && actual.session.identity().remote_incarnation
                                    == replica.remote_incarnation
                        })
                        .ok_or(PrimaryError::InvalidBinding(replica_id))?;
                    if !ids.insert(replica_id) || replica.must_catchup {
                        return Err(PrimaryError::InvalidBinding(replica_id));
                    }
                    if replica_id == id {
                        if !same_route(&actual.route, &predecessor.route)
                            || actual.session.identity().remote_incarnation
                                != predecessor.remote_incarnation
                        {
                            return Err(PrimaryError::InvalidBinding(id));
                        }
                        found = true;
                    }
                    members.push(SessionMember {
                        replica_id,
                        session: actual.session.clone(),
                        must_catchup: false,
                    });
                }
                if !found || state.bindings.len() != members.len() {
                    return Err(PrimaryError::InvalidBinding(id));
                }
                root.manager
                    .lock()
                    .unwrap()
                    .verify_replacement_source(&ReplicaSet {
                        write_quorum: current.write_quorum,
                        members,
                    })?;
                return Ok(());
            }
            if predecessor.remote_incarnation != FROZEN_PREVIOUS_INCARNATION
                || predecessor.progress.is_some()
                || predecessor.must_catchup
                || current.write_quorum <= 1
                || current.replicas.len() < current.write_quorum - 1
                || !state.bindings.is_empty()
                || state.replacement_predecessor.is_some()
                || !root
                    .manager
                    .lock()
                    .unwrap()
                    .can_restore_replacement_source()
            {
                return Err(PrimaryError::InvalidBinding(id));
            }
            let mut found = false;
            let mut denied = None;
            let mut members = Vec::new();
            let mut routes = Vec::new();
            let manager = root.manager.lock().unwrap();
            for replica in current.replicas {
                root.check_route(&replica.route)?;
                let frozen = replica.route.destination.replica == id;
                if frozen {
                    if found
                        || !same_route(&replica.route, &predecessor.route)
                        || replica.remote_incarnation != FROZEN_PREVIOUS_INCARNATION
                        || replica.progress.is_some()
                        || replica.must_catchup
                    {
                        return Err(PrimaryError::InvalidBinding(id));
                    }
                    found = true;
                } else if replica.progress.is_none()
                    || replica
                        .progress
                        .is_some_and(|lsn| lsn < 0 || lsn == Lsn::MAX)
                    || replica.remote_incarnation <= 0
                    || replica.must_catchup
                {
                    return Err(PrimaryError::InvalidBinding(
                        replica.route.destination.replica,
                    ));
                }
                let (identity, epoch) =
                    manager.build_identity(&replica.route, replica.remote_incarnation)?;
                let session = ReplicationSession::new(
                    identity,
                    epoch,
                    replica.progress.unwrap_or(-1),
                    root.sender,
                    root.send.clone(),
                    root.clock.now(),
                );
                if !frozen {
                    session.open();
                } else {
                    denied = Some(session.clone());
                }
                members.push(SessionMember {
                    replica_id: replica.route.destination.replica,
                    session,
                    must_catchup: false,
                });
                routes.push(replica.route);
            }
            drop(manager);
            if !found {
                return Err(PrimaryError::InvalidBinding(id));
            }
            let configuration = EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: current.write_quorum,
                    members,
                },
                previous: None,
                catchup_configuration: false,
            };
            let effects = root
                .manager
                .lock()
                .unwrap()
                .restore_replacement_source(configuration, &denied.unwrap())?;
            // Publish only after the manager accepted the complete metadata set.
            let manager = root.manager.lock().unwrap();
            for route in routes {
                let id = route.destination.replica;
                state.bindings.insert(
                    id,
                    Binding {
                        route,
                        session: manager.current_root(id).unwrap(),
                        owner: SessionOwner::Established(Arc::new(CopySender::new(false))),
                        idle: false,
                    },
                );
            }
            drop(manager);
            (effects, root.activity(&mut state))
        };
        effects.dispatch(root.clock.now())?;
        Ok(())
    }

    pub(super) fn install_replacement_configuration(
        root: &Root,
        state: &mut State,
        configuration: EstablishedConfiguration,
        routes: Vec<Route>,
    ) -> Result<ManagerEffects, PrimaryError> {
        let mut used = vec![false; routes.len()];
        for route in &routes {
            root.check_route(route)?;
        }
        for set in std::iter::once(&configuration.current).chain(configuration.previous.iter()) {
            for member in &set.members {
                let mut matches = state.all_bindings().filter(|binding| {
                    binding.route.destination.replica == member.replica_id
                        && binding.session.same_instance(&member.session)
                });
                let binding = matches
                    .next()
                    .ok_or(PrimaryError::InvalidBinding(member.replica_id))?;
                if matches.next().is_some() {
                    return Err(PrimaryError::InvalidBinding(member.replica_id));
                }
                let mut supplied = routes
                    .iter()
                    .enumerate()
                    .filter(|(_, route)| same_route(route, &binding.route));
                let (index, _) = supplied
                    .next()
                    .ok_or(PrimaryError::InvalidBinding(member.replica_id))?;
                if supplied.next().is_some() {
                    return Err(PrimaryError::InvalidBinding(member.replica_id));
                }
                used[index] = true;
            }
        }
        if let Some(index) = used.iter().position(|used| !used) {
            return Err(PrimaryError::InvalidBinding(
                routes[index].destination.replica,
            ));
        }
        let mut manager = root.manager.lock().unwrap();
        let promotions: Vec<_> = state
            .bindings
            .iter()
            .filter_map(|(id, binding)| {
                (!manager
                    .current_root(*id)
                    .is_some_and(|session| session.same_instance(&binding.session)))
                .then_some((*id, binding.session.clone()))
            })
            .collect();
        let effects = manager.configure_established(configuration)?;
        for (id, session) in promotions {
            if manager
                .current_root(id)
                .is_some_and(|current| current.same_instance(&session))
            {
                session.promote_to_active_secondary(root.clock.now());
            }
        }
        if state
            .replacement_predecessor
            .as_ref()
            .is_some_and(|old| !manager.contains_root(&old.session))
        {
            state.replacement_predecessor = None;
        }
        Ok(effects)
    }
}
