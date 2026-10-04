# rgha

**Serverless GitHub Actions runners.** rgha is a Rust controller that runs
every job in its own throwaway sandbox on **Modal** (or Daytona). Sandboxes
are billed **per second, for what the job actually uses**. There's no cluster
to run, and it's built to be safe on public repos.

```yaml
jobs:
  lint:
    runs-on: rgha-tiny   # a class from your rgha.toml
```

## Where rgha fits

| You want | Use |
|---|---|
| Zero setup, and you're fine with per-minute billing | GitHub-hosted runners (free on public repos) |
| Runners on **your own** Kubernetes or VMs (incl. gVisor/Kata/Firecracker isolation) | [actions-runner-controller](https://github.com/actions/actions-runner-controller) |
| **No infrastructure**, per-second usage-based billing on your own Modal account, scale to zero, public-repo safety | **rgha** |

rgha speaks the same Runner Scale Set protocol as ARC. Where ARC schedules
pods onto your nodes, rgha starts a sandbox per job on a serverless platform.
(An experimental Firecracker/Docker backend lived here; it's archived at tag
`archive/firecracker-backend`. For self-hosted capacity, ARC is the better
home.)

## Pay for what you use

GitHub-hosted runners bill **per minute, rounded up per job**, for the whole
machine. Modal bills **per second**, for the **higher of what you request and
what you use**, for both CPU and memory. So rgha requests a tiny floor and
sets high limits:

```toml
cpu = 0.125              # billed floor (Modal physical cores; 1 core = 2 vCPU)
cpu_limit = 2.0          # jobs can burst to 2 cores; you pay for what they use
memory_mib = 128
memory_limit_mib = 4096
```

Measured with Modal's own usage meter, a ~10-second job costs about
**$0.0001**, against $0.006 for a full minute on a GitHub-hosted 2-core
runner. An idle warm runner costs about **$0.021/hour**.

| Platform | CPU billed on | Memory billed on | Granularity |
|---|---|---|---|
| **Modal** (rgha) | higher of request and usage | higher of request and usage | 1 s |
| Daytona (rgha) | provisioned whole vCPUs | provisioned whole GiB | 1 s |
| GitHub-hosted | whole runner | whole runner | 1 min, rounded up per job |
| ARC | your nodes, busy or idle | your nodes | your cloud's |

**Measured**, side by side with GitHub-hosted `ubuntu-latest` on 42 jobs per
side (shell, Node, Python, Docker, a 10-job burst;
[full results](https://github.com/strawgate/rgha-testbed#github-hosted-vs-rgha-2026-10-04-rgha-main-after-v011)):

| | Queue p50 | Job duration | Cost for 42 jobs |
|---|---|---|---|
| GitHub-hosted (private-repo price) | 3–5 s | baseline | $0.252 |
| rgha on Modal, scale to zero | 7–8 s | same; Docker builds ~2× faster | **~$0.005** metered (~50× less) |
| rgha on Modal, adaptive warm pool | 3–7.5 s | same; Docker builds ~2× faster | **$0.0085** metered, plus idle warm time (~30× less) |

Costs are from Modal's usage meter (`rgha_metered_cost_usd_total`). The
controller's built-in estimate prices busy time at the limits, so it reads 5–8×
higher. [Where the money goes](docs/operations.md#where-the-money-goes-metered).

- **Warm pickups take 0.1 s.** The adaptive warm pool grows by one runner while
  jobs start cold and shrinks by one after quiet periods.
- **Preloaded images** bake actions and toolchains in: `setup-python` went from
  9 s to 1 s, and real test jobs from 15–20 s to 7–8 s.
- **Docker in jobs** works on Modal's VM runtime.
- **The win shrinks for long, large jobs.** A 3-minute job at 1 Modal core /
  4 GiB is only ~1.4× cheaper.
- **Standard GitHub-hosted runners are free on public repos.** The cost
  argument applies to private repos and larger runners.
- **Hardware differs.** Public-repo `ubuntu-latest` is 4 vCPU / 16 GB; these
  rgha classes request 0.125–1 core and burst to 2.

Every finished job logs its metered sandbox cost next to the GitHub-hosted
equivalent, and `/metrics` exports both.

## Safe on public repos

GitHub [advises against](https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/about-self-hosted-runners#self-hosted-runner-security)
self-hosted runners on public repos, because a fork PR can run arbitrary code
that persists on the machine. rgha's design answers that directly:

| Threat | Mitigation |
|---|---|
| Persistence between jobs | One sandbox per job, destroyed afterwards. JIT runner registrations are single-use. |
| Host or kernel escape | Modal gVisor (user-space kernel) or Modal's VM runtime; Daytona `linux-vm` snapshots. Daytona's shared-kernel container class is refused for untrusted classes unless explicitly opted in. |
| Fork PR picks a powerful runner | `runs-on` labels are attacker-controlled, so trust never comes from labels. Each class has a **policy** over server-side facts: event, repo, workflow ref, and whether the PR comes from a **fork** (checked via the REST API). A rejected job's **workflow run is cancelled** before any runner takes it. Verified live with a real fork PR. |
| Controller credential theft | The GitHub App key and Modal token never enter a sandbox. The sandbox receives only a single-use JIT config, via an ephemeral Modal Secret. |
| Exfiltration, scanning, crypto mining | `network = "github-only"` (or an allowlist) is enforced by Modal outside the sandbox. CPU and memory caps, max job time and `max_runners` bound abuse. |

Also turn on GitHub's **"Require approval for fork pull request workflows"**
for outside contributors, and restrict the runner group to the repos that
need it. Caveats: the GitHub runner allowlist includes
`*.blob.core.windows.net` (logs and artifacts), which is broad. Modal's domain
allowlist covers TLS on port 443 only. Modal's VM runtime is alpha.

## How it works

```
GitHub ──long-poll──► rgha (one listener per class)
                        ├─ policy: run only jobs this class may run (else cancel)
                        ├─ pool: one runner per assigned job + adaptive warm buffer
                        └─ backend ─┬─ Modal Sandbox (gVisor, or VM runtime + Docker)
                                    └─ Daytona sandbox (container, or linux-vm snapshot)
sandbox: official actions/runner + single-use JIT config → 1 job → destroyed
```

- `rgha-scaleset`: a Rust port of [actions/scaleset](https://github.com/actions/scaleset),
  the Runner Scale Set protocol that ARC uses (sessions, long-poll messages,
  job acquisition, JIT runner configs).
- `rgha-modal`: a minimal client for Modal's public gRPC API. Modal has no
  Rust SDK; this client mirrors the request shapes of Modal's Go SDK.
- `rgha`: the controller binary.

The runner inside the sandbox is the **official** `actions/runner`, so
workflows behave as on any self-hosted runner.

## Quick start

```bash
cp examples/rgha.toml rgha.toml         # edit github.url and classes
export GITHUB_TOKEN=...                  # or configure a GitHub App (recommended)
modal token new                          # Modal credentials in ~/.modal.toml
cargo run --release -p rgha -- check     # validates auth, runner group, backends
cargo run --release -p rgha -- run --metrics-addr 127.0.0.1:9464
```

Then use `runs-on: rgha-tiny` in a workflow. For running it as a service
(container image, systemd, GitHub App permissions, metrics, sizing), see
[docs/operations.md](docs/operations.md).

## Status

Early, but live-tested against real GitHub jobs: the scale set client; the
Modal backend (gVisor, VM runtime with Docker, preloaded images, adaptive warm
pools); the Daytona backend; policy enforcement (cancellation, fork detection)
on real PRs; Prometheus metrics; and the cost ledger. GitHub App auth is
unit-tested only. The roadmap is tracked in strawgate/rgha#31. Next up:
Cloudflare Containers (CPU billed on usage, a Firecracker microVM per
container), the GitHub App setup flow, a published runner image with an
action compatibility matrix, and finer policies.

## Development

```bash
just ci            # fmt --check, clippy -D warnings, tests
just smoke-modal   # live Modal sandbox round trip (fractions of a cent)
```

Licensed under Apache-2.0. `crates/rgha-modal/proto/api.proto` is vendored
from [modal-labs/modal-client](https://github.com/modal-labs/modal-client)
(Apache-2.0). The scale set protocol is ported from actions/scaleset (MIT).
