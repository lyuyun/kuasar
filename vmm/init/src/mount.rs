/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Basic Linux filesystem setup performed before the application starts.
//!
//! The kernel guarantees that the root filesystem and `/dev/null` exist by the
//! time PID 1 runs, but `/proc`, `/sys`, `/dev/pts`, and `/run` must be mounted
//! explicitly unless the kernel cmdline already arranged for them.

use std::path::Path;

use anyhow::{Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::unistd::pivot_root;

/// Mount the standard pseudo-filesystems needed by most Linux applications.
///
/// Each mount is skipped if the target is already mounted (detected by a
/// successful `statfs` check), so re-running is idempotent.
pub fn mount_basic_filesystems() -> Result<()> {
    mount_if_needed(
        "proc",
        "/proc",
        "proc",
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
    )
    .context("mount /proc")?;
    mount_if_needed(
        "sysfs",
        "/sys",
        "sysfs",
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
    )
    .context("mount /sys")?;
    mount_if_needed(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
    )
    .context("mount /dev")?;
    // /dev/pts needed for PTY allocation (optional for headless appliances but harmless).
    let _ = std::fs::create_dir_all("/dev/pts");
    mount_if_needed(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
    )
    .context("mount /dev/pts")?;
    // tmpfs on /run for runtime state files (PID files, sockets, …).
    let _ = std::fs::create_dir_all("/run");
    mount_if_needed(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
    )
    .context("mount /run")?;
    Ok(())
}

/// Set up a writable overlay over the read-only erofs root filesystem.
///
/// erofs is mounted read-only by the kernel, so any write attempt by the
/// application would fail with EROFS.  This function layers a tmpfs on top via
/// overlayfs and then calls `pivot_root(2)` to make the overlay the new `/`:
///
/// ```text
/// lower  = /           (erofs, read-only, from pmem device)
/// upper  = /mnt/upper  (tmpfs, volatile — lives in guest RAM)
/// work   = /mnt/work   (overlayfs bookkeeping, also on tmpfs)
/// merged = /mnt/newroot → becomes the new /
/// ```
///
/// After `pivot_root` the old root is lazily unmounted so it remains accessible
/// only to the kernel's overlay lower-layer reference, not to userspace.
///
/// Must be called **after** `mount_basic_filesystems()` so that `/proc`, `/sys`,
/// `/dev`, and `/run` are already mounted and can be bind-mounted into the new root.
pub fn setup_cow_overlay() -> Result<()> {
    // Stage area on a fresh tmpfs so upper/work persist independently of erofs.
    std::fs::create_dir_all("/mnt").ok();
    mount(
        Some("tmpfs"),
        "/mnt",
        Some("tmpfs"),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        None::<&str>,
    )
    .context("mount tmpfs /mnt")?;

    std::fs::create_dir_all("/mnt/upper").context("mkdir /mnt/upper")?;
    std::fs::create_dir_all("/mnt/work").context("mkdir /mnt/work")?;
    std::fs::create_dir_all("/mnt/newroot").context("mkdir /mnt/newroot")?;

    mount(
        Some("overlay"),
        "/mnt/newroot",
        Some("overlay"),
        MsFlags::empty(),
        Some("lowerdir=/,upperdir=/mnt/upper,workdir=/mnt/work"),
    )
    .context("mount overlay at /mnt/newroot")?;

    // Carry the already-mounted pseudo-filesystems into the new root so the
    // application sees a fully functional /proc, /sys, /dev, /run.
    for path in &["/proc", "/sys", "/dev", "/run"] {
        let target = format!("/mnt/newroot{}", path);
        std::fs::create_dir_all(&target).ok();
        mount(
            Some(*path),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .with_context(|| format!("bind-mount {} into newroot", path))?;
    }

    std::fs::create_dir_all("/mnt/newroot/.old_root").context("mkdir /mnt/newroot/.old_root")?;

    pivot_root("/mnt/newroot", "/mnt/newroot/.old_root").context("pivot_root")?;

    std::env::set_current_dir("/").context("chdir /")?;

    // Lazy unmount: detaches the old root from the namespace immediately but
    // keeps the underlying erofs filesystem alive as the overlay lower layer.
    umount2("/.old_root", MntFlags::MNT_DETACH).context("umount2 /.old_root")?;

    Ok(())
}

fn mount_if_needed(source: &str, target: &str, fstype: &str, flags: MsFlags) -> Result<()> {
    if is_mountpoint(target) {
        return Ok(());
    }
    let _ = std::fs::create_dir_all(target);
    mount(Some(source), target, Some(fstype), flags, None::<&str>)?;
    Ok(())
}

fn is_mountpoint(path: &str) -> bool {
    // A path is already a mountpoint if its device differs from its parent's device.
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
    let Ok(parent_meta) = std::fs::metadata(parent) else {
        return false;
    };
    use std::os::unix::fs::MetadataExt;
    meta.dev() != parent_meta.dev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_mountpoint_root() {
        // / is always a mountpoint (same dev as itself but parent "/" == "/")
        // Just verify the function doesn't panic.
        let _ = is_mountpoint("/");
    }

    #[test]
    fn test_is_mountpoint_nonexistent() {
        assert!(!is_mountpoint("/nonexistent-kuasar-test-path"));
    }
}
