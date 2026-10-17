// Copyright (c) Microsoft Corporation. All rights reserved.
// Native-derived expectations / selected translated test assertions, MIT:
// ../../../LICENSES/service-fabric-MIT.txt
// Selection, transformations and evidence limits:
// ../../../docs/features/kuberic/sf-v1-port-provenance.md
//
// This is test-private expectation data, NOT a ported queue/sender/engine.

pub const NATIVE_PIN: &str = "3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d";
pub const NATIVE_ROOT: &str = "src/prod/src/Reliability/Replication/";

#[derive(Debug)]
pub struct Scenario {
    pub id: &'static str,
    /// Paths are relative to NATIVE_ROOT unless prefixed with src/.
    pub source: &'static str,
    pub translated_test: Option<&'static str>,
    pub input: &'static str,
    pub expected: &'static str,
}

macro_rules! scenario {
    ($id:literal, $source:literal, $test:expr, $input:literal, $expected:literal) => {
        Scenario {
            id: $id,
            source: $source,
            translated_test: $test,
            input: $input,
            expected: $expected,
        }
    };
}

// Fixed inputs replace random scheduling, sockets and sleeps. Test names below
// identify selected translated assertions, never executions of native tests.
pub const SCENARIOS: &[Scenario] = &[
    scenario!(
        "A1",
        "ReplicationQueueManager.cpp:369-408;ReplicaManager.cpp:349-390;PublicApi.Test.cpp:454-504",
        Some("BeginReplicateReturnsSOKIfAsyncOperationFails"),
        "progress10; enqueue11 accepted, quorum held; enqueue12 full; Begin on secondary",
        "assigned11 before completion; full leaves next12; Begin S_OK, End NotPrimary"
    ),
    scenario!(
        "Q1",
        "OperationQueue.cpp:677-737,990-1047;OperationQueue.Test.cpp:1049-1095",
        Some("TestOutOfOrderOperations1/2 (reduced fixed permutation)"),
        "start1; receive2,1; ACK2,1",
        "contiguous received0,2; completed0,2: later ACK cannot cross gap"
    ),
    scenario!(
        "Q2",
        "OperationQueue.Test.cpp:435-461;OperationQueue.cpp:256-318",
        Some("TestEnqueueOperationsWithMemoryLimit"),
        "start10 initial4 max8 memory600 retained; enqueue 4x100; complete1; enqueue301,100",
        "bytes400 retained100; reject301 leaves400; accept100 gives500; source-derived neighbor300 evicts completed100 and gives600"
    ),
    scenario!(
        "Q3",
        "OperationQueue.Test.cpp:607-615;OperationQueue.h:76-87;ReplicationQueueManager.cpp:206-227",
        Some("TestCompleteBeforeCommit"),
        "enqueue; complete before commit; commit then complete; separately progress100 no retained entry",
        "false then true; empty retained capability0 not100"
    ),
    scenario!(
        "Q4",
        "OperationQueue.Test.cpp:867-901;OperationQueue.cpp:371-399",
        Some("TestSecondaryQueueTrim"),
        "secondary count8 primary history4; commit1..8; complete1,4,5,8; duplicate retained item",
        "first completed sequence0,1,1,2,5; last enqueued8; duplicate no count/byte increment"
    ),
    scenario!(
        "M1",
        "ReplicaManager.Test.cpp:154-220;ReplicaManager.cpp:349-421",
        Some("TestUpdateConfigurationWithQuorumIgnorePreviousConfig / NoQuorum"),
        "W2 remote apply[5,6]; W3 [2,3,5]; W greater than available count",
        "quorum/reclaim6/5 and3/2; insufficient configured quorum pending"
    ),
    scenario!(
        "M2",
        "PrimaryReplicator.CatchupAsyncOperation.cpp:43-59;ReplicationQueueManager.cpp:319-358",
        None,
        "All T12 Acc11; Acc12; new evaluation T13 Acc12; Acc13",
        "false,true,false,true; completed waits are not reopened"
    ),
    scenario!(
        "M3",
        "PrimaryReplicator.CatchupAsyncOperation.cpp:62-104;ReplicationQueueManager.cpp:261-272",
        None,
        "B10 Qcc11 M10 then11; no marked successor; ordinary tail12",
        "false then true; unmarked uses Qcc>=B; tail alone does not recapture B"
    ),
    scenario!(
        "M4",
        "ReplicationQueueManager.cpp:319-357;ReplicaManager.cpp:2471-2524",
        None,
        "CC W1 no remote tail12; PC W1 remote7",
        "CC sentinel normalized12; previous constraint caps normal commitment7"
    ),
    scenario!(
        "M5",
        "ReplicaManager.cpp:1278-1333,2328-2362;ReplicationQueueManager.cpp:261-272,360-366",
        None,
        "B10 commit12 complete11; catchup config on; AllowMultipleQuorumSet off/on; required PC missing",
        "off cap10/10; on no such cap; missing PC blocks queue update; config at tail13 captures B13"
    ),
    scenario!(
        "S1",
        "SecondaryReplicator.cpp:165-193;SecondaryReplicationReceiver.cpp:177-202,632-740",
        None,
        "persisted/required F/F,F/T,T/F,T/T; receive1 before service ACK or dequeue",
        "implicit,explicit,explicit,explicit; persisted current1 complete0; implicit complete at dispatch"
    ),
    scenario!(
        "S2",
        "SecondaryCopyReceiver.cpp:24-80;OperationStream.GetOperationAsyncOperation.cpp:116-136",
        None,
        "two getters consume shared receiver; reacquire after terminal; demotion copyDone",
        "no replay; terminal stays terminal; demotion empty copy closed"
    ),
    scenario!(
        "S3",
        "ComFromBytesOperation.cpp:167-224",
        None,
        "retire normal NotCalled callback; late ACK; repeat completed ACK; EOS callback",
        "canceled normal callback cannot advance receiver; repeat InvalidOperation; EOS not canceled this way"
    ),
    scenario!(
        "R1",
        "ReliableOperationSender.Test.cpp:102-143;ReliableOperationSender.cpp:601-679",
        Some("TestReceiveACKProgress"),
        "add1,2,3; ACK(1,0),(2,0); add4; ACK(2,1),(3,2),(4,4)",
        "pending[2,3],[3],[3,4],[4],[]; timer off at empty"
    ),
    scenario!(
        "R2",
        "ReliableOperationSender.cpp:548-579,636-679,732-755",
        None,
        "active sender window8 max8 noAckSinceLastCallback=true; retry; receive ACK advances with window4; duplicate ACK; receive4 apply2 nothing pending",
        "retry window4; actual ACK progress doubles4->8; duplicate leaves8; RequestAck not quorum success"
    ),
    scenario!(
        "R3",
        "RemoteSession.cpp:585-611;AckSender.cpp:64-112,174-208",
        None,
        "combined replication/copy ACK; batch15ms retry5000ms; final pending ACK at Close",
        "sender progress,build gate,progress callback,copy ACK; batch<retry; Close flushes final ACK"
    ),
    scenario!(
        "C1",
        "ComReplicator.h:12-35;ComReplicator.cpp:1002-1050;ComOperationStream.cpp:48-78;OperationStream.cpp:66-85",
        None,
        "QI settings2,stream2,specific-quorum,atomic; internal batch/reserve; fault disabled/invalid kind",
        "present,present,present,absent; E_NOTIMPL; InvalidOperation/InvalidArgument; V2 marker distinct"
    ),
    scenario!(
        "C2",
        "ComProxyAsyncEnumOperation.cpp:108-121,182-195;CopyAsyncOperation.cpp:171-211;SecondaryReplicator.cpp:982-1003",
        None,
        "provider [empty bytes,x,EOF]; persisted target context vs volatile",
        "three polls two items one EOF no fourth poll; target context forwarded / callback omitted"
    ),
    scenario!(
        "C3",
        "CopyAsyncOperation.Test.cpp:418-449;CopyAsyncOperation.cpp:42-49,242-254;SecondaryCopyReceiver.cpp:388-395",
        Some("CopyAsync_QueueFull_Cancelled"),
        "native counts15/15/4 cancel at12; fixed full queue parked item then cancel",
        "one parked item no additional poll while full; copy count64/1024 with no maximum-memory limit; OperationCanceled never successful EOF"
    ),
    scenario!(
        "C4",
        "PrimaryReplicator.BuildIdleAsyncOperation.cpp:111-144;CopySender.cpp:524-579;ReplicationQueueManager.cpp:165-195",
        None,
        "prefix10; EOF issued12; final copy ACK apply11 then12; quiet baseline10 tail10",
        "target12; pending then replication gate complete; quiet prefix needs no invented op10 record"
    ),
    scenario!(
        "C5",
        "CopySender.cpp:555-579;ReplicaManager.cpp:745-774,1983-2022",
        None,
        "pending target12; freshness commit13; pressure issued15; lower/equal target; completed state",
        "raise13,15; lower/equal refused; ReplCompleted cannot retarget"
    ),
    scenario!(
        "C6",
        "ComReplicator.cpp:490-582,639-808;reconfig.h:12-17;src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h:136-207",
        None,
        "public defaults vs internal EOS; public specified retry5000->6000; legacy initial128; memory600 without/with max1024; EX3 primary max2048 override",
        "public EOS false vs internal true; changed retry InvalidArgument; legacy sets both roles; memory-only effective max0/0 vs explicit1024/1024; EX3 overrides per role before comparison; credentials validated/updated"
    ),
    scenario!(
        "T1",
        "ReplicationTransport.Test.cpp:594-617;PrimaryReplicator.cpp:632-683",
        Some("TestDropMessageToNotMatch (routing assertions only)"),
        "wrong endpoint; same replica wrong incarnation; matching identity",
        "drop; no valid session update; matching still requires role/epoch admission"
    ),
    scenario!(
        "T2",
        "ReplicationSession.cpp:217-243,364-384,423-463;RemoteSession.cpp:757-790",
        None,
        "repl/copy vs StartCopy/RequestAck; full/too-large/accepted sends; slow idle timer",
        "expiry5000ms vs unbounded; stop batch/fault/not service ACK; slow-peer notification retries"
    ),
    scenario!(
        "D1",
        "src/prod/src/idl/public/FabricRuntime.idl:636-683;ReplicationOperationHeader.h:25-42;ReplicationAckMessageBody.h:17-24;ComReplicator.cpp:53-83,1094-1118",
        None,
        "[],[empty],[empty,empty],[ab,c],[a,bc]; item empty; sending/original epoch; receive/apply",
        "all shapes distinct; data not EOF; epochs and sequence spaces distinct; managed null Buffer or Count0 E_POINTER, Offset not bounds-checked here; codec pending P2"
    ),
    scenario!(
        "L1",
        "ReplicatorState.cpp:39-126",
        None,
        "Opened->Primary; Primary->Active; Idle->Active; Active->Idle; RoleNone",
        "initial primary; demote; retain secondary; reject; None not initial Opened"
    ),
    scenario!(
        "L2",
        "ReplicationQueueManager.cpp:58-160;Replicator.OnDatalossAsyncOperation.cpp:82-126",
        None,
        "promotion with incomplete entries; demotion; changed provider progress7 vs unchanged",
        "transfer completed only; demotion tail==commit and trim; changed next8/reset history, unchanged no reset"
    ),
    scenario!(
        "L3",
        "Replicator.CloseAsyncOperation.cpp:134-192;src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp:209-276",
        None,
        "direct child Close error; outer host observes failed child",
        "direct error/fault; outer abort releases refs and Success with diagnostics, not graceful-child success"
    ),
    scenario!(
        "E1",
        "ReplicaManager.cpp:871-920;PrimaryReplicator.UpdateEpochAsyncOperation.cpp:31-90",
        None,
        "tail12 commit10; new epoch; duplicate primary epoch; older epoch",
        "provider previous boundary12; duplicate still callback; older fails"
    ),
    scenario!(
        "E2",
        "SecondaryReplicator.cpp:951-1003",
        None,
        "StartCopy existing epoch(1,2); incoming(1,1) and(1,3)",
        "stale drops; higher faults receiver not stream restart"
    ),
    scenario!(
        "H1",
        "src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp:34-145",
        None,
        "stable secondary; ready primary; same-primary reconfig; demotion during/after catchup",
        "NotPrimary/NotPrimary; Granted/quorum-dependent; read preserved; Granted/TryAgain then TryAgain/TryAgain"
    ),
    scenario!(
        "H2",
        "src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp:58-71,101-114",
        None,
        "promotion transition/catchup; catchup completed; completed",
        "TryAgain/TryAgain; Granted/TryAgain; Granted/quorum-dependent"
    ),
    scenario!(
        "H3",
        "BatchedHealthReporter.Test.cpp:107-135;BatchedHealthReporter.cpp:81-125;src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp:272-312",
        Some("TestWarningsWithinIntervalReport (timer replaced with explicit event)"),
        "schedule Temp1 then Temp2; fire timer; Close after warning",
        "report Temp2 once; Close OK report total2; acceptance not platform convergence"
    ),
];

pub struct Family {
    pub id: &'static str,
    pub name: &'static str,
    pub scenarios: &'static [&'static str],
    pub destination: &'static str,
    pub substitution: &'static str,
}

pub const FAMILIES: &[Family] = &[
    Family {
        id: "F01",
        name: "factory/facade/role state",
        scenarios: &["A1", "C1", "L1"],
        destination: "v1/replicator.rs,state.rs + existing facade",
        substitution: "COM/refcounts -> owned Rust; preserve role admissibility and outcomes",
    },
    Family {
        id: "F02",
        name: "primary/replica/queue managers",
        scenarios: &["A1", "M1", "M2", "M3", "M4", "M5", "E1"],
        destination: "v1/primary.rs,replica_manager.rs",
        substitution: "Begin/End -> owned futures; preserve assignment, CC/PC and captured/live boundaries",
    },
    Family {
        id: "F03",
        name: "operation queue",
        scenarios: &["Q1", "Q2", "Q3", "Q4", "L2"],
        destination: "v1/operation_queue.rs",
        substitution: "ring -> owned storage; preserve all heads, gaps, bytes/counts, trim/reset/transfer",
    },
    Family {
        id: "F04",
        name: "secondary/receivers",
        scenarios: &["S1", "S2", "S3", "E2"],
        destination: "v1/secondary.rs,receiver.rs",
        substitution: "async executor allowed; preserve dispatch vs completion and receiver lifetime",
    },
    Family {
        id: "F05",
        name: "operation/data/stream/provider",
        scenarios: &["D1", "S2", "S3", "C1", "C2"],
        destination: "existing protected facade + v1 adapters",
        substitution: "owned callbacks; Bytes/boxed Result<Bytes> stream/i64 unchanged; explicit framing later",
    },
    Family {
        id: "F06",
        name: "session/sender/ACK",
        scenarios: &["R1", "R2", "R3", "T2"],
        destination: "v1/session.rs,sender.rs,ack.rs",
        substitution: "tasks/injected clocks replace timers; not retries/windows/independent ACKs",
    },
    Family {
        id: "F07",
        name: "copy/context/build",
        scenarios: &["C2", "C3", "C4", "C5"],
        destination: "v1/copy.rs + provider adapters",
        substitution: "futures/queues; preserve target context, serial poll, parked item, EOF and build gates",
    },
    Family {
        id: "F08",
        name: "transport/demux/headers",
        scenarios: &["T1", "T2", "D1"],
        destination: "v1/transport.rs + future replication proto",
        substitution: "tonic carrier only; identity/epochs/sequence/expiry/backpressure preserved, no native wire claim",
    },
    Family {
        id: "F09",
        name: "epoch/recovery/host",
        scenarios: &["E1", "E2", "L2", "L3", "H1", "H2"],
        destination: "v1 + existing host/controller adapters",
        substitution: "no RA/FM port; queue observations separate from optional provider durability",
    },
    Family {
        id: "F10",
        name: "settings/extensions/reporting",
        scenarios: &["C1", "C6", "H3"],
        destination: "existing facade/settings/partition adapters P4/P7",
        substitution: "Rust representations; preserve defaults, validation, unsupported outcomes",
    },
    Family {
        id: "F11",
        name: "async lifetime/diagnostics",
        scenarios: &["R3", "C3", "H3"],
        destination: "Rust async ownership/clock + host reporting",
        substitution: "no Common/KTL copy; preserve drain/cancel ownership and policy, not platform plumbing",
    },
];

pub const CONTRACTS: &[(&str, &[&str])] = &[
    ("FR-001", &["A1", "C3"]),
    ("FR-002", &["A1"]),
    ("FR-003", &["Q1", "S1", "S3", "R1"]),
    ("FR-004", &["S2", "C2", "C3", "C4", "C5", "L1"]),
    ("FR-005", &["Q3", "Q4", "L2", "E1", "E2"]),
    ("FR-006", &["M1", "M2", "M3", "M4", "M5"]),
    ("FR-007", &["H1", "H2", "L1"]),
    ("FR-008", &["S3", "C3", "L3"]),
    ("FR-009", &["C1", "C6", "H3"]),
    ("FR-010", &["T1", "L2"]),
    ("FR-011", &["D1"]),
    ("FR-012", &["A1", "S1", "L3"]),
    ("FR-013", &["Q1", "M1", "S1", "R1", "C2", "L1"]),
    ("FR-014", &["R1", "R2", "R3", "T1", "T2", "D1"]),
    ("FR-015", &["C1", "C6", "H3"]),
];

// Each row names a concrete deterministic classification/default/update case,
// not a claim that Kuberic currently implements the capability.
pub const CAPABILITIES: &[(&str, &str, &str)] = &[
    (
        "settings2",
        "C1",
        "QI present; get returns effective merged settings",
    ),
    (
        "stream2",
        "C1",
        "QI present even with fault/EOS mode disabled",
    ),
    (
        "specific-quorum",
        "C1",
        "public marker present; M3 uses M>=Qcc",
    ),
    (
        "atomic-group",
        "C1",
        "absent on ComReplicator; ServiceGroup wrappers excluded",
    ),
    (
        "internal-batch-reservation",
        "C1",
        "advertised internally; E_NOTIMPL including batch stream",
    ),
    (
        "internal-queue-counters",
        "C1",
        "implemented query; not scalar progress/platform parity",
    ),
    (
        "managed-buffers",
        "D1",
        "null Buffer or Count0 E_POINTER; Offset not bounds-checked here; then ordinary BeginReplicate",
    ),
    (
        "v2-copy-latest",
        "C1",
        "separate internal provider marker; V2 implementation excluded",
    ),
    (
        "stream-fault",
        "C1",
        "invalid kind InvalidArgument; disabled InvalidOperation",
    ),
    (
        "EOS-ACK",
        "C2",
        "ordinary null EOF vs enabled explicit EOS requires separate ACK",
    ),
    (
        "persisted-state",
        "S1",
        "persisted true forces explicit ACK even require=false",
    ),
    (
        "load-fault-move-health",
        "H3",
        "scoped acceptance not immediate convergence",
    ),
    (
        "retry",
        "C6",
        "5000ms default; specified6000 public update InvalidArgument",
    ),
    ("ack-batch", "R3", "15ms default < retry5000"),
    (
        "require-ack",
        "S1",
        "public false; volatile false implicit vs persisted explicit",
    ),
    (
        "copy-queue",
        "C3",
        "public initial64/max1024 count limits; no maximum-memory limit in copy queues",
    ),
    (
        "legacy-initial-replication-queue",
        "C6",
        "default64; specified128 sets both effective roles, later EX3 overrides per role",
    ),
    (
        "legacy-max-replication-queue",
        "C6",
        "default1024; specified1024 sets both effective roles; EX3 overrides before comparison",
    ),
    (
        "legacy-replication-memory",
        "C6",
        "default0; specified600 without legacy max flag sets both effective counts0; with max1024 preserves counts1024",
    ),
    (
        "primary-queue",
        "Q2",
        "public initial64/max1024/memory0; retained bytes count",
    ),
    (
        "secondary-queue",
        "Q4",
        "public initial64/max2048/memory0; trim history separately",
    ),
    (
        "message-size",
        "T2",
        "public max52428800; too-large faults, not successful send",
    ),
    (
        "clear-ACKed",
        "Q4",
        "public false; retained history is separate from progress",
    ),
    (
        "EOS-fault-mode",
        "C6",
        "public false vs internal macro true",
    ),
    (
        "pending-quorum-close",
        "C6",
        "public timeout0; no invented finite universal drain",
    ),
    (
        "addresses",
        "C6",
        "localhost:0; public listen/publish not compared as breaking fields",
    ),
    (
        "security",
        "C6",
        "credential updates validated/enforced; not arbitrary timer upgrades",
    ),
    (
        "internal-policy",
        "M5",
        "AllowMultipleQuorumSet true; idle freshness60s explicit tick",
    ),
    (
        "max-pending-ACK",
        "R3",
        "32 default; dynamic store not public arbitrary upgrade",
    ),
];

pub const CURRENT_EVIDENCE: &[(&str, &str, &str)] = &[
    (
        "A1/E1",
        "current-witness",
        "public_v1_recovery::read_fencing::sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch",
    ),
    (
        "S1",
        "current-witness",
        "public_v1_recovery::read_fencing::sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed",
    ),
    (
        "L3",
        "current-witness",
        "public_v1_recovery::hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children",
    ),
    (
        "Q3",
        "current-source-only",
        "src/replicator/stateless/mod.rs:2997-3008 capability is committed tail",
    ),
    (
        "M2/M3",
        "current-source-only",
        "src/replicator/stateless/mod.rs:3031-3126 committed target and PC/CC wait",
    ),
    (
        "C2",
        "current-source-only",
        "src/replicator/stateless/mod.rs:1955-1964 source-local context",
    ),
    (
        "S2",
        "current-source-only",
        "src/replicator/stream.rs:315-365 single-take streams",
    ),
    (
        "H1",
        "current-source-only",
        "src/host/hosting.rs:545-605 ready active-secondary reads Granted",
    ),
];

pub const PORT_STATUS: &str = "pending-P2+";
pub const NATIVE_EXECUTION: &str = "not-run";
pub const PORT_EXECUTION: &str = "not-implemented/not-run";
pub const DERIVATION_REVIEW: &str = "source-reviewed; selected-test-derived where named";

// Executable predicate/trace slices of the inventory above. Remaining vectors
// intentionally stay inventory-only until the real components are ported.
pub const ALL_CASES: &[(i64, i64, bool)] = &[
    (12, 11, false),
    (12, 12, true),
    (13, 12, false),
    (13, 13, true),
];
pub type QuorumCase = (bool, i64, i64, Option<i64>, bool);
pub const QUORUM_CASES: &[QuorumCase] = &[
    (true, 10, 11, Some(10), false),
    (true, 10, 11, Some(11), true),
    (true, 10, 11, None, true),
    (true, 10, 9, None, false),
    (false, 10, 11, Some(11), false),
];
pub const ACK_POLICY_CASES: &[(bool, bool, bool)] = &[
    (false, false, false),
    (false, true, true),
    (true, false, true),
    (true, true, true),
];
// Q1 observations after receive2, receive1, ACK2, ACK1.
pub const GAP_TRACE: &[(i64, i64, i64)] = &[(2, 0, 0), (1, 2, 0), (2, 2, 0), (1, 2, 2)];
pub type SenderStep = (i64, i64, &'static [i64], &'static [i64]);
pub const SENDER_STEPS: &[SenderStep] = &[
    (1, 0, &[1, 2, 3], &[2, 3]),
    (2, 0, &[1, 2, 3], &[3]),
    (2, 1, &[1, 2, 3, 4], &[3, 4]),
    (3, 2, &[1, 2, 3, 4], &[4]),
    (4, 4, &[1, 2, 3, 4], &[]),
];
// Q2: one completed retained100-byte item, sufficient count growth, no pending
// suffix drop. This is the bounded source predicate, not every MakeRoom path.
pub type MemoryCase = (usize, usize, usize, usize, bool, usize);
pub const MEMORY_CASES: &[MemoryCase] = &[
    (400, 100, 301, 600, false, 400),
    (400, 100, 100, 600, true, 500),
    (400, 100, 300, 600, true, 600),
];
// C6: specified legacy memory600. These are intermediate effective counts,
// before public update validation/comparison, not successful live upgrades.
pub type LegacyCountCase = (Option<u32>, Option<u32>, (u32, u32));
pub const LEGACY_COUNT_CASES: &[LegacyCountCase] = &[
    (None, None, (0, 0)),
    (Some(1024), None, (1024, 1024)),
    (None, Some(2048), (2048, 0)),
];
pub const CAPABILITY_CASES: &[(i64, Option<i64>, i64)] = &[(100, None, 0), (100, Some(95), 95)];
pub const TRIM_CASES: &[(i64, i64)] = &[(0, 0), (1, 1), (4, 1), (5, 2), (8, 5)];
pub const BUILD_CASES: &[(bool, i64, i64, bool)] = &[
    (false, 12, 12, false),
    (true, 11, 12, false),
    (true, 12, 12, true),
];
pub const RETARGET_CASES: &[(bool, i64, i64, bool)] = &[
    (true, 12, 13, true),
    (true, 13, 15, true),
    (true, 15, 15, false),
    (true, 15, 13, false),
    (false, 12, 15, false),
];
