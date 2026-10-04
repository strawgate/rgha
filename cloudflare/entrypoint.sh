#!/bin/sh
set -e
if ! [ -w /dev/shm ]; then
  sudo mkdir -p /dev/shm && sudo mount -t tmpfs -o mode=1777,size=50% tmpfs /dev/shm
fi
exec /home/runner/run.sh
