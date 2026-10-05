# One small always-on pod: Autopilot bills per pod resource request, so the
# cluster costs the Autopilot fee plus ~0.25 vCPU / 512 MiB.
resource "google_container_cluster" "rgha" {
  name     = var.cluster_name
  location = var.region

  enable_autopilot    = true
  deletion_protection = true

  release_channel {
    channel = "REGULAR"
  }

  # Mounts Secret Manager secrets as files (secrets-store-gke.csi.k8s.io).
  secret_manager_config {
    enabled = true
  }

  # Private nodes: the controller only makes outbound calls (GitHub, Modal).
  private_cluster_config {
    enable_private_nodes = true
  }

  # Deploys use the DNS-based control plane endpoint with IAM auth, so no
  # public IP allowlisting is needed for GitHub Actions.
  control_plane_endpoints_config {
    dns_endpoint_config {
      allow_external_traffic = true
    }
  }

  master_authorized_networks_config {
    dynamic "cidr_blocks" {
      for_each = var.master_authorized_networks
      content {
        cidr_block   = cidr_blocks.value.cidr_block
        display_name = cidr_blocks.value.display_name
      }
    }
  }

  depends_on = [google_project_service.services]
}

# Private nodes need Cloud NAT to reach GitHub and Modal.
resource "google_compute_router" "rgha" {
  name    = "${var.cluster_name}-router"
  region  = var.region
  network = "default"

  depends_on = [google_project_service.services]
}

resource "google_compute_router_nat" "rgha" {
  name                               = "${var.cluster_name}-nat"
  router                             = google_compute_router.rgha.name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"
}
