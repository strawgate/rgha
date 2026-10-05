variable "project_id" {
  description = "Dedicated GCP project for the rgha controller (keeps the GitHub App key out of production projects)."
  type        = string
}

variable "region" {
  description = "Region for the Autopilot cluster and Artifact Registry."
  type        = string
  default     = "us-central1"
}

variable "cluster_name" {
  type    = string
  default = "rgha"
}

variable "namespace" {
  description = "Kubernetes namespace of the controller (must match k8s/)."
  type        = string
  default     = "rgha"
}

variable "service_account" {
  description = "Kubernetes ServiceAccount of the controller (must match k8s/)."
  type        = string
  default     = "rgha"
}

variable "github_repository" {
  description = "Repository whose GitHub Actions deploy workflow may push images and deploy (owner/name), e.g. your private deploy repo."
  type        = string
}

variable "deploy_ref" {
  description = "Git ref allowed to deploy (OIDC `ref` claim)."
  type        = string
  default     = "refs/heads/main"
}

variable "master_authorized_networks" {
  description = "CIDRs allowed to reach the cluster's control plane, besides Google-internal and GitHub Actions deploys (which use the DNS endpoint)."
  type        = list(object({ cidr_block = string, display_name = string }))
  default     = []
}

variable "secret_writers" {
  description = "IAM members (e.g. \"user:alice@example.com\") who may add secret versions, such as whoever generates the GitHub App key. Write-only: they cannot read values back."
  type        = list(string)
  default     = []
}
