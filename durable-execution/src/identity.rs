use std::fmt::{self, Write as _};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Bytes compared and persisted without normalization.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExactBytes(Vec<u8>);

impl ExactBytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for ExactBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ExactBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
            .decode(encoded)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl From<Vec<u8>> for ExactBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl<const N: usize> From<[u8; N]> for ExactBytes {
    fn from(value: [u8; N]) -> Self {
        Self::new(value)
    }
}

/// Caller-supplied durable execution identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionId([u8; 16]);

impl ExecutionId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("execution:")?;
        write_hex(formatter, &self.0)
    }
}

/// Immutable caller authority for one durable execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpec {
    execution_id: ExecutionId,
    workflow_input: ExactBytes,
    max_terminal_payload_bytes: u64,
}

impl ExecutionSpec {
    pub fn new(
        execution_id: ExecutionId,
        workflow_input: ExactBytes,
        max_terminal_payload_bytes: u64,
    ) -> Self {
        Self {
            execution_id,
            workflow_input,
            max_terminal_payload_bytes,
        }
    }

    pub const fn execution_id(&self) -> ExecutionId {
        self.execution_id
    }

    pub const fn workflow_input(&self) -> &ExactBytes {
        &self.workflow_input
    }

    pub const fn max_terminal_payload_bytes(&self) -> u64 {
        self.max_terminal_payload_bytes
    }
}

/// Caller-supplied identity for one host epoch.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostEpoch([u8; 16]);

impl HostEpoch {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for HostEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("host:")?;
        write_hex(formatter, &self.0)
    }
}

/// Zero-based position of an activity in a linear workflow history.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActivitySequence(u64);

impl ActivitySequence {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ActivitySequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Immutable caller-supplied activity name with an explicit positive version.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ActivityName {
    name: String,
    version: u32,
}

impl ActivityName {
    pub fn new(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        let name = name.into();
        if name.is_empty() {
            return Err(IdentityError::EmptyActivityName);
        }
        if version == 0 {
            return Err(IdentityError::ZeroActivityVersion);
        }
        Ok(Self { name, version })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn version(&self) -> u32 {
        self.version
    }
}

impl fmt::Display for ActivityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@v{}", self.name, self.version)
    }
}

#[derive(Deserialize)]
struct ActivityNameWire {
    name: String,
    version: u32,
}

impl<'de> Deserialize<'de> for ActivityName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ActivityNameWire::deserialize(deserializer)?;
        Self::new(wire.name, wire.version).map_err(serde::de::Error::custom)
    }
}

/// Immutable semantics of one workflow activity declaration.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ActivitySpec {
    name: ActivityName,
    input: ExactBytes,
    max_result_bytes: u64,
}

impl ActivitySpec {
    pub fn new(name: ActivityName, input: ExactBytes, max_result_bytes: u64) -> Self {
        Self {
            name,
            input,
            max_result_bytes,
        }
    }

    pub const fn name(&self) -> &ActivityName {
        &self.name
    }

    pub const fn input(&self) -> &ExactBytes {
        &self.input
    }

    pub const fn max_result_bytes(&self) -> u64 {
        self.max_result_bytes
    }
}

/// One dispatch attempt. Counter zero is reserved and rejected.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AttemptId {
    host_epoch: HostEpoch,
    counter: u64,
}

impl AttemptId {
    pub fn new(host_epoch: HostEpoch, counter: u64) -> Result<Self, IdentityError> {
        if counter == 0 {
            return Err(IdentityError::ZeroAttemptCounter);
        }
        Ok(Self {
            host_epoch,
            counter,
        })
    }

    pub const fn host_epoch(self) -> HostEpoch {
        self.host_epoch
    }

    pub const fn counter(self) -> u64 {
        self.counter
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("attempt:")?;
        write_hex(formatter, self.host_epoch.as_bytes())?;
        write!(formatter, ":{}", self.counter)
    }
}

#[derive(Deserialize)]
struct AttemptIdWire {
    host_epoch: HostEpoch,
    counter: u64,
}

impl<'de> Deserialize<'de> for AttemptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AttemptIdWire::deserialize(deserializer)?;
        Self::new(wire.host_epoch, wire.counter).map_err(serde::de::Error::custom)
    }
}

/// Complete semantic identity of a logical activity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct LogicalActivityId {
    execution_id: ExecutionId,
    sequence: ActivitySequence,
    spec: ActivitySpec,
}

impl LogicalActivityId {
    pub fn new(execution_id: ExecutionId, sequence: ActivitySequence, spec: ActivitySpec) -> Self {
        Self {
            execution_id,
            sequence,
            spec,
        }
    }

    pub const fn execution_id(&self) -> ExecutionId {
        self.execution_id
    }

    pub const fn sequence(&self) -> ActivitySequence {
        self.sequence
    }

    pub fn name(&self) -> &ActivityName {
        self.spec.name()
    }

    pub fn input(&self) -> &ExactBytes {
        self.spec.input()
    }

    pub const fn max_result_bytes(&self) -> u64 {
        self.spec.max_result_bytes()
    }

    pub const fn spec(&self) -> &ActivitySpec {
        &self.spec
    }

    /// Render every identity tuple member directly in an unambiguous form.
    pub fn to_external_id(&self) -> String {
        let mut rendered = String::from("logical:v2:");
        push_hex(&mut rendered, self.execution_id.as_bytes());
        let name_bytes = self.spec.name().name().as_bytes();
        write!(rendered, ":{}:{}:", self.sequence.get(), name_bytes.len())
            .expect("writing to a String cannot fail");
        push_hex(&mut rendered, name_bytes);
        write!(
            rendered,
            ":{}:{}:",
            self.spec.name().version(),
            self.spec.input().as_slice().len()
        )
        .expect("writing to a String cannot fail");
        push_hex(&mut rendered, self.spec.input().as_slice());
        write!(rendered, ":{}", self.spec.max_result_bytes())
            .expect("writing to a String cannot fail");
        rendered
    }
}

impl fmt::Display for LogicalActivityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_external_id())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IdentityError {
    #[error("activity name must not be empty")]
    EmptyActivityName,
    #[error("activity version must be greater than zero")]
    ZeroActivityVersion,
    #[error("attempt counter must be greater than zero")]
    ZeroAttemptCounter,
}

fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bytes_use_validated_compact_base64_json() {
        let representative: Vec<u8> = (0..=255).collect();
        let exact = ExactBytes::new(representative.clone());
        let encoded = serde_json::to_vec(&exact).unwrap();
        let decoded: ExactBytes = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, exact);

        let integer_array = serde_json::to_vec(&representative).unwrap();
        assert!(
            encoded.len() * 2 < integer_array.len(),
            "base64 JSON should use less than half the representative array JSON: {} vs {}",
            encoded.len(),
            integer_array.len()
        );
        assert!(serde_json::from_str::<ExactBytes>(r#""not base64!""#).is_err());
    }
}
