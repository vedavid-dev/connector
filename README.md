# connector

Answers PromQL queries from inside your cluster, over a connection it dials out.

Your Prometheus stays where it is. Nothing is scraped, copied or shipped
anywhere — a query arrives, the connector forwards it to the Prometheus you
point it at, and returns the result.

## Status

Early. It enrols, dials the relay and answers queries over that connection —
but the relay it talks to is not running anywhere yet.

| RPC | |
| --- | --- |
| `InstantQuery` | `/api/v1/query` |
| `RangeQuery` | `/api/v1/query_range`, with the step derived from a point budget |
| `Labels`, `LabelValues`, `Series` | the matching `/api/v1` endpoints |

`BatchQuery`, `Events`, `InstallCertificate` and `Drain` return
`Unimplemented`. `Events` is the next one worth having: it is the stream the
relay watches to notice a connector that has gone, and without it a dead tunnel
is only discovered when a query fails.

## Running it

With a relay to talk to, the connector enrols and then serves queries over the
connection it opened:

```sh
cargo run
```

Both paths are required and neither has a default, so put them wherever you can
write. In a pod they are mounted volumes — conventionally under `/etc/vedavid`,
which kubelet creates and the container owns — but nothing here assumes that.

`VEDAVID_RELAY_SERVER_NAME` overrides the name checked against the relay's
certificate, which defaults to the host in `VEDAVID_RELAY_ADDR`.

A missing or empty token file **stops the connector at startup** rather than
being retried, so the pod crash-loops and says why. A quiet retry is
indistinguishable from a relay that is down, and Kubernetes has nothing to
report about a process that keeps running. A token that disappears *later* is
retried, because a connector may be mid-life and the mount may come back.

The enrolment token is read from a **file**, not passed as a value. Anything
sharing the pod can read another process's environment at `/proc/<pid>/environ`,
child processes inherit it, and it surfaces in crash dumps and `kubectl describe`
— none of which is true of a mounted secret at mode 0400. It is also re-read on
each enrolment attempt, so rotating the secret takes effect without restarting
the pod, which an environment variable cannot do. A trailing newline is
trimmed.

Without `VEDAVID_RELAY_ADDR` it serves a plain local listener instead, on
`VEDAVID_LISTEN` (default `127.0.0.1:50051`). That mode has no authentication
and exists to exercise the query path on its own — bind it to loopback.

`RUST_LOG` controls logging.

## Enrolment

The private key is generated in this process and never leaves it. The request
carries **no subject and no subject alternative names**: the connector does not
know which identity it will be given, and asking for one is refused rather than
ignored. The relay decides, and the certificate comes back with a SPIFFE ID in
a URI SAN.

Disconnection is routine — every relay deploy causes one — so the connector
reconnects with a backoff that grows to 30 seconds and carries jitter, which
keeps a fleet from reconnecting in lockstep. It re-enrols only when the
transport itself failed, since the enrolment token stays valid across restarts.

## How errors travel

A unary RPC maps a query failure to a gRPC code — `InvalidArgument` for bad
PromQL, `PermissionDenied`, `DeadlineExceeded`, `Unavailable` when Prometheus
cannot be reached, `Internal` otherwise — and passes the upstream message
through verbatim, because "parse error at 1:4" is the only thing that tells
someone what to fix.

Classification prefers Prometheus's own `errorType` over the HTTP status, since
a proxy in front of Prometheus may have rewritten the status.

## Tests

```sh
cargo test                                                   # unit tests only
VEDAVID_TEST_PROMETHEUS=http://127.0.0.1:9099 cargo test     # plus integration
```

The unit tests in `src/convert.rs` run against response bodies captured verbatim
from Prometheus 3.14 rather than hand-written JSON, because two details are easy
to get wrong from memory: matrix timestamps arrive as integers where vector and
scalar send floats, and there is no `warnings` key at all when there are none.
`NaN` and `+Inf` arrive as strings and would silently read as zero if dropped.

`tests/against_prometheus.rs` serves the real service on an ephemeral port and
queries a real Prometheus. Without `VEDAVID_TEST_PROMETHEUS` those tests skip
rather than fail, so `cargo test` stays useful on a machine without one. CI sets
it, so they always run there.

Building needs `protoc` on `PATH`, because the proto is compiled at build time.

## The wire contract

`proto/vedavid.proto` defines how the connector and the relay talk to each
other. This is where it lives, and it is compiled at build time, so changing it
changes the contract.
