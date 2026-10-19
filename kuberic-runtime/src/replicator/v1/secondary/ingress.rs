// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of SecondaryReplicator.cpp:915-1035,1125-1263,
// 1306-1396,1438-1568,2282-2344, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

use super::*;
use crate::replicator::v1::buffers;
use crate::replicator::v1::ingress::{Disposition, DropReason, IngressError};
use crate::replicator::v1::operation_queue::QueueOperation;
use crate::replicator::v1::transport::ProtocolError;

fn epoch(epoch: &wire::Epoch) -> Result<Epoch, ProtocolError> {
    if epoch.data_loss < 0 || epoch.configuration < 0 {
        return Err(ProtocolError::Shape("negative epoch"));
    }
    Ok(Epoch::new(epoch.data_loss, epoch.configuration))
}

impl SecondaryHandle {
    pub(in crate::replicator::v1) fn reported_fault(
        &self,
    ) -> Result<Option<SecondaryError>, SecondaryError> {
        Ok(self.root()?.state.lock().unwrap().fault.clone())
    }

    fn report_ingress_fault(&self, error: SecondaryError) -> Result<Disposition, IngressError> {
        let root = self.root()?;
        {
            let mut state = root.state.lock().unwrap();
            if state.phase != Phase::Open {
                return Ok(Disposition::Dropped(DropReason::Role));
            }
            if state.fault.is_some() {
                return Ok(Disposition::Dropped(DropReason::Faulted));
            }
            state.fault = Some(error.clone());
        }
        root.wake.notify_one();
        (root.report_fault)(error);
        Ok(Disposition::FaultReported)
    }

    pub(in crate::replicator::v1) fn receive_message(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<Disposition, IngressError> {
        let root = self.root()?;
        let (copy_epoch, minimum, primary) = {
            let state = root.state.lock().unwrap();
            if state.phase != Phase::Open {
                return Ok(Disposition::Dropped(DropReason::Role));
            }
            if state.fault.is_some() {
                return Ok(Disposition::Dropped(DropReason::Faulted));
            }
            let (copy_epoch, minimum) = {
                let replication = root.replication.lock().unwrap();
                (
                    replication.copy_epoch(),
                    replication.minimum_sending_epoch(),
                )
            };
            (
                copy_epoch,
                minimum,
                root.establishment
                    .get()
                    .unwrap()
                    .primary_route()
                    .map_err(SecondaryError::from)?,
            )
        };
        let incoming = primary.incoming(envelope)?;
        let dropped = |reason| Ok(Disposition::Dropped(reason));
        use wire::envelope::Body;
        match envelope
            .body
            .as_ref()
            .ok_or(ProtocolError::Missing("body"))?
        {
            Body::StartCopy(_) => match self.start(envelope, false) {
                Ok(_) => {}
                Err(SecondaryError::Establish(EstablishError::StaleEpoch)) => {
                    return dropped(DropReason::StaleEpoch);
                }
                Err(SecondaryError::Establish(EstablishError::WrongReplica)) => {
                    return dropped(DropReason::WrongReplica);
                }
                Err(error @ SecondaryError::Establish(EstablishError::UnexpectedEpoch)) => {
                    return self.report_ingress_fault(error);
                }
                Err(error) => return Err(error.into()),
            },
            _ if copy_epoch.is_none() => return dropped(DropReason::NotInitialized),
            Body::ReplicationOperation(body) => {
                let sending = epoch(
                    body.sending_epoch
                        .as_ref()
                        .ok_or(ProtocolError::Missing("sending epoch"))?,
                )?;
                if minimum.is_some_and(|minimum| sending < minimum) {
                    return dropped(DropReason::StaleEpoch);
                }
                let original = epoch(
                    body.original_epoch
                        .as_ref()
                        .ok_or(ProtocolError::Missing("original epoch"))?,
                )?;
                let segments = &body
                    .buffers
                    .as_ref()
                    .ok_or(ProtocolError::Missing("buffers"))?
                    .segments;
                let required = root.config.replication.has_persisted_state
                    || root.config.replication.require_service_ack;
                let config = root.config.context;
                let mut offset = 0;
                let mut operations = Vec::with_capacity(body.buffer_counts.len());
                for (index, count) in body.buffer_counts.iter().enumerate() {
                    let end = offset + *count as usize;
                    let data = buffers::encode(
                        &segments[offset..end],
                        config.buffer_policy,
                        config.buffer_limits,
                    )?;
                    let lsn = body.first_sequence + index as i64;
                    if lsn <= 0 || lsn == Lsn::MAX {
                        return Err(ProtocolError::Shape("invalid replication LSN").into());
                    }
                    operations.push(ReceivedOperation::new(
                        QueueOperation::from_data(
                            lsn,
                            data,
                            true,
                            config.buffer_policy,
                            config.buffer_limits,
                        )?,
                        original,
                        required,
                    ));
                    offset = end;
                }
                self.receive_replication_from(
                    sending,
                    operations,
                    Some(body.completed_hint),
                    Some(incoming),
                )?;
            }
            Body::CopyOperation(body) => {
                if body.replica != primary.source.replica {
                    return dropped(DropReason::WrongReplica);
                }
                let sending = epoch(
                    body.epoch
                        .as_ref()
                        .ok_or(ProtocolError::Missing("copy epoch"))?,
                )?;
                let established = copy_epoch.unwrap();
                if sending < established {
                    return dropped(DropReason::StaleEpoch);
                }
                if sending > established {
                    return self.report_ingress_fault(SecondaryError::Fault(
                        "Secondary received COPY operation from different primary epoch".into(),
                    ));
                }
                let metadata = body
                    .metadata
                    .as_ref()
                    .ok_or(ProtocolError::Missing("copy metadata"))?;
                let segments = &body
                    .buffers
                    .as_ref()
                    .ok_or(ProtocolError::Missing("buffers"))?
                    .segments;
                let config = root.config.context;
                let operation = if body.is_last {
                    if buffers::payload_len(segments, config.buffer_limits)? != 0 {
                        return Err(ProtocolError::Shape("nonempty copy terminal").into());
                    }
                    if root.config.copy.enable_eos_ack {
                        ReceivedOperation::copy_end_of_stream(
                            metadata.sequence,
                            root.config.copy.require_service_ack,
                        )
                        .map_err(|_| ProtocolError::Shape("invalid copy terminal LSN"))?
                    } else {
                        ReceivedOperation::new(
                            QueueOperation::new(
                                metadata.sequence,
                                crate::application::OperationData::new(),
                                true,
                            ),
                            sending,
                            root.config.copy.require_service_ack,
                        )
                    }
                } else {
                    let data =
                        buffers::encode(segments, config.buffer_policy, config.buffer_limits)?;
                    ReceivedOperation::new(
                        QueueOperation::from_data(
                            metadata.sequence,
                            data,
                            true,
                            config.buffer_policy,
                            config.buffer_limits,
                        )?,
                        sending,
                        root.config.copy.require_service_ack,
                    )
                };
                self.receive_copy(operation, body.is_last)?;
            }
            Body::CopyContextAck(_) | Body::RequestAck(_) => {
                if primary.check(envelope, true).is_err() {
                    return dropped(DropReason::WrongPrimary);
                }
                if matches!(envelope.body, Some(Body::RequestAck(_))) {
                    root.schedule(false)?;
                } else {
                    match self.process_context_ack(envelope) {
                        Ok(()) | Err(SecondaryError::Establish(EstablishError::Remote(_))) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Body::InduceFault(body) => {
                if body.target_replica != primary.source.replica
                    || body.target_incarnation != primary.source.incarnation
                {
                    return dropped(DropReason::WrongReplica);
                }
                return self.report_ingress_fault(SecondaryError::Fault(body.reason.clone()));
            }
            Body::ReplicationAck(_) | Body::CopyContextOperation(_) => {
                return dropped(DropReason::Role);
            }
        }
        Ok(Disposition::Processed)
    }
}
