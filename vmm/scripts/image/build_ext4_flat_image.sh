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
# Build a flat ext4 disk image for Appliance mode microVMs.
#
# The image is a plain ext4 filesystem written directly to the file with no
# partition table.  The kernel cmdline therefore uses `root=/dev/pmem0`
# (whole device), not `root=/dev/pmem0p1`.  This is consistent with the
# erofs appliance path and avoids the GPT+DAX header overhead required by
# Standard mode's build_image.sh.
#
# Cloud Hypervisor requires the pmem file size to be a multiple of 2 MB.
# This script rounds up to the next 2 MB boundary automatically.
#
# Requirements:
#   e2fsprogs (provides mkfs.ext4, tune2fs, e2fsck)
#
# Environment variables:
#   ROOTFS_DIR        source rootfs directory  (default: /tmp/kuasar-rootfs)
#   IMAGE             output image path        (default: /tmp/kuasar.img)
#   ROOT_FREE_SPACE   extra free space in MB   (default: 64)
#   BLOCK_SIZE        filesystem block size    (default: 4096)

set -e

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "INFO: $*"; }
OK()   { echo "[OK] $*" >&2; }

[ "$(id -u)" -eq 0 ] || die "$0: must be run as root"

ROOTFS_DIR="${ROOTFS_DIR:-/tmp/kuasar-rootfs}"
IMAGE="${IMAGE:-/tmp/kuasar.img}"
ROOT_FREE_SPACE="${ROOT_FREE_SPACE:-64}"
BLOCK_SIZE="${BLOCK_SIZE:-4096}"

# Cloud Hypervisor virtio-pmem requires 2 MB alignment.
readonly ALIGN_MB=2

rootfs="$(readlink -f "${ROOTFS_DIR}")"
[ -d "${rootfs}" ]           || die "ROOTFS_DIR '${rootfs}' is not a directory"
[ -x "${rootfs}/sbin/init" ] || die "/sbin/init is not installed in ${rootfs}"

command -v mkfs.ext4 >/dev/null 2>&1 || die "mkfs.ext4 not found — install e2fsprogs"

# ── Calculate image size ──────────────────────────────────────────────────────

rootfs_size_mb=$(du -BM -s "${rootfs}" | awk '{print $1+0}')
img_size_mb=$(( rootfs_size_mb + ROOT_FREE_SPACE ))

# Round up to ALIGN_MB boundary.
remainder=$(( img_size_mb % ALIGN_MB ))
if [ "${remainder}" -ne 0 ]; then
    img_size_mb=$(( img_size_mb + ALIGN_MB - remainder ))
fi

info "rootfs size: ${rootfs_size_mb} MB, image size: ${img_size_mb} MB"

# ── Create and format the image ───────────────────────────────────────────────

rm -f "${IMAGE}"
truncate -s "${img_size_mb}M" "${IMAGE}"
mkfs.ext4 -q -F -b "${BLOCK_SIZE}" "${IMAGE}"
tune2fs -m 0 "${IMAGE}" >/dev/null  # no reserved blocks needed (read-only root)
OK "ext4 flat image formatted: ${IMAGE}"

# ── Populate with rootfs contents ─────────────────────────────────────────────

mount_dir="$(mktemp -d /tmp/kuasar-ext4-mount.XXXX)"
trap 'umount "${mount_dir}" 2>/dev/null; rmdir "${mount_dir}" 2>/dev/null' EXIT

mount -o loop "${IMAGE}" "${mount_dir}"
cp -a "${rootfs}/." "${mount_dir}/"
sync

info "Unmounting image"
umount "${mount_dir}"
rmdir "${mount_dir}"
trap - EXIT

# ── Verify ───────────────────────────────────────────────────────────────────

e2fsck -p -f "${IMAGE}" >/dev/null || true
OK "ext4 flat image written: ${IMAGE}"
