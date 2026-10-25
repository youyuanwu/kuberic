// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of Replicator.cpp:618-759,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.
// Logical registration: ReplicationTransport.h:253-267,301-308.
// Byte-stream metadata constants: public FabricTypes.idl:46,779-781.
//! Private role-aware demux. Callers admit peer routes; the carrier owns bounded
//! delivery and stream generations, not host authentication. Effects run unlocked.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ingress"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::buffers::{BufferError, BufferLimits};
use super::primary::{PrimaryError, PrimaryHandle};
use super::secondary::{SecondaryError, SecondaryHandle};
use super::transport::{ProtocolError, Route, validate_endpoint, validate_shape, wire};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    Unregistered,
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

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PublishedEndpoint {
    pub(super) address: String,
    pub(super) endpoint: wire::Endpoint,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum RegistrationError {
    #[error("message processor endpoint is already registered")]
    DuplicateEndpoint,
}

type EndpointKey = (bytes::Bytes, i64, bytes::Bytes);

fn endpoint_key(endpoint: &wire::Endpoint) -> EndpointKey {
    (
        endpoint.partition.clone(),
        endpoint.replica,
        endpoint.incarnation.clone(),
    )
}

/// Private logical counterpart of ReplicationTransport.h:253-267,301-308.
/// A production authenticated listener/registry is deliberately not selected.
#[derive(Clone, Default)]
pub(super) struct MessageRegistry {
    processors: Arc<Mutex<BTreeMap<EndpointKey, MessageIngress>>>,
}

#[must_use = "retain the registration until Close/Abort"]
pub(super) struct MessageRegistration {
    registry: MessageRegistry,
    ingress: MessageIngress,
}

impl MessageRegistry {
    #[cfg(feature = "testing")]
    pub(super) fn inspect_receivers(&self) -> Vec<serde_json::Value> {
        self.processors
            .lock()
            .unwrap()
            .values()
            .filter_map(|ingress| match &*ingress.target.lock().unwrap() {
                Target::Secondary(secondary) => Some(secondary.inspect_receiver()),
                _ => None,
            })
            .collect()
    }

    pub(super) fn register(
        &self,
        ingress: MessageIngress,
    ) -> Result<MessageRegistration, RegistrationError> {
        let mut processors = self.processors.lock().unwrap();
        let std::collections::btree_map::Entry::Vacant(entry) =
            processors.entry(endpoint_key(&ingress.local.source))
        else {
            return Err(RegistrationError::DuplicateEndpoint);
        };
        entry.insert(ingress.clone());
        Ok(MessageRegistration {
            registry: self.clone(),
            ingress,
        })
    }

    pub(super) fn dispatch(&self, envelope: &wire::Envelope) -> Result<Disposition, IngressError> {
        let endpoint = envelope
            .destination
            .as_ref()
            .ok_or(ProtocolError::Missing("destination"))?;
        validate_endpoint(endpoint)?;
        let ingress = self
            .processors
            .lock()
            .unwrap()
            .get(&endpoint_key(endpoint))
            .cloned();
        match ingress {
            Some(ingress) => ingress.dispatch(envelope),
            None => Ok(Disposition::Dropped(DropReason::Unregistered)),
        }
    }
}

impl Drop for MessageRegistration {
    fn drop(&mut self) {
        self.ingress.set_target(Target::None);
        self.registry
            .processors
            .lock()
            .unwrap()
            .remove(&endpoint_key(&self.ingress.local.source));
    }
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

    pub(super) fn published_endpoint(&self) -> PublishedEndpoint {
        PublishedEndpoint {
            address: self.local.sender_address.clone(),
            endpoint: self.local.source.clone(),
        }
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
