# rgha

**R**ust **G**itHub **A**ctions runner controller. Every job gets its own
throwaway, per-second-billed sandbox (Modal today, local gVisor/Kata
containers too), so short or low-CPU jobs cost a fraction of a
per-minute runner and queue time stays low.

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
cargo run --release -p rgha -- run
```

Then use `runs-on: rgha-tiny` in a workflow.

## Security model (public repos)

GitHub [advises against](https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/about-self-hosted-runners#self-hosted-runner-security)
self-hosted runners on public repos, because a fork PR can run arbitrary code
that persists on the machine. rgha's design answers that directly:

| Threat | Mitigation |
|---|---|
| Persistence between jobs | One sandbox per job, destroyed afterwards. JIT runner registrations are single-use. |
| Host or kernel escape | Modal gVisor (user-space kernel) or VM runtime. Locally, gVisor (`runsc`) or Kata. **Plain runc is refused for untrusted classes.** |
| Fork PR picks a powerful runner | `runs-on` labels are attacker-controlled, so trust never comes from labels. Each class has a **policy** over server-side job fields (event, repo, workflow ref). `trust = "trusted"` classes never take `pull_request*` events or `refs/pull/*` workflow refs. Rejected jobs are never acquired. |
| Controller credential theft | The GitHub App key and Modal token never enter a sandbox. The sandbox receives only a JIT config, via an ephemeral Modal Secret (never argv or the sandbox definition). |
| Exfiltration, scanning, crypto mining | `network = "github-only"` (or an allowlist) is enforced by Modal outside the sandbox. Per-class CPU/memory caps, max job time, and `max_runners` bound abuse. |

Also turn on GitHub's **"Require approval for fork pull request workflows"**
for outside contributors, and restrict the runner group to the repos that need it.

Caveats: the GitHub runner allowlist includes `*.blob.core.windows.net`
(used for logs and artifacts), which is broad. Modal's domain allowlist is
beta and covers TLS on port 443 only. The Modal VM runtime is alpha.

## Status

Early. Working today: the scale set protocol client, the Modal backend
(live-tested), the docker backend, the policy engine, the pool scaler, and the
cost ledger. Planned:

- [ ] Firecracker / Cloud Hypervisor backend for bare-metal Linux hosts
- [ ] Daytona backend (per-second billing, VM class)
- [ ] Prometheus metrics (queue time, boot time, cost per class)
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
