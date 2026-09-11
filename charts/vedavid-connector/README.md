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
report success and the connector quietly serves only its built-in dashboards.
Check `files_scanned` in the app if dashboards do not appear.

If you generate the ConfigMap with kustomize, set
`generatorOptions.disableNameSuffixHash: true` — a hashed name will not match
what this chart mounts. See `examples/dashboards/` in the repository.

## Values worth setting

| Value | |
| --- | --- |
| `clusterLabel` | Shown in the app beside each dashboard. A tenant with several clusters cannot tell them apart without it. |
| `prometheus.url` | In-cluster address of the Prometheus to query. |
| `relay.address` | Where to dial. Defaults to Vedavid's relay. |
| `dashboards.sources` | The objects projected into the dashboard directory. |

`relay.caCertificate` overrides the authority the relay is checked against.
The default is Vedavid's root, which is public and embedded in the chart.
