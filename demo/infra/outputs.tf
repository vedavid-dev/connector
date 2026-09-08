# The spec's interface section names these `instance_name`, `instance_zone`,
# `external_ip` — this module deviates on purpose. A regional, spot,
# stateful-disk MIG recreates its single member in whichever zone had
# capacity, under a name with a random suffix, holding an IP nothing
# reserves (network.tf). None of that is a plan-time value the provider
# schema exposes on `google_compute_region_instance_group_manager`, so an
# output claiming to be one would either be wrong or need a fragile extra
# data lookup this module can't verify without a live apply. What follows is
# what's actually knowable, plus the command for what isn't.

output "instance_group" {
  description = "Self-link a caller scripts against, e.g. `gcloud compute instance-groups managed list-instances`"
  value       = google_compute_region_instance_group_manager.demo.self_link
}

output "region" {
  description = "Derived from var.zone; the group can place its instance in any zone here"
  value       = local.region
}

output "base_instance_name" {
  description = "Prefix of the running instance's actual (randomly suffixed) name"
  value       = google_compute_region_instance_group_manager.demo.base_instance_name
}

output "find_instance_command" {
  description = "Resolves the current instance name, zone, and ephemeral external IP, which change on every preemption/recreate"
  value       = "gcloud compute instance-groups managed list-instances ${google_compute_region_instance_group_manager.demo.name} --region=${local.region} --project=${var.project}"
}
