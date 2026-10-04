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
- Trusted classes refuse `min_idle > 0` unless `allow_warm_trusted = true`.
  GitHub assigns jobs to a scale set before rgha can check them, so a warm
  runner could start a disallowed job before rgha cancels it.
- `docker = true` on a `runtime = "vm"` Modal backend starts `dockerd` before
  the runner. Cold pickup was about 10 s.

`rgha estimate --seconds N --cpu C --memory-mib M` compares a job's cost with
a per-minute GitHub-hosted runner.

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
| `rgha_cost_usd_total` | gauge | estimated spend: busy time at `cpu_limit`, idle time at `cpu` |
| `rgha_github_equivalent_usd_total` | gauge | the same jobs at per-minute GitHub-hosted prices |
| `rgha_runners{state}` | gauge | idle / busy runners |
| `rgha_assigned_jobs` | gauge | jobs assigned to the scale set, minus blocked ones |

Useful alerts: `increase(rgha_runner_start_failures_total[10m]) > 0`;
`histogram_quantile(0.9, rate(rgha_pickup_seconds_bucket[1h])) > 30`.

## Firecracker backend (self-hosted microVMs)

Each job runs in its own Firecracker microVM on a KVM host, built from the
same runner image and `preload` as the Modal backend. The extra cost per job
is $0 on hardware you already have.

**Host requirements:**
- Linux with `/dev/kvm`: bare metal, or a cloud VM with nested virtualization.
- `firecracker` and, recommended, `jailer`, from
  [Firecracker releases](https://github.com/firecracker-microvm/firecracker/releases).
- An uncompressed guest kernel. The Firecracker CI kernels work, e.g.
  `firecracker-ci/v1.15/x86_64/vmlinux-6.1.155`.
- Docker, used once to build the rootfs from the runner image.
- rgha running as **root**, for tap devices, iptables and the jailer.

```toml
[backends.fc]
type = "firecracker"
kernel = "/var/lib/rgha/vmlinux"
firecracker_bin = "/usr/local/bin/firecracker"
jailer_bin = "/usr/local/bin/jailer"   # chroot + unprivileged uid (jailer_uid/gid, default 10000)
state_dir = "/var/lib/rgha/firecracker"
# subnet = "10.213.0.0/16"             # one /30 per VM
# uplink = "eth0"                      # default: the default route's interface
[backends.fc.preload]
actions = ["actions/checkout@v5"]
node = ["22"]

[[class]]
name = "rgha-fc"
backend = "fc"
cpu = 2.0            # vCPUs = ceil(cpu_limit or cpu)
memory_mib = 2048    # guest RAM = memory_limit_mib or memory_mib
```

**How it works:**
- **Rootfs:** built from the image, `preload` and the guest init, converted to
  ext4, cached under `state_dir` by content hash, and shared **read-only** by
  all VMs. Each VM gets a sparse scratch disk; the guest init overlays it on
  the rootfs and pivots into the overlay. Docker `ENV` is recorded into
  `/etc/rgha/image.env` and re-applied in the guest. Stale rootfs images and
  snapshots are removed at startup.
- **Snapshot boot** (`snapshots = true`, the default): for each VM shape (vCPUs,
  memory), rgha boots a template VM until it waits for its config, then takes
  a memory snapshot. This takes about 6 s, once, and is cached on disk. Jobs
  restore the snapshot: VMGenID reseeds the guest kernel RNG, and
  `clock_realtime` corrects the clock. No per-job identity exists in the
  snapshot. If a restore fails, the VM cold-boots.
- **Registration token:** the single-use JIT config arrives on a read-only
  raw drive (read with `O_DIRECT`, so restored clones never see stale data),
  not the kernel command line.
- **Shutdown:** the guest powers off when the runner exits. rgha then removes
  the tap device and disk, and keeps the console log under `state_dir/logs`.
- **Restarts:** VM state lives in `state_dir/vms`. A restarted controller
  adopts VMs that are still running and cleans up those that have exited.

**Docker in jobs** (`docker = true` on the backend): `dockerd` runs inside
each microVM, with `/var/lib/docker` on the VM's ext4 scratch disk. It is
started *before* the snapshot, so restored jobs find it already running.
Verified: `docker build`/`run`, a `container:` job and a redis `services:`
container all worked. VM start took 1.1–1.3 s and pickup 5.4–5.5 s, with
`dockerd` already up. The CI guest kernel has legacy iptables only, without
the `raw` table, so the guest uses `iptables-legacy` and sets
`DOCKER_INSECURE_NO_IPTABLES_RAW=1`. That skips Docker's "direct access
filtering", which protects published ports from other hosts on a shared LAN
and doesn't apply inside a single-tenant microVM.

**Several Firecracker backends on one host** (e.g. one with Docker and one
without) need distinct `subnet`s (second octet) and `state_dir`s; rgha checks
this. Interface and namespace names include the subnet, and the shared
`RGHA-*` chains are never flushed, so one controller restarting doesn't
disturb another backend's VMs or a running VM's egress rules.

**Network isolation:** each VM runs in its own network namespace with an
identical internal tap and guest address, which snapshot restore requires.
That is NATed onto a unique veth /30 and then out of the uplink. Rules live in dedicated `RGHA-FWD` / `RGHA-NAT` chains;
`RGHA-FWD` is inserted into `DOCKER-USER` when Docker is present. Guests
cannot reach the host, RFC1918 ranges (your other VMs, Docker networks, LAN),
CGNAT, or link-local/metadata addresses.

**Egress allowlists** (`network = "github-only"` or `"allowlist"`) are
enforced on the host. A locked-down VM's TCP 443 and 80 are redirected to a
transparent proxy in rgha (ports 15443/15080). The proxy reads the TLS SNI or
the HTTP `Host` header, checks it against the class allowlist (`*.x` matches
subdomains), and connects upstream **by name**. Pairing an allowed name with
another IP doesn't help, and names that resolve to private or link-local
addresses are refused. Connections without an SNI (e.g. ECH) are dropped.
Everything else from the VM is dropped except DNS to `dns` and any
`allow_cidrs`. Verified on the testbed: GitHub worked, while `example.com`
(HTTPS and HTTP), a spoofed resolve, a direct IP and `pypi.org` were blocked.
DNS itself stays open to the configured resolver, which allows DNS-based
exfiltration. To remove the
rules:
`iptables -D DOCKER-USER -j RGHA-FWD; iptables -D INPUT -i rgha+ -j DROP;
iptables -D INPUT -i rgha+ -p tcp -m multiport --dports 15443,15080 -j ACCEPT;
iptables -t nat -D POSTROUTING -j RGHA-NAT; iptables -t nat -D PREROUTING -j RGHA-PRE`.

**Measured** (on a host loaded at 40–74 on 72 cores, so timings are pessimistic):

| | Cold boot, rootfs copied per VM | Snapshot restore, shared rootfs |
|---|---|---|
| VM start (`boot_ms`: JIT config, namespace, disks, restore) | 2.3 s alone; 8.4 s for 3 concurrent | **1.1–1.4 s** for 4 concurrent |
| Pickup (job assigned → runner took it) | 13.7–14.6 s | **5.3–5.7 s** |

After a restore, the runner process start and its GitHub session take about
3 s; that's the remaining floor.
- Verified: shell, `setup-node`/npm, `setup-python`/PyPI and boot-profile jobs
  ran in jailed microVMs (uid 10000, chroot); private ranges were blocked;
  and a controller `kill -9` mid-job left no orphans.

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
```

The scale set message doesn't say whether a PR comes from a fork. When the
decision depends on it, rgha looks the run up through the REST API (cached
per run) and compares the head and base repositories. If the lookup fails
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
