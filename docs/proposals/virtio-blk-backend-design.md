# Virtio-Blk Backend Support for Cloud-Hypervisor

<!-- toc -->
- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [Design Overview](#design-overview)
  - [Risks and Mitigations](#risks-and-mitigations)
- [Design Details](#design-details)
  - [Configuration](#configuration)
  - [Share Backend Selection](#share-backend-selection)
  - [VM Configuration Changes](#vm-configuration-changes)
  - [Sandbox Configuration File Delivery](#sandbox-configuration-file-delivery)
  - [Container Layer Handling](#container-layer-handling)
    - [Overlay Mount Handling](#overlay-mount-handling)
    - [Bind Mount Handling](#bind-mount-handling)
    - [Small Directory Injection](#small-directory-injection)
    - [Large Directory Block Device](#large-directory-block-device)
  - [Cleanup and Resource Management](#cleanup-and-resource-management)
  - [Security Considerations](#security-considerations)
- [Implementation Details](#implementation-details)
  - [Key Data Structures](#key-data-structures)
  - [Helper Functions](#helper-functions)
  - [Image Size Estimation](#image-size-estimation)
- [Test Plan](#test-plan)
  - [Unit Tests](#unit-tests)
  - [Integration Tests](#integration-tests)
- [Future Enhancements](#future-enhancements)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
<!-- /toc -->

## Summary

This document describes the design and implementation of virtio-blk backend support for Cloud-Hypervisor in Kuasar. The virtio-blk backend provides an alternative to the default virtiofs backend for sharing container layers and configuration files with the guest VM. Instead of using a shared filesystem (virtiofs), container layers are packaged into ext4 images and hot-attached as virtio-blk PCI devices to the VM.

The implementation introduces a configurable `share_backend` option that allows users to choose between:
- **virtiofs** (default): Container layers shared via virtiofsd + virtio-fs
- **virtio-blk**: Container layers packaged into ext4 images and hot-attached as block devices

## Motivation

### Goals

1. Provide an alternative storage backend for Cloud-Hypervisor that does not require virtiofsd
2. Enable container workloads in VMs without requiring a shared filesystem daemon
3. Reduce the dependency footprint for minimal VM deployments
4. Support both overlay and bind mount scenarios with virtio-blk backend
5. Maintain backward compatibility with existing virtiofs-based deployments
6. Ensure proper cleanup of temporary resources (ext4 images) when containers are removed

### Non-Goals

1. Support for other hypervisors beyond Cloud-Hypervisor (future work)
2. Performance optimization for large-scale deployments (initial implementation focuses on correctness)
3. Support for symlinks in bind mount directories (currently skipped)
4. Real-time synchronization of file changes between host and guest (block devices are static snapshots)

## Proposal

### User Stories

#### Story 1: Minimal VM Deployment without virtiofsd

A user wants to deploy containers in a minimal VM environment without installing virtiofsd. The virtio-blk backend allows the VM to access container layers via block devices, eliminating the need for a separate virtiofs daemon process.

#### Story 2: Reduced Resource Footprint

A user running multiple VM sandboxes wants to reduce the number of auxiliary processes (virtiofsd instances). Using virtio-blk backend eliminates virtiofsd processes, reducing memory and CPU overhead.

#### Story 3: Secure Isolated Workloads

A user wants stronger isolation between host and guest filesystems. With virtio-blk, container layers are copied into block device images, providing a clean separation rather than a live shared filesystem.

### Design Overview

The virtio-blk backend implementation consists of several key components:

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host                                     │
├─────────────────────────────────────────────────────────────────┤
│  Kuasar VMM                                                      │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────┐ │
│  │ Sandbox Config   │  │ Container Layer  │  │ Bind Mount     │ │
│  │ (hostname, etc)  │  │ (overlay)        │  │ (HostPath)     │ │
│  └────────┬─────────┘  └────────┬─────────┘  └───────┬────────┘ │
│           │ TTRPC               │ ext4 image         │          │
│           │ push                │ creation           │          │
│           ▼                     ▼                    ▼          │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              Hot-Attach / TTRPC File Push                │  │
│  └──────────────────────────────┬───────────────────────────┘  │
└─────────────────────────────────┼──────────────────────────────┘
                                  │ virtio-blk PCI / TTRPC
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Guest VM                                 │
├─────────────────────────────────────────────────────────────────┤
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────┐ │
│  │ /run/kuasar/     │  │ virtio-blk       │  │ ext4 mount     │ │
│  │ state/           │  │ PCI device       │  │ point          │ │
│  │ (config files)   │  │ (block device)   │  │                │ │
│  └────────┬─────────┘  └────────┬─────────┘  └───────┬────────┘ │
│           │                     │                    │          │
│           ▼                     ▼                    ▼          │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                Container Rootfs Mount                     │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### Risks and Mitigations

#### Risk 1: Increased Disk Space Usage

Creating ext4 images for each container layer increases disk space usage compared to virtiofs which directly shares host directories.

**Mitigation**: 
- Use sparse files for ext4 images
- Implement proper cleanup when containers are removed
- Provide configurable `image_size_overhead_percent` for size estimation

#### Risk 2: Startup Latency

Creating ext4 images and copying content adds latency to container startup compared to virtiofs.

**Mitigation**:
- Use concurrent file pushes for small directories
- Optimize ext4 creation with `-O ^has_journal` and lazy init options
- Small directories (<50 files, <10MB) use TTRPC injection instead of block devices

#### Risk 3: No Real-time File Updates

Unlike virtiofs which provides a live shared filesystem, virtio-blk provides static snapshots at container creation time.

**Mitigation**: 
- Document this limitation clearly
- Bind mounts for configuration files (hostname, resolv.conf) are pushed before sandbox setup

## Design Details

### Configuration

The virtio-blk backend is configured via the `share_backend` option in the Cloud-Hypervisor configuration file:

```toml
[hypervisor]
# share_backend controls how container layers are shared with the guest VM.
#
#   "virtiofs"    (default)  — container layers are shared via virtiofsd + virtio-fs.
#                             virtiofsd must be installed at [hypervisor.virtiofsd].path.
#
#   "virtio-blk"  (optional) — overlay mounts are packaged into ext4 images and
#                             hot-attached as virtio-blk PCI devices.
#
share_backend = "virtiofs"

# virtiofsd configuration — only used when share_backend = "virtiofs"
[hypervisor.virtiofsd]
path = "/usr/local/bin/virtiofsd"
log_level = "info"
cache = "never"
thread_pool_size = 4
```

An additional configuration option `image_size_overhead_percent` controls the size estimation overhead:

```toml
[hypervisor]
# Extra percentage added to estimated image sizes to account for filesystem overhead
# Default: 20 (adds 20% to estimated size)
image_size_overhead_percent = 20
```

### Share Backend Selection

The `ShareBackend` enum defines the available backend options:

```rust
#[derive(Serialize, Deserialize, PartialEq, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShareBackend {
    #[default]
    Virtiofs,
    VirtioBlk,
}
```

The backend selection affects:
1. **Memory configuration**: `shared_memory` is enabled only for virtiofs
2. **Kernel parameters**: `task.share_backend=<backend>` is passed to the guest
3. **virtiofsd startup**: Only started when `share_backend == Virtiofs`
4. **Storage handling**: Overlay and bind mounts are processed differently

### VM Configuration Changes

#### Memory Configuration

In virtio-blk mode, shared memory is disabled because there is no virtiofs requirement:

```rust
let memory = Memory::new(
    (vm_config.common.memory_in_mb as u64) * 1024 * 1024,
    vm_config.share_backend == ShareBackend::Virtiofs, // shared_memory
    vm_config.hugepages,
);
```

#### Kernel Command Line

The share backend type is communicated to the guest via kernel parameters:

```rust
let cmdline = format!(
    "{} task.share_backend={} {}",
    DEFAULT_KERNEL_PARAMS,
    vm_config.share_backend.as_str(),
    vm_config.common.kernel_params
);
```

The guest agent reads `task.share_backend` to determine how to handle storage requests.

### Sandbox Configuration File Delivery

In virtio-blk mode, sandbox configuration files must be pushed to the guest before `setup_sandbox()` is called, since there is no shared filesystem.

**Files pushed**:
- `hostname` → `/run/kuasar/state/hostname`
- `resolv.conf` → `/run/kuasar/state/resolv.conf` (with bind mount to `/etc/resolv.conf`)
- `hosts` → `/run/kuasar/state/hosts`

**Implementation flow**:

1. Create `KUASAR_STATE_DIR` in guest via TTRPC
2. Read configuration files from host shared directory
3. Push files concurrently using `tokio::join!` to reduce latency
4. Each push uses `exec_vm_process` TTRPC call with file content as stdin

```rust
async fn push_sandbox_files(&self) -> Result<()> {
    // Create state directory
    self.exec_in_guest(&format!("mkdir -p {}", KUASAR_STATE_DIR)).await?;
    
    // Concurrent file pushes
    let (r1, r2, r3) = tokio::join!(
        push_hostname,
        push_resolv,  // includes bind mount to /etc/resolv.conf
        push_hosts
    );
    // ...
}
```

### Container Layer Handling

#### Overlay Mount Handling

For overlay mounts in virtio-blk mode, the implementation follows these steps:

```
┌──────────────────────────────────────────────────────────────────┐
│                     Overlay Mount Flow                            │
├──────────────────────────────────────────────────────────────────┤
│ Step 1: Mount overlay on host to temporary directory              │
│         overlay_dir = {base_dir}/overlay-{storage_id}             │
│         mount_rootfs(overlay, overlay_dir)                        │
│                                                                   │
│ Step 2: Estimate directory size                                   │
│         du -sm overlay_dir → size_mb                              │
│         size = size_mb + overhead_percent + OVERLAY_IMG_FALLBACK  │
│                                                                   │
│ Step 3: Create ext4 sparse image                                  │
│         truncate {storage_id}.img to size                         │
│         mkfs.ext4 -F -O ^has_journal {storage_id}.img             │
│                                                                   │
│ Step 4: Copy overlay content to ext4                              │
│         mount -o loop {storage_id}.img mnt_dir                    │
│         rsync -aHAX overlay_dir/ mnt_dir/                         │
│         umount mnt_dir                                            │
│                                                                   │
│ Step 5: Unmount overlay from host                                 │
│         unmount overlay_dir                                       │
│         remove overlay_dir                                        │
│                                                                   │
│ Step 6: Hot-attach ext4 image as virtio-blk                       │
│         hot_attach(BlockDeviceInfo { id, path, read_only })       │
│         → returns (bus_type, pci_addr)                            │
│                                                                   │
│ Step 7: Record storage entry                                      │
│         storage.source = pci_addr                                 │
│         storage.driver = BlockDriver::from_bus_type(bus_type)     │
│         storage.fstype = "ext4"                                   │
│         storage.need_guest_handle = true                          │
│         guest mounts via PCI address                              │
└──────────────────────────────────────────────────────────────────┘
```

**Key code path**: `handle_overlay_mount_blk` in `storage/mod.rs`

#### Bind Mount Handling

Bind mounts are handled differently based on the source type:

```
┌──────────────────────────────────────────────────────────────────┐
│                     Bind Mount Decision Tree                      │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  bind mount source                                                │
│       │                                                           │
│       ▼                                                           │
│  ┌─────────────┐                                                  │
│  │ Is it a     │──── Yes ──► Push single file via TTRPC           │
│  │ single file?│         dest = /run/kuasar/state/{storage_id}   │
│  └─────────────┘         driver = "guest-file"                   │
│       │                   need_guest_handle = false              │
│       No                                                          │
│       ▼                                                           │
│  ┌─────────────────────────────────────────────┐                  │
│  │ Count files and total bytes                  │                  │
│  │ count ≤ 50 files && bytes ≤ 10MB?            │                  │
│  └─────────────────────────────────────────────┘                  │
│       │                                                           │
│       ├──── Yes ──► Small directory injection                     │
│       │             Push each file via TTRPC                      │
│       │             Create directories in guest                   │
│       │             driver = "guest-file"                         │
│       │                                                           │
│       No                                                          │
│       │                                                           │
│       ▼                                                           │
│  ┌─────────────────────────────────────────────┐                  │
│  │ Large directory handling                     │                  │
│  │ 1. Estimate size                             │                  │
│  │ 2. Create ext4 image                         │                  │
│  │ 3. Copy content via rsync                    │                  │
│  │ 4. Hot-attach as virtio-blk                  │                  │
│  │ driver = BlockDriver::from_bus_type          │                  │
│  │ fstype = "ext4"                              │                  │
│  │ need_guest_handle = true                     │                  │
│  └─────────────────────────────────────────────┘                  │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

**Threshold constants**:
- `SMALL_DIR_MAX_FILES = 50`
- `SMALL_DIR_MAX_BYTES = 10 * 1024 * 1024` (10 MB)
- `OVERLAY_IMG_FALLBACK_SIZE_MB = 64`
- `BIND_IMG_FALLBACK_SIZE_MB = 8`

#### Small Directory Injection

For small directories, files are injected one-by-one via TTRPC to avoid creating unnecessary block devices:

```rust
async fn inject_small_dir(
    &mut self,
    storage_id: &str,
    container_id: &str,
    src_dir: &str,
    dest_dir_in_guest: &str,
    m: &Mount,
) -> Result<()> {
    // Create destination directory
    self.exec_in_guest(&format!("mkdir -p {}", dest_dir_in_guest)).await?;
    
    // Walk source directory and push each file
    let mut stack = vec![src_dir.to_string()];
    while let Some(dir) = stack.pop() {
        // For each entry:
        // - Directory: mkdir in guest, add to stack
        // - File: push content + chmod
        // - Symlink: skipped
    }
    
    // Record storage with driver = "guest-file"
}
```

#### Large Directory Block Device

For large directories (e.g., HostPath volumes), an ext4 image is created and hot-attached:

```rust
// Create ext4 image
let size_mb = estimate_dir_size_mb(&source).await.unwrap_or(BIND_IMG_FALLBACK_SIZE_MB);
let size_mb = apply_overhead(base, overhead_percent) + BIND_IMG_FALLBACK_SIZE_MB;
create_ext4_image(&img_path, size_mb).await?;
copy_dir_to_ext4(&source, &img_path).await?;

// Hot-attach
let device_id = format!("blk{}", self.increment_and_get_id());
let (bus_type, pci_addr) = self.vm.hot_attach(DeviceInfo::Block(BlockDeviceInfo {
    id: device_id.clone(),
    path: img_path.clone(),
    read_only,
})).await?;

// Record storage with need_guest_handle = true
// Guest mounts using PCI address
```

### Cleanup and Resource Management

When a container is removed, resources are cleaned up based on the storage type:

```rust
async fn detach_storage(&mut self, id: &str, device_id: Option<&str>, fs_type: &str) -> Result<()> {
    if let Some(did) = device_id {
        self.vm.hot_detach(&did).await?;
        // Clean up ext4 image for virtio-blk container layers
        if fs_type == "ext4" {
            let img_path = format!("{}/{}.img", self.base_dir, id);
            tokio::fs::remove_file(&img_path).await;
        }
    } else if fs_type == "bind" {
        // Unmount bind mount point
        unmount(&mount_point, MNT_DETACH | MNT_NOFOLLOW)?;
    }
    // "guest-file" type: no host-side cleanup needed
}
```

### Security Considerations

#### Shell Injection Prevention

All paths passed to guest shell commands are validated and quoted:

```rust
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn validate_guest_path(path: &str) -> Result<()> {
    if path.chars().all(|c| c.is_ascii_alphanumeric() || "/_.:-".contains(c)) {
        Ok(())
    } else {
        Err(Error::InvalidArgument(...))
    }
}
```

**Path validation rules**:
- Container IDs from CRI are validated: must be alphanumeric + `/_.:-`
- Internally generated paths are validated before use
- All paths are shell-quoted to prevent injection

#### Example validation:

```rust
let bundle_guest = format!("{}/{}", KUASAR_STATE_DIR, self.container_id);
if bundle_guest.chars().any(|c| !c.is_ascii_alphanumeric() && !"/_.:-".contains(c)) {
    return Err(anyhow!("container_id contains unsafe characters"));
}
```

## Implementation Details

### Key Data Structures

#### Storage Record

```rust
pub struct Storage {
    host_source: String,        // Original source path on host
    type: String,               // "overlay" or "bind"
    id: String,                 // Storage identifier
    device_id: Option<String>,  // Block device ID (for virtio-blk)
    ref_container: HashMap<String, u32>,  // Container references
    need_guest_handle: bool,    // True for block devices, false for guest-file
    source: String,             // PCI address for blk, empty for guest-file
    driver: String,             // "virtio-blk", "virtio-scsi", or "guest-file"
    driver_options: Vec<String>,
    fstype: String,             // "ext4" for blk, "bind" for guest-file
    options: Vec<String>,       // Mount options
    mount_point: String,        // Guest mount point path
}
```

#### Driver Types

- `virtio-blk`: Block device attached via virtio-blk
- `virtio-scsi`: Block device attached via virtio-scsi
- `guest-file`: File pushed via TTRPC (no block device)

### Helper Functions

#### Directory Size Estimation

```rust
async fn estimate_dir_size_mb(dir: &str) -> Result<u64> {
    let output = tokio::process::Command::new("du")
        .args(["-sm", dir])
        .output()
        .await?;
    // Parse first number from output
    let size_mb = stdout.split_whitespace().next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(64);
    Ok(size_mb)
}
```

#### Ext4 Image Creation

```rust
async fn create_ext4_image(path: &str, size_mb: u64) -> Result<()> {
    // Create sparse file
    let file = tokio::fs::File::create(path).await?;
    file.set_len(size_mb * 1024 * 1024).await?;
    drop(file);

    // Format as ext4 without journal (faster)
    tokio::process::Command::new("mkfs.ext4")
        .args([
            "-F",
            "-O", "^has_journal",        // Disable journal
            "-E", "lazy_itable_init=0,lazy_journal_init=0",
            path,
        ])
        .status()
        .await?;
}
```

#### Content Copy to Ext4

```rust
async fn copy_dir_to_ext4(src_dir: &str, img_path: &str) -> Result<()> {
    let mnt_dir = format!("{}.mnt", img_path);
    
    // Mount ext4 image via loop device
    tokio::process::Command::new("mount")
        .args(["-o", "loop", img_path, &mnt_dir])
        .status()
        .await?;

    // Copy content preserving attributes
    tokio::process::Command::new("rsync")
        .args(["-aHAX", "--delete", &format!("{}/", src_dir), &format!("{}/", mnt_dir)])
        .status()
        .await?;

    // Unmount (force detach if needed)
    unmount(&mnt_dir, MNT_DETACH | MNT_NOFOLLOW)?;
}
```

### Image Size Estimation

The image size is calculated with overhead to account for filesystem metadata:

```rust
fn apply_overhead(base: u64, overhead_percent: u32) -> u64 {
    base * (100 + overhead_percent as u64) / 100
}

// For overlay mounts:
let size_mb = apply_overhead(estimated_size, overhead_percent) + OVERLAY_IMG_FALLBACK_SIZE_MB;

// For bind mounts:
let size_mb = apply_overhead(estimated_size, overhead_percent) + BIND_IMG_FALLBACK_SIZE_MB;
```

Default overhead: 20% (configurable via `image_size_overhead_percent`)

## Test Plan

### Unit Tests

The implementation includes comprehensive unit tests:

1. **Configuration parsing tests**:
   - `test_default_share_backend_virtiofs`: Verify default is virtiofs
   - `test_valid_share_backend_virtio_blk`: Accept valid virtio-blk value
   - `test_valid_share_backend_virtiofs`: Accept valid virtiofs value
   - `test_invalid_share_backend_rejected`: Reject invalid values

2. **Path validation tests**:
   - `test_validate_guest_path_ok`: Accept safe paths
   - `test_validate_guest_path_reject_shell_special`: Reject dangerous characters

3. **Shell quoting tests**:
   - `test_shell_quote`: Verify quoting handles special characters

4. **Directory counting tests**:
   - `test_count_dir_contents_empty`: Empty directory returns 0
   - `test_count_dir_contents_with_files`: Count files in nested directories

5. **Threshold logic tests**:
   - `test_small_dir_threshold_logic`: Small directories below threshold
   - `test_large_dir_threshold_logic`: Large directories exceed threshold

### Integration Tests

Integration tests require root privileges:

1. `test_create_ext4_image_integration`: 
   - Create ext4 image
   - Verify with `file` command

2. `test_copy_dir_to_ext4_integration`:
   - Create ext4 image
   - Copy directory content
   - Mount and verify content

**Note**: Integration tests are marked with `#[ignore]` and require root for `mkfs.ext4` and `mount -o loop`.

## Future Enhancements

1. **Performance optimization**: Cache ext4 images for frequently used layers
2. **Snapshot support**: Implement incremental updates to ext4 images
3. **Other hypervisors**: Extend support to other hypervisor backends
4. **Symlink handling**: Support symbolic links in bind mount directories
5. **Size tuning**: Dynamic size adjustment based on actual usage

## Drawbacks

1. **Increased disk usage**: Each container layer requires a separate ext4 image
2. **Startup latency**: Creating and copying ext4 images takes time
3. **No live updates**: Changes to host files are not reflected in guest
4. **Symlink limitation**: Symbolic links in bind mounts are skipped

## Alternatives

### Alternative 1: virtiofs Only

Continue using virtiofs as the sole backend. This avoids the complexity of block device management but requires virtiofsd deployment.

**Rejected**: Does not meet goal of minimal VM deployment without virtiofsd.

### Alternative 2: virtio-9p Backend

Use virtio-9p instead of virtio-blk for file sharing.

**Rejected**: virtio-9p has known performance limitations compared to virtio-blk.

### Alternative 3: PMEM (Persistent Memory)

Use PMEM devices for container layer sharing.

**Rejected**: Requires specific hardware support and is less portable.

### Alternative 4: Direct Block Device Passthrough

Pass host block devices directly to the VM without creating images.

**Rejected**: Does not work for overlay mounts which are synthetic filesystems constructed from multiple layers.