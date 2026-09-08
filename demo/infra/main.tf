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

# Spot preemption needs the group to recreate the node, in whichever zone has capacity.
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

  # k3s's datastore and the PVCs live on the boot disk, so a preemption would wipe Prometheus.
  stateful_disk {
    device_name = "boot"
    delete_rule = "NEVER"
  }

  # A stateful MIG rejects PROACTIVE, so a template change rolls with update-instances.
  update_policy {
    type                         = "OPPORTUNISTIC"
    minimal_action               = "REPLACE"
    max_surge_fixed              = 0
    max_unavailable_fixed        = 3
    instance_redistribution_type = "NONE"
  }
}
