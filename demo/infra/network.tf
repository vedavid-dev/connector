locals {
  # "europe-west1-b" -> "europe-west1". The MIG is regional; only a zone is
  # a required input, so the region a caller didn't have to think about is
  # derived rather than asked for twice.
  region = join("-", slice(split("-", var.zone), 0, length(split("-", var.zone)) - 1))
}

data "google_compute_zones" "demo" {
  project = var.project
  region  = local.region
}

# Break-glass only. No inbound rule for anything the cluster serves — the
# cluster accepts no inbound traffic from the internet at all.
resource "google_compute_firewall" "iap_ssh" {
  name        = "vedavid-demo-iap-ssh"
  project     = var.project
  network     = "default"
  description = "SSH via IAP TCP forwarding only"
  # The IAP TCP forwarding range. Not 0.0.0.0/0 — nothing here is.
  source_ranges = ["35.235.240.0/20"]
  target_tags   = ["vedavid-demo"]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}
