// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

#[test]
fn retained_local_initialization_is_closed_without_claiming_remote_copy_completion() {
    for required in [false, true] {
        for eos in [false, true] {
            let mut receiver =
                SecondaryCopyReceiver::for_retained_state(config(required, eos)).unwrap();
            assert!(receiver.queue().is_none());
            assert!(!receiver.is_copy_in_progress());
            assert!(!receiver.all_operations_received());
            assert!(receiver.copy_queue_closed());
            assert!(!receiver.enqueued_end_of_stream());
            assert!(!receiver.stream_faulted());
            assert!(receiver.all_operations_acked());
            assert_eq!(receiver.pending_ack_progress(), 0);
            assert_eq!(
                receiver.capture_ack(),
                SecondaryCopyAck {
                    received: -1,
                    applied: -1,
                }
            );
            assert_eof(&mut receiver.stream());
            assert!(!receiver.enqueued_end_of_stream());
            let rejected = receive(&mut receiver, data(1, required), false);
            assert_eq!(rejected.admission, CopyAdmission::AdmissionClosed);
            assert!(!rejected.progress_done);
            assert!(!rejected.check_copy_sender);
            assert!(!receiver.all_operations_received());
            assert!(!receiver.enqueued_end_of_stream());
            assert_eof(&mut receiver.stream());
            if eos {
                assert!(matches!(
                    receiver.enqueue_eos(&root()),
                    Err(SecondaryCopyError::Stream(StreamError::Closed))
                ));
            }
        }
    }
}

#[test]
fn retained_copy_constructor_rejects_a_completed_copy_claim_and_foreign_ack() {
    assert!(matches!(
        SecondaryCopyReceiver::for_retained_state(SecondaryCopyConfig {
            copy_done: true,
            ..config(true, true)
        }),
        Err(SecondaryCopyError::InvalidInitialization)
    ));
    let mut receiver = SecondaryCopyReceiver::for_retained_state(config(true, true)).unwrap();
    let old = data(1, true);
    let work = old.acknowledge().unwrap().unwrap();
    assert!(matches!(
        receiver.process_ack(work).dispatch(),
        Err(SecondaryCopyError::ForeignAck)
    ));
    assert!(!receiver.all_operations_received());
    assert!(!receiver.enqueued_end_of_stream());
}
