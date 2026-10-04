# Operating rgha

## Deploy

rgha is one process. It needs **outbound HTTPS only**: to GitHub, and to
Modal. It needs no inbound ports except the optional metrics endpoint. Run one
instance per set of classes: GitHub allows one message session per scale set,
and a second instance waits until the first one's session closes.

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
KillSignal=SIGINT                    # graceful: idle runners deregistered, busy ones finish
TimeoutStopSec=60

[Install]
WantedBy=multi-user.target
```

On SIGINT, rgha deregisters and stops idle runners and leaves busy ones to
finish; each sandbox's lifetime is capped. On startup, and every 5 minutes,
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
| Both | Actions: read & write | cancel runs that a class policy rejects |
| Both | Metadata: read | required by GitHub |

A PAT (`token_env`) also works: classic `repo` (plus `admin:org` for org
scope), or fine-grained with the permissions above.

**Modal**: `MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`, or `~/.modal.toml`
(`modal token new`). Sandboxes live in the configured `app` (default `rgha`),
tagged `rgha=1`, `rgha-class=<class>`, `rgha-runner=<name>`.

## Choosing class sizes

Modal bills per second for the higher of the requested CPU and actual usage,
and for requested memory.

- `cpu` is the billed floor. `cpu_limit` lets runner boot and bursty steps
  use more. With `cpu = 0.25`, raising `cpu_limit` from 0.25 to 2.0 cut
  runner pickup from 8–10 s to 3–5 s and job time from 14–45 s to 6–8 s.
- `min_idle` keeps warm runners. Measured pickup with a warm runner was 0.1–0.2 s,
  against 3–5 s from cold. An idle runner bills at the `cpu` request:
  0.25 core / 1 GiB is about $0.06/hour (about $43/month), and 0.125 core /
  512 MiB is about $0.03/hour.
- Trusted classes refuse `min_idle > 0` unless `allow_warm_trusted = true`.
  GitHub assigns jobs to a scale set before rgha can check them, so a warm
  runner could start a disallowed job before rgha cancels it.
- `docker = true` on a `runtime = "vm"` Modal backend starts `dockerd` before
  the runner. Cold pickup was about 10 s.

`rgha estimate --seconds N --cpu C --memory-mib M` compares a job's cost with
a per-minute GitHub-hosted runner.

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
| `rgha_cost_usd_total` | gauge | estimated spend: busy time at `cpu_limit`, idle time at `cpu` |
| `rgha_github_equivalent_usd_total` | gauge | the same jobs at per-minute GitHub-hosted prices |
| `rgha_runners{state}` | gauge | idle / busy runners |
| `rgha_assigned_jobs` | gauge | jobs assigned to the scale set, minus blocked ones |

Useful alerts: `increase(rgha_runner_start_failures_total[10m]) > 0`;
`histogram_quantile(0.9, rate(rgha_pickup_seconds_bucket[1h])) > 30`.

## Public-repo checklist

- [ ] Untrusted classes: small `cpu`/`memory_mib`, `network = "github-only"`, short `max_job_minutes`
- [ ] Trusted classes: `trust = "trusted"`, `allowed_repos`, `min_idle = 0`
- [ ] Repo setting: require approval for fork PR workflows from outside contributors
- [ ] Runner group limited to the repos that use it
- [ ] Alert on `rgha_policy_rejections_total`. It means someone pointed a PR at a trusted class.
