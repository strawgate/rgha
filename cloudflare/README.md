# Cloudflare Containers prototype

A Worker plus Durable Objects that start one container per single-use GitHub
Actions runner (strawgate/rgha#42). There's no Rust backend yet:
`scripts/start-runner.sh` mints a JIT config with `gh` and calls the Worker.

```bash
npm install && npx wrangler deploy
openssl rand -hex 32 > token && npx wrangler secret put RGHA_TOKEN < token
RGHA_CF_URL=https://rgha-runners.<account>.workers.dev RGHA_CF_TOKEN_FILE=token \
  scripts/start-runner.sh owner/repo rgha-cf standard-1
```

## Sizing results (2026-10-04, strawgate/rgha-testbed `cf` workflow)

| Job | Cloudflare | Modal (rgha-tiny) | GitHub-hosted |
|---|---|---|---|
| Pickup after start call | 6–7 s (all sizes) | ~4 s cold, 0.2 s warm | — |
| Smoke job (checkout + echo) | 6–12 s, ~$0.0001–0.0002 on `basic` | ~$0.0001 | $0.006 |
| 1 thread, ~25 CPU-s | 28 s on `standard-2`, ~$0.0013–0.0016 | 29 s, $0.0013 | 25 s, $0.006 |
| 4 processes, ~110 CPU-s | **26–28 s** on `standard-4`, ~$0.0034–0.0038 | 29 s (`cpu_limit = 4`), $0.0046 | 34–47 s, $0.012 |
| Idle warm runner | ~$0.010/h (`basic`) | ~$0.021/h | — |

Costs: CPU from inside the VM (`/proc/stat`), times Cloudflare list prices
($0.00002/vCPU-s active), plus memory and disk for the instance size times the
container's lifetime. The ranges cover the setup-python download, which Modal's
preloaded image skips.

## What differs from Modal

- **CPU costs half as much and is billed only when used, but it can't burst
  past the instance type.** `basic` is capped at 1/4 vCPU (a 25 CPU-s job took
  143 s) and `standard-1` at 1/2. Pick the size per class.
- **Memory and disk are billed as provisioned, at ≥ 3 GiB per vCPU.** On
  `standard-4` (12 GiB), memory is ~40% of a CPU-heavy job's cost.
- **Container classes have fixed sizes.** There is one Durable Object class
  per instance type; `start()` rejects a per-request `instance`.
- **There is no `/dev/shm`.** Python `multiprocessing` fails with
  `SemLock: FileNotFoundError`. `entrypoint.sh` mounts a tmpfs, using the
  runner's sudo.
- **Analytics can't price single jobs.** `containersUsageAdaptiveGroups` and
  `containersMetricsAdaptiveGroups` are sampled and missed most of the CPU of
  sub-minute jobs (e.g. 6–16 CPU-s recorded for ~110 used).
- **Starts fail during a rollout.** After `wrangler deploy`, starts failed with
  `internal error` until every class reported `ready`, which took a few minutes.
