#!/usr/bin/env bash
set -euo pipefail

test -f VM-Images/README.md
test -f VM-Images/linux-x86_64/README.md
test -f VM-Images/linux-x86_64/ubuntu-24.04/image.yaml

grep -Fq "schema: gitrun-vm-image/v1" VM-Images/linux-x86_64/ubuntu-24.04/image.yaml
grep -Fq 'release: "24.04"' VM-Images/linux-x86_64/ubuntu-24.04/image.yaml
grep -Fq "boot_test_required_before_release: true" VM-Images/linux-x86_64/ubuntu-24.04/image.yaml

echo "VM image definition checks passed."
