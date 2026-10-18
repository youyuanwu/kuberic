//! Explicit representation substitution for classic V1's vector of buffers.
//! This is not native wire framing. Protected provider byte streams stay intact.

#![allow(dead_code, reason = "private, unselected V1 representation adapter")]

use std::pin::Pin;

use bytes::{BufMut, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use thiserror::Error;

use crate::application::OperationDataStream;
use crate::error::RuntimeError;

const MAGIC: &[u8; 4] = b"SFVB";
const VERSION: u8 = 1;
const HEADER: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BufferPolicy {
    RawSingle,
    FramedSegments,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BufferLimits {
    pub(super) max_segments: usize,
    /// Application bytes only, not the frame header or segment-length table.
    pub(super) max_payload_bytes: usize,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum BufferError {
    #[error("raw-single representation requires exactly one buffer")]
    NotSingle,
    #[error("buffer count exceeds the configured or representable bound")]
    TooManySegments,
    #[error("logical buffer payload exceeds the configured bound")]
    TooLarge,
    #[error("malformed, truncated, overflowing, or trailing buffer frame")]
    Malformed,
    #[error("unsupported buffer frame version {0}")]
    UnsupportedVersion(u8),
}

pub(super) fn payload_len(segments: &[Bytes], limits: BufferLimits) -> Result<usize, BufferError> {
    if segments.len() > limits.max_segments || u32::try_from(segments.len()).is_err() {
        return Err(BufferError::TooManySegments);
    }
    let size = segments.iter().try_fold(0usize, |size, segment| {
        if u32::try_from(segment.len()).is_err() {
            return Err(BufferError::TooLarge);
        }
        size.checked_add(segment.len()).ok_or(BufferError::TooLarge)
    })?;
    if size > limits.max_payload_bytes {
        return Err(BufferError::TooLarge);
    }
    Ok(size)
}

pub(super) fn encode(
    segments: &[Bytes],
    policy: BufferPolicy,
    limits: BufferLimits,
) -> Result<Bytes, BufferError> {
    let payload = payload_len(segments, limits)?;
    if policy == BufferPolicy::RawSingle {
        return match segments {
            [single] => Ok(single.clone()),
            _ => Err(BufferError::NotSingle),
        };
    }
    let size = segments
        .len()
        .checked_mul(4)
        .and_then(|table| HEADER.checked_add(table))
        .and_then(|size| size.checked_add(payload))
        .ok_or(BufferError::TooLarge)?;
    let mut frame = BytesMut::with_capacity(size);
    frame.extend_from_slice(MAGIC);
    frame.put_u8(VERSION);
    frame.put_u32_le(segments.len() as u32);
    for segment in segments {
        frame.put_u32_le(segment.len() as u32);
    }
    for segment in segments {
        frame.extend_from_slice(segment);
    }
    Ok(frame.freeze())
}

pub(super) fn decode(
    bytes: Bytes,
    policy: BufferPolicy,
    limits: BufferLimits,
) -> Result<Vec<Bytes>, BufferError> {
    if policy == BufferPolicy::RawSingle {
        let segments = vec![bytes];
        payload_len(&segments, limits)?;
        return Ok(segments);
    }
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return Err(BufferError::Malformed);
    }
    if bytes[4] != VERSION {
        return Err(BufferError::UnsupportedVersion(bytes[4]));
    }
    let count = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
    if count > limits.max_segments {
        return Err(BufferError::TooManySegments);
    }
    let table_end = count
        .checked_mul(4)
        .and_then(|table| HEADER.checked_add(table))
        .filter(|end| *end <= bytes.len())
        .ok_or(BufferError::Malformed)?;
    let payload = bytes.len() - table_end;
    if payload > limits.max_payload_bytes {
        return Err(BufferError::TooLarge);
    }
    let mut offset = table_end;
    let mut segments = Vec::with_capacity(count);
    for entry in bytes[HEADER..table_end].as_chunks::<4>().0 {
        let length = u32::from_le_bytes(*entry) as usize;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or(BufferError::Malformed)?;
        segments.push(bytes.slice(offset..end));
        offset = end;
    }
    if offset != bytes.len() {
        return Err(BufferError::Malformed);
    }
    Ok(segments)
}

pub(super) type SegmentedStream = Pin<Box<dyn Stream<Item = crate::Result<Vec<Bytes>>> + Send>>;

fn runtime_error(error: BufferError) -> RuntimeError {
    RuntimeError::InvalidReplication(error.to_string())
}

pub(super) fn encode_stream(
    stream: SegmentedStream,
    policy: BufferPolicy,
    limits: BufferLimits,
) -> OperationDataStream {
    Box::pin(stream.map(move |item| {
        item.and_then(|segments| encode(&segments, policy, limits).map_err(runtime_error))
    }))
}

pub(super) fn decode_stream(
    stream: OperationDataStream,
    policy: BufferPolicy,
    limits: BufferLimits,
) -> SegmentedStream {
    Box::pin(stream.map(move |item| {
        item.and_then(|bytes| decode(bytes, policy, limits).map_err(runtime_error))
    }))
}

#[cfg(test)]
mod tests;
