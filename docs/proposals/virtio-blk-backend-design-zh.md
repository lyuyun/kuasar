# Cloud-Hypervisor Virtio-Blk 后端支持

<!-- toc -->
- [概述](#概述)
- [动机](#动机)
  - [目标](#目标)
  - [非目标](#非目标)
- [方案](#方案)
  - [用户场景](#用户场景)
  - [设计概览](#设计概览)
  - [风险与缓解措施](#风险与缓解措施)
- [设计细节](#设计细节)
  - [配置](#配置)
  - [共享后端选择](#共享后端选择)
  - [虚拟机配置变更](#虚拟机配置变更)
  - [Sandbox 配置文件传递](#sandbox-配置文件传递)
  - [容器层处理](#容器层处理)
    - [Overlay 挂载处理](#overlay-挂载处理)
    - [Bind 挂载处理](#bind-挂载处理)
    - [小目录注入](#小目录注入)
    - [大目录块设备处理](#大目录块设备处理)
  - [清理与资源管理](#清理与资源管理)
  - [安全考量](#安全考量)
- [实现细节](#实现细节)
  - [关键数据结构](#关键数据结构)
  - [辅助函数](#辅助函数)
  - [镜像大小估算](#镜像大小估算)
- [测试计划](#测试计划)
  - [单元测试](#单元测试)
  - [集成测试](#集成测试)
- [未来增强](#未来增强)
- [缺点](#缺点)
- [替代方案](#替代方案)
<!-- /toc -->

## 概述

本文档描述了 Kuasar 中 Cloud-Hypervisor 的 virtio-blk 后端支持的设计与实现。virtio-blk 后端提供了除默认 virtiofs 后端之外的另一种选择，用于与 guest VM 共享容器层和配置文件。与使用共享文件系统（virtiofs）不同，容器层被打包成 ext4 镜像，并通过 virtio-blk PCI 设备热添加到虚拟机中。

该实现引入了可配置的 `share_backend` 选项，允许用户在以下两种模式间选择：
- **virtiofs**（默认）：容器层通过 virtiofsd + virtio-fs 共享
- **virtio-blk**：容器层被打包成 ext4 镜像并作为块设备热添加

## 动机

### 目标

1. 为 Cloud-Hypervisor 提供一种不依赖 virtiofsd 的替代存储后端
2. 使容器工作负载能够在无需共享文件系统守护进程的虚拟机中运行
3. 减少最小化 VM 部署的依赖项
4. 支持使用 virtio-blk 后端处理 overlay 和 bind 挂载场景
5. 保持与现有 virtiofs 部署的向后兼容性
6. 确保容器移除时正确清理临时资源（ext4 镜像）

### 非目标

1. 支持除 Cloud-Hypervisor 外的其他 hypervisor（未来工作）
2. 大规模部署的性能优化（初始实现侧重于正确性）
3. 支持 bind 挂载目录中的符号链接（当前跳过）
4. 主机与 guest 之间文件变更的实时同步（块设备是静态快照）

## 方案

### 用户场景

#### 场景 1：无需 virtiofsd 的最小化 VM 部署

用户希望在最小化虚拟机环境中部署容器，而不安装 virtiofsd。virtio-blk 后端允许虚拟机通过块设备访问容器层，消除了对单独 virtiofs 守护进程的需求。

#### 场景 2：减少资源占用

运行多个 VM sandbox 的用户希望减少辅助进程（virtiofsd 实例）的数量。使用 virtio-blk 后端可以消除 virtiofsd 进程，降低内存和 CPU 开销。

#### 场景 3：安全隔离工作负载

用户希望主机和 guest 文件系统之间有更强的隔离。使用 virtio-blk 时，容器层被复制到块设备镜像中，提供了清晰的分离，而非实时的共享文件系统。

### 设计概览

virtio-blk 后端实现包含以下关键组件：

```
┌─────────────────────────────────────────────────────────────────┐
│                         主机 (Host)                              │
├─────────────────────────────────────────────────────────────────┤
│  Kuasar VMM                                                      │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────┐ │
│  │ Sandbox 配置     │  │ 容器层           │  │ Bind 挂载      │ │
│  │ (hostname 等)    │  │ (overlay)        │  │ (HostPath)     │ │
│  └────────┬─────────┘  └────────┬─────────┘  └───────┬────────┘ │
│           │ TTRPC               │ ext4 镜像         │          │
│           │ 推送                │ 创建              │          │
│           ▼                     ▼                   ▼          │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              热添加 / TTRPC 文件推送                      │  │
│  └──────────────────────────────┬───────────────────────────┘  │
└─────────────────────────────────┼──────────────────────────────┘
                                  │ virtio-blk PCI / TTRPC
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Guest VM                                 │
├─────────────────────────────────────────────────────────────────┤
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────┐ │
│  │ /run/kuasar/     │  │ virtio-blk       │  │ ext4 挂载点    │ │
│  │ state/           │  │ PCI 设备         │  │                │ │
│  │ (配置文件)       │  │ (块设备)         │  │                │ │
│  └────────┬─────────┘  └────────┬─────────┘  └───────┬────────┘ │
│           │                     │                    │          │
│           ▼                     ▼                    ▼          │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                容器 Rootfs 挂载                           │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 风险与缓解措施

#### 风险 1：磁盘空间使用增加

为每个容器层创建 ext4 镜像会增加磁盘空间使用，相比之下 virtiofs 直接共享主机目录。

**缓解措施**：
- 使用稀疏文件作为 ext4 镜像
- 容器移除时实施正确的清理
- 提供可配置的 `image_size_overhead_percent` 用于大小估算

#### 风险 2：启动延迟

创建 ext4 镜像和复制内容会增加容器启动延迟，相比之下 virtiofs 更快。

**缓解措施**：
- 对小目录使用并发文件推送
- 通过 `-O ^has_journal` 和 lazy init 选项优化 ext4 创建
- 小目录（<50 文件，<10MB）使用 TTRPC 注入而非块设备

#### 风险 3：无实时文件更新

virtiofs 提供实时共享文件系统，而 virtio-blk 在容器创建时提供静态快照。

**缓解措施**：
- 清晰文档化此限制
- 配置文件（hostname、resolv.conf）的 bind 挂载在 sandbox 设置前推送

## 设计细节

### 配置

virtio-blk 后端通过 Cloud-Hypervisor 配置文件中的 `share_backend` 选项进行配置：

```toml
[hypervisor]
# share_backend 控制容器层如何与 guest VM 共享。
#
#   "virtiofs"    (默认)  — 容器层通过 virtiofsd + virtio-fs 共享。
#                             virtiofsd 必须安装在 [hypervisor.virtiofsd].path。
#
#   "virtio-blk"  (可选)   — overlay 挂载被打包成 ext4 镜像并
#                             作为 virtio-blk PCI 设备热添加。
#
share_backend = "virtiofs"

# virtiofsd 配置 — 仅当 share_backend = "virtiofs" 时使用
[hypervisor.virtiofsd]
path = "/usr/local/bin/virtiofsd"
log_level = "info"
cache = "never"
thread_pool_size = 4
```

额外的配置选项 `image_size_overhead_percent` 控制大小估算的额外开销：

```toml
[hypervisor]
# 添加到估算镜像大小的额外百分比，用于文件系统开销
# 默认值：20（在估算大小上增加 20%）
image_size_overhead_percent = 20
```

### 共享后端选择

`ShareBackend` 枚举定义了可用的后端选项：

```rust
#[derive(Serialize, Deserialize, PartialEq, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShareBackend {
    #[default]
    Virtiofs,
    VirtioBlk,
}
```

后端选择会影响：
1. **内存配置**：仅在 virtiofs 模式下启用 `shared_memory`
2. **内核参数**：向 guest 传递 `task.share_backend=<backend>`
3. **virtiofsd 启动**：仅在 `share_backend == Virtiofs` 时启动
4. **存储处理**：Overlay 和 bind 挂载的处理方式不同

### 虚拟机配置变更

#### 内存配置

在 virtio-blk 模式下，共享内存被禁用，因为没有 virtiofs 的需求：

```rust
let memory = Memory::new(
    (vm_config.common.memory_in_mb as u64) * 1024 * 1024,
    vm_config.share_backend == ShareBackend::Virtiofs, // shared_memory
    vm_config.hugepages,
);
```

#### 内核命令行

共享后端类型通过内核参数传递给 guest：

```rust
let cmdline = format!(
    "{} task.share_backend={} {}",
    DEFAULT_KERNEL_PARAMS,
    vm_config.share_backend.as_str(),
    vm_config.common.kernel_params
);
```

Guest agent 读取 `task.share_backend` 来确定如何处理存储请求。

### Sandbox 配置文件传递

在 virtio-blk 模式下，sandbox 配置文件必须在调用 `setup_sandbox()` 之前推送到 guest，因为没有共享文件系统。

**推送的文件**：
- `hostname` → `/run/kuasar/state/hostname`
- `resolv.conf` → `/run/kuasar/state/resolv.conf`（并 bind 挂载到 `/etc/resolv.conf`）
- `hosts` → `/run/kuasar/state/hosts`

**实现流程**：

1. 通过 TTRPC 在 guest 中创建 `KUASAR_STATE_DIR`
2. 从主机共享目录读取配置文件
3. 使用 `tokio::join!` 并发推送文件以减少延迟
4. 每次推送使用 `exec_vm_process` TTRPC 调用，文件内容作为 stdin

```rust
async fn push_sandbox_files(&self) -> Result<()> {
    // 创建状态目录
    self.exec_in_guest(&format!("mkdir -p {}", KUASAR_STATE_DIR)).await?;
    
    // 并发文件推送
    let (r1, r2, r3) = tokio::join!(
        push_hostname,
        push_resolv,  // 包含 bind 挂载到 /etc/resolv.conf
        push_hosts
    );
    // ...
}
```

### 容器层处理

#### Overlay 挂载处理

对于 virtio-blk 模式下的 overlay 挂载，实现遵循以下步骤：

```
┌──────────────────────────────────────────────────────────────────┐
│                     Overlay 挂载处理流程                          │
├──────────────────────────────────────────────────────────────────┤
│ 步骤 1：在主机上将 overlay 挂载到临时目录                          │
│         overlay_dir = {base_dir}/overlay-{storage_id}             │
│         mount_rootfs(overlay, overlay_dir)                        │
│                                                                   │
│ 步骤 2：估算目录大小                                               │
│         du -sm overlay_dir → size_mb                              │
│         size = size_mb + overhead_percent + OVERLAY_IMG_FALLBACK  │
│                                                                   │
│ 步骤 3：创建 ext4 稀疏镜像                                         │
│         truncate {storage_id}.img to size                         │
│         mkfs.ext4 -F -O ^has_journal {storage_id}.img             │
│                                                                   │
│ 步骤 4：将 overlay 内容复制到 ext4                                 │
│         mount -o loop {storage_id}.img mnt_dir                    │
│         rsync -aHAX overlay_dir/ mnt_dir/                         │
│         umount mnt_dir                                            │
│                                                                   │
│ 步骤 5：从主机卸载 overlay                                         │
│         unmount overlay_dir                                       │
│         remove overlay_dir                                        │
│                                                                   │
│ 步骤 6：将 ext4 镜像作为 virtio-blk 热添加                         │
│         hot_attach(BlockDeviceInfo { id, path, read_only })       │
│         → 返回 (bus_type, pci_addr)                               │
│                                                                   │
│ 步骤 7：记录存储条目                                               │
│         storage.source = pci_addr                                 │
│         storage.driver = BlockDriver::from_bus_type(bus_type)     │
│         storage.fstype = "ext4"                                   │
│         storage.need_guest_handle = true                          │
│         guest 通过 PCI 地址挂载                                    │
└──────────────────────────────────────────────────────────────────┘
```

**关键代码路径**：`storage/mod.rs` 中的 `handle_overlay_mount_blk`

#### Bind 挂载处理

Bind 挂载根据源类型采用不同的处理方式：

```
┌──────────────────────────────────────────────────────────────────┐
│                     Bind 挂载决策树                                │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  bind 挂载源                                                       │
│       │                                                           │
│       ▼                                                           │
│  ┌─────────────┐                                                  │
│  │ 是单个      │──── 是 ──► 通过 TTRPC 推送单个文件                │
│  │ 文件？      │         dest = /run/kuasar/state/{storage_id}   │
│  └─────────────┘         driver = "guest-file"                   │
│       │                   need_guest_handle = false              │
│       否                                                          │
│       ▼                                                           │
│  ┌─────────────────────────────────────────────┐                  │
│  │ 统计文件数量和总字节大小                      │                  │
│  │ 数量 ≤ 50 文件 && 字节 ≤ 10MB？              │                  │
│  └─────────────────────────────────────────────┘                  │
│       │                                                           │
│       ├──── 是 ──► 小目录注入                                     │
│       │             通过 TTRPC 推送每个文件                       │
│       │             在 guest 中创建目录                           │
│       │             driver = "guest-file"                         │
│       │                                                           │
│       否                                                          │
│       │                                                           │
│       ▼                                                           │
│  ┌─────────────────────────────────────────────┐                  │
│  │ 大目录处理                                   │                  │
│  │ 1. 估算大小                                  │                  │
│  │ 2. 创建 ext4 镜像                            │                  │
│  │ 3. 通过 rsync 复制内容                       │                  │
│  │ 4. 作为 virtio-blk 热添加                    │                  │
│  │ driver = BlockDriver::from_bus_type          │                  │
│  │ fstype = "ext4"                              │                  │
│  │ need_guest_handle = true                     │                  │
│  └─────────────────────────────────────────────┘                  │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

**阈值常量**：
- `SMALL_DIR_MAX_FILES = 50`
- `SMALL_DIR_MAX_BYTES = 10 * 1024 * 1024`（10 MB）
- `OVERLAY_IMG_FALLBACK_SIZE_MB = 64`
- `BIND_IMG_FALLBACK_SIZE_MB = 8`

#### 小目录注入

对于小目录，文件通过 TTRPC 一个一个注入，避免创建不必要的块设备：

```rust
async fn inject_small_dir(
    &mut self,
    storage_id: &str,
    container_id: &str,
    src_dir: &str,
    dest_dir_in_guest: &str,
    m: &Mount,
) -> Result<()> {
    // 创建目标目录
    self.exec_in_guest(&format!("mkdir -p {}", dest_dir_in_guest)).await?;
    
    // 遍历源目录并推送每个文件
    let mut stack = vec![src_dir.to_string()];
    while let Some(dir) = stack.pop() {
        // 对于每个条目：
        // - 目录：在 guest 中 mkdir，添加到栈
        // - 文件：推送内容 + chmod
        // - 符号链接：跳过
    }
    
    // 记录存储，driver = "guest-file"
}
```

#### 大目录块设备处理

对于大目录（如 HostPath 卷），创建 ext4 镜像并热添加：

```rust
// 创建 ext4 镜像
let size_mb = estimate_dir_size_mb(&source).await.unwrap_or(BIND_IMG_FALLBACK_SIZE_MB);
let size_mb = apply_overhead(base, overhead_percent) + BIND_IMG_FALLBACK_SIZE_MB;
create_ext4_image(&img_path, size_mb).await?;
copy_dir_to_ext4(&source, &img_path).await?;

// 热添加
let device_id = format!("blk{}", self.increment_and_get_id());
let (bus_type, pci_addr) = self.vm.hot_attach(DeviceInfo::Block(BlockDeviceInfo {
    id: device_id.clone(),
    path: img_path.clone(),
    read_only,
})).await?;

// 记录存储，need_guest_handle = true
// Guest 使用 PCI 地址挂载
```

### 清理与资源管理

容器移除时，根据存储类型清理资源：

```rust
async fn detach_storage(&mut self, id: &str, device_id: Option<&str>, fs_type: &str) -> Result<()> {
    if let Some(did) = device_id {
        self.vm.hot_detach(&did).await?;
        // 清理 virtio-blk 容器层的 ext4 镜像
        if fs_type == "ext4" {
            let img_path = format!("{}/{}.img", self.base_dir, id);
            tokio::fs::remove_file(&img_path).await;
        }
    } else if fs_type == "bind" {
        // 卸载 bind 挂载点
        unmount(&mount_point, MNT_DETACH | MNT_NOFOLLOW)?;
    }
    // "guest-file" 类型：无需主机侧清理
}
```

### 安全考量

#### Shell 注入防护

传递给 guest shell 命令的所有路径都经过验证和引号处理：

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

**路径验证规则**：
- 来自 CRI 的容器 ID 需验证：必须是字母数字 + `/_.:-`
- 内部生成的路径在使用前需验证
- 所有路径都经过 shell 引号处理以防止注入

**验证示例**：

```rust
let bundle_guest = format!("{}/{}", KUASAR_STATE_DIR, self.container_id);
if bundle_guest.chars().any(|c| !c.is_ascii_alphanumeric() && !"/_.:-".contains(c)) {
    return Err(anyhow!("container_id 包含不安全字符"));
}
```

## 实现细节

### 关键数据结构

#### 存储记录

```rust
pub struct Storage {
    host_source: String,        // 主机上的原始源路径
    type: String,               // "overlay" 或 "bind"
    id: String,                 // 存储标识符
    device_id: Option<String>,  // 块设备 ID（用于 virtio-blk）
    ref_container: HashMap<String, u32>,  // 容器引用
    need_guest_handle: bool,    // 块设备为 true，guest-file 为 false
    source: String,             // blk 为 PCI 地址，guest-file 为空
    driver: String,             // "virtio-blk"、"virtio-scsi" 或 "guest-file"
    driver_options: Vec<String>,
    fstype: String,             // blk 为 "ext4"，guest-file 为 "bind"
    options: Vec<String>,       // 挂载选项
    mount_point: String,        // Guest 挂载点路径
}
```

#### 驱动类型

- `virtio-blk`：通过 virtio-blk 挂载的块设备
- `virtio-scsi`：通过 virtio-scsi 挂载的块设备
- `guest-file`：通过 TTRPC 推送的文件（无块设备）

### 辅助函数

#### 目录大小估算

```rust
async fn estimate_dir_size_mb(dir: &str) -> Result<u64> {
    let output = tokio::process::Command::new("du")
        .args(["-sm", dir])
        .output()
        .await?;
    // 从输出解析第一个数字
    let size_mb = stdout.split_whitespace().next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(64);
    Ok(size_mb)
}
```

#### Ext4 镜像创建

```rust
async fn create_ext4_image(path: &str, size_mb: u64) -> Result<()> {
    // 创建稀疏文件
    let file = tokio::fs::File::create(path).await?;
    file.set_len(size_mb * 1024 * 1024).await?;
    drop(file);

    // 格式化为 ext4（无 journal，更快）
    tokio::process::Command::new("mkfs.ext4")
        .args([
            "-F",
            "-O", "^has_journal",        // 禁用 journal
            "-E", "lazy_itable_init=0,lazy_journal_init=0",
            path,
        ])
        .status()
        .await?;
}
```

#### 内容复制到 Ext4

```rust
async fn copy_dir_to_ext4(src_dir: &str, img_path: &str) -> Result<()> {
    let mnt_dir = format!("{}.mnt", img_path);
    
    // 通过 loop 设备挂载 ext4 镜像
    tokio::process::Command::new("mount")
        .args(["-o", "loop", img_path, &mnt_dir])
        .status()
        .await?;

    // 复制内容，保留属性
    tokio::process::Command::new("rsync")
        .args(["-aHAX", "--delete", &format!("{}/", src_dir), &format!("{}/", mnt_dir)])
        .status()
        .await?;

    // 卸载（必要时强制分离）
    unmount(&mnt_dir, MNT_DETACH | MNT_NOFOLLOW)?;
}
```

### 镜像大小估算

镜像大小计算包含额外开销以容纳文件系统元数据：

```rust
fn apply_overhead(base: u64, overhead_percent: u32) -> u64 {
    base * (100 + overhead_percent as u64) / 100
}

// 对于 overlay 挂载：
let size_mb = apply_overhead(estimated_size, overhead_percent) + OVERLAY_IMG_FALLBACK_SIZE_MB;

// 对于 bind 挂载：
let size_mb = apply_overhead(estimated_size, overhead_percent) + BIND_IMG_FALLBACK_SIZE_MB;
```

默认开销：20%（可通过 `image_size_overhead_percent` 配置）

## 测试计划

### 单元测试

实现包含全面的单元测试：

1. **配置解析测试**：
   - `test_default_share_backend_virtiofs`：验证默认值为 virtiofs
   - `test_valid_share_backend_virtio_blk`：接受有效的 virtio-blk 值
   - `test_valid_share_backend_virtiofs`：接受有效的 virtiofs 值
   - `test_invalid_share_backend_rejected`：拒绝无效值

2. **路径验证测试**：
   - `test_validate_guest_path_ok`：接受安全路径
   - `test_validate_guest_path_reject_shell_special`：拒绝危险字符

3. **Shell 引号处理测试**：
   - `test_shell_quote`：验证引号处理能正确处理特殊字符

4. **目录统计测试**：
   - `test_count_dir_contents_empty`：空目录返回 0
   - `test_count_dir_contents_with_files`：统计嵌套目录中的文件

5. **阈值逻辑测试**：
   - `test_small_dir_threshold_logic`：小目录低于阈值
   - `test_large_dir_threshold_logic`：大目录超出阈值

### 集成测试

集成测试需要 root 权限：

1. `test_create_ext4_image_integration`：
   - 创建 ext4 镜像
   - 使用 `file` 命令验证

2. `test_copy_dir_to_ext4_integration`：
   - 创建 ext4 镜像
   - 复制目录内容
   - 挂载并验证内容

**注意**：集成测试标记为 `#[ignore]`，需要 root 权限执行 `mkfs.ext4` 和 `mount -o loop`。

## 未来增强

1. **性能优化**：为常用层缓存 ext4 镜像
2. **快照支持**：实现对 ext4 镜像的增量更新
3. **其他 hypervisor**：扩展支持其他 hypervisor 后端
4. **符号链接处理**：支持 bind 挂载目录中的符号链接
5. **大小调优**：根据实际使用动态调整大小

## 缺点

1. **磁盘使用增加**：每个容器层需要单独的 ext4 镜像
2. **启动延迟**：创建和复制 ext4 镜像需要时间
3. **无实时更新**：主机文件变更不会反映到 guest
4. **符号链接限制**：bind 挂载中的符号链接被跳过

## 替代方案

### 替代方案 1：仅使用 virtiofs

继续使用 virtiofs 作为唯一后端。这避免了块设备管理的复杂性，但需要部署 virtiofsd。

**未采纳原因**：不满足无需 virtiofsd 的最小化 VM 部署目标。

### 替代方案 2：virtio-9p 后端

使用 virtio-9p 替代 virtio-blk 进行文件共享。

**未采纳原因**：virtio-9p 有已知的性能限制，相比 virtio-blk 性能较差。

### 替代方案 3：PMEM（持久内存）

使用 PMEM 设备共享容器层。

**未采纳原因**：需要特定硬件支持，可移植性较差。

### 替代方案 4：直接块设备透传

直接将主机块设备透传给 VM，不创建镜像。

**未采纳原因**：对于 overlay 挂载无效，因为 overlay 是由多层合成的合成文件系统。