# MISC
Why quorum commit of WAL is durable?
Consider 3 replicas, 1 primary and 2 secondaries: A, B and C. Quorum size is 2.
Consider a single write X that is acked by A and B. C has network error.
* B goes down. A can continue operating and serving X.
* A goes down, and B is elected primary. B can continue serving X.
* If A and B go down at the same time, it is a quorum loss and replica set needs to wait for either of them to come back.


Consider 5 replicas with 1 primary; A-E and A is primary. Quorum size is 3.
Consider a single write X that is acked by A,B,C. D and E has network error.
* B,C goes down, A can continue to serve X.
* A,B (or A,C) goes down, C is elected primary and serves X.
  * C,D,E is the new quorum.
  * C sees X and roll it forward. 
    * If A and B fail to commit and C committed, C can roll forward because client disconnects with A and does not know the state.
    * If A and B are permanently lost the same logic apply.
* A,B,C all go down, it is quorum loss and replica set needs to wait for one of them to comeback.


## General analysis
Let total replica size be N, and quorum size floor(N/2) + 1.
Quorum committed means that a quorum number of replicas has persisted the data to disk.
Client write is success for client only after it is quorum committed.
Quorum loss means less than quorum size of replica is available.

Theorem: Client successful write X is persisted. I.e. no data loss.
Case:
  1. Quorum available
      1. Primary down: Secondary that has the X is promoted and continue to serve.
      1. Secondary down: No change.
      In general N/2 + 1 number of replica is available, at least 1 of them has X, and this one has to be the original primary or promoted to be primary.
  1. Quorum loss
      1. Wait for quorum recover, and once has quorum, at least 1 of them has X.
  1. Client write error/disconnect: Client does not know if the operation is persisted or not, so the replica set can always roll forward (for simplicity). Once replica set has a quorum (from recovery), the highest LSN if replicated to all other replicas.
