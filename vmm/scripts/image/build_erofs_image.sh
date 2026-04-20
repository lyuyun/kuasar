#!/usr/bin/env bash
# Copyright 2024 The Kuasar Authors.
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
# Build an erofs disk image for Appliance mode microVMs.
#
# mkfs.erofs packs a directory tree into a flat image with no partition table,
# so the kernel cmdline uses `root=/dev/pmem0` (whole device).  Appliance ext4
# uses the standard build_image.sh pipeline (same partitioned pmem0p1 format as
# Standard mode) and does NOT go through this script.
#
# Requirements:
#   erofs-utils package (provides mkfs.erofs); guest kernel ≥ 5.4
#
# Environment variables:
#   ROOTFS_DIR   source rootfs directory  (default: /tmp/kuasar-rootfs)
#   IMAGE        output image path        (default: /tmp/kuasar-appliance.erofs)

set -e

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "INFO: $*"; }
OK()   { echo "[OK] $*" >&2; }

[ "$(id -u)" -eq 0 ] || die "$0: must be run as root"

ROOTFS_DIR="${ROOTFS_DIR:-/tmp/kuasar-rootfs}"
IMAGE="${IMAGE:-/tmp/kuasar.erofs}"

rootfs="$(readlink -f "${ROOTFS_DIR}")"
[ -d "${rootfs}" ]           || die "ROOTFS_DIR '${rootfs}' is not a directory"
[ -x "${rootfs}/sbin/init" ] || die "/sbin/init is not installed in ${rootfs}"

command -v mkfs.erofs >/dev/null 2>&1 \
    || die "mkfs.erofs not found — install erofs-utils (e.g. 'yum install erofs-utils')"

info "Building erofs image → ${IMAGE}"
rm -f "${IMAGE}"
mkfs.erofs "${IMAGE}" "${rootfs}"
OK "erofs image written: ${IMAGE}"
