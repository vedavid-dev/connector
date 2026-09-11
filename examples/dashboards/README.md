# Reference dashboard source

Copy this directory into your own repository, add your dashboards beside
`checkout-api.yaml`, list each one in `kustomization.yaml`, and let Flux or
Argo reconcile it into the namespace the connector runs in.

Two things here are load-bearing, and both fail silently if changed:

- **`disableNameSuffixHash: true`.** Without it kustomize appends a content
  hash to the ConfigMap name. The connector's chart mounts a fixed name, so the
  two would never meet: the pod mounts nothing and serves only built-in
  dashboards, while Flux and Argo both report success.
- **The generated name matches the chart's `dashboards.sources[0]`.** It is an
  unchecked string on both sides. CI asserts the two agree.

Check the connector's inventory if dashboards do not appear: `files_scanned: 0`
against a configured source means the mount delivered nothing.
