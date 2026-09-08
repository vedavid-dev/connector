locals {
  # "europe-west1-b" -> "europe-west1", so a caller supplies only the zone.
  region = join("-", slice(split("-", var.zone), 0, length(split("-", var.zone)) - 1))
}

data "google_compute_zones" "demo" {
  project = var.project
  region  = local.region
}

# Break-glass only; the cluster serves no inbound traffic from the internet.
resource "google_compute_firewall" "iap_ssh" {
  name        = "vedavid-demo-iap-ssh"
  project     = var.project
  network     = "default"
  description = "SSH via IAP TCP forwarding only"
  # Google's IAP TCP forwarding range.
  source_ranges = ["35.235.240.0/20"]
  target_tags   = ["vedavid-demo"]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}
