use std::fmt::{self, Write as _};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Bytes compared and persisted without normalization.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
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
    name: ActivityName,
    input: ExactBytes,
}

impl LogicalActivityId {
    pub fn new(
        execution_id: ExecutionId,
        sequence: ActivitySequence,
        name: ActivityName,
        input: ExactBytes,
    ) -> Self {
        Self {
            execution_id,
            sequence,
            name,
            input,
        }
    }

    pub const fn execution_id(&self) -> ExecutionId {
        self.execution_id
    }

    pub const fn sequence(&self) -> ActivitySequence {
        self.sequence
    }

    pub fn name(&self) -> &ActivityName {
        &self.name
    }

    pub fn input(&self) -> &ExactBytes {
        &self.input
    }

    /// Render every identity tuple member directly in an unambiguous form.
    pub fn to_external_id(&self) -> String {
        let mut rendered = String::from("logical:v1:");
        push_hex(&mut rendered, self.execution_id.as_bytes());
        let name_bytes = self.name.name().as_bytes();
        write!(rendered, ":{}:{}:", self.sequence.get(), name_bytes.len())
            .expect("writing to a String cannot fail");
        push_hex(&mut rendered, name_bytes);
        write!(
            rendered,
            ":{}:{}:",
            self.name.version(),
            self.input.as_slice().len()
        )
        .expect("writing to a String cannot fail");
        push_hex(&mut rendered, self.input.as_slice());
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
