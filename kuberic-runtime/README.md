# kuberic-runtime

Application and replication runtime for the level-triggered Kuberic stack.

The crate provides a caller-driven `PodRuntime`, application lifecycle and
operation-stream callbacks, durable authority admission, ordered idempotent
effects, exact-incarnation replication, and PC/CC quorum tracking. Runtime role
and write access are separate: startup is write-closed, becoming Primary does
not grant writes, and direct client writes require an explicit granted
`WriteStatus`.

The runtime does not create an operator, replica agent, or control-plane
server. A caller supplies `RuntimeControlPlane` and `AuthorityStore`
implementations. Replication acknowledgements are accepted only when exact
identity, epoch, and configuration fences match durable authority.

Replica builds use separate exact-target authority outside quorum membership.
Copy closes at a captured replication boundary, then a live build lane carries
new writes until the target joins configuration authority.
