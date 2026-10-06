# Operating rgha

## Deploy

rgha is one process. It needs **outbound HTTPS only**: to GitHub, and to
Modal. It needs no inbound ports except the optional metrics endpoint. Run one
instance per set of classes: GitHub allows one message session per scale set,
and a second instance waits until the first one's session closes.

**GKE:** [deploy/gke](../deploy/gke) has a Terraform module (Autopilot,
Secret Manager, Workload Identity), kustomize manifests and a reusable
deploy workflow.

**Container** (published to `ghcr.io/strawgate/rgha` on every `v*` tag):

```bash
docker run -d --name rgha --restart unless-stopped \
  -v /etc/rgha/rgha.toml:/etc/rgha/rgha.toml:ro \
  -v /etc/rgha/app.pem:/etc/rgha/app.pem:ro \
  -e MODAL_TOKEN_ID -e MODAL_TOKEN_SECRET \
  -p 9464:9464 \
  ghcr.io/strawgate/rgha:latest
```

The image defaults to `RGHA_CONFIG=/etc/rgha/rgha.toml` and
`RGHA_METRICS_ADDR=0.0.0.0:9464`.

**Binary + systemd** (release tarballs for Linux x86_64/arm64 and macOS arm64):

```ini
# /etc/systemd/system/rgha.service
[Service]
ExecStart=/usr/local/bin/rgha run --config /etc/rgha/rgha.toml --metrics-addr 127.0.0.1:9464 --log-json
EnvironmentFile=/etc/rgha/env        # MODAL_TOKEN_ID, MODAL_TOKEN_SECRET, ...
Restart=always
TimeoutStopSec=60                    # SIGTERM is graceful: idle runners deregistered, busy ones finish

[Install]
WantedBy=multi-user.target
```

On SIGTERM or SIGINT, rgha deregisters and stops idle runners, closes its
message sessions, and leaves busy ones to finish; each sandbox's lifetime is
capped. Give it ~30–60 s to stop (`docker stop -t 60`, `terminationGracePeriodSeconds`). On startup, and every 5 minutes,
it stops instances it doesn't track, such as those left by a crash, unless
their runner is mid-job.

## Credentials

**GitHub App** (recommended). Create it under the org or user that owns the
repos, install it, and set `app_client_id`, `app_installation_id` and
`app_private_key_path` (or `RGHA_GITHUB_APP_PRIVATE_KEY`). Permissions:

| Scope | Permission | Why |
|---|---|---|
| Repository-level config URL | Administration: read & write | register scale sets / JIT runners |
| Org-level config URL | Organization → Self-hosted runners: read & write | same, org-wide |
| Both | Actions: read & write | cancel runs that a class policy rejects; look up runs for fork detection |
| Both | Metadata: read | required by GitHub |

A PAT (`token_env`) also works: classic `repo` (plus `admin:org` for org
scope), or fine-grained with the permissions above.

**Modal**: `MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`, or `~/.modal.toml`
(`modal token new`). Sandboxes live in the configured `app` (default `rgha`),
tagged `rgha=1`, `rgha-class=<class>`, `rgha-runner=<name>`.

## Choosing class sizes

Modal bills per second for the higher of the request and actual usage, for
both CPU and memory. Set small requests and high limits: idle time is cheap,
and jobs can still burst.

- `cpu` is the billed floor. `cpu_limit` lets runner boot and bursty steps
  use more. With `cpu = 0.25`, raising `cpu_limit` from 0.25 to 2.0 cut
  runner pickup from 8–10 s to 3–5 s and job time from 14–45 s to 6–8 s.
- `min_idle` keeps warm runners. Measured pickup with a warm runner was 0.1–0.2 s,
  against 3–5 s from cold. An idle runner bills at its requests. With
  `cpu = 0.125`, `memory_mib = 256` (and `cpu_limit = 2.0`,
  `memory_limit_mib = 2048` for jobs), that is about **$0.024/hour**, and
  pickup was still 0.1 s.
- `warm_max` makes the warm pool adaptive. It starts at `min_idle`, grows by
  one runner (at most every `warm_grow_secs`, default 60) while jobs start
  cold, and shrinks by one after every `warm_shrink_secs` (default 300) without
  cold starts. A 10-job burst nudges the pool up by one runner, not ten; only
  sustained demand grows it further.
- `warm_for_secs` (alternative to `warm_max`) keeps the full `min_idle` pool only for that long after the last job.
  The first job of a burst starts cold; the rest of the burst gets warm
  pickup; quiet periods cost nothing. With `warm_for_secs = 600`, the pool
  switched off 10 minutes after the last job, and that idle runner had cost
  $0.004.
- `warm_schedule` keeps a different warm pool in time windows, for example
  more runners during working hours, when CI fans out, and none at night:
  ```toml
  [[class.warm_schedule]]
  days = ["mon", "tue", "wed", "thu", "fri"]
  from = "08:00"
  to = "18:00"                  # exclusive; earlier than `from` wraps past midnight
  timezone = "America/Chicago"  # IANA name, DST-aware; default UTC
  min_idle = 4
  ```
  The largest open window wins; outside every window the class `min_idle`
  applies. With `warm_max`, the window raises the adaptive pool's floor.
- Trusted classes refuse a warm pool (`min_idle`, `warm_max` or
  `warm_schedule`) unless `allow_warm_trusted = true`. GitHub assigns jobs
  to a scale set before rgha can check them, so a warm runner could pick up
  a disallowed job before rgha cancels its run. The job-started hook closes
  that gap.
- `job_started_hook` (default on, Modal backend) installs a runner
  job-started hook that re-checks the class policy inside the sandbox before
  any step runs: event, workflow ref, repository, actors, and whether a PR
  comes from a fork, read from the job's own event payload. A rejected job
  fails before its steps start. Its logic is tested for parity with the
  controller's policy over a matrix of over 1,000 cases.
- `docker = true` on a `runtime = "vm"` Modal backend starts `dockerd` before
  the runner. Cold pickup was about 10 s.

`rgha estimate --seconds N --cpu C --memory-mib M` compares a job's cost with
a per-minute GitHub-hosted runner.

### Where the money goes (metered)

After each sandbox ends, rgha reads Modal's usage meter. Modal reports billed
quantities: the higher of request and use, from container start to stop. rgha
logs `runner metered` with core-seconds, GiB-seconds and USD, next to the
estimate. Measured on the testbed (2026-10-04, `cpu = 0.125`, `memory_mib = 128`):

| What | Metered cost |
|---|---|
| ~10 s shell job (checkout + echo) | ~$0.00009, ~2 core-s; CPU is ~90% of it |
| Node/Python test job | ~$0.0002–0.00027 |
| Docker build job (VM runtime) | ~$0.00016 |
| Idle warm runner | ~$0.021/hour (0.125 core + 128 MiB floor), ≈ 4 short jobs per minute |

CPU-bound jobs (2026-10-04, `cpu/burn.py` in the testbed, fixed work,
against public `ubuntu-latest` with 4 vCPUs). GitHub cost is at private-repo
per-minute prices:

| Job | GitHub-hosted | rgha, `cpu_limit = 2` | rgha, `cpu_limit = 4` |
|---|---|---|---|
| 1 thread, ~25 CPU-s | 25 s, $0.006 (1 min) | 29 s, $0.0013–0.0017 | 29 s, $0.0013 |
| 4 processes, ~115 CPU-s | 47 s, $0.012 (4-core, 1 min) | 52–57 s, $0.0012–0.0046 | **29 s, $0.0046** |

- One busy thread is metered as about one Modal core, and a Sandbox sees
  `cpu_limit` CPUs. A fixed amount of work costs about the same at any limit,
  so a **higher `cpu_limit` is faster at no extra cost**. The tradeoff is
  how much an abusive job can burn before `max_job_secs`.
- Per saturated CPU-second, a Modal Sandbox costs $0.0000394 against $0.00005
  per vCPU-second on GitHub-hosted runners, only ~1.3× less. For short jobs,
  most of rgha's advantage comes from GitHub's per-minute rounding and from
  not paying for idle cores. For long CPU-bound jobs it converges to ~1.3×.
- A single thread ran about 15% slower than on `ubuntu-latest`.
- Throttled runs (`cpu_limit = 2` with 4 processes) metered inconsistently,
  between 30 and 116 core-s for the same work. Unthrottled runs matched guest
  CPU time.

Levers, biggest first:

- **Warm runners that never get a job dominate spend for light, bursty use.**
  In one round, 3 unused warm runners cost 40% of the total. Use `min_idle = 0`
  with a small `warm_max`, or a short `warm_shrink_secs`, unless queue time
  matters more than about $0.0004 per idle minute.
- **Request floors:** `cpu = 0.125` is Modal's minimum. `memory_mib = 128` is
  enough for the runner because it bursts to `memory_limit_mib`. Going from 256
  to 128 MiB halves memory cost, which is only ~5% of a job.
- **Runner overhead:** the runner's own startup is about 1 core-second per
  job. Tuning it would save around $0.00001 per job.

### Preloading the image

`[backends.<name>.preload]` bakes actions into the runner's action cache, and
Node/Python into the tool cache, at image build time. Modal caches the image,
so jobs skip those downloads:

```toml
[backends.modal.preload]
actions = ["actions/checkout@v5", "actions/setup-node@v5", "actions/setup-python@v6"]
node = ["22"]
python = ["3.12"]
```

Measured: `setup-node` went from 4 s to 0 s and `setup-python` from 9 s to 1 s.
The node test job went from 14.6 s to 6.9 s, and the python one from 19.5 s to
8.1 s. A tag that moves after the image was built only causes a cache miss,
and the runner downloads as usual. Runner self-update is disabled on rgha's
scale sets, because the image is the unit of upgrade.

### Where cold-start time goes, and what didn't help

Here is a cold pickup of about 4–6 s, measured from the runner's own diagnostic log:

| Step | Time |
|---|---|
| JIT config + Modal create RPC | ~0.8 s |
| Sandbox scheduled + booted | ~1 s |
| Runner process start | ~0.5–1 s |
| Runner auth + broker session (per-runner credentials) | ~2 s |
| Broker hands over the job | ~1 s |

- **Region pinning.** We compared `regions = ["us-east"]` with the default region.
  Round trips from Modal to GitHub were already 9–55 ms connect and 35–97 ms
  TLS. Pickup averaged 3.6 s against 4.0 s over three runs each, which is
  within noise, and not worth the 1.75× price.
- **Memory snapshots of a registered, listening runner** (`rgha lab
  hibernate-prepare` / `restore`). Restoring a runner should give warm-pool
  pickup with no idle compute. Modal restored the sandbox in about 2 s, and
  GitHub showed the runner as online, but it never took a job: the job was
  still queued after 7.5 minutes. Snapshots close open connections, and the
  stock runner retries its broker connection with 15–30 s randomized backoff
  and session-conflict retries (`BrokerMessageListener.cs`). Restoring a plain
  sandbox (1.8 s) was also no faster than creating one (2.0 s). This would
  need a patched runner that reconnects immediately after a restore.

## Metrics

Enabled with `--metrics-addr` / `RGHA_METRICS_ADDR`. All metrics are labelled `class`.

| Metric | Type | Meaning |
|---|---|---|
| `rgha_pickup_seconds` | histogram | GitHub assigned the job → a runner took it |
| `rgha_job_seconds` | histogram | job duration (GitHub timestamps) |
| `rgha_sandbox_seconds` | histogram | billed sandbox lifetime |
| `rgha_runner_start_seconds` | histogram | JIT config + sandbox create latency |
| `rgha_jobs_total{result}` | counter | finished jobs by result |
| `rgha_runner_start_failures_total` | counter | failed starts (retried, then backed off ≤ 60 s) |
| `rgha_policy_rejections_total` | counter | jobs rejected by class policy (runs cancelled) |
| `rgha_orphans_stopped_total` | counter | untracked instances stopped by reconciliation |
| `rgha_metered_cost_usd_total` | gauge | real spend from Modal's usage meter, read ~10 s after each sandbox ends |
| `rgha_cost_usd_total` | gauge | pessimistic estimate: busy time at `cpu_limit`, idle time at `cpu` (5–8× above metered in practice) |
| `rgha_github_equivalent_usd_total` | gauge | the same jobs at per-minute GitHub-hosted prices |
| `rgha_runners{state}` | gauge | idle / busy runners |
| `rgha_assigned_jobs` | gauge | jobs assigned to the scale set, minus blocked ones |

Useful alerts: `increase(rgha_runner_start_failures_total[10m]) > 0`;
`histogram_quantile(0.9, rate(rgha_pickup_seconds_bucket[1h])) > 30`.

## Self-hosted capacity

rgha targets serverless platforms (Modal, Daytona; Cloudflare Containers
planned). To run runners on your own Kubernetes cluster or VMs, including
sandboxed pods via gVisor or Kata (which can use Firecracker), use
[actions-runner-controller](https://github.com/actions/actions-runner-controller).
Both speak the same Runner Scale Set protocol and can serve the same repos with
different labels. rgha's experimental Firecracker and Docker backends
(snapshot fast boot, an egress allowlist proxy, Docker in jobs) are archived at
tag `archive/firecracker-backend`.

## Class policies

Each class decides which jobs it takes, using fields GitHub fills in on the
server: event, repository and workflow ref. It never uses the `runs-on` label,
which a fork PR author controls. A rejected job's workflow run is cancelled.

```toml
[class.policy]
trust = "untrusted"            # or "trusted": never PR refs, only branch/tag code
allowed_events = ["push", "pull_request"]   # default: all (untrusted) / branch events (trusted)
denied_events = ["pull_request_target"]
allowed_repos = ["my-org/*"]
allowed_workflow_refs = ["my-org/*/.github/workflows/*@refs/heads/main"]
allow_fork_prs = false         # default: true (untrusted), false (trusted)
allow_same_repo_prs = true     # trusted only: also take PRs from branches of the same repo
allowed_actors = ["alice", "bob"]  # only runs started *and* triggered by these logins
denied_actors = ["mallory"]
```

`allowed_actors` restricts a class to specific people. Both the run's `actor`
(e.g. the PR author) and its `triggering_actor` (e.g. whoever re-ran it) must
be listed, so a maintainer re-running an outsider's PR doesn't put it on these
runners. Use it to keep a deployment private to a team, even on a public repo.
Verified live: with `allowed_actors = ["octocat"]`, our own run was cancelled
before any runner started; with our login listed, everything ran.

The scale set message doesn't say whether a PR comes from a fork or who
triggered it. When the decision depends on either, rgha looks the run up
through the REST API (cached per run). It compares the head and base
repositories, and reads `actor` and `triggering_actor`. If the lookup fails
after retries, the job is rejected (fail closed). Verified on the testbed: a
real fork PR was cancelled in 4 s on a class with `allow_fork_prs = false`,
while the same-repo PR ran.

## Public-repo checklist

- [ ] Untrusted classes: small `cpu`/`memory_mib`, `network = "github-only"`, short `max_job_minutes`
- [ ] Trusted classes: `trust = "trusted"`, `allowed_repos`, `min_idle = 0`
- [ ] Mid-tier classes (more CPU or open egress) for contributors only: `allow_fork_prs = false`
- [ ] Repo setting: require approval for fork PR workflows from outside contributors
- [ ] Runner group limited to the repos that use it
- [ ] Alert on `rgha_policy_rejections_total`. It means someone pointed a PR at a trusted class.
