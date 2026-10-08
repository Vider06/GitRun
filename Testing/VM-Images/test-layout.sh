#!/usr/bin/env bash
set -euo pipefail
test -f VM-Images/README.md
test -f VM-Images/linux-x86_64/README.md
test -d VM-Images/linux-x86_64
echo "VM image layout checks passed."
