# Kuasar Snapshot + Template Pool 方案设计

## 一、目标与范围

### 1.1 现阶段目标

在当前环境（Linux 5.10+）下，优先基于 **Cloud Hypervisor** 实现：

| 目标 | 具体内容 |
|------|---------|
| VM Snapshot | 捕获完整 VM 状态（RAM + CPU + 设备），保存到磁盘 |
| VM Restore | 从 snapshot 恢复 VM，启动时间目标 < 500ms |
| Template Pool | 维护预热模板池，命中时直接 restore，无冷启动 |
| virtio-blk 容器层 | **前置条件**：将容器层从 virtio-fs 切换到 virtio-blk，消除 snapshot 中的外部进程依赖 |
| Kuasar 集成 | 新增 CreateTemplate / 基于模板 restore Sandbox 流程，对 CRI 层透明 |

### 1.2 关键性能指标

| 场景 | 目标 |
|------|------|
| 冷启动（无模板） | < 2s（当前基线） |
| 模板命中 restore | < 500ms |
| Snapshot 捕获耗时 | < 300ms |
| 容器层 ext4 镜像准备 | < 1s（小镜像），可后台预构建 |

### 1.3 不在本阶段范围内

- EROFS / SquashFS rootfs
- userfaultfd Lazy Loading
- 跨节点模板迁移
- QEMU / StratoVirt snapshot（后续 Phase）

---

## 二、当前架构分析

### 2.1 设备组合（factory.rs）

VM 启动时固定装配以下设备：

```
CloudHypervisorVM
├── Pmem("rootfs", image_path, discard_writes=true)   ← VM rootfs，ext4，只读
├── Rng("rng", entropy_source)
├── Vsock(cid=3, socket="{base_dir}/task.vsock")       ← hvsock 通信
├── Console(path="/tmp/{id}-task.log")
└── Fs("fs", socket="{base_dir}/virtiofs.sock",        ← 容器层共享（当前路径）
         tag="kuasar")
```

### 2.2 容器层当前路径：virtio-fs

```
Host                                     Guest (task agent)
                                         initialize():
                                           match sharefs_type:
                                             "virtiofs" →
virtiofsd 进程                               mount("kuasar", KUASAR_STATE_DIR,
  shared_dir={base_dir}/shared/  ←────────    "virtiofs")   (/run/kuasar/state/)
  socket={base_dir}/virtiofs.sock
                                         late_init_call():
容器请求 → handle_overlay_mount():          resolv.conf ← KUASAR_STATE_DIR/resolv.conf
  host 上 mount overlay
  → {shared_dir}/{storage_id}/           spec.mounts[].source = KUASAR_STATE_DIR/{storage_id}
  need_guest_handle: false               （guest 经 virtiofs 访问此路径）
```

关键代码（`vmm/task/src/main.rs`）：

```rust
match &*config.sharefs_type {
    "9p"       => mount_static_mounts(SHAREFS_9P_MOUNTS.clone()).await?,
    "virtiofs" => mount_static_mounts(SHAREFS_VIRTIOFS_MOUNTS.clone()).await?,
    _          => warn!("sharefs_type should be either 9p or virtiofs"),
}
// late_init_call: resolv.conf 来自 KUASAR_STATE_DIR（virtiofs 共享路径）
```

### 2.3 Rootfs 路径：pmem（与 snapshot 兼容）

VM rootfs 通过 pmem 设备挂载，`discard_writes=on` 使其只读。CH snapshot **不包含** pmem DAX 区域，多 VM 可共享同一 pmem 文件。**此路径无需改动。**

```
image_path (ext4 img)
  --pmem file=...,discard_writes=on
  → /dev/pmem0p1 in guest
  → mount -t ext4 -o ro → / (VM rootfs)
```

---

## 三、virtiofs 对 Snapshot 的阻断分析

### 3.1 根本问题

Cloud Hypervisor 当前**不能正确 restore 含 virtiofs 设备的 snapshot**：

| 阶段 | 问题 |
|------|------|
| Snapshot 时 | virtiofsd 是 CH 进程外的独立进程，其 FUSE session 状态（open fd、inflight request）不在 CH snapshot 范围内 |
| Restore 时 | CH 从 config.json 中读到 `--fs socket=virtiofs.sock`，尝试连接 virtiofsd socket |
| 时序冲突 | 若 virtiofsd 未就绪，CH restore 失败；即使重启 virtiofsd，新 FUSE session 与 snapshot 内的 session 状态不兼容 |
| 根因 | virtiofs 设备的状态依赖外部进程（virtiofsd），而 CH snapshot 机制仅能捕获 CH 内部状态 |

### 3.2 解决方向

**将容器层从 virtio-fs 切换到 virtio-blk：**

- 块设备完全由 CH 内部管理，其状态（PCI 配置空间、写缓冲等）完整包含在 CH snapshot 中
- 无外部进程依赖，snapshot/restore 路径闭合
- 块设备 backing file 在 restore 后由 CH 从 config.json 重新打开，对 sandbox 专属路径的适配通过 patch config.json 实现（详见第六章）

### 3.3 影响范围

切换到 virtio-blk 涉及以下组件：

| 组件 | 当前 | 目标 |
|------|------|------|
| `cloud_hypervisor/factory.rs` | 添加 Fs 设备 + 启动 virtiofsd | 不添加 Fs 设备；添加 config-blk 设备 |
| `cloud_hypervisor/config.rs` | `task.sharefs_type=virtiofs` | `task.sharefs_type=virtio-blk` |
| `vmm/task/src/main.rs` | 仅支持 `9p`/`virtiofs` | 新增 `virtio-blk` 分支 |
| `storage/mod.rs` | overlay → host mount → virtiofs | overlay → ext4 img → hot-attach blk |
| 配置文件传递 | virtiofs 共享目录（resolv.conf 等） | TTRPC 推送 |

---

## 四、Phase 0：virtio-blk 容器层能力（前置实现）

### 4.1 容器层存储：overlay → ext4 virtio-blk

#### 4.1.1 现有路径（virtio-fs）

```
containerd overlay
  lowerdir=layer1:layer2:...,upperdir=writable,workdir=work
     ↓ mount_rootfs() on host
  {shared_dir}/{storage_id}/   (host overlay mount point)
     ↓ virtiofsd
  /run/kuasar/state/{storage_id}  (in guest, via virtiofs)
```

存储标记：`need_guest_handle: false`，`driver: ""`（guest 直接访问 virtiofs 路径，不需要 Storage 协议）

#### 4.1.2 新路径（virtio-blk）

```
containerd overlay
  lowerdir=layer1:layer2:...,upperdir=writable,workdir=work
     ↓ 在 host 上 mount overlay
     ↓ 创建 ext4 镜像，将 overlay 内容写入
  {sandbox_dir}/{storage_id}.img
     ↓ vm.hot_attach(DeviceInfo::Block)
  /dev/vdX  (in guest, PCI 地址由 CH 分配)
     ↓ guest task agent 按 Storage.source（PCI地址）mount -t ext4
  /run/kuasar/storage/containers/{storage_id}  (in guest)
```

存储标记：`need_guest_handle: true`，`driver: "virtio-blk"`（guest 需按 Storage 协议挂载块设备）

#### 4.1.3 实现：`handle_overlay_mount_blk()`

```rust
// vmm/sandbox/src/storage/mod.rs

async fn handle_overlay_mount_blk(
    &mut self,
    storage_id: &str,
    container_id: &str,
    m: &Mount,
) -> Result<()> {
    // 1. 在 host 上 mount overlay（与现有逻辑相同）
    let overlay_dir = format!("{}/overlay-{}", self.base_dir, storage_id);
    tokio::fs::create_dir_all(&overlay_dir).await?;
    mount_rootfs(Some(&m.r#type), Some(&m.source), &m.options, &overlay_dir)
        .map_err(|e| anyhow!("mount overlay: {}", e))?;

    // 2. 计算所需大小（overlay 实际使用量 + 20% 余量 + ext4 元数据）
    let size_mb = estimate_dir_size_mb(&overlay_dir).await? * 12 / 10 + 64;

    // 3. 创建 ext4 镜像
    let img_path = format!("{}/{}.img", self.base_dir, storage_id);
    create_ext4_image(&img_path, size_mb).await?;

    // 4. 将 overlay 内容写入 ext4
    copy_dir_to_ext4(&overlay_dir, &img_path).await?;

    // 5. 卸载 host overlay（内容已写入 img）
    unmount(&overlay_dir, MNT_DETACH | MNT_NOFOLLOW)?;
    tokio::fs::remove_dir(&overlay_dir).await.unwrap_or_default();

    // 6. hot-attach 为 virtio-blk（r/w，容器可写入 upper layer）
    let device_id = format!("blk{}", self.increment_and_get_id());
    let read_only = m.options.contains(&"ro".to_string());
    let (bus_type, pci_addr) = self
        .vm
        .hot_attach(DeviceInfo::Block(BlockDeviceInfo {
            id: device_id.clone(),
            path: img_path.clone(),
            read_only,
        }))
        .await?;

    // 7. 创建 Storage 条目（need_guest_handle=true，guest 按 PCI 地址挂载）
    let mut storage = Storage {
        host_source: m.source.clone(),
        r#type: m.r#type.clone(),
        id: storage_id.to_string(),
        device_id: Some(device_id),
        ref_container: HashMap::new(),
        need_guest_handle: true,
        source: pci_addr,           // guest 用此 PCI 地址找到 /dev/vdX
        driver: BlockDriver::from_bus_type(&bus_type).to_driver_string(),
        driver_options: vec![],
        fstype: "ext4".to_string(),
        options: if read_only { vec!["ro".to_string()] } else { vec![] },
        mount_point: format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id),
    };
    storage.refer(container_id);
    self.storages.push(storage);
    Ok(())
}

/// 创建 ext4 镜像文件
async fn create_ext4_image(path: &str, size_mb: u64) -> Result<()> {
    // truncate 创建稀疏文件（比 dd 快）
    let file = tokio::fs::File::create(path).await?;
    file.set_len(size_mb * 1024 * 1024).await?;
    drop(file);

    tokio::process::Command::new("mkfs.ext4")
        .args(["-O", "^has_journal",
               "-E", "lazy_itable_init=0,lazy_journal_init=0",
               path])
        .status().await
        .map_err(|e| anyhow!("mkfs.ext4: {}", e))?;
    Ok(())
}

/// 将目录内容写入已有 ext4 镜像（通过 loop mount + rsync）
async fn copy_dir_to_ext4(src_dir: &str, img_path: &str) -> Result<()> {
    let mnt_dir = format!("{}.mnt", img_path);
    tokio::fs::create_dir_all(&mnt_dir).await?;

    // mount ext4 img 到临时目录
    nix::mount::mount(
        Some(img_path), mnt_dir.as_str(),
        Some("ext4"),
        nix::mount::MsFlags::empty(),
        Some("loop"),
    ).map_err(|e| anyhow!("mount ext4 img: {}", e))?;

    // 将 overlay 内容 rsync 进去
    tokio::process::Command::new("rsync")
        .args(["-aHAX", "--delete",
               &format!("{}/", src_dir), &format!("{}/", mnt_dir)])
        .status().await
        .map_err(|e| anyhow!("rsync to ext4: {}", e))?;

    unmount(&mnt_dir, MNT_DETACH | MNT_NOFOLLOW)?;
    tokio::fs::remove_dir(&mnt_dir).await.unwrap_or_default();
    Ok(())
}
```

#### 4.1.4 `attach_storage()` 分发逻辑

```rust
// storage/mod.rs

pub async fn attach_storage(
    &mut self, container_id: &str, m: &Mount, is_rootfs_mount: bool,
) -> Result<()> {
    // ...（现有逻辑）

    if is_overlay(m) {
        if self.vm.supports_blk_sharefs() {
            self.handle_overlay_mount_blk(id, container_id, m).await?;
        } else {
            self.handle_overlay_mount(id, container_id, m).await?;
        }
        return Ok(());
    }
    // ...
}
```

`supports_blk_sharefs()` 由 `CloudHypervisorVM` 实现，返回 `true`；QEMU VM 返回 `false` 直到 QEMU 路径实现。

### 4.2 绑定挂载：分类处理策略

bind mount 按内容特征分三类处理，避免为小文件创建块设备（ext4 最小镜像 4MB，为单文件创建代价过高）：

| 类型 | 判断条件 | 处理方式 |
|------|---------|---------|
| 单文件 | 源路径是普通文件 | `SandboxFile` TTRPC 注入 |
| 小目录 | 文件数 < 50 且总大小 < 10MB | tmpfs 挂载 + 文件逐一注入 |
| 大目录（HostPath 等） | 其余 | ext4 virtio-blk（同 overlay blk） |

**单文件注入**（ConfigMap/Secret 单文件、`/etc/hosts` 等）：

扩展 `SandboxFile` 协议（与 resolv.conf 同一机制），由 `ContainerService::create_container()` 在发送 Storage 前先注入：

```protobuf
message SandboxFile {
    string dest_path = 1;   // guest 内目标挂载路径
    bytes  content   = 2;   // 文件内容（TTRPC 传输，< 1MB 适用）
    uint32 mode      = 3;   // 文件权限
}
```

**小目录注入**（ConfigMap 目录挂载）：

Guest agent 收到注入请求后在 guest 内创建 tmpfs，将文件写入，再 bind mount 到容器目标路径：

```rust
// guest task agent
async fn inject_small_dir(req: &SmallDirInjectRequest) -> Result<()> {
    nix::mount::mount(None, &req.tmpfs_path, Some("tmpfs"),
        MsFlags::empty(), None)?;
    for f in &req.files {
        tokio::fs::write(Path::new(&req.tmpfs_path).join(&f.name), &f.content).await?;
    }
    Ok(())
}
```

**大目录 bind mount**（HostPath Volume 等）：

```rust
async fn handle_bind_mount_blk(
    &mut self, source: &str, target: &str, container_id: &str, read_only: bool,
) -> Result<()> {
    // 创建 ext4 镜像，rsync 目录内容，hot-attach virtio-blk
    // 逻辑同 handle_overlay_mount_blk，bind mount 通常设为 ro
}
```

**分类判断入口**：

```rust
pub async fn attach_bind_mount(
    &mut self, source: &str, target: &str, container_id: &str, read_only: bool,
) -> Result<BindMountResult> {
    let meta = tokio::fs::metadata(source).await?;
    if meta.is_file() {
        let content = tokio::fs::read(source).await?;
        return Ok(BindMountResult::Inject(SandboxFile {
            dest_path: target.to_string(), content, mode: meta.permissions().mode(),
        }));
    }
    let (file_count, total_bytes) = count_dir_contents(source).await?;
    if file_count < 50 && total_bytes < 10 * 1024 * 1024 {
        return Ok(BindMountResult::Tmpfs(collect_dir_files(source).await?));
    }
    self.handle_bind_mount_blk(source, target, container_id, read_only).await?;
    Ok(BindMountResult::Block)
}
```

### 4.3 沙箱配置文件（resolv.conf / hosts / hostname）

**当前路径（virtiofs）**：

sandboxer 将 resolv.conf 等写入 `{base_dir}/shared/`，由 virtiofsd 共享给 guest，task agent 在 `late_init_call()` 中从 `KUASAR_STATE_DIR/resolv.conf` 绑定挂载到 `/etc/resolv.conf`。

**新路径（TTRPC 推送）**：

移除 virtiofs 后，通过扩展 `SetupSandboxRequest`（或独立 TTRPC 调用）将配置文件内容直接推送到 guest agent：

```protobuf
// vmm/common/src/protos/sandbox.proto（扩展）

message SandboxFile {
    string dest_path = 1;   // guest 内目标路径，如 "/etc/resolv.conf"
    bytes  content   = 2;   // 文件内容
    uint32 mode      = 3;   // 文件权限
}

message SetupSandboxRequest {
    // ...（现有字段）
    repeated SandboxFile sandbox_files = N;  // 新增
}
```

Guest task agent `SandboxService::setup_sandbox()` 收到后直接写入目标路径，**不依赖任何共享目录**。

此方案的优点：
- 不需要为 resolv.conf 这类小文件创建块设备
- TTRPC 本身就是 sandbox 建立后的首条控制通道
- resolv.conf 内容来自 sandboxer，与网络配置一起在 `post_start` 时推送

### 4.4 Guest Task Agent 适配：新增 `virtio-blk` sharefs_type

```rust
// vmm/task/src/main.rs

match &*config.sharefs_type {
    "9p"          => mount_static_mounts(SHAREFS_9P_MOUNTS.clone()).await?,
    "virtiofs"    => mount_static_mounts(SHAREFS_VIRTIOFS_MOUNTS.clone()).await?,
    "virtio-blk"  => {
        // 不挂载任何共享目录
        // 容器层由 host 侧 hot-attach 的 virtio-blk 块设备提供
        // resolv.conf 等由 TTRPC SetupSandboxRequest 推送
        info!("virtio-blk sharefs mode, no shared fs mount needed");
    }
    _ => warn!("unsupported sharefs_type: {}", config.sharefs_type),
}
```

```rust
// vmm/task/src/main.rs（late_init_call 修改）

async fn late_init_call(config: &TaskConfig) -> Result<()> {
    if config.sharefs_type != "virtio-blk" {
        // 旧路径：从 virtiofs/9p 共享目录读取 resolv.conf
        let dns_file = Path::new(KUASAR_STATE_DIR).join(RESOLV_FILENAME);
        if dns_file.exists() {
            nix::mount::mount(/* bind mount resolv.conf */)?;
        }
    }
    // virtio-blk 路径：resolv.conf 由 TTRPC setup_sandbox() 写入，此处无需操作
    Ok(())
}
```

### 4.5 Factory 适配：移除 Fs 设备

```rust
// vmm/sandbox/src/cloud_hypervisor/factory.rs

async fn create_vm(&self, id: &str, s: &SandboxOption) -> Result<Self::VM> {
    let mut vm = CloudHypervisorVM::new(id, &netns, &s.base_dir, &self.vm_config);

    // pmem rootfs（不变）
    if !self.vm_config.common.image_path.is_empty() {
        vm.add_device(Pmem::new("rootfs", &self.vm_config.common.image_path, true));
    }

    vm.add_device(Rng::new("rng", &self.vm_config.entropy_source));

    let guest_socket_path = format!("{}/task.vsock", s.base_dir);
    vm.add_device(Vsock::new(3, &guest_socket_path, "vsock"));
    vm.agent_socket = format!("hvsock://{}:1024", guest_socket_path);

    let console_path = format!("/tmp/{}-task.log", id);
    vm.add_device(Console::new(&console_path, "console"));

    // virtio-fs 设备：仅在 sharefs_type=virtiofs 时添加（兼容旧配置）
    if self.vm_config.sharefs_type() == "virtiofs"
        && !vm.virtiofsd_config.socket_path.is_empty()
    {
        let fs = Fs::new("fs", &vm.virtiofsd_config.socket_path, "kuasar");
        vm.add_device(fs);
    }
    // virtio-blk 模式下：不添加 Fs 设备，不启动 virtiofsd

    Ok(vm)
}
```

### 4.6 默认内核参数修改

```rust
// vmm/sandbox/src/cloud_hypervisor/config.rs

// 旧
const DEFAULT_KERNEL_PARAMS: &str = "console=hvc0 \
root=/dev/pmem0p1 rootflags=data=ordered,errors=remount-ro \
ro rootfstype=ext4 \
task.sharefs_type=virtiofs";

// 新
const DEFAULT_KERNEL_PARAMS: &str = "console=hvc0 \
root=/dev/pmem0p1 rootflags=data=ordered,errors=remount-ro \
ro rootfstype=ext4 \
task.sharefs_type=virtio-blk";
```

---

## 五、Phase 1：Cloud Hypervisor Snapshot/Restore

Phase 0 完成后，模板 VM 的设备组合变为：

```
Template VM 设备（snapshot 友好）
├── Pmem("rootfs", template_rootfs.img, discard_writes=true)  ← 只读，多 VM 共享
├── Rng("rng", ...)
├── Vsock(cid=3, socket="{base_dir}/task.vsock")
└── Console(...)
   （无 Fs/virtiofsd）
```

所有设备均在 CH 内部管理，snapshot/restore 路径完全闭合。

### 5.1 Snapshottable trait

```rust
// vmm/sandbox/src/vm/mod.rs

#[async_trait]
pub trait Snapshottable {
    /// Pause（可选），捕获 RAM + CPU + 设备状态，resume。
    async fn snapshot(&self, dest_dir: &Path) -> Result<SnapshotMeta>;

    /// 从 snapshot 恢复 VM（替代 vm.start()）。
    /// 调用前提：所有外部依赖（如 virtiofsd，现已移除）已就绪。
    async fn restore(&mut self, src: &RestoreSource) -> Result<()>;
}

pub struct RestoreSource {
    pub snapshot_dir: PathBuf,   // 模板 snapshot 目录（memory-ranges/ + state.json）
    pub work_dir: PathBuf,       // 本次 restore 的工作目录（config.json 写入此处）
    pub path_overrides: HashMap<String, String>,  // socket 路径 patch
}
```

### 5.2 Cloud Hypervisor 实现

**Snapshot**（含一致性保障）：

```rust
impl Snapshottable for CloudHypervisorVM {
    async fn snapshot(&self, dest_dir: &Path) -> Result<SnapshotMeta> {
        tokio::fs::create_dir_all(dest_dir).await?;

        // 1. Guest sync：确保 ext4 journal 全部提交到块设备
        //    通过 TTRPC 在 guest 内执行 sync，避免未提交写入被 snapshot 截断
        self.exec_in_guest(&["sync"]).await?;

        // 2. Pause VM：冻结 CPU + 所有设备队列，确保 snapshot 原子一致
        //    pause 后 VM 不再接受新 I/O，journal 不会再变化
        self.api_call("PUT", "vm.pause", &json!({})).await?;

        // 3. 捕获 snapshot（pmem DAX 区域不包含在内，pmem 本身是持久的）
        let body = json!({ "destination_url": format!("file://{}", dest_dir.display()) });
        let snap_result = self.api_call("PUT", "vm.snapshot", &body).await;

        // 4. 无论 snapshot 成功与否，立即 resume，避免模板 VM 被永久 pause
        self.api_call("PUT", "vm.resume", &json!({})).await?;

        snap_result?;
        Ok(SnapshotMeta { snapshot_dir: dest_dir.to_path_buf(), ..Default::default() })
    }
```

**Restore**：

```rust
    async fn restore(&mut self, src: &RestoreSource) -> Result<()> {
        // 1. 将模板 config.json patch 后写入 work_dir
        patch_snapshot_config(
            &src.snapshot_dir.join("config.json"),
            &src.work_dir.join("config.json"),
            &src.path_overrides,
        ).await?;

        // 2. memory-ranges 和 state.json 软链接到 work_dir（无需复制）
        symlink(&src.snapshot_dir.join("memory-ranges"),
                &src.work_dir.join("memory-ranges")).await?;
        symlink(&src.snapshot_dir.join("state.json"),
                &src.work_dir.join("state.json")).await?;

        // 3. 启动 CH 进程（仅 --api-socket，设备配置从 work_dir/config.json 读取）
        self.launch_for_restore(&src.work_dir).await?;

        // 4. PUT /api/v1/vm.restore，prefault=false（按需 page fault）
        let body = json!({
            "source_url": format!("file://{}", src.work_dir.display()),
            "prefault": false
        });
        self.api_call("PUT", "vm.restore", &body).await?;

        // 5. 等待 hvsock agent 就绪
        self.wait_agent_ready(Duration::from_secs(15)).await?;
        Ok(())
    }
}
```

### 5.3 config.json 路径 patch

Restore 时需将 config.json 中的 sandbox 专属路径替换为新 sandbox 的实际路径。**不使用字符串全文替换**（fragile，路径前缀相似时易误替换）；改为解析为 JSON 后按已知结构字段精确更新：

```rust
/// Restore 时需要 patch 的字段（仅 sandbox 专属路径，pmem 路径不动）
pub struct SnapshotPathOverrides {
    pub task_vsock: String,    // 新 sandbox 的 hvsock 文件路径
    pub console_path: String,  // 新 sandbox 的 console log 路径
}

/// 解析 JSON → 精确字段更新 → 序列化写出，避免字符串替换误伤
async fn patch_snapshot_config(
    src: &Path, dst: &Path, overrides: &SnapshotPathOverrides,
) -> Result<()> {
    let content = tokio::fs::read_to_string(src).await?;
    let mut cfg: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow!("parse config.json: {}", e))?;

    // CH config.json 结构（payload.vsock.socket_path）
    if let Some(v) = cfg.pointer_mut("/payload/vsock/socket_path") {
        *v = json!(overrides.task_vsock);
    } else {
        // vsock 字段缺失说明 config.json 结构变化，及早报错而非静默跳过
        return Err(anyhow!("config.json missing /payload/vsock/socket_path"));
    }

    if let Some(v) = cfg.pointer_mut("/payload/console/file") {
        *v = json!(overrides.console_path);
    }
    // pmem 路径（/payload/pmem[0]/file）：模板共享，不替换

    write_file_atomic(dst, &serde_json::to_string_pretty(&cfg)?).await
}
```

模板创建时记录原始路径（用于生成 overrides），无需在 config.json 中写占位符：

```rust
pub struct TemplateMeta {
    // ...（其余字段不变）
    pub original_task_vsock: String,   // 模板创建时的 hvsock 路径，restore 时用作 override key
    pub original_console_path: String,
}
```

### 5.4 CH Snapshot 目录结构

```
{snapshot_dir}/
├── config.json.tmpl    # 含占位符（模板存储；非 CH 原生文件名）
├── config.json         # CH 原生：设备列表、路径等（restore 时从 tmpl patch 生成）
├── state.json          # CPU / 设备状态
└── memory-ranges/      # RAM 内容（pmem DAX 区域不在此，体积 ≈ VM RAM）
    ├── 0x00000000-0x0009ffff
    ├── 0x00100000-0x{end}
    └── ...
```

---

## 六、Phase 2：Template Pool

### 6.1 数据结构

```rust
// vmm/sandbox/src/template/mod.rs（新文件）

pub struct TemplateMeta {
    pub id: String,
    pub key: TemplateKey,
    pub snapshot_dir: PathBuf,      // 含 memory-ranges/ + state.json + config.json.tmpl
    pub pmem_path: PathBuf,         // ext4 pmem 镜像（只读，多 VM 共享）
    pub kernel_path: PathBuf,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub created_at: SystemTime,
    pub warmup_ms: u64,
}

#[derive(Hash, Eq, PartialEq, Clone)]
pub struct TemplateKey {
    pub image_digest: String,   // OCI image digest（pmem 内容唯一标识）
    pub kernel_version: String,
    pub vcpus: u32,
    pub memory_mb: u32,
}

pub struct TemplatePool {
    available: Mutex<HashMap<TemplateKey, VecDeque<TemplateMeta>>>,
    store_dir: PathBuf,
    max_per_key: usize,          // 每个 key 最多保留模板数，默认 3
}
```

### 6.2 模板创建流程

```
CreateTemplate(image, vcpus, memory_mb, warmup_script?)
    │
    ├─ 1. 构建 ext4 pmem 镜像（若不存在）
    │      mkfs.ext4 + 解包 OCI → rootfs
    │
    ├─ 2. 冷启动 VM（virtio-blk 模式，无 virtiofsd）
    │      factory.create_vm() → vm.start()
    │
    ├─ 3. 等待 hvsock agent 就绪
    │
    ├─ 4. 执行 Warmup（可选）
    │      TTRPC 执行预热命令：加载 /bin、libc、runtime
    │      目标：将常用 page cache 预热到 RAM snapshot 中
    │
    ├─ 5. vm.snapshot(dest_dir)
    │      PUT /api/v1/vm.snapshot → memory-ranges/ + state.json + config.json
    │
    ├─ 6. 将 config.json 中的 sandbox 专属路径替换为占位符
    │      保存为 config.json.tmpl
    │
    ├─ 7. vm.stop()
    │
    └─ 8. 写入 metadata.json，加入 TemplatePool
```

### 6.3 模板命中 Restore 流程

```
CreateSandbox(template_id)
    │
    ├─ 1. TemplatePool.acquire(key) → TemplateMeta
    │
    ├─ 2. factory.create_vm(id, opts)
    │      新 sandbox 专属路径：task.vsock, api.sock
    │      （无 virtiofsd，无 Fs 设备）
    │
    ├─ 3. patch config.json（task.vsock → 新路径）
    │      memory-ranges symlink → 模板 snapshot_dir
    │
    ├─ 4. vm.restore(RestoreSource)
    │      CH 进程启动（仅 --api-socket）
    │      PUT /api/v1/vm.restore
    │      wait_agent_ready（hvsock ping）
    │
    ├─ 5. SetupSandboxRequest（TTRPC）
    │      网络配置（新 MAC / IP / 路由）
    │      + sandbox_files（resolv.conf / hosts / hostname）
    │
    ├─ 6. sandbox.dump()（含 template_id 字段）
    │
    └─ 7. 后台异步 refill（补充模板池水位）
```

容器创建（sandbox 建立后，按需 hot-attach）：

```
CreateContainer(spec)
    │
    ├─ MountHandler → handle_overlay_mount_blk()
    │    创建 ext4 img → hot_attach → Storage(need_guest_handle=true)
    │
    └─ StorageHandler → TTRPC 发送 Storage 到 guest
         guest task agent：/dev/vdX → ext4 mount → /run/kuasar/storage/containers/{id}
```

---

## 七、文件系统与目录布局

```
/var/lib/kuasar/
├── templates/
│   └── {template_id}/
│       ├── metadata.json           # TemplateMeta 序列化
│       ├── rootfs.img              # ext4 pmem 镜像（只读，多 VM 共享）
│       └── snapshot/
│           ├── config.json.tmpl    # 含占位符（##TASK_VSOCK## 等）
│           ├── state.json          # CPU / 设备状态
│           └── memory-ranges/      # RAM snapshot（≈ VM 内存大小）
│
└── sandboxes/
    └── {sandbox_id}/
        ├── sandbox.json            # KuasarSandbox 序列化（含 template_id）
        ├── restore/                # restore 工作目录
        │   ├── config.json         # patch 后的 CH 配置（task.vsock → 本 sandbox 路径）
        │   ├── state.json ──────────→ symlink → ../../templates/{id}/snapshot/state.json
        │   └── memory-ranges/ ──────→ symlink → ../../templates/{id}/snapshot/memory-ranges/
        ├── {storage_id}.img        # 容器层 ext4 镜像（热插拔 virtio-blk）
        ├── task.vsock              # hvsock 文件（每 sandbox 独立）
        └── api.sock                # CH REST API socket（每 sandbox 独立）
        （无 virtiofs.sock，无 shared/ 目录）
```

**资源共享分析：**

| 资源 | 是否跨 sandbox 共享 | 说明 |
|------|-------------------|------|
| `rootfs.img` (pmem) | 是 | DAX 只读，N 个 VM 共享同一文件 |
| `memory-ranges/` | 是（只读 symlink）| restore 加载为各 VM 独立 RAM |
| `state.json` | 是（只读 symlink）| 设备/CPU 状态，restore 时读取 |
| RAM（restore 后）| 否 | 各 VM 独立，snapshot 只是初始内容 |
| `{storage_id}.img` | 否 | 每个容器实例独立的 ext4 镜像 |

---

## 八、实现计划

### Phase 0 — virtio-blk 容器层能力（前置，3 周）

| 周次 | 任务 | 关键文件 |
|------|------|---------|
| W1 | `create_ext4_image()` / `copy_dir_to_ext4()` 工具函数；`handle_overlay_mount_blk()` | `storage/mod.rs` |
| W1 | `attach_bind_mount()` 分类逻辑（单文件→TTRPC注入，小目录→tmpfs，大目录→blk）；`attach_storage()` 分发 | `storage/mod.rs` |
| W2 | Guest task agent 新增 `virtio-blk` 分支；`late_init_call` 适配；`inject_small_dir()` 实现 | `vmm/task/src/main.rs` |
| W2 | `SetupSandboxRequest` 扩展（sandbox_files 字段）；host 侧推送 resolv.conf 等 | `sandbox.proto` / `sandbox.rs` |
| W3 | `factory.rs` 条件移除 Fs 设备；`config.rs` 修改默认 kernel params | `factory.rs` / `config.rs` |
| W3 | 端到端验证：容器创建 → virtio-blk storage → guest 挂载正常；ConfigMap 文件注入验证 | e2e test |

**验收标准：**
- 不启动 virtiofsd，容器能正常运行（rootfs 可访问，网络配置正确）
- `make test-e2e-runc` 通过

### Phase 1 — Cloud Hypervisor Snapshot/Restore（4 周）

| 周次 | 任务 | 产出 |
|------|------|------|
| W4 | `Snapshottable` trait；`ChClient.vm_snapshot()` / `vm_pause()` / `vm_resume()` / `vm_restore()` | trait + API 封装 |
| W5 | `patch_snapshot_config()`（JSON 结构化 patch，`SnapshotPathOverrides`）；restore work_dir 准备 | 路径 patch 逻辑 |
| W6 | `CloudHypervisorVM::snapshot()`（含 pause/sync/resume 一致性保障）/ `restore()` 完整实现 | 端到端 snapshot → restore |
| W7 | 单 sandbox 端到端测试；restore 耗时基准；错误处理与冷启动回退 | 性能数据 |

**验收标准：**
- `cargo test -- snapshot_restore_roundtrip` 通过
- CH restore P99 < 800ms（256MB RAM）
- restore 失败自动回退冷启动

### Phase 2 — Template Pool + CreateTemplate API（3 周）

| 周次 | 任务 | 产出 |
|------|------|------|
| W8 | `TemplateMeta` / `TemplatePool`；`acquire()` / `add()` / `evict()` | 模板池核心 |
| W9 | `CreateTemplate` gRPC handler；Warmup script 支持；config.json 占位符化 | CreateTemplate API |
| W10 | `create_from_template()` 集成；后台 refill；metrics（命中率、restore 耗时）| 完整 restore 路径 |

**验收标准：**
- 20 次连续相同 image 请求，命中率 > 90%，restore P50 < 500ms

### Phase 3 — 稳定性（2 周）

Prometheus metrics；自动水位补充；容器层 ext4 镜像缓存（按 image digest 复用）。

### Phase 4 — QEMU Snapshot（独立排期）

复用 `Snapshottable` trait，通过 QMP `migrate` / `migrate-incoming` 实现。

### Phase 5 — EROFS rootfs（依赖 kernel 5.15+）

替换 pmem 中的 ext4 为 EROFS，通过 `RootfsProvider` trait 插拔。

---

## 九、性能优化

### 9.1 容器层 ext4 镜像准备延迟

overlay → ext4 是阻塞操作，优化方向：

| 策略 | 说明 |
|------|------|
| 按 image digest 缓存 | 相同 OCI image 只构建一次 ext4 镜像，后续 COW 复制（`cp --reflink`）|
| 并行准备 | 多个 storage 的 ext4 构建并行执行 |
| 后台预构建 | 在 `image pull` 完成后异步预构建 ext4（future work）|
| sparse + 稀疏文件 | 用 `truncate` + 稀疏文件，未写入区域不占磁盘空间 |

### 9.2 Snapshot 大小控制

snapshot 前在 guest 内执行：

```bash
sync
echo 1 > /proc/sys/vm/drop_caches   # 仅清 page cache，保留 anon（runtime 预热状态）
```

典型内存分布（512MB VM，virtiofs 已移除）：

| 区域 | 大小 | 进入 snapshot |
|------|------|--------------|
| pmem rootfs（DAX） | 整个 rootfs.img | **否** |
| Guest kernel / 内核数据 | ~30MB | 是 |
| Guest task agent | ~10MB | 是 |
| Warmup 后 anon 内存 | ~50MB | 是（这是预热价值） |
| 零页/空闲（按需 fault）| ~412MB | 是，但 `prefault=false` 时 restore 不加载 |

### 9.3 Restore 并发控制

参考 `recover_concurrency` PR（#242）的 semaphore 模式：

```rust
static RESTORE_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(4));
```

---

## 十、错误处理与回退

| 场景 | 处理策略 |
|------|---------|
| `acquire()` 返回 None（池空） | 回退冷启动，异步 refill |
| `create_ext4_image()` 失败（磁盘满）| 报错，清理临时文件，容器创建失败 |
| `copy_dir_to_ext4()` 失败 | 清理 img + mount，报错 |
| `vm.restore()` 失败（CH API 报错）| kill CH 进程，清理 work_dir，回退冷启动 |
| `wait_agent_ready()` 超时 | kill CH 进程，回退冷启动，记录告警 |
| pmem 文件缺失 | restore 必然失败，标记模板 invalid，告警 |

---

## 十一、演进路线总结

```
Phase 0（W1-W3，前置）
  virtio-blk 容器层能力
  移除 virtiofs 依赖，guest 支持 virtio-blk sharefs_type
  sandbox 配置文件通过 TTRPC 推送
       │
       ▼
Phase 1（W4-W7）
  Cloud Hypervisor snapshot / restore 基础实现
  模板 VM：pmem + vsock + console（无 virtiofs）
  config.json 路径 patch + memory-ranges symlink
       │
       ▼
Phase 2（W8-W10）
  Template Pool + CreateTemplate API
  restore 路径接入 KuasarSandboxer
  容器层热插拔 virtio-blk
       │
       ▼
Phase 3（W11-W12）
  稳定性 + metrics + ext4 镜像缓存
       │
       ▼
Phase 4（独立）
  QEMU snapshot（复用 Snapshottable trait）
       │
       ▼
Phase 5（kernel 5.15+）
  EROFS 替换 pmem ext4
       │
       ▼
Phase 6（长期）
  userfaultfd Lazy Loading
  memory-ranges 对象存储后端
```

## 十二、关键原则

- **Phase 0 是硬前提**：没有 virtio-blk 容器层能力，CH snapshot 中的 virtiofsd 依赖无法消除
- **pmem 共享无需 COW**：rootfs.img 只读，多 VM 直接共享，无需 qcow2 overlay
- **配置文件 TTRPC 化**：resolv.conf 等通过 TTRPC 推送，不依赖共享目录
- **向后兼容**：`sharefs_type=virtiofs` 分支保留，现有部署不受影响
- **回退优先**：restore 任何一步失败均退回冷启动，不丢请求
- **先稳定再优化**：ext4 镜像缓存、EROFS、Lazy Loading 均为后续阶段
