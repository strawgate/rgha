terraform {
  required_version = ">= 1.6"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
  }
  # Configure a GCS backend per environment, e.g.:
  # backend "gcs" { bucket = "<project>-tfstate" prefix = "rgha" }
}

provider "google" {
  project = var.project_id
  region  = var.region
}
