# Classic V1 Runtime

Kuberic's sole `DefaultReplicatorFactory::new()` composes the source-guided
Service Fabric classic V1 queue, session, primary, secondary, copy, role and
recovery owners. The owners remain private; applications use the public runtime
interfaces. There is no old-engine selector, dual writer, or fallback.

This experimental implementation is not native SF wire-compatible and does not
port RA/FM, placement or native process hosting. Source/component and Rust
integration evidence are not native binary differential or power-loss evidence.
The [port ledger](sf-v1-port-ledger.md) and
[provenance](sf-v1-port-provenance.md) retain the pinned native source mapping.

## Assignment and application commitment

`StateReplicator::begin_replicate` returns `Replication { lsn, completion }`
after native queue admission. The completion awaits native quorum; the async
`replicate` convenience method waits that same completion. Neither API commits
application state or certifies SQL publication.

The service retains accepted work independently of a client's waiter. It owns
durable preparation, FIFO commitment, committed read visibility and client
response. A cancelled observer is not rollback. Failed preparation, commitment,
publication and owned-worker drainage remain explicit errors.

KVStore2 selects its local committer. SQLite selects its retained SQL/WAL
companion: prepare frames, await quorum, commit frame history, publish WAL,
then return client success. An independent read connection cannot see merely
prepared frames. The runtime has no production dependency on either example.

`Replicator`, `PrimaryReplicator` and `StateProvider` retain their protected
method sets and `Bytes`/`i64`/boxed Send-stream representations. An independently
compiled provider needs no extra durable-progress hook or runtime test helper.
Optional application persistence helpers are not engine prerequisites.

## Receiver lifetimes

Copy and replication operations come from shared native receiver streams.
An operation carries kind, LSN, epoch, data, end-of-stream information and
parameterless `acknowledge()`.

Retransmission uses native LSN-based duplicate detection, not a byte-identity
attestation for a repeated frame. No host replay-fingerprint ledger or stronger
out-of-window retry rejection is added. Application snapshot/history validation
remains a separate service responsibility.

Persisted services durably accept before ACK. Volatile mode can complete at
ordered dispatch without service ACK; do not interpret its native ACK as durable
application commitment. Explicit terminal operations obey their native ACK
rules. Retired streams return typed `RuntimeError::Closed`, distinct from normal
EOF and application failure.

Copy acquisition uses target context and source `get_copy_state`. Service
snapshot enumeration waits for its committed boundary. Native Build captures
its replication target at enumeration EOF and retains the native copy and
replication completion gates. No old durable receipt envelope or engine
commit oracle remains.

## Host, transport and controller

One host creation binds one interface bundle, endpoint lifetime and provider
writer reservation. The host owns serving access and outer lifecycle policy;
queue progress alone cannot grant application visibility.

The host allocates the actual opaque creation identity before Open and consumes
it when attaching the bundle. An advertised reserved identity is not successful
Open or a serving grant. It does not change from a placeholder into a different
identity after renewal authority is frozen.

The bounded tonic carrier uses the host's agent bearer credential and checks
exact process, storage, history, address, configuration and Build authority
before native dispatch. A successful handshake is not permanent authority:
delivery checks the current journal again. Reconnect and RPC completion do not
manufacture ACK. This is not TLS; isolate the deployment network and protect
credentials and namespace resources independently.

An authenticated ACK can outlive its native session after idle removal. The
production pump drops that typed missing-session ACK without faulting transport
health. This exception does not suppress non-ACK dispatch errors, impossible
live-session progress, or unexpected worker/provider/transport failures.

Reports preserve independent `current_progress` and `catch_up_capability`
outcomes: `Value`, `NotQueried`, `NotReady`, or `Failed`. Capability and current
progress are not an ordered applied/committed pair. Routine telemetry avoids
destructive pre-copy queries. Recovery does not label reset native values with
predecessor history.

The installed-configuration observation combines exact voting authority with
native idle IDs. Running control intents identify retained host observers, not
proof of native task drainage. Retrying a timed-out Build reattaches its frozen
intent instead of retiring that same active idle copy.

The query-outcome integration matrix uses a public control decorator over the
real native bundle to exercise independent values and errors through authenticated
agent RPC, fsynced/reopened observation snapshots and controller eligibility.
Those snapshots are not agent recovery-journal evidence or proof of every native
query failure mode.

Bootstrap establishes a primary and really copies cold secondaries before
voting admission. Cold process-session renewal also copies retained application
storage: native queues do not survive process restart. Reconfiguration stages
PC/CC, catch-up and final CC before cold peer admission. Source-local idle
retirement releases a native slot; it does not retire remote storage.

Historical PC need not include the new local primary. In the native-valid
local-only CC/W1 and former-primary-only PC/W1 case, the former primary's applied
progress still constrains normal completion; receive progress alone is not
enough. Current configuration still requires the exact local primary, and W1
with remote CC members remains invalid.

Renewal closes access, advances survivor barriers, replays both native and
application roles and installs exact fresh bindings. Necessary native epoch and
joint-configuration preparations can run under the captured renewal fence;
they cannot release it. Only successful exact final configuration authorizes
release. Stable secondary access is `NotPrimary`, not an application read grant.

A captured contained transient-fault restart can reopen the same storage through
an owned Existing Open before role replay. Only that exact new process can
reactivate the predecessor's terminal lifetime; successful Open does not release
the serving fence. Current/permanent faults, uncontained predecessors and
crossed storage/history remain rejected. Swap retains pre-Revoke access, then
Revoke shares a short admission gate with the write-access sample and native
sequence assignment. A pre-fence call assigns before the refreshed final
configuration captures its boundary, or is rejected. The gate ends before send
effects, pressure policy or foreign callbacks; Revoke does not drain service
commitment or wait for arbitrary provider callbacks. Final catch-up still uses
its captured boundary and live successor requirement, not future assignments.

## Formats and shutdown

Public protocol 12 and agent schema 13 are breaking formats. Earlier schemas,
wire identities, unsupported application layouts and corrupt established state
reject explicitly before relevant mutation. There is no automatic migration,
mixed-version mode, silent wipe or success-shaped fresh-state fallback.

Controller accepted-operation evidence stays append-only and bounded. Its
capacity accommodates the expanded native stage receipts; reaching the bound
is an explicit rejection, not implicit evidence pruning or a new history.

Close retains and drains accepted work and reports child failures. Abort revokes
access and requests containment; a cancelled public observer does not certify
native drainage. Native containment is complete only after facade and transport
tasks have joined. The services distinguish intentional typed cancellation from
real worker, storage and publication failures.

Native idle removal cancels/releases the session without promising that arbitrary
provider callbacks have already joined. Retained callback owners must not install
late results or resurrect a cancelled Build; their eventual drainage is a
separate lifecycle obligation.

## Reproducible entry points

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production
cargo nextest run -p kuberic-controller --all-features --profile ordinary
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
scripts/check_runtime_public_api.sh
just nextest-postgres-smoke
```

The public production journeys use real host initialization, control RPCs,
listeners, KV/SQLite files and an external helper-free provider. In-process
controller journeys simulate only Kubernetes resource realization. They do not
substitute for the separately owned KinD tier, hosted native comparisons or
deployment security qualification. See [Testing](testing.md) for exact tiers.

### Recorded qualification

| Boundary | Observed result |
|---|---|
| Workspace all-target/all-feature compiler, strict Clippy, formatting and whitespace | Passed |
| Complete ordinary regression | Passed; source-runner cleanup follow-up regressions also passed after its infrastructure repair |
| Complete source/API/privacy/package and exact live tier partitions | Passed, with every genuine execution partition retained |
| Normal and no-default runtime/example builds; independent helper-free provider | Passed |
| Routine PostgreSQL custom-replicator smoke | Passed; the full PostgreSQL suite was not selected |

The source runner keeps its outer cap unchanged and reserves work, cleanup and
owner margin inside it. Exhausted budgets and exited children whose descendants
retain pipes are explicitly bounded failure cases, not success fallbacks.
No hosted/native differential parity or power-loss qualification is inferred
from these checks.
