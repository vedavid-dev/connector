resource "google_compute_instance_template" "demo" {
  name_prefix  = "vedavid-demo-"
  machine_type = var.machine_type
  tags         = ["vedavid-demo"]
  project      = var.project

  disk {
    source_image = "projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64"
    auto_delete  = false # required for the stateful_disk policy below to hold it
    boot         = true
    disk_size_gb = 40
    disk_type    = "pd-balanced"
  }

  network_interface {
    network = "default"
    access_config {
      # No nat_ip: ephemeral by choice, not oversight — see network.tf's
      # data source comment and this module's README.
    }
  }

  scheduling {
    provisioning_model  = "SPOT"
    preemptible         = true
    automatic_restart   = false
    on_host_maintenance = "TERMINATE"
  }

  service_account {
    email  = var.node_service_account_email
    scopes = ["cloud-platform"]
  }

  metadata = {
    enable-oslogin = "TRUE"
    startup-script = templatefile("${path.module}/startup-script.sh.tftpl", {
      k3s_version        = var.k3s_version
      git_repository_url = var.git_repository_url
      flux_semver        = var.flux_semver
    })
  }

  lifecycle {
    create_before_destroy = true
  }
}

# Spot mandates automatic_restart = false: a preempted standalone instance
# has nothing to recover it. A regional MIG both recovers it and, being
# regional rather than zonal, can place the replacement in whichever zone
# still has Spot capacity — materially improving recovery time over a
# zonal group when one zone is out.
resource "google_compute_region_instance_group_manager" "demo" {
  name               = "vedavid-demo"
  project            = var.project
  region             = local.region
  base_instance_name = "vedavid-demo"
  target_size        = 1

  distribution_policy_zones = data.google_compute_zones.demo.names

  version {
    instance_template = google_compute_instance_template.demo.self_link
  }

  # k3s's sqlite datastore and local-path's PVC directories both live on the
  # boot disk. Without this, the disk is recreated from the image on every
  # preemption and Prometheus loses its entire history, not just a gap.
  stateful_disk {
    device_name = "boot"
    delete_rule = "NEVER"
  }

  # No autohealing health check: a MIG already recreates an instance that
  # isn't RUNNING, which is exactly the preemption case. A health check
  # would need a port opened to Google's probe ranges for no added coverage.

  # Stateful MIGs reject PROACTIVE — this is the only legal value, not a
  # preference. Roll a template change with `gcloud compute
  # instance-groups managed update-instances`; `rolling-action replace`
  # triggers a proactive rollout and fails here.
  update_policy {
    type                         = "OPPORTUNISTIC"
    minimal_action               = "REPLACE"
    max_surge_fixed              = 0
    max_unavailable_fixed        = 3
    instance_redistribution_type = "NONE"
  }
}
