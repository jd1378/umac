#!/usr/bin/env bash
# Version used in release asset names: the tag without its leading "v" (v0.2.0 -> 0.2.0), or
# "dev-<short sha>" for builds that are not from a tag.
set -euo pipefail
ref=${GITHUB_REF:-}
if [[ $ref == refs/tags/v* ]]; then
    echo "${ref#refs/tags/v}"
else
    echo "dev-$(git rev-parse --short HEAD)"
fi
