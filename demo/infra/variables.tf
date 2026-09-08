variable "project" {
  description = "GCP project the demo cluster runs in"
  type        = string
}

variable "zone" {
  description = "Build zone; the region is derived from it and the managed instance group spans every zone in that region"
  type        = string
}

variable "node_service_account_email" {
  description = "Service account the node's instance template runs as"
  type        = string
}

variable "machine_type" {
  description = "Node size the cost estimate assumes; changing it invalidates that estimate"
  type        = string
  default     = "e2-medium"
}

variable "git_repository_url" {
  description = "Public HTTPS URL of this repository, cloned unauthenticated by both Flux and the node's one-time bootstrap"
  type        = string
  default     = "https://github.com/vedavid-dev/vedavid-connector"
}

variable "flux_semver" {
  description = "Flux GitRepository semver range. A branch is deliberately not an option here — see demo/clusters/demo/flux-system/gotk-sync.yaml"
  type        = string
  default     = ">=0.1.0 <1.0.0"
}

variable "k3s_version" {
  description = "Pinned k3s release. Never \"stable\" — an unannounced k3s upgrade on a preemption-recovered node is not a moment to also absorb a Kubernetes version bump"
  type        = string
  default     = "v1.34.1+k3s1"
}
