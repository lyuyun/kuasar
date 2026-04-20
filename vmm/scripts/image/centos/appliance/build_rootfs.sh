#!/bin/bash
# Copyright 2022 The Kuasar Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Builds a minimal CentOS/Fedora-based rootfs for Appliance Mode microVMs.
#
# Differences from the standard centos build:
#   - kuasar-init replaces vmm-task as /sbin/init (PID 1)
#   - No runc or container runtime — the appliance VM runs a single user app
#   - Minimal package set: basic shell, networking, and agent tooling
#   - /etc/kuasar/init.d/ hook directory is pre-created
#
# Usage (called by vmm/scripts/image/build.sh):
#   bash vmm/scripts/image/build.sh image centos/appliance

set -e

cert_file_path="/kuasar/proxy.crt"
ARCH=$(uname -m)

current_dir=$(dirname "$(realpath "$0")")
source "${current_dir}/../../common.sh"

main() {
    local rootfs_dir="${ROOTFS_DIR:-/tmp/kuasar-rootfs}"
    local repo_dir="${REPO_DIR:-/kuasar}"

    # ── 1. Build kuasar-init static binary ───────────────────────────────────
    make_kuasar_init "${repo_dir}"

    # ── 2. Create rootfs skeleton ─────────────────────────────────────────────
    create_tmp_rootfs "${rootfs_dir}"
    mkdir -p "${rootfs_dir}/etc/kuasar/init.d"

    # ── 3. Install kuasar-init as PID 1 ──────────────────────────────────────
    cp "${repo_dir}/bin/kuasar-init" "${rootfs_dir}/sbin/init"
    ln -sf /sbin/init "${rootfs_dir}/init"

    # ── 4. Install RPM packages into the rootfs ───────────────────────────────
    install_and_copy_rpm "${current_dir}/rpm.list" "${rootfs_dir}"
    copy_binaries "${current_dir}/binaries.list" "${rootfs_dir}"
    copy_libs "${current_dir}/binaries.list" "${rootfs_dir}"

    # ── 5. Copy libnss and resolver libs required for DNS ─────────────────────
    cp /lib64/libnss_dns* "${rootfs_dir}/lib64"
    cp /lib64/libnss_files* "${rootfs_dir}/lib64"
    cp /lib64/libresolv* "${rootfs_dir}/lib64"

    echo "Succeeded building appliance rootfs"
}

main "$@"
