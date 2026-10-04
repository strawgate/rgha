# rgha

**R**ust **G**itHub **A**ctions runner controller. Every job gets its own
throwaway, per-second-billed sandbox (Modal, Daytona, or local gVisor/Kata
containers), so short or low-CPU jobs cost a fraction of a per-minute runner
and queue time stays low.

```yaml
jobs:
  lint:
    runs-on: rgha-tiny   # a class from your rgha.toml
```

## Why

GitHub-hosted runners bill **per minute, rounded up per job**. A 20-second
lint job on a 2-core Linux runner costs a full minute ($0.006). On a Modal
Sandbox billed per second at 0.25 core / 512 MiB, the same job, plus ~10 s of
boot and runner registration, costs about **$0.0004, roughly 15× less**.

```console
$ rgha estimate --seconds 20 --cpu 0.25 --memory-mib 512
job: 20s (+10s overhead) at 0.25 cores / 512 MiB
  Modal sandbox : $0.000396
  GitHub-hosted : $0.006000  (rounded up to 1 min)
  ratio         : 15.2x
```

Measured on this repo's [demo workflow](.github/workflows/rgha-demo.yml)
(checkout + a few shell steps) with Modal, `cpu = 0.25`, `cpu_limit = 2.0`,
1 GiB, scale-to-zero:

| | rgha on Modal | GitHub-hosted 2-core |
|---|---|---|
| Runner online after GitHub assigns the job | 0.1–0.2 s warm (`min_idle = 1`), 3–5 s cold | n/a |
| Job duration | 6–8 s | — |
| Billed sandbox lifetime | 9–13 s | 60 s (1 min minimum) |
| Cost per job | ≤ $0.0011 (priced at the 2-core cap) | $0.006 |

Without the burst cap (`cpu_limit = cpu = 0.25`), the .NET runner and Node
actions are CPU-starved: pickup took 8–10 s and the same job took 14–45 s.

**Cheap warm pools.** A warm runner requesting 0.125 core / 256 MiB (with
limits of 2 cores / 2 GiB for jobs) costs about $0.024/hour and still picks
up in 0.1 s. With `warm_for_secs`, the pool only exists for a while after the
last job.

**Preloaded images.** Baking actions and Node/Python into the image cut
`setup-python` from 9 s to 1 s, and real test jobs from 15–20 s to 7–8 s. See
[docs/operations.md](docs/operations.md#preloading-the-image).

**Docker-in-job** works on Modal's VM runtime (`runtime = "vm"`, `docker = true`):
`docker run hello-world` passed with about 10 s cold pickup.

The same job on **Daytona** (1 vCPU / 1 GiB, cached image): runner online
3.7 s after assignment, job 5.5 s, sandbox billed for 7.7 s, about $0.00014.

**Side by side with GitHub-hosted `ubuntu-latest`** (42 jobs per side:
shell, Node, Python, Docker, 10-job burst; [full results](https://github.com/strawgate/rgha-testbed#github-hosted-vs-rgha-2026-10-04-rgha-main-after-v011)):

| | Queue p50 | Job duration | Cost for 42 jobs |
|---|---|---|---|
| GitHub-hosted (private-repo price) | 3–5 s | baseline | $0.252 |
| rgha, scale to zero | 7–8 s | same; Docker builds ~2× faster | **$0.040** (~6× less) |
| rgha, fixed warm pools sized to the burst | 3–4 s | same; Docker builds ~2× faster | $0.174 incl. a 10-min idle tail of 14 warm runners |
| rgha, adaptive warm pools + tiny idle requests | 3–7.5 s | same; Docker builds ~2× faster | **$0.061** incl. a 14-min gradual shrink (~4× less) |

The advantage shrinks for long, bigger jobs: a 3-minute job at 1 Modal core
(2 vCPU) / 4 GiB is only ~1.4× cheaper than a 2-core hosted runner. Standard GitHub-hosted runners are **free for public repos**, so
the cost win applies to private repos, to larger runners, and to anyone who
wants more control over isolation or queue time. Every finished job logs its
actual sandbox cost next to the GitHub-hosted equivalent.

## How it works

```
GitHub ──long-poll──► rgha (one listener per class)
                        ├─ policy: acquire only jobs this class may run
                        ├─ pool: one runner per assigned job (+ optional warm buffer)
                        └─ backend ─┬─ Modal Sandbox (gVisor, or VM runtime)
                                    ├─ Daytona sandbox (container, or linux-vm snapshot)
                                    └─ docker --runtime runsc|kata (local)
sandbox: official actions/runner + single-use JIT config → 1 job → destroyed
```

- `rgha-scaleset`: a Rust port of [actions/scaleset](https://github.com/actions/scaleset),
  the Runner Scale Set protocol that actions-runner-controller uses
  (sessions, long-poll messages, job acquisition, JIT runner configs).
- `rgha-modal`: a minimal client for Modal's public gRPC API (create, list and
  terminate Sandboxes; ephemeral Secrets; registry images). Modal has no Rust
  SDK. This client mirrors the request shapes of Modal's Go SDK.
- `rgha`: the controller binary.

The runner inside the sandbox is the **official** `actions/runner`
(`ghcr.io/actions/actions-runner`), so workflows behave as on any
self-hosted runner. That image is minimal: use `setup-*` actions or bake a
custom image (`image` / `image_commands` in the backend config).

## Quick start

```bash
cp examples/rgha.toml rgha.toml         # edit github.url and classes
export GITHUB_TOKEN=...                  # or configure a GitHub App (recommended)
modal token new                          # Modal credentials in ~/.modal.toml
cargo run --release -p rgha -- check     # validates auth, runner group, backends
cargo run --release -p rgha -- run --metrics-addr 127.0.0.1:9464
```

Then use `runs-on: rgha-tiny` in a workflow. To run it as a service (container
image, systemd, GitHub App permissions, metrics, sizing), see
[docs/operations.md](docs/operations.md).

## Security model (public repos)

GitHub [advises against](https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/about-self-hosted-runners#self-hosted-runner-security)
self-hosted runners on public repos, because a fork PR can run arbitrary code
that persists on the machine. rgha's design answers that directly:

| Threat | Mitigation |
|---|---|
| Persistence between jobs | One sandbox per job, destroyed afterwards. JIT runner registrations are single-use. |
| Host or kernel escape | Modal gVisor (user-space kernel) or VM runtime. Daytona `linux-vm` snapshots. Locally, gVisor (`runsc`) or Kata. **Shared-kernel options (plain runc, Daytona's container class) are refused for untrusted classes** unless explicitly opted in. |
| Fork PR picks a powerful runner | `runs-on` labels are attacker-controlled, so trust never comes from labels. Each class has a **policy** over server-side job fields (event, repo, workflow ref). `trust = "trusted"` classes never take `pull_request*` events or `refs/pull/*` workflow refs. GitHub assigns jobs to a scale set directly, so a rejected job's **workflow run is cancelled** and the job is excluded from the runner count. Verified live with a real PR aimed at the trusted Docker class: the run was cancelled and no runner started. Trusted classes default to `min_idle = 0`, so no warm runner can grab a job before it is checked. |
| Controller credential theft | The GitHub App key and Modal token never enter a sandbox. The sandbox receives only a JIT config, via an ephemeral Modal Secret (never argv or the sandbox definition). |
| Exfiltration, scanning, crypto mining | `network = "github-only"` (or an allowlist) is enforced by Modal outside the sandbox. Per-class CPU/memory caps, max job time, and `max_runners` bound abuse. |

Also turn on GitHub's **"Require approval for fork pull request workflows"**
for outside contributors, and restrict the runner group to the repos that need it.

Caveats: the GitHub runner allowlist includes `*.blob.core.windows.net`
(used for logs and artifacts), which is broad. Modal's domain allowlist is
beta and covers TLS on port 443 only. The Modal VM runtime is alpha. On lower
Daytona tiers, network policy is fixed at the organization level, so
per-sandbox allowlists (`network = "github-only"`) are rejected; creating
VM-class snapshots needs an API key with snapshot permissions.

## Status

Early. Live-tested against real GitHub jobs: the scale set protocol client,
the Modal backend (gVisor, warm pools, VM runtime with Docker), the Daytona
backend, policy enforcement on real PRs, the pool scaler, Prometheus metrics,
and the cost ledger. The local docker backend and GitHub App auth are
unit-tested only. Planned:

- [ ] Firecracker / Cloud Hypervisor backend for bare-metal Linux hosts
- [ ] Cloudflare Containers backend (Firecracker microVM per job, CPU billed on usage)
- [ ] Network allowlists for the local backend
- [ ] Fork detection via the REST API (head repo ≠ base repo) for finer policies

## Development

```bash
just ci            # fmt --check, clippy -D warnings, tests
just smoke-modal   # live Modal sandbox round trip (fractions of a cent)
```

Licensed under Apache-2.0. `crates/rgha-modal/proto/api.proto` is vendored
from [modal-labs/modal-client](https://github.com/modal-labs/modal-client)
(Apache-2.0). The scale set protocol is ported from actions/scaleset (MIT).
