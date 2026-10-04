# The official runner image. The Worker passes the JIT config in
# ACTIONS_RUNNER_INPUT_JITCONFIG. Cloudflare Containers have no /dev/shm, which
# breaks POSIX semaphores (Python multiprocessing, Chromium), so the entrypoint
# mounts one before starting the runner.
FROM ghcr.io/actions/actions-runner:latest
COPY --chmod=755 entrypoint.sh /home/runner/rgha-entrypoint.sh
