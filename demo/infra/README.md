# The demo cluster's GCE footprint

OpenTofu. A **module**, not a root — it has no backend, no provider block,
and no credentials of any kind. It declares `project`, `zone`, and
`node_service_account_email` with no defaults; everything else has one.
Run it in your own project like this:

```hcl
module "demo" {
  source = "git::https://github.com/vedavid-dev/connector.git//demo/infra?ref=<tag or commit>"

  project                    = "your-project"
  zone                       = "europe-west1-b"
  node_service_account_email = google_service_account.demo_node.email
}
```

The `ref` should be a tag or commit SHA, never a branch — this module is
untrusted input to whatever privileged root applies it, and the pin is what
makes that acceptable. See the calling project's own README for how it
provisions `node_service_account_email` and its roles; this module only
consumes the address.

## What it creates

- A `google_compute_instance_template`: Spot `e2-medium`, a 40GB
  `pd-balanced` boot disk, Ubuntu 24.04 LTS, an ephemeral external IP.
- A regional `google_compute_region_instance_group_manager` holding that
  template, with a `stateful_disk` policy on the boot disk and
  `target_size = 1`.
- One firewall rule: TCP 22 from the IAP range, target-tagged
  `vedavid-demo`. Nothing else opens inbound, from anywhere.

## Decisions worth knowing

**The boot disk is stateful, and that is the load-bearing choice in this
module.** k3s keeps its datastore in sqlite on the node filesystem, and its
`local-path` provisioner backs every PersistentVolumeClaim with a directory
on that same filesystem. A Spot instance can be preempted at any time with
`automatic_restart = false` forced by Spot itself — so without a stateful
disk policy, a preemption would recreate the boot disk from the image and
Prometheus would lose its entire retained history, not just the outage
window. `auto_delete = false` on the template's disk is required for the
`stateful_disk` block to be able to hold onto it.

**The group is regional, not zonal.** Spot capacity is allocated per zone.
A regional group can place the replacement instance in whichever of the
region's zones still has capacity, which materially shortens recovery time
over a zonal group when one zone is dry.

**No autohealing health check.** A managed instance group already recreates
an instance that isn't `RUNNING` — exactly the preemption case — so a health
check would add a port opened to Google's probe ranges for no additional
coverage.

**`OPPORTUNISTIC` is the only legal `update_policy.type` here, not a
preference.** Stateful MIGs reject `PROACTIVE`. To roll a template change
(a new machine type, a new `k3s_version`), use `gcloud compute
instance-groups managed update-instances`; `rolling-action replace` triggers
a proactive rollout and this group will refuse it.

**No reserved IP.** Nothing dials in — see the firewall rule above — so the
address may change freely whenever the instance is recreated. An in-use
ephemeral external IPv4 on a Spot VM also bills at $0.0025/hr rather than
$0.005 for a reserved one. `outputs.tf` explains why this module can't hand
you the live IP as a plan-time output, and gives you the `gcloud` command
that resolves it instead.

**Consequences of the stateful disk you should plan for, not surprises in
this module:**

- OS patching no longer happens by replacing the image — the disk survives.
  The startup script installs and configures `unattended-upgrades`; treat a
  deliberate disk-recreate as the alternative if that's ever not enough.
- The startup script reruns on every boot against a disk that may already
  have k3s, a populated datastore, and existing PVC directories — every
  step in it has to be a no-op on that disk, not a reset. Read it before
  changing it.
- Every preemption leaves a multi-minute gap in every metric series. That's
  the real cost of Spot here, and this module does not try to hide it.

## Flux, without a repo credential

The startup script runs `flux install` (renders the controllers into the
cluster) rather than `flux bootstrap` (which would commit `flux-system/`
manifests back to this repo, needing write access this node must never
hold). It then applies a `GitRepository`/`Kustomization` pair — baked into
the script from `var.git_repository_url` and `var.flux_semver` — that
points Flux at `demo/clusters/demo` in this repo, tracking a semver range
rather than a branch. From there, Flux reconciles everything under
`demo/clusters/demo/` itself; this script never touches Prometheus or the
workload directly. A copy of the same manifest lives at
`demo/clusters/demo/flux-system/gotk-sync.yaml` for reference — it isn't
read by anything; the version this module actually applies is the one
templated into the startup script.

**The semver range, not a pinned tag, is what makes an ordinary `main`
merge unable to reach the running cluster.** Publishing a new version means
pushing a tag that satisfies `var.flux_semver`, which is the same reviewed
gesture as bumping this module's own `ref` in a caller — see the top of
this file.

## Not done here

- The connector Deployment (`demo/connector/`) and the image it would run —
  deferred.
- The identity/CI rework and the enrollment-token Secret — both belong to
  the private root module that calls this one.
