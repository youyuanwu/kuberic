//! A repair is subordinate to one selected reformation, never a copy receipt.
use super::*;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepairRoot {
    path: PathBuf,
    device: u64,
    inode: u64,
    control: String,
}

impl RepairRoot {
    fn read(path: PathBuf) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(&path).map_err(application_error)?;
        if !metadata.is_dir() || path.canonicalize().map_err(application_error)? != path {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let control_path = path.join("global/pg_control");
        let control = std::fs::symlink_metadata(&control_path).map_err(application_error)?;
        if !control.is_file() || control.nlink() != 1 || control.len() != 8192 {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            control: hex(&Sha256::digest(
                std::fs::read(control_path).map_err(application_error)?,
            )),
        })
    }

    fn matches_at(&self, path: &Path, control: bool) -> Result<bool> {
        if !present(path)? {
            return Ok(false);
        }
        if !control {
            let actual = std::fs::symlink_metadata(path).map_err(application_error)?;
            return Ok(actual.is_dir()
                && actual.dev() == self.device
                && actual.ino() == self.inode
                && path.canonicalize().map_err(application_error)? == path);
        }
        let actual = Self::read(path.to_owned())?;
        Ok(actual.device == self.device
            && actual.inode == self.inode
            && (!control || actual.control == self.control))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RepairStage {
    Selected,
    Cloning,
    Cloned,
    Rewinding,
    Rewound,
    InstallSelected,
    OriginalRetained,
    Installed,
    Following,
    Replayed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PgNativeRepair {
    quiescence: crate::durable::PgNativeQuiescence,
    nonce: [u8; 16],
    original: RepairRoot,
    candidate_path: PathBuf,
    archive_path: PathBuf,
    candidate: Option<RepairRoot>,
    clone_digest: Option<String>,
    stage: RepairStage,
    failure: Option<String>,
}

impl PgNativeRepair {
    pub(super) fn retained_prefix(&self) -> (&PgLineage, i64, i64) {
        (
            &self.quiescence.lineage,
            self.quiescence.required_certified_lsn,
            self.quiescence.retained_flush_lsn,
        )
    }

    pub(super) fn replayed(&self) -> bool {
        self.stage == RepairStage::Replayed
    }

    pub(super) fn continues(&self, old: &Self) -> bool {
        let mut expected = old.clone();
        expected.stage = self.stage;
        expected.failure = self.failure.clone();
        expected.candidate = self.candidate.clone();
        expected.clone_digest = self.clone_digest.clone();
        expected == *self
            && self.stage >= old.stage
            && old
                .failure
                .as_ref()
                .is_none_or(|failure| self.failure.as_ref() == Some(failure))
            && old
                .clone_digest
                .as_ref()
                .is_none_or(|digest| self.clone_digest.as_ref() == Some(digest))
            && old.candidate.as_ref().is_none_or(|before| {
                self.candidate.as_ref().is_some_and(|after| {
                    before.path == after.path
                        && before.device == after.device
                        && before.inode == after.inode
                        && (before.control == after.control
                            || (old.stage == RepairStage::Rewinding
                                && self.stage == RepairStage::Rewound))
                })
            })
    }

    pub(super) fn is_installed_successor_of(&self, after: &Self) -> bool {
        let mut expected = self.clone();
        expected.stage = RepairStage::Replayed;
        self.stage == RepairStage::Following && self.failure.is_none() && expected == *after
    }

    pub(super) fn validate(&self, id: &PgSessionId) -> std::result::Result<(), String> {
        id.validate().map_err(|error| error.to_string())?;
        let (candidate, archive) =
            paths(&self.original.path, &self.nonce).map_err(|error| error.to_string())?;
        if !quiescence_scope(&self.quiescence, id)
            || self.nonce == [0; 16]
            || self.candidate_path != candidate
            || self.archive_path != archive
            || !self.original.path.is_absolute()
            || self.original.inode == 0
            || self.original.control.len() != 64
            || (self.stage >= RepairStage::Cloned
                && (self.candidate.as_ref().is_none_or(|root| {
                    root.path != candidate
                        || root.device != self.original.device
                        || root.inode == self.original.inode
                        || root.control.len() != 64
                }) || self
                    .clone_digest
                    .as_ref()
                    .is_none_or(|digest| digest.len() != 64)))
        {
            return Err("invalid exact native repair ownership".into());
        }
        Ok(())
    }
}

fn repair_epoch_continues(fenced: Epoch, selected: Epoch) -> bool {
    fenced.data_loss_number == selected.data_loss_number
        && (selected == fenced
            || fenced.configuration_number.checked_add(1) == Some(selected.configuration_number))
}

fn quiescence_scope(quiescence: &crate::durable::PgNativeQuiescence, id: &PgSessionId) -> bool {
    let Some(retained) = &quiescence.configuration else {
        return false;
    };
    let selected = &id.source.scope.configuration;
    let local = &id.target.scope.storage.replica;
    let remote = &id.source;
    let fenced = quiescence.selected_epoch;
    let selected_epoch = id.source.scope.epoch;
    let continuing_epoch = repair_epoch_continues(fenced, selected_epoch);
    quiescence.process_session_id == id.target.scope.process_session_id
        && continuing_epoch
        && quiescence.predecessor_epoch <= quiescence.selected_epoch
        && retained.current.primary_id == local.replica_id
        && retained.current.write_quorum == selected.current.write_quorum
        && retained
            .current
            .members
            .iter()
            .map(|member| &member.identity)
            .collect::<BTreeSet<_>>()
            == selected
                .current
                .members
                .iter()
                .map(|member| &member.identity)
                .collect::<BTreeSet<_>>()
        && match &retained.previous {
            None => selected.previous.as_ref() == Some(&retained.current),
            Some(previous) => {
                selected.previous.as_ref() == Some(previous) && retained.current.epoch == fenced
            }
        }
        && retained
            .current_participants
            .iter()
            .chain(&retained.previous_participants)
            .any(|peer| {
                peer.identity == remote.scope.storage.replica
                    && peer.storage_incarnation.as_ref() == Some(&remote.scope.storage)
                    && peer.process_session_id == remote.scope.process_session_id
                    && peer.replication_address == remote.replication_address
                    && peer.role == ReplicaRole::ActiveSecondary
            })
}

#[cfg(test)]
#[test]
fn m3_repair_epoch_keeps_historical_fence_and_only_one_same_history_continuation() {
    let fenced = Epoch::new(4, 17);
    assert!(repair_epoch_continues(fenced, fenced));
    assert!(repair_epoch_continues(fenced, Epoch::new(4, 18)));
    assert!(!repair_epoch_continues(fenced, Epoch::new(4, 16)));
    assert!(!repair_epoch_continues(fenced, Epoch::new(4, 19)));
    assert!(!repair_epoch_continues(fenced, Epoch::new(5, 18)));
    assert!(!repair_epoch_continues(fenced, Epoch::new(3, 18)));
    assert!(!repair_epoch_continues(
        Epoch::new(4, i64::MAX),
        Epoch::new(4, 0)
    ));
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairOffer {
    pub(in crate::adapter::public_v1) binding: PeerBinding,
    source_lineage: PgLineage,
    retained_lineage: PgLineage,
    certified_lsn: i64,
    source_boundary: i64,
    candidate_nonce: [u8; 16],
}

#[derive(Clone, Copy)]
struct RepairExecution<'a> {
    binding: &'a PeerBinding,
    generation: u64,
    cancellation: &'a CancellationToken,
    physical: &'a NativeCredential,
    lineage: &'a PgLineage,
    boundary: i64,
}

impl RepairOffer {
    fn id(&self) -> Result<&PgSessionId> {
        self.binding
            .session
            .as_deref()
            .filter(|id| id.mode == PgSessionMode::Reformation)
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    fn validate(&self) -> Result<()> {
        self.id()?.validate()?;
        if self.candidate_nonce == [0; 16]
            || !self.retained_lineage.prefix_is_contained_by(
                &self.source_lineage,
                self.certified_lsn,
                self.source_boundary,
            )
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
}

fn paths(original: &Path, nonce: &[u8; 16]) -> Result<(PathBuf, PathBuf)> {
    let name = original
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((
        original.with_file_name(format!("{name}-{nonce}")),
        original.with_file_name(format!("{name}-retained-{nonce}")),
    ))
}

fn present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(application_error(error)),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().ok_or(RuntimeError::AuthorityNotAdmitted)?)
        .and_then(|file| file.sync_all())
        .map_err(application_error)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn tree_digest(root: &Path, sync: bool) -> Result<String> {
    super::super::super::public_transport::check_rewind_candidate_layout(root.to_owned())
        .map_err(application_error)?;
    fn visit(root: &Path, path: &Path, digest: &mut Sha256, sync: bool) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path).map_err(application_error)?;
        let relative = path.strip_prefix(root).map_err(application_error)?;
        let relative = relative
            .to_str()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update(metadata.permissions().mode().to_le_bytes());
        if metadata.is_file() {
            digest.update(metadata.len().to_le_bytes());
            let mut file = File::open(path).map_err(application_error)?;
            let mut buffer = [0; 65536];
            loop {
                let length = file.read(&mut buffer).map_err(application_error)?;
                if length == 0 {
                    break;
                }
                digest.update(&buffer[..length]);
            }
            if sync {
                file.sync_all().map_err(application_error)?;
            }
        } else if metadata.is_dir() {
            let mut entries = std::fs::read_dir(path)
                .map_err(application_error)?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(application_error)?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                visit(root, &entry.path(), digest, sync)?;
            }
            if sync {
                File::open(path)
                    .and_then(|file| file.sync_all())
                    .map_err(application_error)?;
            }
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest, sync)?;
    Ok(hex(&digest.finalize()))
}

#[cfg(all(test, feature = "testing"))]
pub(in crate::adapter::public_v1) fn retained_tree_digest(root: &Path) -> Result<String> {
    tree_digest(root, false)
}

impl PgReplicator {
    // Publications/lineage are checked around effects and before issuance.
    // Each frame checks the two actual native lease owners, including the
    // physical gateway's liveness, without rereading the entire host journal.
    async fn pg_repair_local_lease(&self, offer: &RepairOffer) -> Result<()> {
        let id = offer.id()?;
        id.validate()?;
        let (generation, _) = self.public_owner().sessions.lease(id)?;
        let context = self.public_owner().fence.context()?;
        let source = context.identity == id.source.scope.storage.replica;
        let selected = if source { &id.source } else { &id.target };
        let role = if source {
            ReplicaRole::Primary
        } else {
            ReplicaRole::ActiveSecondary
        };
        if context.identity != selected.scope.storage.replica
            || context.epoch != selected.scope.epoch
            || context.role != role
            || self.public_owner().fence.local.process_session_id
                != selected.scope.process_session_id
            || self.public_owner().fence.local.replication_address != selected.replication_address
            || offer.binding.primary.replica != id.source.scope.storage.replica
            || offer.binding.secondary.replica != id.target.scope.storage.replica
            || offer.binding.primary.process_session_id != id.source.scope.process_session_id
            || offer.binding.secondary.process_session_id != id.target.scope.process_session_id
            || offer.binding.primary.replication_address != id.source.replication_address
            || offer.binding.secondary.replication_address != id.target.replication_address
            || offer.binding.primary.role != ReplicaRole::Primary
            || offer.binding.secondary.role != ReplicaRole::ActiveSecondary
            || offer.binding.epoch != selected.scope.epoch
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let host = self
            .public_owner()
            .host
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.public_attempt_live(&self.public_owner().work.lock().unwrap().clone())?;
        let original = &selected.scope.configuration;
        if host.creation_id()? != selected.scope.creation_id
            || host.identity()? != selected.scope.storage.replica
            || host.process_session_id()? != selected.scope.process_session_id
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().sessions.revalidate(id, generation)?;
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions
            .entries
            .get_mut(&id.nonce)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if (source && entry.stage != SessionStage::Active)
            || (!source
                && !matches!(
                    entry.stage,
                    SessionStage::Receiving | SessionStage::Completed
                ))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let membership = entry
            .membership
            .as_mut()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if membership.current != original.current
            || !membership.installed
            || offer.binding.current_configuration_id != membership.current.configuration_id
            || offer.binding.previous_configuration_id
                != id
                    .source
                    .scope
                    .configuration
                    .previous
                    .as_ref()
                    .map(|previous| previous.configuration_id.clone())
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    async fn pg_repair_record(&self, id: &PgSessionId) -> Result<PgNativeReformation> {
        self.durable
            .revalidate()
            .await
            .map_err(application_error)?
            .native_reformations
            .into_iter()
            .find(|record| record.session == *id)
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    async fn pg_update_repair(&self, id: &PgSessionId, repair: PgNativeRepair) -> Result<()> {
        let serial = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .map(|entry| entry.repair_serial.clone());
        let _serial = match serial.as_ref() {
            Some(serial) => Some(serial.lock().await),
            None => None,
        };
        self.durable
            .update(|state| {
                let record = state
                    .native_reformations
                    .iter_mut()
                    .find(|record| record.session == *id)
                    .ok_or_else(|| {
                        crate::durable::PgDurableError::Invalid("repair session disappeared".into())
                    })?;
                if record.repair.as_ref().is_some_and(|old| {
                    old.nonce != repair.nonce
                        || old.original != repair.original
                        || old.candidate_path != repair.candidate_path
                        || old.archive_path != repair.archive_path
                        || old
                            .candidate
                            .as_ref()
                            .zip(repair.candidate.as_ref())
                            .is_some_and(|(before, after)| {
                                before.path != after.path
                                    || before.device != after.device
                                    || before.inode != after.inode
                            })
                        || (old.clone_digest.is_some() && old.clone_digest != repair.clone_digest)
                        || old.stage > repair.stage
                        || (old.failure.is_some() && old.failure != repair.failure)
                }) {
                    return Err(crate::durable::PgDurableError::Invalid(
                        "repair cannot replace or resurrect an attempt".into(),
                    ));
                }
                if repair.stage == RepairStage::Following {
                    record.stage = PgReformationStage::Following;
                }
                record.repair = Some(repair);
                Ok(())
            })
            .await
            .map_err(application_error)?;
        Ok(())
    }

    // This callback never takes PublicState or calls back to the source. It can
    // safely run while a target helper is waiting on the source's gateway.
    pub(in crate::adapter::public_v1) async fn pg_confirm_repair_candidate(
        &self,
        offer: &RepairOffer,
    ) -> Result<()> {
        offer.validate()?;
        let id = offer.id()?;
        if id.target.scope.storage.replica != self.public_owner().fence.local.identity {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let (generation, _) = self.public_owner().sessions.lease(id)?;
        self.pg_repair_local_lease(offer).await?;
        let serial = self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce]
            .repair_serial
            .clone();
        let _serial = serial.lock().await;
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        let record = self.pg_repair_record(id).await?;
        let repair = record.repair.ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if record.source_lineage != offer.source_lineage
            || record.retained_lineage.as_ref() != Some(&offer.retained_lineage)
            || record.required_certified_lsn != Some(offer.certified_lsn)
            || record.source_boundary != offer.source_boundary
            || repair.nonce != offer.candidate_nonce
            || repair.failure.is_some()
            || !matches!(repair.stage, RepairStage::Cloned | RepairStage::Rewinding)
            || !durable.external_access_closed
            || !durable.postgres_stopped
            || durable.quiescence.as_ref() != Some(&repair.quiescence)
            || durable.quiescence.as_ref().is_none_or(|quiescence| {
                quiescence.lineage != offer.retained_lineage
                    || quiescence.required_certified_lsn != offer.certified_lsn
            })
            || self.instance.is_running().await
            || self.instance.data_dir().join("postmaster.pid").exists()
            || !repair.original.matches_at(self.instance.data_dir(), true)?
            || !repair
                .candidate
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
                .matches_at(&repair.candidate_path, repair.stage == RepairStage::Cloned)?
            || repair.candidate_path.join("postmaster.pid").exists()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().sessions.revalidate(id, generation)
    }

    async fn pg_validate_repair_source(&self, offer: &RepairOffer) -> Result<()> {
        offer.validate()?;
        let id = offer.id()?;
        if id.source.scope.storage.replica != self.public_owner().fence.local.identity {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.pg_validate_native_gateway(&TransportBinding::Peer(offer.binding.clone()))
            .await?;
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        if durable.role != PgDurableRole::Primary
            || durable.quiescence.is_some()
            || !self.instance.is_running().await
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let record = self.pg_repair_record(id).await?;
        if record.source_lineage != offer.source_lineage
            || record.source_boundary != offer.source_boundary
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        match Box::pin(self.public_rpc(
            &id.target.replication_address,
            &Request::RepairConfirm(offer.clone()),
        ))
        .await?
        {
            Response::RepairConfirmed(actual) if actual == *offer => {}
            _ => return Err(RuntimeError::AuthorityNotAdmitted),
        }
        self.pg_validate_native_gateway(&TransportBinding::Peer(offer.binding.clone()))
            .await
    }

    async fn pg_live_repair_gateway(
        &self,
        offer: &RepairOffer,
        process: u64,
        mut client: crate::proto::pg_data_service_client::PgDataServiceClient<
            tonic::transport::Channel,
        >,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let id = offer.id()?;
        self.pg_repair_local_lease(offer).await?;
        if self.instance.generation_id() != process
            || !self.instance.is_running().await
            || self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce]
                .native_lineage
                .as_ref()
                != Some(&offer.source_lineage)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut request = tonic::Request::new(crate::proto::RecoveryRequest {
            envelope_json: encode(&Request::RepairConfirm(offer.clone()))
                .map_err(application_error)?,
        });
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.coordination.bearer_token)
                .parse()
                .map_err(application_error)?,
        );
        let response = client
            .public_v1(request)
            .await
            .map_err(application_error)?
            .into_inner();
        match decode(&response.envelope_json).map_err(application_error)? {
            Response::RepairConfirmed(actual) if actual == *offer => {}
            _ => return Err(RuntimeError::AuthorityNotAdmitted),
        }
        let result = self.pg_repair_local_lease(offer).await;
        if started.elapsed() > Duration::from_millis(100) {
            tracing::warn!(elapsed = ?started.elapsed(), "selected rewind frame lease validation delayed");
        }
        result
    }

    pub(in crate::adapter::public_v1) async fn pg_issue_repair_source(
        &self,
        offer: RepairOffer,
    ) -> Result<Response> {
        let id = offer.id()?.clone();
        let (generation, cancellation) = self.public_owner().sessions.lease(&id)?;
        let serial = self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce]
            .repair_serial
            .clone();
        let _serial = serial.lock().await;
        self.pg_validate_repair_source(&offer).await?;
        if let Some((previous, credential, running)) =
            &self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce].repair_grant
        {
            if previous != &offer || !running.load(std::sync::atomic::Ordering::Acquire) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            return Ok(Response::RepairSource {
                offer,
                credential: credential.clone(),
            });
        }
        let driver = self
            .public_owner()
            .driver
            .get()
            .cloned()
            .ok_or(RuntimeError::Closed)?;
        let selected = offer.clone();
        let process = self.instance.generation_id();
        let client = crate::proto::pg_data_service_client::PgDataServiceClient::connect(
            id.target.replication_address.clone(),
        )
        .await
        .map_err(application_error)?;
        let valid: super::super::super::public_transport::GatewayValidity = Arc::new(move || {
            let driver = driver.upgrade();
            let offer = selected.clone();
            let client = client.clone();
            Box::pin(async move {
                match driver {
                    Some(driver) => driver
                        .pg_live_repair_gateway(&offer, process, client)
                        .await
                        .is_ok(),
                    None => false,
                }
            })
        });
        let name = format!(
            "rewind_{}",
            offer
                .candidate_nonce
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let (gateway, credential) = NativeGateway::start_rewind(
            self.instance.listen_host(),
            self.instance.data_dir().to_owned(),
            self.instance
                .socket_dir()
                .join(format!(".s.PGSQL.{}", self.instance.port())),
            name,
            valid,
            cancellation.child_token(),
        )
        .await?;
        // Retain the actual drain owner before any further await. Revocation of
        // this read lane does not falsely complete/retire the physical WAL lane.
        let running = gateway.liveness();
        let granted_running = running.clone();
        let driver = self
            .public_owner()
            .driver
            .get()
            .cloned()
            .ok_or(RuntimeError::Closed)?;
        let owned_id = id.clone();
        let cancelled = cancellation.clone();
        let task = crate::owned_task::PgTask::new(tokio::spawn(async move {
            loop {
                if !running.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    _ = cancelled.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
            if let Err(error) = gateway.drain().await
                && let Some(driver) = driver.upgrade()
                && let Some(entry) = driver
                    .public_owner()
                    .sessions
                    .gate
                    .lock()
                    .unwrap()
                    .entries
                    .get_mut(&owned_id.nonce)
            {
                entry.native_failure = Some(error.to_string());
            }
        }));
        {
            let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = sessions
                .entries
                .get_mut(&id.nonce)
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            entry.joins.push(task);
            entry.repair_grant = Some((offer.clone(), credential.clone(), granted_running));
        }
        self.public_owner().sessions.revalidate(&id, generation)?;
        self.pg_validate_repair_source(&offer).await?;
        Ok(Response::RepairSource { offer, credential })
    }

    pub(super) async fn pg_repair_retained_primary(
        &self,
        binding: PeerBinding,
        generation: u64,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let id = binding
            .session
            .as_deref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let transport = TransportBinding::Peer(binding.clone());
        self.pg_validate_native_session(&transport, true).await?;
        let durable = self.public_validate_history().await?;
        let quiescence = durable
            .quiescence
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let retained = self.public_lineage().await?;
        if retained != quiescence.lineage
            || !quiescence_scope(quiescence, id)
            || durable.role != PgDurableRole::Primary
            || !durable.external_access_closed
            || !durable.postgres_stopped
            || durable.policy_certified_lsn != quiescence.required_certified_lsn
            || self.instance.is_running().await
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let (physical, lineage, boundary) = self.public_source_for_admission(&transport).await?;
        if !retained.prefix_is_contained_by(&lineage, quiescence.required_certified_lsn, boundary) {
            return Err(application_error(
                "selected repair source does not contain the certified acknowledged timeline prefix",
            ));
        }
        if durable
            .native_reformations
            .iter()
            .any(|record| record.session == *id)
        {
            return Err(application_error(
                "retained repair attempt requires exact stage recovery; no implicit retry",
            ));
        }
        let random = super::super::super::public_transport::random_bytes()?;
        let nonce: [u8; 16] = random[..16].try_into().unwrap();
        let original = RepairRoot::read(self.instance.data_dir().to_owned())?;
        let (candidate_path, archive_path) = paths(&original.path, &nonce)?;
        if present(&candidate_path)? || present(&archive_path)? {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut repair = PgNativeRepair {
            quiescence: quiescence.clone(),
            nonce,
            original,
            candidate_path,
            archive_path,
            candidate: None,
            clone_digest: None,
            stage: RepairStage::Selected,
            failure: None,
        };
        self.durable
            .update(|state| {
                if state
                    .native_reformations
                    .iter()
                    .any(|record| record.session == *id)
                    || state.quiescence != durable.quiescence
                {
                    return Err(crate::durable::PgDurableError::Invalid(
                        "repair selection changed".into(),
                    ));
                }
                state.native_reformations.push(PgNativeReformation {
                    session: id.clone(),
                    source_lineage: lineage.clone(),
                    retained_lineage: Some(retained),
                    required_certified_lsn: Some(quiescence.required_certified_lsn),
                    required_replay_lsn: Some(durable.flush_lsn),
                    source_boundary: boundary,
                    stage: PgReformationStage::Selected,
                    evidence: None,
                    repair: Some(repair.clone()),
                });
                Ok(())
            })
            .await
            .map_err(application_error)?;
        let execution = RepairExecution {
            binding: &binding,
            generation,
            cancellation: &cancellation,
            physical: &physical,
            lineage: &lineage,
            boundary,
        };
        let result = self.pg_run_repair(execution, &mut repair).await;
        if let Err(error) = &result {
            repair.failure = Some(error.to_string());
            // A failed receipt remains failed even when the filesystem effect
            // happened. No successor can reinterpret it as helper success.
            self.pg_update_repair(id, repair.clone()).await?;
            self.instance.stop().await.map_err(application_error)?;
        }
        result
    }

    async fn pg_repair_scope(
        &self,
        binding: &PeerBinding,
        generation: u64,
        lineage: &PgLineage,
        boundary: i64,
    ) -> Result<()> {
        let id = binding
            .session
            .as_deref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.public_owner().sessions.revalidate(id, generation)?;
        self.pg_validate_native_session(&TransportBinding::Peer(binding.clone()), true)
            .await?;
        let (_, actual, actual_boundary) = self
            .public_source_for_admission(&TransportBinding::Peer(binding.clone()))
            .await?;
        if actual != *lineage || actual_boundary != boundary {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().sessions.revalidate(id, generation)
    }

    async fn pg_run_repair(
        &self,
        execution: RepairExecution<'_>,
        repair: &mut PgNativeRepair,
    ) -> Result<()> {
        let RepairExecution {
            binding,
            generation,
            cancellation,
            lineage,
            boundary,
            ..
        } = execution;
        let id = binding
            .session
            .as_deref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        #[cfg(all(test, feature = "testing"))]
        self.native_cut("repair-before-clone").await;
        self.instance
            .prepare_native_build()
            .await
            .map_err(application_error)?;
        let before = tree_digest(self.instance.data_dir(), false)?;
        repair.stage = RepairStage::Cloning;
        self.pg_update_repair(id, repair.clone()).await?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        let driver = self
            .public_owner()
            .driver
            .get()
            .cloned()
            .ok_or(RuntimeError::Closed)?;
        let selected = binding.clone();
        let selected_lineage = lineage.clone();
        let clone_valid: super::super::super::public_transport::GatewayValidity =
            Arc::new(move || {
                let driver = driver.upgrade();
                let binding = selected.clone();
                let lineage = selected_lineage.clone();
                Box::pin(async move {
                    match driver {
                        Some(driver) => driver
                            .pg_repair_scope(&binding, generation, &lineage, boundary)
                            .await
                            .is_ok(),
                        None => false,
                    }
                })
            });
        self.instance
            .clone_stopped_repair_candidate(
                &repair.candidate_path,
                &repair.nonce,
                clone_valid,
                cancellation.clone(),
            )
            .await
            .map_err(application_error)?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        let after = tree_digest(self.instance.data_dir(), false)?;
        let cloned = tree_digest(&repair.candidate_path, true)?;
        if before != after || before != cloned {
            return Err(application_error(
                "repair clone is not a faithful independent retained tree",
            ));
        }
        sync_parent(&repair.candidate_path)?;
        repair.candidate = Some(RepairRoot::read(repair.candidate_path.clone())?);
        repair.clone_digest = Some(cloned);
        repair.stage = RepairStage::Cloned;
        self.pg_update_repair(id, repair.clone()).await?;
        #[cfg(all(test, feature = "testing"))]
        self.native_cut("repair-cloned").await;
        let record = self.pg_repair_record(id).await?;
        let offer = RepairOffer {
            binding: binding.clone(),
            source_lineage: lineage.clone(),
            retained_lineage: record
                .retained_lineage
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
            certified_lsn: record
                .required_certified_lsn
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
            source_boundary: boundary,
            candidate_nonce: repair.nonce,
        };
        self.pg_confirm_repair_candidate(&offer).await?;
        let normal = match self
            .public_rpc(
                &id.source.replication_address,
                &Request::RepairSource(offer.clone()),
            )
            .await?
        {
            Response::RepairSource {
                offer: observed,
                credential,
            } if observed == offer => {
                credential.rewind_conninfo()?;
                credential
            }
            _ => return Err(RuntimeError::AuthorityNotAdmitted),
        };
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        let candidate = Arc::new(
            self.instance
                .repair_candidate(repair.candidate_path.clone()),
        );
        candidate
            .prepare_native_build()
            .await
            .map_err(application_error)?;
        let driver = self
            .public_owner()
            .driver
            .get()
            .cloned()
            .ok_or(RuntimeError::Closed)?;
        let selected = offer.clone();
        let valid: super::super::super::public_transport::GatewayValidity = Arc::new(move || {
            let driver = driver.upgrade();
            let offer = selected.clone();
            Box::pin(async move {
                match driver {
                    Some(driver) => {
                        driver.pg_confirm_repair_candidate(&offer).await.is_ok()
                            && driver
                                .pg_validate_native_session(
                                    &TransportBinding::Peer(offer.binding.clone()),
                                    true,
                                )
                                .await
                                .is_ok()
                    }
                    None => false,
                }
            })
        });
        repair.stage = RepairStage::Rewinding;
        self.pg_update_repair(id, repair.clone()).await?;
        #[cfg(all(test, feature = "testing"))]
        self.native_cut("repair-rewinding").await;
        candidate
            .rewind_owned_candidate(
                &self.instance,
                &repair.nonce,
                &normal,
                valid,
                cancellation.clone(),
            )
            .await
            .map_err(application_error)?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        if tree_digest(self.instance.data_dir(), false)? != before {
            return Err(application_error("repair modified original retained tree"));
        }
        let (system, timeline) = candidate
            .control_identity()
            .await
            .map_err(application_error)?;
        let actual = PgLineage::read(candidate.data_dir(), system, timeline)
            .await
            .map_err(application_error)?;
        if !offer
            .retained_lineage
            .prefix_is_contained_by(&actual, offer.certified_lsn, boundary)
            || !actual.prefix_is_contained_by(lineage, offer.certified_lsn, boundary)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        tree_digest(candidate.data_dir(), true)?;
        repair.candidate = Some(RepairRoot::read(repair.candidate_path.clone())?);
        repair.stage = RepairStage::Rewound;
        self.pg_update_repair(id, repair.clone()).await?;
        candidate.stop().await.map_err(application_error)?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        repair.stage = RepairStage::InstallSelected;
        self.pg_update_repair(id, repair.clone()).await?;
        self.pg_install_repair(id, repair, Some((binding, generation, lineage, boundary)))
            .await?;
        self.pg_repair_scope(binding, generation, lineage, boundary)
            .await?;
        self.pg_follow_installed_repair(execution, repair).await
    }

    async fn pg_install_repair(
        &self,
        id: &PgSessionId,
        repair: &mut PgNativeRepair,
        scope: Option<(&PeerBinding, u64, &PgLineage, i64)>,
    ) -> Result<()> {
        self.instance
            .ensure_native_helpers_complete()
            .await
            .map_err(application_error)?;
        if let Some((binding, generation, lineage, boundary)) = scope {
            self.pg_repair_scope(binding, generation, lineage, boundary)
                .await?;
        }
        let candidate = repair
            .candidate
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if self.instance.is_running().await || self.instance.data_dir() != repair.original.path {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if repair.stage == RepairStage::InstallSelected {
            if repair.original.matches_at(&repair.original.path, true)?
                && candidate.matches_at(&repair.candidate_path, true)?
                && !present(&repair.archive_path)?
            {
                std::fs::rename(&repair.original.path, &repair.archive_path)
                    .map_err(application_error)?;
                sync_parent(&repair.original.path)?;
                #[cfg(all(test, feature = "testing"))]
                self.native_cut("repair-original-move-effect").await;
                if let Some((binding, generation, lineage, boundary)) = scope {
                    self.pg_repair_scope(binding, generation, lineage, boundary)
                        .await?;
                }
            }
            if !repair.original.matches_at(&repair.archive_path, true)?
                || present(&repair.original.path)?
                || !candidate.matches_at(&repair.candidate_path, true)?
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            repair.stage = RepairStage::OriginalRetained;
            self.pg_update_repair(id, repair.clone()).await?;
        }
        if repair.stage == RepairStage::OriginalRetained {
            if let Some((binding, generation, lineage, boundary)) = scope {
                self.pg_repair_scope(binding, generation, lineage, boundary)
                    .await?;
            }
            if !repair.original.matches_at(&repair.archive_path, true)? {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if !present(&repair.original.path)?
                && candidate.matches_at(&repair.candidate_path, true)?
            {
                std::fs::rename(&repair.candidate_path, &repair.original.path)
                    .map_err(application_error)?;
                sync_parent(&repair.original.path)?;
                #[cfg(all(test, feature = "testing"))]
                self.native_cut("repair-install-move-effect").await;
                if let Some((binding, generation, lineage, boundary)) = scope {
                    self.pg_repair_scope(binding, generation, lineage, boundary)
                        .await?;
                }
            }
            if !candidate.matches_at(&repair.original.path, true)?
                || present(&repair.candidate_path)?
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            repair.stage = RepairStage::Installed;
            self.pg_update_repair(id, repair.clone()).await?;
        }
        Ok(())
    }

    pub(in crate::adapter::public_v1) async fn pg_recover_repair_moves(&self) -> Result<()> {
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        let latest = durable
            .native_reformations
            .iter()
            .rev()
            .find(|record| record.repair.is_some())
            .map(PgNativeReformation::nonce);
        for record in durable.native_reformations {
            if let Some(mut repair) = record.repair {
                if repair.original.path != self.instance.data_dir() {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                if Some(record.session.nonce) != latest {
                    if repair.stage >= RepairStage::OriginalRetained
                        && !repair.original.matches_at(&repair.archive_path, true)?
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    continue;
                }
                if matches!(
                    repair.stage,
                    RepairStage::InstallSelected | RepairStage::OriginalRetained
                ) {
                    // This only completes already selected directory placement.
                    // No process, credential, lease, readiness or old ACK revives.
                    self.pg_install_repair(&record.session, &mut repair, None)
                        .await?;
                }
                if repair.stage >= RepairStage::Installed {
                    if !repair.original.matches_at(&repair.archive_path, true)?
                        || !repair
                            .candidate
                            .as_ref()
                            .ok_or(RuntimeError::AuthorityNotAdmitted)?
                            .matches_at(
                                self.instance.data_dir(),
                                repair.stage == RepairStage::Installed,
                            )?
                        || present(&repair.candidate_path)?
                        || self.instance.data_dir().join("postmaster.pid").exists()
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    if repair.stage >= RepairStage::Following {
                        let actual = self.public_lineage().await?;
                        if !record.source_lineage.prefix_is_contained_by(
                            &actual,
                            record
                                .required_certified_lsn
                                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
                            record.source_boundary,
                        ) {
                            return Err(RuntimeError::AuthorityNotAdmitted);
                        }
                    }
                } else {
                    if !repair.original.matches_at(self.instance.data_dir(), true)?
                        || present(&repair.archive_path)?
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    match repair.stage {
                        RepairStage::Selected if present(&repair.candidate_path)? => {
                            return Err(RuntimeError::AuthorityNotAdmitted);
                        }
                        RepairStage::Cloning | RepairStage::Rewinding => {
                            return Err(application_error(
                                "uncertain native repair helper remains recovery-pending; original and candidate retained",
                            ));
                        }
                        RepairStage::Cloned | RepairStage::Rewound
                            if !repair
                                .candidate
                                .as_ref()
                                .ok_or(RuntimeError::AuthorityNotAdmitted)?
                                .matches_at(&repair.candidate_path, true)?
                                || repair.candidate_path.join("postmaster.pid").exists()
                                || (repair.stage == RepairStage::Cloned
                                    && repair.clone_digest.as_ref()
                                        != Some(&tree_digest(&repair.candidate_path, false)?)) =>
                        {
                            return Err(RuntimeError::AuthorityNotAdmitted);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    async fn pg_follow_installed_repair(
        &self,
        execution: RepairExecution<'_>,
        repair: &mut PgNativeRepair,
    ) -> Result<()> {
        let RepairExecution {
            binding,
            generation,
            cancellation,
            physical,
            lineage,
            boundary,
        } = execution;
        let id = binding
            .session
            .as_deref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let transport = TransportBinding::Peer(binding.clone());
        self.public_owned_with(cancellation.clone(), async {
            self.pg_repair_scope(binding, generation, lineage, boundary)
                .await?;
            let durable = self.durable.revalidate().await.map_err(application_error)?;
            if !durable.external_access_closed
                || durable.quiescence.is_none()
                || repair.stage != RepairStage::Installed
                || repair.failure.is_some()
                || !repair.original.matches_at(&repair.archive_path, true)?
                || !repair
                    .candidate
                    .as_ref()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?
                    .matches_at(self.instance.data_dir(), true)?
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.public_owner().fence.admit_peer(binding)?;
            self.instance
                .config()
                .public_receiver(
                    self.instance.data_dir(),
                    &physical.conninfo()?,
                    &crate::native::replication_slot_name(transport.target()),
                    lineage,
                )
                .await
                .map_err(application_error)?;
            self.pg_repair_scope(binding, generation, lineage, boundary)
                .await?;
            // The ordinary start remains closed to every quiescent tree. Only
            // this private installed-record corridor can launch the candidate.
            crate::config::PgConfig::public_replication_fence(self.instance.data_dir())
                .await
                .map_err(application_error)?;
            repair.stage = RepairStage::Following;
            self.pg_update_repair(id, repair.clone()).await?;
            self.instance
                .start_public_receiver(self.fault_tx.clone(), self.cancellation.clone(), physical)
                .await
                .map_err(application_error)?;
            loop {
                self.pg_repair_scope(binding, generation, lineage, boundary)
                    .await?;
                let snapshot = self.observer.snapshot().await.map_err(application_error)?;
                if let Some(evidence) = snapshot.evidence
                    && evidence.in_recovery
                    && !evidence.wal_receiver_stopped
                    && evidence.system_identifier == lineage.system_identifier
                    && evidence.timeline_id == lineage.timeline
                    && evidence.received_lsn.is_some_and(|lsn| lsn >= boundary)
                    && evidence.replay_lsn.is_some_and(|lsn| lsn >= boundary)
                {
                    let digest = timeline_history_digest(self.instance.data_dir(), &evidence)
                        .await
                        .map_err(application_error)?;
                    self.pg_repair_scope(binding, generation, lineage, boundary)
                        .await?;
                    self.durable
                        .update(|state| {
                            let record = state
                                .native_reformations
                                .iter_mut()
                                .find(|record| record.session == *id)
                                .ok_or_else(|| {
                                    crate::durable::PgDurableError::Invalid(
                                        "installed repair session missing".into(),
                                    )
                                })?;
                            let owned = record.repair.as_mut().ok_or_else(|| {
                                crate::durable::PgDurableError::Invalid(
                                    "installed repair missing".into(),
                                )
                            })?;
                            if owned != repair
                                || !state.external_access_closed
                                || state.quiescence.is_none()
                            {
                                return Err(crate::durable::PgDurableError::Invalid(
                                    "installed repair changed during following".into(),
                                ));
                            }
                            owned.stage = RepairStage::Replayed;
                            record.stage = PgReformationStage::Replayed;
                            record.evidence = Some(evidence.clone());
                            state.quiescence = None;
                            state.role = PgDurableRole::Standby;
                            state.postgres_stopped = false;
                            state.system_identifier = Some(evidence.system_identifier.clone());
                            state.timeline_id = Some(evidence.timeline_id);
                            state.timeline_history_digest = Some(digest);
                            state.current_lsn = evidence.flush_lsn;
                            state.flush_lsn = evidence.flush_lsn;
                            state.received_lsn = evidence.received_lsn;
                            state.replay_lsn = evidence.replay_lsn;
                            state.synchronous = None;
                            state.synchronous_process_generation = None;
                            Ok(())
                        })
                        .await
                        .map_err(application_error)?;
                    repair.stage = RepairStage::Replayed;
                    let mut state = self.public_owner().state.lock().await;
                    state.receiver = Some(transport.clone());
                    state.receiver_boundary = Some(boundary);
                    self.pg_native_reformation_completed(id)?;
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
    }
}
