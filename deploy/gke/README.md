# Deploying rgha on GKE

One always-on controller pod on a GKE Autopilot cluster in a dedicated GCP
project. Jobs run in Modal, so the cluster hosts only the controller
(~0.25 vCPU / 512 MiB).

```
GitHub ◄──long-poll── rgha pod (GKE Autopilot, private nodes, Cloud NAT)
                         ├─ GitHub App key ─┐ Secret Manager, mounted as files
                         ├─ Modal token ────┘ (Workload Identity, no keys)
                         └─► Modal sandboxes (one per job)
```

- **Why no webhook or Cloud Run request handler:** the controller holds an
  outbound long-poll session to GitHub's scale set broker, the same protocol
  as ARC. It needs no inbound endpoint and no public ingress.
- **Why a dedicated project:** the GitHub App key can register runners on
  your repos. Keep it out of production projects.
- **One replica, `Recreate` rollouts:** GitHub allows one message session per
  scale set. Busy runners live in Modal and survive controller restarts, and
  on start the controller reconciles and stops orphaned idle runners.

## Layout

| Path | What |
|---|---|
| `terraform/` | APIs, Autopilot cluster (Secret Manager add-on, private nodes, DNS control-plane endpoint), Cloud NAT, Artifact Registry, secret containers readable only by the controller's KSA, and Workload Identity Federation for the deploy workflow |
| `k8s/base/` | Namespace, ServiceAccount, SecretProviderClass, Deployment (non-root, read-only root fs, no capabilities) |
| `render.sh` | Renders the manifests from the three per-deployment inputs: `rgha.toml`, project ID, image |
| `example/rgha.toml` | Starting point for your config |
| `../../.github/workflows/deploy-gke.yml` | Reusable workflow: build, push, render, apply, wait for the rollout |

## What's yours vs generic

Everything above is generic. A deployment adds only:

| Yours | Where |
|---|---|
| `rgha.toml`: org URL, runner group, App IDs, classes, policies | your fork or private mirror, e.g. `deploy/<org>/rgha.toml` |
| `terraform.tfvars` and backend config: project, region, deploy repo | same place, or your IaC repo |
| Three repo variables from `terraform output` | GitHub repo settings |
| A ~15-line workflow calling `deploy-gke.yml` | your fork or mirror |
| Secret values: App key, Modal token | Secret Manager only, never in a repo |

## Setup

1. **Infrastructure.** In `terraform/`, copy `terraform.tfvars.example` to
   `terraform.tfvars`, configure a GCS backend, then:
   ```bash
   terraform init && terraform apply
   ```
2. **Secrets.** Add versions out of band, so the values never enter Terraform
   state. Whoever generates the App key can upload it directly if listed in
   `secret_writers` (write-only), so the key never passes through anyone else:
   ```bash
   gcloud secrets versions add rgha-github-app-key --data-file=app.pem
   printf '[default]\ntoken_id = "ak-..."\ntoken_secret = "as-..."\nactive = true\n' \
     | gcloud secrets versions add rgha-modal-toml --data-file=-
   ```
3. **GitHub App.** See [docs/operations.md](../../docs/operations.md#credentials)
   for the permissions: org Self-hosted runners read & write, Actions read &
   write, Metadata read. No webhook.
4. **Config.** Copy `example/rgha.toml` and fill in your org, App IDs and
   classes. Preview the manifests with:
   ```bash
   deploy/gke/render.sh deploy/my-org/rgha.toml my-project us-central1-docker.pkg.dev/my-project/rgha/rgha:test
   ```
5. **Deploy.** Call the reusable workflow from your fork or mirror with
   `config`, `project_id` and the Terraform outputs
   `workload_identity_provider` and `deployer_service_account`. Only
   `github_repository` on `deploy_ref` may deploy.

## Operating

- Logs are JSON; Prometheus metrics are on port 9464 (`rgha_metered_cost_usd_total`,
  `rgha_assigned_jobs`, `rgha_runners`, …). Alert on listener errors and on
  jobs queued for more than a few minutes.
- To roll back, re-run the deploy workflow on an earlier commit. To stop
  sending jobs to rgha, point your workflows back at GitHub-hosted runners,
  e.g. by unsetting the repo variable behind
  `runs-on: ${{ vars.LIGHT_RUNNER || 'ubuntu-slim' }}`.
