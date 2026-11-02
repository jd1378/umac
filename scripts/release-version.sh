#!/usr/bin/env bash
# Version used in release asset names: the tag without its leading "v" (v1.0.0 -> 1.0.0), or
# "dev-<short sha>" for builds that are not from a tag.
set -euo pipefail
ref=${GITHUB_REF:-}
if [[ $ref == refs/tags/v* ]]; then
    tag=${ref#refs/tags/v}
    crate=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$(dirname "$0")/../Cargo.toml" | head -n1)
    if [[ $tag != "$crate" ]]; then
        echo "tag v$tag does not match the workspace version $crate in Cargo.toml" >&2
        exit 1
    fi
    echo "$tag"
else
    echo "dev-$(git rev-parse --short HEAD)"
fi
