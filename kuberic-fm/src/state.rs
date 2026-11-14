use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use fube_node::control::wire;
use fube_ra::protocol::*;
use fube_transport::{message::Message, serialization::serde::ObjectArray};
use serde::Serialize;

use crate::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Bootstrap,
    Creating,
    Building,
    Configuring,
    Reconfiguring,
    Restoring,
    Serving,
    Deleting,
    AwaitingNodeShutdown,
    Stopped,
    ControlUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub deployment: String,
    pub phase: Phase,
    pub bootstrapped_nodes: usize,
    pub deleted_nodes: usize,
    pub primary_node_id: String,
    pub service_endpoint: Option<String>,
    pub replication_endpoint: Option<String>,
    pub error: Option<String>,
    pub restoring_node_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outbound {
    pub node: usize,
    pub message: Message,
}

#[derive(Clone, Copy)]
enum Operation {
    Primary,
    Build(usize),
    Configure,
    Delete(usize),
    RepairDelete(usize),
    RepairBuild(usize),
    RepairConfigure,
}

struct Pending {
    operation: Operation,
    outbound: Outbound,
    started: Duration,
    sent: Option<Duration>,
}

struct Completion {
    node: usize,
    action: String,
    body: Vec<u8>,
}

struct Repair {
    node: usize,
    target: Replica,
    deleted: bool,
    built: Option<AddReplicaReply>,
}

pub struct State {
    config: Config,
    primary: usize,
    initial: AddPrimary,
    node_up: Vec<Option<NodeUp>>,
    bootstrapped: [bool; 3],
    primary_reply: Option<AddPrimaryReply>,
    builds: Vec<AddReplicaReply>,
    configuration: Option<DoReconfiguration>,
    configured: Option<ConfigurationReply>,
    promotion: Option<usize>,
    losses: Vec<Option<ReplicaUp>>,
    retired_losses: Vec<(usize, ReplicaUp)>,
    identities: Vec<(i64, i64)>,
    repair: Option<Repair>,
    pending: Option<Pending>,
    completed: Vec<Completion>,
    deleting: [bool; 3],
    deleted: [bool; 3],
    disconnected: [bool; 3],
    stopping_at: Option<Duration>,
    now: Duration,
    error: Option<String>,
}

impl State {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        let primary = config.primary_index()?;
        let service = config.service.native();
        let partition = config.partition_id()?;
        let node = &config.nodes[primary];
        let initial = AddPrimary {
            unit: FailoverUnit {
                id: partition.clone(),
                consistency: ConsistencyUnit {
                    id: partition,
                    ..ConsistencyUnit::default()
                },
                current: Epoch {
                    data_loss: 1,
                    configuration: (1 << 32) + 1,
                },
                target: 3,
                minimum: 2,
                ..FailoverUnit::default()
            },
            replica: Replica {
                node: node.identity.native().map_err(anyhow::Error::msg)?,
                id: node.replica_id,
                instance: node.replica_instance,
                current_role: 4,
                state: 1,
                version: service.version.clone(),
                ..Replica::default()
            },
            service,
        };
        let identities = config
            .nodes
            .iter()
            .map(|node| (node.replica_id, node.replica_instance))
            .collect();
        Ok(Self {
            config,
            primary,
            initial,
            node_up: vec![None; 3],
            bootstrapped: [false; 3],
            primary_reply: None,
            builds: Vec::new(),
            configuration: None,
            configured: None,
            promotion: None,
            losses: vec![None; 3],
            retired_losses: Vec::new(),
            identities,
            repair: None,
            pending: None,
            completed: Vec::new(),
            deleting: [false; 3],
            deleted: [false; 3],
            disconnected: [false; 3],
            stopping_at: None,
            now: Duration::ZERO,
            error: None,
        })
    }

    pub fn phase(&self) -> Phase {
        if self.error.is_some() {
            Phase::ControlUnknown
        } else if self.stopping_at.is_some() {
            if self.disconnected.iter().all(|closed| *closed) {
                Phase::Stopped
            } else if self.deleted.iter().all(|deleted| *deleted) {
                Phase::AwaitingNodeShutdown
            } else {
                Phase::Deleting
            }
        } else if self.promotion.is_some() {
            Phase::Reconfiguring
        } else if self.repair.is_some() {
            Phase::Restoring
        } else if !self.bootstrapped.iter().all(|ready| *ready) {
            Phase::Bootstrap
        } else if self.primary_reply.is_none() {
            Phase::Creating
        } else if self.builds.len() != 2 {
            Phase::Building
        } else if self.configured.is_none() {
            Phase::Configuring
        } else {
            Phase::Serving
        }
    }

    pub fn status(&self) -> Status {
        let primary = self.configured.as_ref().and_then(|configuration| {
            configuration.replicas.0.iter().find(|replica| {
                replica.node
                    == self.config.nodes[self.primary]
                        .identity
                        .native()
                        .expect("validated node identity")
            })
        });
        let serving = matches!(self.phase(), Phase::Serving | Phase::Restoring);
        Status {
            deployment: self.config.deployment.clone(),
            phase: self.phase(),
            bootstrapped_nodes: self.bootstrapped.iter().filter(|ready| **ready).count(),
            deleted_nodes: self.deleted.iter().filter(|deleted| **deleted).count(),
            primary_node_id: self.config.nodes[self.primary].identity.id.clone(),
            service_endpoint: primary
                .filter(|_| serving)
                .map(|replica| replica.service_endpoint.clone()),
            replication_endpoint: primary
                .filter(|_| serving)
                .map(|replica| replica.replication_endpoint.clone()),
            error: self.error.clone(),
            restoring_node_id: self
                .repair
                .as_ref()
                .map(|repair| self.config.nodes[repair.node].identity.id.clone()),
        }
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(error.into());
        }
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.error {
            bail!("{error}");
        }
        Ok(())
    }

    fn clock(&mut self, now: Duration) -> Result<()> {
        ensure!(now >= self.now, "FM clock moved backwards");
        self.now = now;
        Ok(())
    }

    pub fn reconcile(&mut self, now: Duration) -> Result<Vec<Outbound>> {
        let result = self.reconcile_inner(now);
        if let Err(error) = &result {
            self.fail(format!("{error:#}"));
        }
        result
    }

    fn reconcile_inner(&mut self, now: Duration) -> Result<Vec<Outbound>> {
        self.healthy()?;
        self.clock(now)?;
        if let Some(stopping) = self.stopping_at {
            ensure!(
                self.phase() == Phase::Stopped
                    || now - stopping < Duration::from_millis(self.config.shutdown_timeout_ms),
                "shutdown timed out; native deletion or original node disconnects remain unconfirmed"
            );
        }
        if self.phase() == Phase::Bootstrap {
            ensure!(
                now < Duration::from_millis(self.config.operation_timeout_ms),
                "fresh node bootstrap timed out"
            );
            return Ok(Vec::new());
        }
        if self.pending.is_none() {
            self.pending = self.next_operation()?.map(|(operation, outbound)| Pending {
                operation,
                outbound,
                started: now,
                sent: None,
            });
        }
        let Some(pending) = &mut self.pending else {
            return Ok(Vec::new());
        };
        ensure!(
            now - pending.started < Duration::from_millis(self.config.operation_timeout_ms),
            "native {} semantic reply timed out",
            pending.outbound.message.properties()?.action
        );
        if pending
            .sent
            .is_none_or(|sent| now - sent >= Duration::from_millis(self.config.retry_interval_ms))
        {
            pending.sent = Some(now);
            return Ok(vec![pending.outbound.clone()]);
        }
        Ok(Vec::new())
    }

    fn next_operation(&mut self) -> Result<Option<(Operation, Outbound)>> {
        let generation = self.config.generation()?;
        let (operation, node, message) = if self.stopping_at.is_some() {
            let Some(node) = (0..3)
                .filter(|node| *node != self.primary)
                .chain(std::iter::once(self.primary))
                .find(|node| !self.deleted[*node])
            else {
                return Ok(None);
            };
            let (unit, replica) = self.deletion_target(node)?;
            let request = DeleteReplica::new(
                AddInstance {
                    unit,
                    service: self.initial.service.clone(),
                    replica,
                },
                false,
            );
            self.deleting[node] = true;
            (
                Operation::Delete(node),
                node,
                delete_replica_message(&request, &generation)?,
            )
        } else if let Some(candidate) = self.promotion {
            (
                Operation::Configure,
                candidate,
                do_reconfiguration_message(
                    self.configuration
                        .as_ref()
                        .context("missing failover intent")?,
                    &generation,
                )?,
            )
        } else if let Some(repair) = &self.repair {
            if !repair.deleted {
                let (unit, replica) = self.deletion_target(repair.node)?;
                (
                    Operation::RepairDelete(repair.node),
                    repair.node,
                    delete_replica_message(
                        &DeleteReplica::new(
                            AddInstance {
                                unit,
                                replica,
                                service: self.initial.service.clone(),
                            },
                            false,
                        ),
                        &generation,
                    )?,
                )
            } else if repair.built.is_none() {
                (
                    Operation::RepairBuild(repair.node),
                    self.primary,
                    add_replica_message(
                        &AddReplica {
                            unit: self
                                .configured
                                .as_ref()
                                .context("missing repair base")?
                                .unit
                                .clone(),
                            service: self.initial.service.clone(),
                            replica: repair.target.clone(),
                        },
                        &generation,
                    )?,
                )
            } else {
                let completed = self.configured.as_ref().context("missing repair base")?;
                let mut unit = completed.unit.clone();
                unit.previous = unit.current;
                unit.current.configuration = unit
                    .current
                    .configuration
                    .checked_add(1)
                    .context("repair epoch overflow")?;
                ensure!(
                    unit.previous.configuration & !0xffff_ffff
                        == unit.current.configuration & !0xffff_ffff,
                    "same-primary repair epoch exhausted"
                );
                let mut replicas = completed.replicas.clone();
                for replica in &mut replicas.0 {
                    replica.previous_role = replica.current_role;
                }
                let mut built = repair
                    .built
                    .as_ref()
                    .context("missing repair build")?
                    .replica
                    .clone();
                built.previous_role = 2;
                built.current_role = 3;
                replicas.0.push(built);
                let request = DoReconfiguration {
                    configuration: Configuration {
                        unit,
                        replicas,
                        service: self.initial.service.clone(),
                    },
                    phase0_duration: i64::MAX,
                };
                let message = do_reconfiguration_message(&request, &generation)?;
                self.configuration = Some(request);
                (Operation::RepairConfigure, self.primary, message)
            }
        } else if self.primary_reply.is_none() {
            (
                Operation::Primary,
                self.primary,
                add_primary_message(&self.initial, &generation)?,
            )
        } else if self.builds.len() < 2 {
            let node = (0..3)
                .filter(|node| *node != self.primary)
                .nth(self.builds.len())
                .context("missing build destination")?;
            let placement = &self.config.nodes[node];
            let request = AddReplica {
                replica: Replica {
                    node: placement.identity.native().map_err(anyhow::Error::msg)?,
                    id: placement.replica_id,
                    instance: placement.replica_instance,
                    current_role: 2,
                    state: 1,
                    version: self.initial.service.version.clone(),
                    ..Replica::default()
                },
                ..self.initial.clone()
            };
            (
                Operation::Build(node),
                self.primary,
                add_replica_message(&request, &generation)?,
            )
        } else if self.configured.is_none() {
            let mut replicas = vec![self
                .primary_reply
                .as_ref()
                .context("missing primary")?
                .replica
                .clone()];
            replicas[0].previous_role = 4;
            for build in &self.builds {
                let mut replica = build.replica.clone();
                replica.previous_role = 2;
                replica.current_role = 3;
                replicas.push(replica);
            }
            let mut unit = self.initial.unit.clone();
            unit.previous = unit.current;
            unit.current.configuration += 1;
            let request = DoReconfiguration {
                configuration: Configuration {
                    unit,
                    replicas: ObjectArray(replicas),
                    service: self.initial.service.clone(),
                },
                phase0_duration: i64::MAX,
            };
            let message = do_reconfiguration_message(&request, &generation)?;
            self.configuration = Some(request);
            (Operation::Configure, self.primary, message)
        } else {
            return Ok(None);
        };
        Ok(Some((operation, Outbound { node, message })))
    }

    pub fn observe(&mut self, node: usize, message: &Message) -> Result<Option<Message>> {
        let result = self.observe_inner(node, message);
        if let Err(error) = &result {
            self.fail(format!("{error:#}"));
        }
        result
    }

    fn observe_inner(&mut self, node: usize, message: &Message) -> Result<Option<Message>> {
        self.healthy()?;
        ensure!(
            node < 3 && !self.disconnected[node],
            "foreign or disconnected source node"
        );
        wire::predecode(message).map_err(anyhow::Error::msg)?;
        let generation = self.config.generation()?;
        let mut generations = message
            .headers
            .iter()
            .filter(|header| header.id == GENERATION_HEADER);
        if let Some(header) = generations.next() {
            ensure!(
                generations.next().is_none()
                    && wire::decode::<GenerationHeader>(&header.value)
                        .map_err(anyhow::Error::msg)?
                        == generation,
                "foreign, stale or duplicate FM generation"
            );
        }
        let properties = message.properties()?;
        ensure!(
            properties.actor == FM_ACTOR,
            "foreign native FM message actor"
        );
        let action = properties.action.as_str();
        ensure!(
            self.bootstrapped[node] || matches!(action, "NodeUp" | "ReplicaUp"),
            "node report arrived before fresh bootstrap completed"
        );
        let response = match action {
            "NodeUp" => {
                let body = decode_node_up(message)?;
                body.validate_fresh_stateless()?;
                let placement = &self.config.nodes[node];
                ensure!(
                    body.node.address == placement.address.ip().to_string()
                        && body.node.cluster_connection_port == u32::from(placement.address.port())
                        && body.node.version == FabricVersionInstance::native_default()
                        && !body.node.name.is_empty()
                        && body.node.node_type == "fube-node"
                        && self.node_up[node]
                            .as_ref()
                            .is_none_or(|original| original == &body),
                    "NodeUp changed the original node address, version or description"
                );
                let ack = NodeUpAck::fresh_activated(body.node.version.clone(), 0)?;
                self.node_up[node] = Some(body);
                Some(node_up_ack_message(&ack, &generation)?)
            }
            "ReplicaUp" => {
                let (body, owner) = decode_replica_up(message)?;
                ensure!(
                    owner == Owner::Fm && !body.from_fmm,
                    "foreign replica inventory owner"
                );
                ensure!(
                    self.node_up[node].is_some(),
                    "ReplicaUp arrived before NodeUp"
                );
                if self
                    .retired_losses
                    .iter()
                    .any(|(source, original)| *source == node && original == &body)
                {
                    return Ok(Some(replica_up_reply_message(&body, &generation)?));
                }
                if body.replicas.0.is_empty() && body.dropped.0.is_empty() && body.last {
                    self.bootstrapped[node] = true;
                } else {
                    ensure!(
                        !body.last && body.replicas.0.is_empty() && body.dropped.0.len() == 1,
                        "unsupported replica inventory"
                    );
                    let report = &body.dropped.0[0];
                    ensure!(
                        report.service == self.initial.service
                            && report.intermediate == Epoch::default()
                            && !report.report_from_primary
                            && !report.local_deleted
                            && report.replicas.0.len() == 1
                            && report.replicas.0[0].intermediate_role == 1
                            && report.replicas.0[0].description == self.dropped(node)?,
                        "dropped inventory does not match the exact original replica"
                    );
                    if self.deleting[node]
                        || self
                            .repair
                            .as_ref()
                            .is_some_and(|repair| repair.node == node)
                    {
                        ensure!(
                            report.unit == self.deletion_target(node)?.0,
                            "dropped inventory changed the retained deletion epoch"
                        );
                    } else if let Some(original) = &self.losses[node] {
                        ensure!(
                            original == &body,
                            "changed original application-loss report"
                        );
                    } else {
                        self.app_host_lost(node, &body)?;
                    }
                }
                Some(replica_up_reply_message(&body, &generation)?)
            }
            "ServiceTypeNotification" => {
                let body = decode_service_type_notification(message)?;
                ensure!(
                    !body.infos.0.is_empty()
                        && body.infos.0.iter().all(|info| {
                            info.versioned.id == self.initial.service.service_type
                                && info.versioned.version == self.initial.service.version
                        }),
                    "foreign service-type notification"
                );
                Some(service_type_notification_reply_message(&body, &generation)?)
            }
            "ServiceTypeEnabled" | "ServiceTypeDisabled" => {
                let body = if action == "ServiceTypeEnabled" {
                    decode_service_type_enabled(message)?
                } else {
                    decode_service_type_disabled(message)?
                };
                ensure!(
                    !body.types.0.is_empty()
                        && body
                            .types
                            .0
                            .iter()
                            .all(|kind| kind == &self.initial.service.service_type),
                    "foreign service-type status"
                );
                let reply = ServiceTypeUpdateReply {
                    sequence: body.sequence,
                };
                Some(if action == "ServiceTypeEnabled" {
                    service_type_enabled_reply_message(&reply, &generation)?
                } else {
                    service_type_disabled_reply_message(&reply, &generation)?
                })
            }
            "ReplicaEndpointUpdated" => {
                let (body, owner) = decode_replica_endpoint_updated(message)?;
                ensure!(owner == Owner::Fm, "foreign endpoint-report owner");
                self.validate_replica(node, &body.replica)?;
                ensure!(
                    body.service == self.initial.service
                        && (body.unit == self.initial.unit
                            || self
                                .configuration
                                .as_ref()
                                .is_some_and(|request| request.configuration.unit == body.unit)
                            || self
                                .configured
                                .as_ref()
                                .is_some_and(|reply| reply.unit == body.unit)),
                    "endpoint report does not match the original service/epoch"
                );
                Some(replica_endpoint_updated_reply_message(
                    &AddInstanceReply {
                        unit: body.unit,
                        replica: body.replica,
                        error: 0,
                    },
                    &generation,
                )?)
            }
            "AddPrimaryReply"
            | "AddReplicaReply"
            | "DoReconfigurationReply"
            | "DeleteReplicaReply" => {
                if self.completed.iter().any(|completed| {
                    completed.node == node
                        && completed.action == action
                        && completed.body == message.body
                }) {
                    return Ok(None);
                }
                self.complete(node, action, message)?;
                self.completed.push(Completion {
                    node,
                    action: action.to_owned(),
                    body: message.body.clone(),
                });
                self.pending = None;
                None
            }
            _ => bail!("unsupported external-FM action: {action}"),
        };
        Ok(response)
    }

    fn complete(&mut self, node: usize, action: &str, message: &Message) -> Result<()> {
        let pending = self
            .pending
            .as_ref()
            .context("unsolicited native completion")?;
        ensure!(
            node == pending.outbound.node,
            "completion came from the wrong original channel"
        );
        match (pending.operation, action) {
            (Operation::RepairBuild(target), "AddReplicaReply") => {
                let (reply, owner) = decode_add_replica_reply(message)?;
                let repair = self.repair.as_mut().context("missing retained repair")?;
                ensure!(
                    owner == Owner::Fm
                        && reply.error == 0
                        && repair.node == target
                        && reply.unit
                            == self
                                .configured
                                .as_ref()
                                .context("missing repair base")?
                                .unit
                        && reply.replica.node == repair.target.node
                        && reply.replica.id == repair.target.id
                        && reply.replica.instance == repair.target.instance
                        && reply.replica.version == repair.target.version
                        && reply.replica.previous_role == 1
                        && reply.replica.current_role == 2
                        && reply.replica.state == 2
                        && reply.replica.up
                        && reply.replica.service_endpoint.is_empty()
                        && !reply.replica.replication_endpoint.is_empty(),
                    "repair build did not complete the exact fresh Ready Idle"
                );
                repair.built = Some(reply);
            }
            (Operation::Primary, "AddPrimaryReply") | (Operation::Build(_), "AddReplicaReply") => {
                let (reply, owner) = if action == "AddPrimaryReply" {
                    decode_add_primary_reply(message)?
                } else {
                    decode_add_replica_reply(message)?
                };
                ensure!(
                    owner == Owner::Fm && reply.error == 0,
                    "{action} failed with native error {}",
                    reply.error
                );
                ensure!(
                    reply.unit == self.initial.unit,
                    "completion changed the original partition/epoch"
                );
                let target = match pending.operation {
                    Operation::Build(target) => target,
                    _ => self.primary,
                };
                self.validate_replica(target, &reply.replica)?;
                ensure!(
                    reply.replica.current_role == if target == self.primary { 4 } else { 2 }
                        && reply.replica.state == 2
                        && reply.replica.up
                        && !reply.replica.replication_endpoint.is_empty()
                        && if target == self.primary {
                            !reply.replica.service_endpoint.is_empty()
                        } else {
                            reply.replica.service_endpoint.is_empty()
                        },
                    "completion is not the exact Ready primary or built Idle"
                );
                if target == self.primary {
                    self.primary_reply = Some(reply);
                } else {
                    self.builds.push(reply);
                }
            }
            (Operation::Configure | Operation::RepairConfigure, "DoReconfigurationReply") => {
                let (reply, owner) = decode_do_reconfiguration_reply(message)?;
                ensure!(
                    owner == Owner::Fm && reply.error == 0,
                    "DoReconfigurationReply failed with native error {}",
                    reply.error
                );
                let request = &self
                    .configuration
                    .as_ref()
                    .context("missing frozen configuration")?
                    .configuration;
                let mut expected = request.clone();
                expected.unit.previous = Epoch::default();
                for replica in &mut expected.replicas.0 {
                    replica.previous_role = 1;
                }
                if let Some(candidate) = self.promotion {
                    expected
                        .replicas
                        .0
                        .retain(|replica| replica.current_role >= 3);
                    ensure!(
                        reply.unit == expected.unit
                            && reply.replicas.0.len() == 2
                            && expected.replicas.0.iter().all(|replica| {
                                reply.replicas.0.iter().filter(|actual| {
                                    actual.node == replica.node
                                        && actual.id == replica.id
                                        && actual.instance == replica.instance
                                        && actual.version == replica.version
                                        && actual.previous_role == 1
                                        && actual.current_role == replica.current_role
                                        && actual.state == 2
                                        && actual.up
                                        && !actual.replication_endpoint.is_empty()
                                        && if actual.current_role == 4 {
                                            !actual.service_endpoint.is_empty()
                                        } else {
                                            actual.service_endpoint.is_empty()
                                        }
                                }).count() == 1
                            }),
                        "failover completion differs from the retained epoch, quorum or original membership"
                    );
                    self.primary = candidate;
                    self.promotion = None;
                    let lost = self
                        .losses
                        .iter()
                        .position(Option::is_some)
                        .context("missing failed replica")?;
                    let next = self
                        .identities
                        .iter()
                        .flat_map(|(id, instance)| [*id, *instance])
                        .max()
                        .context("missing replica identities")?
                        .checked_add(1)
                        .context("repair identity overflow")?;
                    self.repair = Some(Repair {
                        node: lost,
                        target: Replica {
                            node: self.config.nodes[lost]
                                .identity
                                .native()
                                .map_err(anyhow::Error::msg)?,
                            id: next,
                            instance: next,
                            previous_role: 1,
                            current_role: 2,
                            state: 1,
                            up: true,
                            first_lsn: -1,
                            last_lsn: -1,
                            version: self.initial.service.version.clone(),
                            ..Replica::default()
                        },
                        deleted: false,
                        built: None,
                    });
                } else {
                    ensure!(
                        reply.unit == expected.unit
                            && reply.replicas.0.len() == 3
                            && expected.replicas.0.iter().all(|replica| {
                                reply
                                    .replicas
                                    .0
                                    .iter()
                                    .filter(|actual| *actual == replica)
                                    .count()
                                    == 1
                            }),
                        "configuration completion differs from the exact frozen native membership"
                    );
                }
                if matches!(pending.operation, Operation::RepairConfigure) {
                    let repair = self.repair.take().context("missing retained repair")?;
                    self.identities[repair.node] = (repair.target.id, repair.target.instance);
                    self.retired_losses.push((
                        repair.node,
                        self.losses[repair.node]
                            .take()
                            .context("missing predecessor loss")?,
                    ));
                    self.deleting[repair.node] = false;
                }
                self.configured = Some(reply);
            }
            (Operation::Delete(target) | Operation::RepairDelete(target), "DeleteReplicaReply") => {
                let (reply, owner) = decode_delete_replica_reply(message)?;
                ensure!(
                    owner == Owner::Fm && reply.error == 0,
                    "DeleteReplicaReply failed with native error {}",
                    reply.error
                );
                ensure!(
                    reply.unit == self.deletion_target(target)?.0
                        && reply.replica == self.dropped(target)?,
                    "deletion completion differs from the exact original replica"
                );
                if matches!(pending.operation, Operation::RepairDelete(_)) {
                    self.repair
                        .as_mut()
                        .context("missing retained repair")?
                        .deleted = true;
                } else {
                    self.deleted[target] = true;
                }
            }
            _ => bail!("unexpected {action} for the retained native operation"),
        }
        Ok(())
    }

    fn validate_replica(&self, node: usize, replica: &Replica) -> Result<()> {
        let placement = &self.config.nodes[node];
        let identity = (replica.id, replica.instance);
        let current = self.identities[node];
        let repairing = self.repair.as_ref().is_some_and(|repair| {
            repair.node == node && identity == (repair.target.id, repair.target.instance)
        });
        ensure!(
            replica.node == placement.identity.native().map_err(anyhow::Error::msg)?
                && (identity == current || repairing)
                && replica.version == self.initial.service.version,
            "foreign node, replica instance or package version"
        );
        Ok(())
    }

    fn app_host_lost(&mut self, node: usize, report: &ReplicaUp) -> Result<()> {
        ensure!(
            self.phase() == Phase::Serving && self.pending.is_none() && node == self.primary,
            "only verified Serving primary application loss admits failover; bootstrap, secondary loss and interrupted reconfiguration are unsupported"
        );
        let completed = self
            .configured
            .as_ref()
            .context("missing committed history")?;
        ensure!(
            report.dropped.0[0].unit == completed.unit
                && completed.unit.previous == Epoch::default()
                && completed.replicas.0.len() == 3
                && completed.replicas.0.iter().all(|replica| {
                    replica.up && replica.state == 2 && replica.current_role >= 3
                })
                && self.losses.iter().all(Option::is_none),
            "application loss has stale history or insufficient original quorum"
        );
        ensure!(
            self.retired_losses.len() < 32,
            "owned restoration history budget exhausted"
        );
        let candidate = (0..3)
            .find(|index| *index != node)
            .context("no original failover candidate")?;
        let mut configuration = Configuration {
            unit: completed.unit.clone(),
            replicas: completed.replicas.clone(),
            service: self.initial.service.clone(),
        };
        configuration.unit.previous = configuration.unit.current;
        configuration.unit.current.configuration = configuration
            .unit
            .current
            .configuration
            .checked_add(0x1_0000_0001)
            .context("failover configuration epoch overflow")?;
        for replica in &mut configuration.replicas.0 {
            replica.previous_role = replica.current_role;
            if replica.node
                == self.config.nodes[node]
                    .identity
                    .native()
                    .map_err(anyhow::Error::msg)?
            {
                replica.current_role = 1;
                replica.up = false;
                replica.state = 3;
            } else if replica.node
                == self.config.nodes[candidate]
                    .identity
                    .native()
                    .map_err(anyhow::Error::msg)?
            {
                replica.current_role = 4;
            }
            // GetLSN and PC/CC quorum selection belong to native RA, not FM guesses.
            replica.first_lsn = -1;
            replica.last_lsn = -1;
        }
        self.configuration = Some(DoReconfiguration {
            configuration,
            phase0_duration: i64::MAX,
        });
        self.losses[node] = Some(report.clone());
        self.promotion = Some(candidate);
        Ok(())
    }

    fn deletion_target(&self, node: usize) -> Result<(FailoverUnit, Replica)> {
        if let Some(report) = &self.losses[node] {
            return Ok((report.dropped.0[0].unit.clone(), self.dropped(node)?));
        }
        let completed = self
            .configured
            .as_ref()
            .context("missing completed configuration")?;
        let replica = completed
            .replicas
            .0
            .iter()
            .find(|replica| {
                replica.node
                    == self.config.nodes[node]
                        .identity
                        .native()
                        .expect("validated node identity")
            })
            .context("completed configuration omitted an original live node")?;
        Ok((completed.unit.clone(), replica.clone()))
    }

    fn dropped(&self, node: usize) -> Result<Replica> {
        let placement = &self.config.nodes[node];
        Ok(Replica {
            node: placement.identity.native().map_err(anyhow::Error::msg)?,
            id: self.identities[node].0,
            instance: self.identities[node].1,
            up: false,
            state: 3,
            ..Replica::default()
        })
    }

    pub fn request_shutdown(&mut self, now: Duration) -> Result<()> {
        let result = (|| {
            self.healthy()?;
            self.clock(now)?;
            ensure!(
                self.phase() == Phase::Serving,
                "shutdown before initial create completes requires external node teardown"
            );
            self.stopping_at = Some(now);
            Ok(())
        })();
        if let Err(error) = &result {
            self.fail(format!("{error:#}"));
        }
        result
    }

    pub fn disconnected(&mut self, node: usize) -> Result<()> {
        self.healthy()?;
        ensure!(node < 3, "foreign disconnected node");
        if !self.deleted[node] {
            self.fail(format!(
                "original node {node} channel lost; reconnect and promotion are unsupported"
            ));
            self.healthy()?;
        }
        self.disconnected[node] = true;
        Ok(())
    }
}
