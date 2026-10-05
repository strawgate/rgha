# Secret containers only: add versions out of band so values never enter
# Terraform state, e.g.
#   gcloud secrets versions add rgha-github-app-key --data-file=app.pem
#   gcloud secrets versions add rgha-modal-toml --data-file=modal.toml
locals {
  secrets = {
    "rgha-github-app-key" = "GitHub App private key (PEM)"
    "rgha-modal-toml"     = "Modal credentials file ([default] token_id / token_secret)"
  }
}

resource "google_secret_manager_secret" "rgha" {
  for_each  = local.secrets
  secret_id = each.key
  labels    = { app = "rgha" }
  annotations = {
    description = each.value
  }
  replication {
    auto {}
  }
  depends_on = [google_project_service.services]
}

# Only the controller's Kubernetes ServiceAccount can read them.
resource "google_secret_manager_secret_iam_member" "controller" {
  for_each  = google_secret_manager_secret.rgha
  secret_id = each.value.id
  role      = "roles/secretmanager.secretAccessor"
  member    = local.controller_principal
}

resource "google_secret_manager_secret_iam_member" "writers" {
  for_each = {
    for pair in setproduct(keys(local.secrets), var.secret_writers) : "${pair[0]}/${pair[1]}" => { secret = pair[0], member = pair[1] }
  }
  secret_id = google_secret_manager_secret.rgha[each.value.secret].id
  role      = "roles/secretmanager.secretVersionAdder"
  member    = each.value.member
}
