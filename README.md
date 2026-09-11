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
| `Events` | announces the dashboard inventory |
| `GetRenderTree` | serves one compiled dashboard |

`BatchQuery`, `InstallCertificate` and `Drain` return `Unimplemented`. `Events`
carries only the inventory so far — not the heartbeat that would let the relay
notice a connector that has gone, so a dead tunnel is still only discovered
when a query fails.

## Dashboards

Dashboards are the customer's own YAML, reviewed and rolled back like the rest
of their code. They reach the connector as files, never through the Kubernetes
API — `get`/`watch` on ConfigMaps would be a line item in a security review,
and this connector holds no Kubernetes permissions at all.

Each file is compiled with
[`vedavid-dashboard-dsl`](https://github.com/vedavid-dev/dashboard-dsl) into a
render tree, which is held in memory and announced to the relay.

```
VEDAVID_DASHBOARD_DIR           default /etc/vedavid/dashboards
VEDAVID_DASHBOARD_POLL_SECONDS  default 30
VEDAVID_DASHBOARD_SOURCES       names of the projected objects, counted not read
VEDAVID_DASHBOARD_BUILTINS      false drops the embedded defaults
VEDAVID_DASHBOARDS_ENABLED      false turns the whole feature off
VEDAVID_CLUSTER_LABEL           shown in the app beside each dashboard
VEDAVID_MAX_DASHBOARDS          default 100
VEDAVID_MAX_DASHBOARD_BYTES     default 262144
VEDAVID_MAX_DIRECTORY_BYTES     default 4194304
```

A connector that could serve no dashboard at all — the feature enabled, no
sources, built-ins off — refuses to start. That is a misconfiguration rather
than a degraded mode. No sources with built-ins on is the first-install state
and is fine.

Two dashboards are compiled into the binary by `build.rs` — as render trees,
not as YAML parsed at startup, so a built-in that stops compiling breaks this
repository's build rather than a customer's pod. A mounted file with the same
`id` replaces the built-in of that name.

The `id` comes from inside the document, never from the filename. Two files
declaring the same `id` admit neither and report both: filename order is not
identity, so using it as a tiebreak would make behaviour depend on something
that carries no meaning.

### Mounting the ConfigMap

Three constraints, each of which silently stops updates arriving:

- **Mount the whole directory.** A `subPath` mount never receives updates.
- **The ConfigMap must not be immutable.** Immutable ConfigMaps never update.
- **A ConfigMap caps at about 1 MiB**, which is the real ceiling on how many
  dashboards one connector can serve.

The directory — not any file in it — is watched, because a ConfigMap update
lands as an atomic symlink swap that `inotify` on a file path would miss.
Events are coalesced over 500 ms so one swap causes one scan, and a poll runs
unconditionally alongside the watch: a missed event is a dashboard that never
updates again, and that failure would be silent.

See `examples/dashboards/` for the source side of this — a `kustomization.yaml`
to copy, with the two settings that otherwise fail silently.

### When a dashboard fails to compile

Failure is per dashboard. One bad file does not disturb the others, and a
dashboard that compiled before and fails now **keeps serving its last good
render tree** — a bad merge must not take a working dashboard away.

Each dashboard is announced as one of:

| status | |
| --- | --- |
| `ok` | compiled |
| `stale` | serving the last good tree; the newest source failed |
| `failed` | never compiled, so there is nothing to serve |

`stale` and `failed` carry the compiler's diagnostics, so the app can say what
is wrong rather than only that something is. CI validates the same YAML, but
CI's compiler and the deployed connector's compiler are two different programs
at two different versions, so compile failures do reach production.

The inventory also reports `mounted_sources_seen` and `files_scanned`. A
configured source that delivered no files is the one failure this design cannot
otherwise show: the chart mounts one name, the customer's generator produced
another, every layer reports success, and the connector quietly serves built-ins
alone.

Three limits bound what a scan will accept: 100 dashboards, 256 KiB per file,
and 4 MiB across the directory. Exceeding the directory limit aborts the scan
and keeps the previous inventory rather than emptying it.

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
