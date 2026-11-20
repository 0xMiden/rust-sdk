#!/bin/bash
# Prints the release notes for a tag from `.changelog/<tag>/<PR>.md`, ordered by PR number. A final
# release (e.g. `v1.0.0`) also includes the entries of its pre-releases (e.g. `v1.0.0-rc.1`).
set -euo pipefail

TAG="$1"

shopt -s nullglob
files=(".changelog/${TAG}"/*.md)
if [[ "${TAG}" != *-* ]]; then
    files+=(".changelog/${TAG}"-*/*.md)
fi

if [ ${#files[@]} -eq 0 ]; then
    >&2 echo "No changelog entries found in .changelog/${TAG}/"
    exit 1
fi

printf '%s\n' "${files[@]}" | sort -t/ -k3 -n | tr '\n' '\0' | xargs -0 awk 1
