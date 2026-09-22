# kuberic-wire

Versioned protobuf and tonic contracts for the level-triggered Kuberic stack.

This crate owns the new control-plane, replica-peer, and replication schemas.
It validates wire messages before converting them into
`kuberic-protocol` authority types.

The wire contract carries exact replica incarnation, durable agent generation,
epoch, configuration identity, protocol version, and replication progress.
Unknown versions, enum values, missing fields, and contradictory authority are
rejected rather than defaulted.

`kuberic-wire` contains transport definitions only; protocol decisions remain
in `kuberic-protocol`.
