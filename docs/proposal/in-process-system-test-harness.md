# In-Process Controller and Application Test Harness

## Status and Scope

Implemented in `kuberic-level-tests/src/in_process`. This document describes
the architecture and invariants of the cluster-free integration harness for one
production controller and multiple real KVStore2 replicas inside one test
process.

The harness uses Tokio tasks, loopback TCP, temporary directories, and real
application and agent storage. It does not launch application binaries,
containers, Kubernetes, or a separate API server. "In-process" describes the
execution boundary, not a replacement of networking and storage with mocks.

The initial application is KVStore2. Its public service methods provide the
application request boundary; its HTTP router and executable startup are outside
this design. SQLite and PostgreSQL application journeys are not part of the
initial harness.

## Motivation

The repository has separate building blocks for cluster-free validation:

- production reconciliation with `InMemoryClusterApi`;
- hosted replica lifecycle and recovery tests;
- real KVStore2 provider and application tests.

Individually, they do not form one continuous controller-driven journey. The in-memory
cluster adapter synthesizes agent state and operation completions, while the
hosted harness can dispatch through replica handles. Neither arrangement alone
proves that production resource observation, controller persistence, agent RPCs,
and application replication converge together.

The in-process harness connects those production paths. It complements the
[current test strategy](../features/kuberic/testing.md), rather than replacing
the existing unit tests or owned KinD scenarios.

## Architecture

```text
Scenario
  |
  +-- reconciliation driver
  |     |
  |     production Reconciler
  |     |
  |     production KubeClusterApi
  |       +-- kube::Client --> limited Kubernetes API test service
  |       |                         |
  |       |                   resource store
  |       |                         |
  |       |                   local host supervisor
  |       |                         |
  |       +-- GrpcAgentApi --------> ReplicaHost tasks
  |                                   |
  +-- KvService::replicate_put/get --> real KVStore2 services
                                      |
                                real Replicator
                                      |
                                TCP replication
                                      |
                                real provider storage
```

All components run in the same process. The controller uses its normal
Kubernetes adapter and normal agent gRPC client. Only Kubernetes resource
storage and Pod/PVC realization are simulated. Replication endpoint discovery
uses the real Replicator Open result published by each agent.

Do not introduce another `ProductionClusterApi` implementation or extend the
existing fake to manufacture more realistic agent outcomes. Keeping the
substitution below `kube::Client` exercises the production adapter's observation,
identity validation, command dispatch, and status updates.

### Kubernetes API Test Service

Inject a stateful Tower service through `kube::Client::new`. No HTTP listener or
external mock-server framework is necessary. Never load an ambient kubeconfig
or fall back to a real Kubernetes client.

Support only the controller's requests for namespaced `KubericSet`, Pod, PVC,
Service, and Secret resources:

| Behavior | Required semantics |
|---|---|
| Get and list | Preserve namespace boundaries and implement the label selectors used by the controller. A failed list is an error, not an empty successful result. |
| Create | Assign a fresh UID and resource version; reject an existing name. |
| Replace | Enforce the supplied resource version and preserve server-owned identity. |
| Status replacement | Update only status, preserve spec, and reject a stale resource version. |
| JSON patch | Apply the used operations, including UID and resource-version tests, atomically. |
| Delete | Enforce supplied preconditions and distinguish acceptance from actual removal. |
| Errors | Return Kubernetes-shaped errors that exercise the existing adapter classifications. |

Assign opaque, monotonically changing resource versions and globally unique
UUIDs, including across concurrent harness instances and name reuse.
Initialize `KubericSet` generation and advance it only for spec
changes, not status changes. An accepted mutation with an intentionally lost
reply remains visible to later reads.

Deletion is a two-step state change. Retain an object with deletion metadata
until the supervisor completes its local effects and finalizes removal. A
scenario may hold that completion to exercise cleanup-pending reconciliation.
Deleting a Pod must not implicitly delete its PVC.

The store implements only the Kubernetes semantics needed for these requests.
Unsupported routes, patch operations, and selectors fail explicitly. It does
not implement schema admission, scheduling, watch streams, garbage collection,
or general Kubernetes compatibility. Focused tests of the test service protect
its precondition, status-isolation, selector, and deletion semantics.

### Local Host Supervisor

The supervisor observes resource-store changes and owns local replica tasks.
Creating an API object does not synchronously await a fully initialized host.

| Resource or event | Local effect |
|---|---|
| PVC creation | Allocate a test-owned data directory keyed by PVC UID. |
| Pod creation | Resolve its declared PVC and create `KvPersistence`, `KvService`, and `ReplicaHost` with the generated identities. |
| Host restart | Stop the old host, release its service instance, and construct fresh objects with the same Pod/PVC identities and data directory. |
| Pod replacement | Use the new Pod UID and the PVC declared by that Pod; never infer storage reuse from replica number. |
| Pod deletion completion | Cancel or shut down its host, await owned work, and then remove the Pod object. |
| PVC deletion completion | Require that no local host still uses the volume, then remove its owned directory and object. |

Resolve the controller-generated Pod's environment values, downward-API
references, Secret references, and PVC mount needed by KVStore2. Translate only
local bind and advertised endpoint addresses and the mounted data path.
Unsupported configuration must fail rather than silently use a hand-authored
replica configuration. Map the scenario's declared KVStore2 image to the
in-process application factory; reject other images instead of pretending to
execute them.

The supervisor publishes infrastructure facts such as the loopback Pod IP and
allocated control address. It never publishes replication endpoints, role,
write access, agent history, operation completion, or accepted controller
status. Those must come from the production controller and hosts.

Fresh `ReplicaHost` startup serves an initialization endpoint before returning
a `RunningReplica`. Run `start_with_shutdown` in an owned task while the
controller continues reconciling. Track control-endpoint reachability, completed
host startup, and topology admission separately; waiting for admission before
exposing the initialization endpoint would deadlock bootstrap.

### Networking and Storage

Configure each replica's control and replication bind addresses as
`127.0.0.1:0`. The OS allocates ports when the respective owner binds its socket.
All simulated Pods may use `127.0.0.1`; no loopback-IP allocator, fixed test ports,
or host DNS and network changes are required.

Keep each listener bound from allocation until its serving task shuts down.
Do not discover a free port by binding, closing, and rebinding.
Every caller binds the control listener and passes it by value to
`ReplicaHost::start` or `start_with_shutdown`. Production applications bind their
configured address; the supervisor binds loopback port zero and publishes
`local_addr()`. The host builder carries no listener, its configuration has no
separate control bind address, and it never binds a fallback socket.

Startup moves the control listener into one server task, which owns it without
`Arc` across initialization and established serving. Initialization takes the
listener by value, owns a `TcpListenerStream`, and lends that stream to Tonic.
After its server has stopped and been awaited, initialization recovers and
returns the listener. The same task then moves it into the established agent's
owned incoming stream. Do not run both accept loops concurrently or release the
socket between them. Startup cancellation or errors await the server task and
drain any prepared runtime before returning; established shutdown likewise
acknowledges server completion and runtime draining.

The replicator owns replication binding. Construct `KvService` with
`http://127.0.0.1:0`; its built-in replicator binds inside `Replicator::open()`,
substitutes the actual port in its advertised settings, and returns that URI.
Repeated Open returns the same endpoint. Explicit fixed advertised endpoints
remain unchanged, including when their host differs from the bind address.
There is no injected replication listener or harness endpoint map.

The host publishes the successful current-session Open return in
`PublicV1AgentReport::replication_address`, including over agent gRPC. It is
absent before Open and after successful Close or Abort. Durable Open outcomes
from a predecessor process session are not endpoint evidence for a new process.
The controller first authorizes endpoint-independent Open operations, then
waits for their returned addresses before constructing configuration or Build
inputs. Those inputs use exactly the advertised endpoint, matching the built-in
replicator's inbound Build target validation.

Publish the actual endpoints through these paths:

| Endpoint | Discovery |
|---|---|
| Control | Set the existing `operator.kuberic.io/control-address` Pod annotation to the allocated address. The production `GrpcAgentApi` still performs real RPCs. |
| Replication | Read the current process's Open result from its exact agent report in both ordinary topology and session-renewal configuration construction. Never infer it from Pod IP or a fixed port. |
| Application service | Retain the existing `RoleChange::service_address` path. Application Open returns the replicator; promotion returns the client-facing service address. The harness invokes `KvService` directly and does not bind an application HTTP endpoint. |

Cancellation during startup must stop and await listener owners. Never advertise
port `0` in an Open result or rebind to an unreported replacement port.

Validate resource and exact Pod identity, Service selection, storage incarnation,
and process session before consuming the report. Missing endpoints mean Open
has not yet been observed; they never permit a fixed-port fallback. Discovery
must not bypass resource, Service-selector, agent-report, or process-session
validation.

A restart may allocate new ports for the same Pod/PVC identities. Stop and drain
the old host before starting its replacement; publish the new control address
from the supervisor and the new replication address only through the
replacement's Open result. The real session-renewal path governs admission;
the harness must not rewrite accepted operations or durable endpoint evidence.

These changes require narrow control-listener and reported-endpoint
plumbing, not changes to `Replicator`, `PrimaryReplicator`, or `StateProvider`.
Production callers retain their existing address-based startup behavior.

Harness scenarios may run concurrently, including in separate nextest
processes. OS-owned sockets prevent allocation races; per-instance resource
stores and temporary roots isolate the other state. Do not add a concurrency-one
test group solely to avoid port conflicts. Bind failures are
explicit errors, not permission to connect to an existing listener.

Use real agent SQLite metadata and real KVStore2 application files under a
per-scenario temporary root. Start fresh resources without preseeded agent
databases. Initialization and admission must be controller-authorized.

A PVC UID, not a replica number or Pod name, determines storage ownership.
Creating a replacement PVC creates empty storage; attaching an existing PVC
does not erase or rewrite it to make startup pass.

### Application Boundary

Scenarios call `KvService::replicate_put` and `KvService::get`. They do not
directly apply provider operations, install configurations, grant access, or
write agent journals.

The selected writer must match both accepted controller topology and live agent
evidence. Tests also address individual service instances directly when checking
closed access. A Service-selector assertion checks exact selected Pod identity,
not packet routing through a Kubernetes Service.

No application HTTP server or router extraction is required for the initial
harness. HTTP serialization, executable configuration, images, and deployment
behavior remain covered outside this integration boundary.

## Reconciliation and Time

Run the production `Reconciler::reconcile` entry point from an explicit driver.
The supervisor and hosted replicas run independently on the same multithreaded
Tokio runtime. Do not run the production watch/controller runner.

The driver handles successful actions and errors separately, like the
production controller runner:

| Result | Driver behavior |
|---|---|
| Successful action | Schedule from its returned requeue policy. Resource-store changes and host-startup completion may wake the driver sooner. Register the wakeup before reconciliation so changes during the call are not lost. |
| Reconcile error | Record the error and retry after a nonzero bounded backoff, within the scenario deadline. Do not retry only typed `ObservationStale` errors: Public V1 paths can wrap status conflicts as string-backed evidence errors. |
| Fatal harness failure | Stop the scenario and enter teardown; do not let reconciliation retries hide it. |

The API test service latches unsupported requests independently of reconcile
errors. The supervisor returns invalid fixture configuration, bind failures,
and unexpected host exits through its owned task; the driver checks this task
separately from reconcile results. Host and application drains surface owned-task
errors and panics during teardown. These failures and failed cleanup are fatal.
Planned transient API failures and deliberate host stops are not fatal errors.

Check fatal failure state during waits, after application assertions, and before
declaring success. An injected conflict must be followed by an observed retry
and successful reconciliation, not merely a still-readable application value.
A permanent reconcile error that is not a harness failure still fails at the
overall deadline with the last recorded error and retry count.

Each scenario has one overall deadline; readiness waits, error backoffs, and
progress waits are bounded within it. Resource changes act as local wakeups,
not simulated watch-delivery coverage.

Use real time. Production observation uses `SystemTime`, and provider behavior
can include OS threads, so Tokio paused time is not a complete clock model.
The scenarios use a one-second failover delay and nonzero one-second waiting
requeues. Agent clients use the production five-second RPC deadline rather than
turning ordinary scheduling contention into injected transport failures.
Do not edit stored timestamps or invent reports to force progress.

Assertions wait for observable predicates rather than fixed sleep sequences.
Deadline failures include the last reconcile error and resource snapshot, with
Secret data excluded. Supervisor and owned-task errors are preserved alongside
scenario failures, and failed scenarios retain their storage for inspection.

## Task Ownership and Teardown

One harness instance owns its resource store, controller driver, supervisor,
startup tasks, running replicas, application instances, bound listeners,
endpoint registrations, and temporary root.

Normal teardown stops new reconciliation, cancels unfinished startup through
its shutdown channel, shuts down running replicas, awaits completion, releases
application handles, and only then retires endpoint registrations and removes
storage. Listeners close as their owners finish. Preserve cleanup errors
alongside a scenario failure. Drop guards request shutdown on early exit, but
asynchronous teardown is the normal completion path.

`RunningReplica::wait` includes a private host/built-in-replicator
shutdown-and-drain path. Aborting a listener or descendant requests
cancellation; a completion guard acknowledges actual termination before drain
returns. KVStore2 owns its application stream consumers separately, and
`KvService::shutdown_and_drain` joins them after host shutdown.

Keep joinable completion ownership for the control and data listeners,
operation runtime, replicator descendants, and application consumers. An
`AbortHandle` or an empty abort registry is not completion evidence. Close
admission for new descendant work, signal cancellation or shutdown, then await
all registered work without holding registry locks across joins. Preserve
unexpected task errors and panics in the harness result; expected
cancellation during a deliberate stop is not a failure.

The host scope retains access to this drain path through the private factory
context even if application Close has released its replicator handle.
Synchronous abort must request shutdown without discarding the completion
ownership needed by the asynchronous drain. The supervisor acknowledges a host
stop only after this scope and the application consumers have terminated.

This is local resource cleanup, not controller-authorized terminal retirement:
it must not fabricate a retirement record, erase journals, or consume authority
needed for restart. Keep the drain plumbing private to host and built-in
replicator ownership; do not change `Replicator`, `PrimaryReplicator`, or
`StateProvider`.

A task that cannot stop by the teardown deadline is a test failure. Do not
reuse its addresses or remove its storage as though cleanup succeeded.

## Connected Journeys

Keep the first implementation to a fixed KVStore2 application and three-replica
configuration, without a generic application-plugin framework.

| Journey | What it establishes |
|---|---|
| Fresh bootstrap and quorum write | Empty resources converge through real initialization and admission; application writes succeed and committed values remain readable. |
| Contained host restart | A fresh host/service instance reopens the same Pod/PVC storage, follows real session renewal, and returns to admitted operation without losing acknowledged data. |
| Secondary replacement | Pod loss drives controller-owned replacement; the new incarnation is built and admitted, old resources retire, and subsequent writes succeed. |
| Stopped-primary failover | A cooperatively stopped primary becomes unavailable; survivors converge through the configured delay, preserve acknowledged data, and accept new writes. |
| Controller recreation | Reconstructing the reconciler over the same resource store and live hosts resumes accepted work without test-side authority repair. |

The stopped-primary journey leaves its Pod present and admits the two surviving
members. It verifies preserved data and a new quorum write without requiring a
replacement for the still-present stopped Pod.

Exercise delayed deletion and one stale-status conflict within those journeys.
Observe that replacement cleanup remains pending until exact resource absence,
and that retrying status convergence does not duplicate authority effects.
Keep other fault combinations in focused tests rather than building an initial
fault-scheduling framework.

Use explicit, one-shot resource-service barriers for the interruption cases:

| Case | Cut point and assertion |
|---|---|
| Controller recreation | Commit a status replacement containing a newly accepted nonterminal Public V1 operation and its exact command, then hold the response before the originating reconcile call receives it. Pause the driver, cancel and await its in-flight controller call, recreate the reconciler over the unchanged store and hosts, and release the barrier before resuming the driver. Re-observation must resume that operation without test-side status repair. |
| Stale-status conflict | After a real agent reports completion, intercept the status write that accepts or clears that completion. Perform an independent metadata-only update that advances the resource version without changing spec or authority; reject the original write with HTTP 409. Let the error-backoff path re-observe and complete the same accepted operation. |
| Delayed deletion | Accept deletion of an exact retiring resource but hold final removal. Assert cleanup authority remains pending while it is still observable, then release removal and require subsequent reconciliation to clear it. |

Barriers never retain the resource-store lock while waiting. Record the real
operation identity, revision, command digest, and agent completion across the
cut. Convergent RPC redispatch is permitted; the assertion is no new conflicting
authority or duplicate logical provider effect, not exactly one network call.

For every journey, validate acknowledged application data, exact admitted
identities, and live access grants. Do not use a status-only assertion as a
substitute for a successful application write.

## Failure Model and Limits

An in-process host restart is a cooperative shutdown and reconstruction, not a
process crash. All replicas share a scheduler, address space, allocator, and
failure domain.

In particular, stopped-primary failover does not prove fencing of a
still-running partitioned primary. Reopening that host creates a new session;
it is not equivalent to resuming frozen execution. Do not claim crash recovery,
arbitrary network partition coverage, or old-process resume coverage from this
harness.

The following remain outside scope:

- child processes, process kills, pause/resume, and crash-durability claims;
- application HTTP/gRPC client listeners beyond the agent and replication RPCs;
- a generic packet proxy or distributed fault simulator;
- a real API server, `envtest`, admission, RBAC, and watch delivery;
- Kubernetes scheduling, DNS, Service networking, PVC provisioning, and images;
- SQLite/PostgreSQL application adapters and cross-provider scenario matrices;
- replacement of the existing KinD scenarios.

The retained KinD suite continues to establish deployment and container
behavior that this harness deliberately cannot reproduce.

## Repository Integration

Place the harness and scenarios in `kuberic-level-tests`, alongside but separate
from the owned-cluster module. Use the public controller and host entry points
and a test-only dependency on the KVStore2 library. Prefer existing Tokio,
Tower, Kubernetes types, and temporary-directory utilities; no additional mock
framework is required.

Keep the API test service, resource store, supervisor, and scenario helpers
private to the test crate. Open-result publication, Open-before-discovery
planning, and owned control-listener startup require narrow production changes,
together with private host/built-in-replicator shutdown-and-drain ownership and
application consumer joining. Keep replication binding inside the replicator
rather than adding listener transfer to the factory context or another public
lifecycle framework. Do not expose a production fake-cluster API or add test-only
lifecycle capabilities to protected interfaces.

The five journeys run without cluster environment variables in the ordinary
nextest tier:

```bash
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
```

To run all five concurrently inside one test process:

```bash
cargo test -p kuberic-level-tests --lib --all-features \
  in_process::journeys -- --test-threads=5
```

The harness adds no test tier. The API test service and listener/task ownership
helpers also have focused ordinary-tier tests. See the
[test strategy](../features/kuberic/testing.md) for discovered inventory and
partition accounting.
