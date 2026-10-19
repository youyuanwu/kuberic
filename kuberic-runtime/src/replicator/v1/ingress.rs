// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of Replicator.cpp:618-759,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.
// Byte-stream metadata constants: public FabricTypes.idl:46,779-781.
//! Private role-aware demux. Callers admit peer routes; the carrier owns bounded
//! delivery and stream generations, not host authentication. Effects run unlocked.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ingress"))]

use std::sync::{Arc, Mutex};

use super::buffers::{BufferError, BufferLimits};
use super::primary::{PrimaryError, PrimaryHandle};
use super::secondary::{SecondaryError, SecondaryHandle};
use super::transport::{ProtocolError, Route, validate_shape, wire};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    Role,
    Faulted,
    NotInitialized,
    StaleEpoch,
    WrongReplica,
    WrongPrimary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    Processed,
    Dropped(DropReason),
    FaultReported,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum IngressError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error(transparent)]
    Primary(#[from] PrimaryError),
    #[error(transparent)]
    Secondary(#[from] SecondaryError),
}

#[derive(Clone)]
pub(super) enum Target {
    None,
    Primary(PrimaryHandle),
    Secondary(SecondaryHandle),
}

#[derive(Clone)]
pub(super) struct MessageIngress {
    target: Arc<Mutex<Target>>,
    local: Route,
    limits: BufferLimits,
}

impl MessageIngress {
    pub(super) fn new(local: Route, limits: BufferLimits) -> Self {
        Self {
            target: Arc::new(Mutex::new(Target::None)),
            local,
            limits,
        }
    }

    pub(super) fn set_target(&self, target: Target) {
        *self.target.lock().unwrap() = target;
    }

    pub(super) fn dispatch(&self, envelope: &wire::Envelope) -> Result<Disposition, IngressError> {
        self.local.incoming(envelope)?;
        validate_shape(envelope, self.limits)?;
        use wire::envelope::Body;
        let metadata = match envelope.body.as_ref().unwrap() {
            Body::ReplicationOperation(body) => {
                body.metadata.as_ref().map(|metadata| (metadata, false))
            }
            Body::CopyOperation(body) => body
                .metadata
                .as_ref()
                .map(|metadata| (metadata, body.is_last)),
            Body::CopyContextOperation(body) => body
                .metadata
                .as_ref()
                .map(|metadata| (metadata, body.is_last)),
            _ => None,
        };
        if let Some((metadata, terminal)) = metadata
            && (metadata.atomic_group != -1 || (!terminal && metadata.operation_type != 1))
        {
            return Err(ProtocolError::Shape("unsupported byte-stream operation metadata").into());
        }
        let target = self.target.lock().unwrap().clone();
        if let Target::Primary(primary) = &target
            && primary.ingress_faulted()?
        {
            return Ok(Disposition::Dropped(DropReason::Faulted));
        }
        match (target, envelope.body.as_ref().unwrap()) {
            (Target::Primary(primary), Body::ReplicationAck(_)) => {
                primary.receive_ack(envelope)?;
            }
            (Target::Primary(primary), Body::CopyContextOperation(_)) => {
                primary.receive_context(envelope)?;
            }
            (Target::Secondary(secondary), body)
                if !matches!(
                    body,
                    Body::ReplicationAck(_) | Body::CopyContextOperation(_)
                ) =>
            {
                return secondary.receive_message(envelope);
            }
            _ => return Ok(Disposition::Dropped(DropReason::Role)),
        }
        Ok(Disposition::Processed)
    }
}
