use futures::stream;

use super::*;

const LIMITS: BufferLimits = BufferLimits {
    max_segments: 8,
    max_payload_bytes: 64,
};

#[test]
fn explicit_framing_preserves_zero_single_empty_and_repeated_empty_segments() {
    for segments in [
        vec![],
        vec![Bytes::new()],
        vec![Bytes::new(), Bytes::new(), Bytes::new()],
        vec![Bytes::from_static(b"ab"), Bytes::from_static(b"c")],
        vec![Bytes::from_static(b"a"), Bytes::from_static(b"bc")],
        vec![
            Bytes::from_static(b"\0\xff"),
            Bytes::new(),
            Bytes::from_static(b"x"),
        ],
    ] {
        let frame = encode(&segments, BufferPolicy::FramedSegments, LIMITS).unwrap();
        let decoded = decode(frame.clone(), BufferPolicy::FramedSegments, LIMITS).unwrap();
        assert_eq!(decoded, segments);
        for segment in decoded {
            assert!(segment.is_empty() || segment.as_ptr() >= frame.as_ptr());
        }
    }
}

#[test]
fn raw_never_sniffs_magic_and_empty_raw_is_one_buffer() {
    let magic_raw = Bytes::from_static(b"SFVB\x01\x00\x00\x00\x00");
    assert_eq!(
        decode(magic_raw.clone(), BufferPolicy::RawSingle, LIMITS).unwrap(),
        vec![magic_raw.clone()]
    );
    assert_eq!(
        encode(
            std::slice::from_ref(&magic_raw),
            BufferPolicy::RawSingle,
            LIMITS
        )
        .unwrap(),
        magic_raw
    );
    assert_eq!(
        decode(Bytes::new(), BufferPolicy::RawSingle, LIMITS).unwrap(),
        vec![Bytes::new()]
    );
    for segments in [vec![], vec![Bytes::new(), Bytes::new()]] {
        assert_eq!(
            encode(&segments, BufferPolicy::RawSingle, LIMITS),
            Err(BufferError::NotSingle)
        );
    }
}

#[test]
fn frame_parser_rejects_truncation_trailing_lengths_versions_and_bounds() {
    let frame = encode(
        &[Bytes::from_static(b"payload"), Bytes::new()],
        BufferPolicy::FramedSegments,
        LIMITS,
    )
    .unwrap();
    for length in 0..frame.len() {
        assert!(decode(frame.slice(..length), BufferPolicy::FramedSegments, LIMITS).is_err());
    }
    let mut trailing = frame.to_vec();
    trailing.push(0);
    assert_eq!(
        decode(trailing.into(), BufferPolicy::FramedSegments, LIMITS),
        Err(BufferError::Malformed)
    );
    let mut overflow = frame.to_vec();
    overflow[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode(overflow.into(), BufferPolicy::FramedSegments, LIMITS),
        Err(BufferError::Malformed)
    );
    let mut unknown = frame.to_vec();
    unknown[4] = 2;
    assert_eq!(
        decode(unknown.into(), BufferPolicy::FramedSegments, LIMITS),
        Err(BufferError::UnsupportedVersion(2))
    );
    let mut count = frame.to_vec();
    count[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode(count.into(), BufferPolicy::FramedSegments, LIMITS),
        Err(BufferError::TooManySegments)
    );
    let exact = BufferLimits {
        max_segments: 2,
        max_payload_bytes: 7,
    };
    assert!(
        decode(frame.clone(), BufferPolicy::FramedSegments, exact).is_ok(),
        "framing overhead is not logical payload"
    );
    assert_eq!(
        decode(
            frame,
            BufferPolicy::FramedSegments,
            BufferLimits {
                max_payload_bytes: 6,
                ..exact
            }
        ),
        Err(BufferError::TooLarge)
    );
    let empties = vec![Bytes::new(); LIMITS.max_segments];
    let exact_count = encode(&empties, BufferPolicy::FramedSegments, LIMITS).unwrap();
    assert_eq!(
        decode(exact_count, BufferPolicy::FramedSegments, LIMITS).unwrap(),
        empties
    );
    assert_eq!(
        encode(
            &vec![Bytes::new(); LIMITS.max_segments + 1],
            BufferPolicy::FramedSegments,
            LIMITS,
        ),
        Err(BufferError::TooManySegments)
    );
}

#[tokio::test]
async fn protected_stream_wrappers_preserve_items_errors_and_eof_separately() {
    let source: SegmentedStream = Box::pin(stream::iter(vec![
        Ok(vec![]),
        Err(RuntimeError::Application("provider failure".into())),
        Ok(vec![Bytes::new()]),
        Ok(vec![Bytes::new(), Bytes::new()]),
    ]));
    let protected = encode_stream(source, BufferPolicy::FramedSegments, LIMITS);
    let mut decoded = decode_stream(protected, BufferPolicy::FramedSegments, LIMITS);
    assert!(decoded.next().await.unwrap().unwrap().is_empty());
    assert!(
        matches!(decoded.next().await.unwrap(), Err(RuntimeError::Application(message)) if message == "provider failure")
    );
    assert_eq!(decoded.next().await.unwrap().unwrap(), vec![Bytes::new()]);
    assert_eq!(
        decoded.next().await.unwrap().unwrap(),
        vec![Bytes::new(), Bytes::new()]
    );
    assert!(decoded.next().await.is_none());
    let bad: OperationDataStream = Box::pin(stream::iter(vec![
        Ok(Bytes::new()),
        Ok(Bytes::from_static(b"bad")),
    ]));
    let errors = decode_stream(bad, BufferPolicy::FramedSegments, LIMITS)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(errors.len(), 2, "bad items must not masquerade as EOF");
    assert!(errors.iter().all(Result::is_err));
}
