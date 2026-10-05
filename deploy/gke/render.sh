#!/usr/bin/env bash
# Renders the controller's Kubernetes manifests for one deployment.
#   deploy/gke/render.sh <rgha.toml> <project-id> <image[:tag]>
# The only per-deployment inputs are the config file, the GCP project (for
# the Secret Manager paths) and the image; everything else is in k8s/base.
set -euo pipefail
config=$1 project=$2 image=$3
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# A tag is a ":" in the last path segment (not a registry port).
name=$image tag=latest
if [[ "${image##*/}" == *:* ]]; then name=${image%:*} tag=${image##*:}; fi
mkdir -p "$work/overlay"
cp -r "$here/k8s/base" "$work/base"
cp "$config" "$work/overlay/rgha.toml"
cat > "$work/overlay/kustomization.yaml" <<YAML
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources: [../base]
images:
  - name: rgha
    newName: ${name}
    newTag: "${tag}"
configMapGenerator:
  - name: rgha-config
    namespace: rgha
    files: [rgha.toml]
patches:
  - target: { kind: SecretProviderClass, name: rgha-secrets }
    patch: |
      - op: replace
        path: /spec/parameters/secrets
        value: |
          - resourceName: "projects/${project}/secrets/rgha-github-app-key/versions/latest"
            path: "github-app.pem"
          - resourceName: "projects/${project}/secrets/rgha-modal-toml/versions/latest"
            path: "modal.toml"
YAML
kubectl kustomize "$work/overlay"
