#!/bin/sh
# Removes distrusted roots from a PEM bundle, for base images whose
# ca-certificates package hasn't caught up with a distrust yet.
# Usage: distrust.sh BUNDLE DISTRUSTED.pem... > FILTERED
# Fails if any distrusted root is still in the output.
set -eu
bundle=$1
shift
out=$(mktemp)
awk -v nlists="$#" '
FNR == 1 { file++ }
/-----BEGIN CERTIFICATE-----/ { inblock = 1; body = ""; block = $0 "\n"; next }
inblock && /-----END CERTIFICATE-----/ {
  inblock = 0
  block = block $0 "\n"
  if (file <= nlists) distrusted[body] = 1
  else if (body in distrusted) removed++
  else printf "%s", block
  next
}
inblock { body = body $0; block = block $0 "\n"; next }
file > nlists { print }
END { printf "distrust: removed %d root(s)\n", removed + 0 > "/dev/stderr" }
' "$@" "$bundle" > "$out"
# Self-check: the output must contain none of the distrusted roots.
if awk -v nlists="$#" '
FNR == 1 { file++ }
/-----BEGIN CERTIFICATE-----/ { inblock = 1; body = ""; next }
inblock && /-----END CERTIFICATE-----/ {
  inblock = 0
  if (file <= nlists) distrusted[body] = 1
  else if (body in distrusted) found = 1
  next
}
inblock { body = body $0 }
END { exit !found }
' "$@" "$out"; then
  echo "distrust: a distrusted root is still present" >&2
  exit 1
fi
cat "$out"
rm -f "$out"
