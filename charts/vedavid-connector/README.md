# vedavid-connector

Answers PromQL queries from inside your cluster, over a connection it dials
out. It listens on no port, and it holds no Kubernetes permissions.

```sh
kubectl create secret generic vedavid-enrolment-token \
  --from-literal=token=<token from the admin console>

helm install vedavid oci://ghcr.io/vedavid-dev/charts/vedavid-connector \
  --set clusterLabel=prod-us-east \
  --set prometheus.url=http://prometheus-server.monitoring.svc.cluster.local
```

## What it is allowed to do

The chart ships no `Role`, `RoleBinding`, `ClusterRole` or `ClusterRoleBinding`,
and sets `automountServiceAccountToken: false`. The pod has no credential for
the Kubernetes API and no way to obtain one. Dashboards reach it as files,
which is what keeps that true.

The container runs as uid 65532 with a read-only root filesystem, every
capability dropped, and `RuntimeDefault` seccomp.

## Dashboards

Dashboards are your own YAML, reconciled into a ConfigMap by whatever you
already use, and projected into the connector as files.

The ConfigMap must be named exactly what `dashboards.sources` says. It is an
unchecked string on both sides: if the names disagree, Flux and Argo both
report success and the connector quietly serves nothing.
Check `files_scanned` in the app if dashboards do not appear.

If you generate the ConfigMap with kustomize, set
`generatorOptions.disableNameSuffixHash: true` — a hashed name will not match
what this chart mounts. The `connector/` directory of
<https://github.com/vedavid-dev/demo-connector> is a working example.

## Ask

Ask lets someone type "which service started having errors recently" into the
app and get a dashboard back. The relay recognises the question and assembles
the answer from signal rules that say which metrics mean "errors", "latency",
"traffic" and so on in a cluster. The connector's part is small: it announces
whether Ask is on and any corrections you have made.

```yaml
ask:
  enabled: true
  signals: []
```

`ask.enabled: false` switches Ask off for this cluster. The app says so and
points here.

When the built-in rules pick the wrong metric or find none — a homegrown
`myapp_failures_total`, a service label called `component` — add an override.
An override replaces every built-in rule for its signal:

```yaml
ask:
  signals:
    - signal: errors
      entities: { service: component }
      provides:
        rate: sum by ($by) (rate(myapp_failures_total{$filter}[$window]$offset))
        total: sum by ($by) (rate(myapp_requests_total{$filter}[$window]$offset))
      describe: myapp_failures_total over myapp_requests_total
```

| Field | Required | Content |
| --- | --- | --- |
| `signal` | yes | The signal name, as the app shows it in an answer's "Using" line: `errors`, `latency`, `traffic`, `node_cpu`, `node_memory`, `restarts`, `oomkilled`, `pg_deadlocks`. |
| `entities` | yes | Entity kind to the label carrying it. Kinds: `service`, `pod`, `namespace`, `node`, `container`, `database`. |
| `provides` | yes | Function to PromQL: `rate`, `total`, `quantile` (must use `$q`), `value`. No other key. `ratio` cannot be set; Ask derives it from `rate` and `total`, so provide both. |
| `describe` | yes | One line shown to users as what this signal means on this cluster. |

Expressions may use only these placeholders, which the relay fills:

| Placeholder | Filled with |
| --- | --- |
| `$by` | the entity label being grouped by |
| `$window` | a duration, such as `15m` |
| `$offset` | ` offset 15m`, or nothing |
| `$filter` | `label="value"`, or nothing, for a selector with no other matchers |
| `$and_filter` | `,label="value"`, or nothing, for a selector that has some |
| `$q` | a quantile in `[0,1]` |

The connector checks the shape at startup and refuses to start on an unknown
field, an unknown function or placeholder, more than 20 overrides, or an
expression over 1,000 characters. It does not parse PromQL: an override that is
well-formed but wrong is ignored by the relay, which uses the built-in rule
instead, and the app's "Using" line shows which. Changing `ask` values restarts
the pod.

## Values worth setting

| Value | |
| --- | --- |
| `clusterLabel` | Shown in the app beside each dashboard. A tenant with several clusters cannot tell them apart without it. |
| `prometheus.url` | In-cluster address of the Prometheus to query. |
| `relay.address` | Where to dial. Defaults to Vedavid's relay. |
| `dashboards.sources` | The objects projected into the dashboard directory. |
| `ask.enabled` | Whether the app may ask this cluster questions in plain English. |
| `ask.signals` | Corrections to how Ask reads this cluster's metrics. |

`relay.caCertificate` overrides the authority the relay is checked against.
The default is Vedavid's root, which is public and embedded in the chart.
