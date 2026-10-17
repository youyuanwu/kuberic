// Copyright (c) Microsoft Corporation. All rights reserved.
// Selected native-derived expected assertions, MIT; see
// ../../LICENSES/service-fabric-MIT.txt and
// ../../docs/features/kuberic/sf-v1-port-provenance.md.
//
// These tests validate the P1 expectation oracle/inventory, not a native or
// ported replicator. Actual current-engine witnesses remain in public_v1_recovery.
#[path = "support/sf_v1_vectors.rs"]
mod vectors;

use std::collections::BTreeSet;
use vectors::*;

#[test]
fn source_ledger_has_complete_family_contract_and_scenario_inventory() {
    assert_eq!(NATIVE_PIN, "3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d");
    assert_eq!(NATIVE_ROOT, "src/prod/src/Reliability/Replication/");
    let ids: BTreeSet<_> = SCENARIOS.iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), 33);
    assert_eq!(SCENARIOS.len(), ids.len());
    let expected = [
        "A1", "Q1", "Q2", "Q3", "Q4", "M1", "M2", "M3", "M4", "M5", "S1", "S2", "S3", "R1", "R2",
        "R3", "C1", "C2", "C3", "C4", "C5", "C6", "T1", "T2", "D1", "L1", "L2", "L3", "E1", "E2",
        "H1", "H2", "H3",
    ]
    .into_iter()
    .collect();
    assert_eq!(ids, expected);
    for scenario in SCENARIOS {
        assert!(
            scenario.source.contains(':') && scenario.source.contains('-'),
            "{scenario:?}"
        );
        assert!(!scenario.input.trim().is_empty() && !scenario.expected.trim().is_empty());
        if let Some(case) = scenario.translated_test {
            assert!(!case.is_empty());
            assert!(scenario.source.contains(".Test.cpp:"));
        }
    }
    assert_eq!(FAMILIES.len(), 11);
    let mut referenced = BTreeSet::new();
    for (i, family) in FAMILIES.iter().enumerate() {
        assert_eq!(family.id, format!("F{:02}", i + 1));
        assert!(
            !family.name.is_empty()
                && !family.destination.is_empty()
                && !family.substitution.is_empty()
        );
        assert!(!family.scenarios.is_empty());
        for id in family.scenarios {
            assert!(ids.contains(id), "{} -> {id}", family.id);
            referenced.insert(*id);
        }
    }
    assert_eq!(ids, referenced);
    assert_eq!(CONTRACTS.len(), 15);
    for (i, (contract, scenarios)) in CONTRACTS.iter().enumerate() {
        assert_eq!(*contract, format!("FR-{:03}", i + 1));
        assert!(!scenarios.is_empty());
        assert!(scenarios.iter().all(|id| ids.contains(id)));
    }
    assert_eq!(CAPABILITIES.len(), 29);
    let names: BTreeSet<_> = CAPABILITIES.iter().map(|row| row.0).collect();
    assert_eq!(names.len(), CAPABILITIES.len());
    for (_, scenario, observation) in CAPABILITIES {
        assert!(ids.contains(scenario) && !observation.is_empty());
    }
}

#[test]
fn evidence_classes_do_not_promote_expectations_or_ksf_to_port_execution() {
    assert_eq!(PORT_STATUS, "pending-P2+");
    assert_eq!(NATIVE_EXECUTION, "not-run");
    assert_eq!(PORT_EXECUTION, "not-implemented/not-run");
    assert!(DERIVATION_REVIEW.starts_with("source-reviewed"));
    assert_eq!(CURRENT_EVIDENCE.len(), 8);
    assert_eq!(
        CURRENT_EVIDENCE
            .iter()
            .filter(|e| e.1 == "current-witness")
            .count(),
        3
    );
    assert!(CURRENT_EVIDENCE.iter().all(|e| !e.0.is_empty()
        && !e.2.is_empty()
        && ["current-witness", "current-source-only"].contains(&e.1)));
    // KSF remains the existing 16-case structural/current-contract catalog.
    #[cfg(feature = "testing")]
    kuberic_runtime::testing::assert_public_v1_conformance_subject("built-in-replacement");
}

// Direct source predicate slices, not a manager implementation. Inputs include
// precomputed per-configuration progress; PC/CC queue arithmetic stays P2/P3.
fn all_expected(enough: bool, issued: i64, all_current_apply: i64) -> bool {
    enough && all_current_apply >= issued
}

fn write_quorum_expected(
    enough: bool,
    captured: i64,
    quorum_apply: i64,
    marked: Option<i64>,
) -> bool {
    enough && quorum_apply >= captured && marked.is_none_or(|m| m >= quorum_apply)
}

#[test]
fn native_catchup_predicates_reject_frozen_tail_and_weakened_successor() {
    for &(tail, all, expected) in ALL_CASES {
        assert_eq!(all_expected(true, tail, all), expected); // M2
    }
    for &(enough, b, q, m, expected) in QUORUM_CASES {
        assert_eq!(write_quorum_expected(enough, b, q, m), expected); // M3
    }
    assert!(!all_expected(false, 12, 12));
    // These wrong alternatives are explicit current/target counterexamples,
    // NOT executions of the current engine's catch-up implementation.
    assert_ne!(all_expected(true, 13, 12), all_expected(true, 12, 12));
    let (b, q, m) = (10, 11, 10);
    assert_ne!(write_quorum_expected(true, b, q, Some(m)), q >= b && m >= b);
}

#[test]
fn native_queue_and_sender_selected_assertions_keep_receive_apply_and_retention_distinct() {
    // Q1 source-derived fixed gap trace. No queue implementation is executed.
    let trace = GAP_TRACE;
    assert_eq!(trace.iter().map(|r| r.2).collect::<Vec<_>>(), [0, 0, 0, 2]);
    let mut skip_gap = trace.to_vec();
    skip_gap[2].2 = 2;
    let valid_gap_trace = |t: &[(i64, i64, i64)]| t == [(2, 0, 0), (1, 2, 0), (2, 2, 0), (1, 2, 2)];
    assert!(valid_gap_trace(trace));
    assert!(!valid_gap_trace(&skip_gap));
    // R1 translated TestReceiveACKProgress: fixed native pending assertions.
    for &(received, applied, items, pending) in SENDER_STEPS {
        let remaining: Vec<_> = items.iter().copied().filter(|n| *n > received).collect();
        assert_eq!(remaining, pending);
        assert!(applied <= received);
    }
    assert_ne!(SENDER_STEPS[1].0, SENDER_STEPS[1].1);
    // Q2 selected native memory assertions; retained completion does not free100.
    for &(total, completed_retained, next_bytes, limit, accepted, after) in MEMORY_CASES {
        let fits = total - completed_retained + next_bytes <= limit;
        assert_eq!(fits, accepted);
        assert_eq!(
            if fits {
                let evict = if total + next_bytes > limit {
                    completed_retained
                } else {
                    0
                };
                total - evict + next_bytes
            } else {
                total
            },
            after
        );
    }
    // Q3/Q4 expected values are history, not latest progress.
    for &(progress, first_retained, expected) in CAPABILITY_CASES {
        let capability = first_retained.unwrap_or(0);
        assert_eq!(capability, expected);
        assert_ne!(capability, progress); // not a tail alias
    }
    for &(completed, floor) in TRIM_CASES {
        let retained_floor = if completed == 0 {
            0
        } else {
            (completed - 3).max(1)
        };
        assert_eq!(retained_floor, floor);
    }
}

#[test]
fn native_ack_policy_defaults_and_exposure_expectations_are_explicit() {
    let explicit_ack = |persisted: bool, require: bool| persisted || require;
    for &(p, r, expected) in ACK_POLICY_CASES {
        assert_eq!(explicit_ack(p, r), expected);
    }
    let fault_outcome = |valid_kind: bool, enabled: bool| match (valid_kind, enabled) {
        (false, _) => "InvalidArgument",
        (true, false) => "InvalidOperation",
        (true, true) => "Success",
    };
    assert_eq!(fault_outcome(false, false), "InvalidArgument");
    assert_eq!(fault_outcome(true, false), "InvalidOperation");
    assert_eq!(fault_outcome(true, true), "Success");
    let public_eos = false;
    let internal_eos = true;
    assert_ne!(public_eos, internal_eos);
    let update_retry = |specified: Option<u64>| specified.is_none_or(|v| v == 5000);
    assert!(update_retry(None) && update_retry(Some(5000)));
    assert!(!update_retry(Some(6000)));
    for &(legacy_max, primary_ex3, expected) in LEGACY_COUNT_CASES {
        // With legacy memory specified, no legacy max flag zeros both counts;
        // a later EX3 field overrides only its role before comparison.
        let both = legacy_max.unwrap_or(0);
        assert_eq!((primary_ex3.unwrap_or(both), both), expected);
    }
    for (name, expected) in [
        ("settings2", "QI present"),
        ("stream2", "QI present"),
        ("specific-quorum", "public marker present"),
        ("atomic-group", "absent"),
        (
            "internal-batch-reservation",
            "advertised internally; E_NOTIMPL",
        ),
    ] {
        let row = CAPABILITIES.iter().find(|c| c.0 == name).unwrap();
        assert!(row.2.starts_with(expected));
    }
}

#[test]
fn native_copy_expected_gates_reject_early_completion_and_late_retarget() {
    // C4: the target is sampled at enumeration EOF, not frozen copy prefix.
    for &(final_copy_ack, applied, issued_at_eof, expected) in BUILD_CASES {
        assert_eq!(final_copy_ack && applied >= issued_at_eof, expected);
    }
    // C5: only pending LSNSet can be raised; explicit freshness/pressure events.
    let raise = |pending: bool, old, new| pending && new > old;
    for &(pending, old, new, expected) in RETARGET_CASES {
        assert_eq!(raise(pending, old, new), expected);
    }
    // C2: native provider null is EOF; a zero-length data item is not null.
    let enumeration = [Some(&b""[..]), Some(&b"x"[..]), None];
    assert_eq!(
        enumeration.iter().take_while(|item| item.is_some()).count(),
        2
    );
    assert_ne!(enumeration[0], enumeration[2]);
    assert_eq!(enumeration.len(), 3); // no post-EOF poll in the expected trace
}

#[test]
fn protected_buffer_shape_expectations_do_not_encode_or_flatten_segments() {
    // D1 is an inventory of lossless shapes, NOT the P2 framing codec.
    let shapes: Vec<Vec<&[u8]>> = vec![
        vec![],
        vec![b""],
        vec![b"", b""],
        vec![b"ab", b"c"],
        vec![b"a", b"bc"],
    ];
    let distinct: BTreeSet<_> = shapes.iter().cloned().collect();
    assert_eq!(distinct.len(), 5);
    assert_eq!(shapes[3].concat(), shapes[4].concat());
    assert_ne!(shapes[3], shapes[4]); // flattening alone loses segmentation
}
