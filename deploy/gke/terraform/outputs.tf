output "cluster" {
  value = google_container_cluster.rgha.name
}

output "image_repository" {
  value = "${var.region}-docker.pkg.dev/${var.project_id}/${google_artifact_registry_repository.rgha.repository_id}/rgha"
}

output "workload_identity_provider" {
  description = "For google-github-actions/auth in the deploy workflow."
  value       = google_iam_workload_identity_pool_provider.github.name
}

output "deployer_service_account" {
  value = google_service_account.deployer.email
}

output "secrets" {
  value = [for s in google_secret_manager_secret.rgha : s.secret_id]
}
