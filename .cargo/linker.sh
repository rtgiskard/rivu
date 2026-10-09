#!/bin/sh
# Prefer mold when installed; do not hide failures from an available linker.
if command -v mold >/dev/null 2>&1; then
    exec cc "$@" -fuse-ld=mold
fi
exec cc "$@"
