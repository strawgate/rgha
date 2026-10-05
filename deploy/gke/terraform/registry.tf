resource "google_artifact_registry_repository" "rgha" {
  repository_id = "rgha"
  location      = var.region
  format        = "DOCKER"
  description   = "rgha controller images"

  cleanup_policies {
    id     = "keep-recent"
    action = "KEEP"
    most_recent_versions {
      keep_count = 20
    }
  }

  depends_on = [google_project_service.services]
}
