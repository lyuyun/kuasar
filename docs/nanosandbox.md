# 面向 Serverless 场景的极速安全容器方案设计

<!-- toc -->
- [摘要](#摘要)
- [背景与动机](#背景与动机)
  - [目标](#目标)
  - [非目标](#非目标)
- [方案总览](#方案总览)
- [核心设计](#核心设计)
  - [一、极速冷启动](#一极速冷启动)
    - [传统冷启动的瓶颈分析](#传统冷启动的瓶颈分析)
    - [全流程优化路径](#全流程优化路径)
    - [启动时序对比](#启动时序对比)
  - [二、基于快照的瞬时启动](#二基于快照的瞬时启动)
    - [快照模型](#快照模型)
    - [快照捕获机制](#快照捕获机制)
    - [快照存储格式](#快照存储格式)
    - [快照恢复机制](#快照恢复机制)
    - [冷启动与快照启动性能对比](#冷启动与快照启动性能对比)
  - [三、极简 Sandbox 架构](#三极简-sandbox-架构)
    - [架构原则](#架构原则)
    - [与传统架构的对比](#与传统架构的对比)
    - [1:1 VM-per-Container 模型详解](#11-vm-per-container-模型详解)
    - [安全性分析](#安全性分析)
    - [资源效率分析](#资源效率分析)
    - [稳定性与故障隔离分析](#稳定性与故障隔离分析)
- [API 设计](#api-设计)
  - [设计原则](#设计原则)
  - [Sandbox API](#sandbox-api)
  - [Task API](#task-api)
  - [Snapshot API](#snapshot-api)
  - [核心工作流](#核心工作流)
    - [工作流一：从零冷启动容器](#工作流一从零冷启动容器)
    - [工作流二：从快照热启动容器](#工作流二从快照热启动容器)
    - [工作流三：为运行中容器创建快照](#工作流三为运行中容器创建快照)
- [部署模式](#部署模式)
  - [模式一：CRI 兼容模式](#模式一cri-兼容模式)
    - [CRI 调用映射](#cri-调用映射)
    - [VM 两阶段启动机制](#vm-两阶段启动机制)
    - [工作流：CRI 兼容模式冷启动](#工作流cri-兼容模式冷启动)
    - [Task API 最小支持范围（CRI 兼容模式）](#task-api-最小支持范围cri-兼容模式)
  - [模式二：原生 NanoSandbox 模式](#模式二原生-nanosandbox-模式)
    - [设计原则](#设计原则-1)
    - [工作流：原生模式冷启动](#工作流原生模式冷启动)
    - [工作流：原生模式快照启动](#工作流原生模式快照启动)
    - [Task API 最小支持范围（原生模式）](#task-api-最小支持范围原生模式)
  - [两种模式对比](#两种模式对比)
- [设计细节](#设计细节)
  - [MicroVM 内核裁剪规范](#microvm-内核裁剪规范)
  - [根文件系统设计](#根文件系统设计)
  - [设备模型最小化](#设备模型最小化)
  - [内存管理优化](#内存管理优化)
  - [快照分级预热策略](#快照分级预热策略)
- [风险与缓解措施](#风险与缓解措施)
- [方案局限性](#方案局限性)
- [替代方案讨论](#替代方案讨论)
<!-- /toc -->

---

## 摘要

本文档设计一个面向 Serverless 场景（Function、AI Agent 等）的安全容器方案，代号 **NanoSandbox**。该方案以"极致启动速度"和"极简架构"为核心目标，围绕三个支柱构建：

1. **极速冷启动**：通过微型内核、预构建 erofs rootfs（经 virtio-blk direct kernel boot 挂载）、懒加载镜像、设备模型最小化等手段，将 MicroVM 冷启动时间压缩到 100ms 以内。
2. **快照瞬时启动**：捕获完整的 VM 运行时状态（内存、CPU、设备、文件系统），实现毫秒级"零冷启动"恢复，目标 < 50ms。
3. **极简 Sandbox 架构**：采用 1:1 VM-per-Container 模型，彻底去除 containerd-shim、runc 等中间管理进程，由外部管理器直接与 Hypervisor 交互，最小化攻击面与运行时开销。

API 设计与 containerd Sandbox API / Task API 保持接口风格一致，并新增 Snapshot API 支撑快照全生命周期管理。方案不绑定具体 Hypervisor 实现，不绑定镜像缓存加速技术。

---

## 背景与动机

### 传统容器方案的不足

Docker/containerd 的经典架构如下：

```
Kubernetes (kubelet)
    └── CRI (gRPC)
        └── containerd
            └── containerd-shim-runc-v2   ← 每个 Pod 一个 shim 进程
                └── runc                  ← OCI 容器运行时
                    └── container process
                        └── (pause 容器 + 业务容器共享 netns)
```

在 Serverless 场景下，这一架构面临根本性挑战：

| 痛点 | 描述 |
|------|------|
| **冷启动慢** | 镜像拉取 + 层解压 + runc create + 进程 fork 链路长，典型耗时 500ms~2s |
| **中间层开销大** | 每个 Pod 常驻一个 shim 进程（~10MB 内存），万 Pod 节点累计开销不可忽视 |
| **安全隔离弱** | runc 容器共享宿主机内核，容器逃逸攻击面广 |
| **故障域大** | shim 进程异常可能影响同节点多个容器 |
| **1:1 shim 模型** | shim 与 Pod 强绑定，无法复用，管理复杂度随 Pod 数线性增长 |

Kata Containers 等安全容器方案通过 MicroVM 解决了隔离问题，但仍保留了 containerd-shim 层，且冷启动速度未能满足 Serverless 对毫秒级响应的诉求。

### 目标

- 设计一套 MicroVM 冷启动耗时 < **100ms** 的极速启动方案（不含镜像拉取）
- 设计基于 VM 快照的热启动路径，启动耗时 < **50ms**
- 设计一套不含 shim/runc 等中间守护进程的极简 Sandbox 架构
- 定义与 containerd API 风格一致的 Sandbox、Task、Snapshot API
- 方案对 Hypervisor 实现保持中立（可适配 Cloud Hypervisor、Firecracker、QEMU、StratoVirt 等）
- 方案对镜像加速技术保持中立（可适配 Nydus、eStargz、DADI 等）

### 非目标

- 不设计具体的 Hypervisor 实现
- 不设计镜像分发和缓存加速的具体实现
- 不涉及网络插件（CNI）的具体实现
- 不覆盖 Pod 级别的资源调度（由 Kubernetes Scheduler 负责）
- 不设计多租户场景下的资源配额管理

---

## 方案总览

NanoSandbox 的整体架构如下图所示：

```
┌─────────────────────────────────────────────────────────┐
│                   Kubernetes (kubelet)                   │
│                        CRI gRPC                          │
└───────────────────────────┬─────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────┐
│              NanoSandbox Manager (sandboxer)             │
│  ┌─────────────────┐  ┌──────────────┐  ┌────────────┐  │
│  │   Sandbox API   │  │   Task API   │  │Snapshot API│  │
│  └────────┬────────┘  └──────┬───────┘  └─────┬──────┘  │
│           │                  │                │          │
│  ┌────────▼──────────────────▼────────────────▼──────┐  │
│  │              VM Lifecycle Controller               │  │
│  │  (直接与 Hypervisor 交互，无 shim/runc 中间层)      │  │
│  └──────────────────────────┬─────────────────────────┘  │
└─────────────────────────────┼───────────────────────────┘
                              │ Hypervisor API
                              │ (平台无关接口)
        ┌─────────────────────┼──────────────────────┐
        │                     │                      │
┌───────▼──────┐    ┌─────────▼──────┐    ┌─────────▼──────┐
│   MicroVM 1  │    │   MicroVM 2    │    │   MicroVM N    │
│  ┌─────────┐ │    │  ┌──────────┐  │    │  ┌──────────┐  │
│  │Container│ │    │  │Container │  │    │  │Container │  │
│  │Process  │ │    │  │Process   │  │    │  │Process   │  │
│  │(PID 1)  │ │    │  │(PID 1)   │  │    │  │(PID 1)   │  │
│  └─────────┘ │    │  └──────────┘  │    │  └──────────┘  │
│  Micro Kernel│    │  Micro Kernel  │    │  Micro Kernel  │
└──────────────┘    └────────────────┘    └────────────────┘
   Cold Start          Snapshot Restore      Running
```

**核心思想**：

- **去中间层**：NanoSandbox Manager 直接与 Hypervisor 交互，管理 VM 生命周期，彻底消除 containerd-shim 和 runc。
- **VM 即 Container**：每个 MicroVM 承载单个容器，VM 的内核启动 → 容器进程即为全部启动流程，无额外 init 系统。
- **双路启动**：支持冷启动（from scratch）和快照恢复（from snapshot）两种路径，按场景选择。

---

## 核心设计

### 一、极速冷启动

#### 传统冷启动的瓶颈分析

以 Kata Containers + QEMU 的冷启动链路为例，典型耗时约 800ms~1.5s：

```
[镜像拉取]        ─── 不计（网络依赖）
[containerd处理]  ─── ~20ms  （层解压、overlayfs 挂载）
[shim 启动]       ─── ~30ms  （fork shim 进程）
[QEMU 启动]       ─── ~300ms （QEMU 进程初始化，设备枚举）
[内核引导]        ─── ~200ms （内核解压、驱动初始化）
[guest init]      ─── ~150ms （systemd/init 启动，kata-agent 就绪）
[runc create]     ─── ~50ms  （在 guest 内 runc 创建容器）
[容器进程启动]    ─── ~50ms
─────────────────────────────
总计              ≈  800ms+
```

瓶颈主要集中在三处：① QEMU 进程本身的初始化开销；② 内核引导与驱动初始化；③ guest init 系统的层层依赖拉起。

#### 全流程优化路径

**1. 镜像准备阶段（异步，不在关键路径）**

| 优化点 | 说明 |
|--------|------|
| 懒加载镜像 | 对接懒加载格式（如 Nydus、eStargz），容器可在镜像数据到齐前启动，按需拉取块数据 |
| 本地块缓存 | 常用镜像层在节点 SSD 预缓存，命中时直接 `mmap` 映射为 virtio-blk 后端，无需解压 |
| rootfs 预构建 | 针对高频函数，预先将镜像展开为 ext4/erofs 只读镜像文件，直接挂载，跳过 overlayfs 层合并 |

**2. Hypervisor 进程启动阶段**

传统 VM 每次冷启动都需要完整初始化 Hypervisor 进程，开销巨大。NanoSandbox 引入以下机制：

- **VM 模板（VM Template / Standby Pool）**：预先启动一批处于"刚完成内核引导、等待任务"状态的 MicroVM 实例（即 standby pool），当新请求到来时，Manager 从池中取出一个实例并独占分配给该请求，通过 vsock 注入容器进程规格后触发 exec。取出后池异步补充一个新的预热实例。此机制可将 Hypervisor+内核引导耗时完全从关键路径中移除。"clone"语义等同于"取出并独占"，每个池实例是完全独立的 VM，而非进程级 fork。

- **Hypervisor 快照克隆**：通过将预热 VM 序列化为 Kernel-Only Snapshot 后批量恢复，快速生成多个同状态的新 VM 实例，避免重新加载 VMM 二进制和完整内核引导。各 Hypervisor 的支持情况：

  | Hypervisor | 克隆机制 |
  |------------|---------|
  | Firecracker | `PUT /snapshot/load`（从快照文件恢复新实例）|
  | Cloud Hypervisor | `--restore` 参数从快照目录恢复 |
  | QEMU | `loadvm` QMP 命令从快照恢复 |
  | StratoVirt | `restore` 接口从快照文件恢复 |

  注意：Firecracker 的 `jailer` 是安全沙箱工具（通过 `chroot`/`seccomp`/`cgroup` 对 Firecracker 进程进行隔离），不提供进程克隆或 fork 能力，不应与克隆机制混淆。

**3. 内核裁剪与快速引导**

这是冷启动优化的核心环节，目标是将内核引导时间压缩到 **20ms 以内**。

```
标准 Linux 内核引导序列（已裁剪版本）：

[decompress] kernel image → ~5ms
[setup]      memory map, cpu topology → ~2ms
[init]       必要子系统（virtio总线、vsock、blk）→ ~8ms
[mount]      rootfs（erofs/ext4 over virtio-blk）→ ~3ms
[exec]       /init → container entrypoint → ~2ms
─────────────────────────────────────────────────
总计  ≈ 20ms
```

关键裁剪措施见 [MicroVM 内核裁剪规范](#microvm-内核裁剪规范)。

**4. 根文件系统（rootfs）优化**

- 使用 **erofs**（Enhanced Read-Only File System）格式：元数据紧凑，随机读性能优于 ext4，且支持内核态解压，挂载速度比 overlayfs 快 3~5 倍。
- rootfs 仅包含：动态链接器（`ld-linux`）、libc、容器进程依赖的最小库集合。无 shell、无包管理器、无调试工具。
- rootfs 预构建为单一只读 **erofs** 镜像文件（压缩后 < 8MB），在宿主机侧以 `mmap` 方式映射为 virtio-blk 设备后端。Hypervisor 采用 **direct kernel boot**（直接指定内核镜像路径与命令行参数，无需独立 bootloader），内核通过 `root=/dev/vda` 参数在引导完成后挂载该 virtio-blk 设备为 rootfs。注意：`initrd` 是由 Hypervisor 通过 `initrd=` 参数注入内核的 ramdisk，与 virtio-blk 是两种互斥的 rootfs 加载方式，本方案选用 virtio-blk 路径。

**5. 设备模型最小化**

每个 MicroVM 仅暴露以下设备，且全部基于 virtio 总线（无遗留设备模拟开销）：

| 设备 | 用途 | 备注 |
|------|------|------|
| virtio-blk | 挂载容器 rootfs | 只读，直接 `mmap` 镜像文件 |
| virtio-vsock | 宿主机与容器的控制通道 | 替代传统 TCP/Unix socket |
| virtio-net（可选）| 容器网络 | 如使用 macvtap 或 TC redirect 可免去此设备 |
| virtio-rng | 熵源 | 容器内 `/dev/random` 初始化 |

不包含：串口模拟（UART）、USB 控制器、ACPI 热插拔、PCI 总线枚举（使用 MMIO 总线代替）。

**6. 容器进程直启动（Zero-Init）**

传统安全容器（Kata）在 VM 内仍运行 kata-agent，再由其 fork 出容器进程。NanoSandbox 的做法：

- VM 的 `/init`（rootfs 根目录下的入口二进制，由内核命令行 `init=/init` 指定）**直接 exec 为容器的 entrypoint 进程**，该进程成为 VM 内的 PID 1。
- 容器的环境变量、工作目录、用户等参数通过 vsock 控制通道在 `exec` 前注入。
- 无 kata-agent，无 runc，无 cgroup v2 设置（cgroup 在宿主机由 NanoSandbox Manager 在 VM 外设置）。

#### 启动时序对比

```
【传统 Kata+QEMU 冷启动】
t=0ms    containerd 收到 RunPodSandbox
t=20ms   shim 进程启动
t=320ms  QEMU 进程初始化完成
t=520ms  内核引导完成
t=670ms  kata-agent 就绪
t=720ms  runc create 完成
t=770ms  容器进程启动
──────────────────────
总计: ~770ms

【NanoSandbox 冷启动（无 standby pool）】
t=0ms    Manager 收到 CreateSandbox
t=10ms   Hypervisor 进程就绪（预加载二进制）
t=30ms   内核引导完成（裁剪内核 + erofs rootfs）
t=35ms   容器 entrypoint 直接 exec（Zero-Init）
──────────────────────
总计: ~35ms

【NanoSandbox 冷启动（有 standby pool）】
t=0ms    Manager 收到 CreateSandbox
t=2ms    从 standby pool 取出预热 VM
t=5ms    注入容器配置（env/cmd/rootfs）
t=8ms    容器 entrypoint exec 完成
──────────────────────
总计: ~8ms
```

---

### 二、基于快照的瞬时启动

快照启动（Snapshot Restore）是 Serverless 场景中解决冷启动问题的终极手段。其核心思想是：预先运行一个容器直到某个"热身完成"的状态，将该状态完整冻结保存，后续每次新实例启动时直接从该状态恢复，而非从零开始引导。

#### 快照模型

NanoSandbox 的 VM 快照包含以下四个维度的状态：

```
┌────────────────────────────────────────────────────┐
│                   VM Snapshot                       │
│                                                      │
│  ┌──────────────┐  ┌──────────────────────────────┐ │
│  │  CPU State   │  │       Memory State            │ │
│  │              │  │                               │ │
│  │ • 寄存器状态 │  │ • 完整物理内存页（可 diff）   │ │
│  │ • MSR 值     │  │ • 内存脏页位图（增量快照用）  │ │
│  │ • VMCS/VMCB  │  │ • 已初始化内核数据结构        │ │
│  └──────────────┘  └──────────────────────────────┘ │
│                                                      │
│  ┌──────────────┐  ┌──────────────────────────────┐ │
│  │ Device State │  │    Filesystem State           │ │
│  │              │  │                               │ │
│  │ • virtio 队列│  │ • rootfs 为只读，无需快照      │ │
│  │ • vsock 状态 │  │ • 可写层（overlayfs upper）   │ │
│  │ • 中断控制器 │  │   的增量 diff 或 COW 快照     │ │
│  └──────────────┘  └──────────────────────────────┘ │
└────────────────────────────────────────────────────┘
```

#### 快照捕获机制

**捕获时机**有两种典型选择：

1. **Base Snapshot**（推荐）：在容器进程完成初始化（如 Python 解释器加载完毕、模型权重加载完毕）但尚未处理第一个请求时捕获。恢复后的实例处于"温热就绪"状态，可立即处理请求。

2. **Kernel-Only Snapshot**：仅在内核引导完成、`/init` exec 前捕获，类似 standby pool 的持久化形式。恢复速度快，但容器进程仍需重新启动，适合启动本身极快的应用。

**捕获流程**：

```
1. 调用 CreateSnapshot(sandbox_id, snapshot_id)
2. （可选但推荐）通过 vsock 控制通道向容器进程发送 SYNC 信号，
   等待容器进程完成当前写操作并 fsync，确保文件系统处于一致状态。
   对于无状态函数（纯计算型），可跳过此步骤。
3. Manager 通过 Hypervisor API 暂停 VM（freeze vCPU）
4. 保存 CPU 状态到 snapshot.cpu
5. 扫描内存脏页位图，dump 内存到 snapshot.mem
   （对于 Base Snapshot，需 dump 完整内存；增量时仅 dump 自上次以来的脏页）
6. 保存设备状态到 snapshot.dev
7. 对文件系统可写层执行 COW 快照，记录 snapshot.fs_diff
   - 若使用 overlay upper 目录：在宿主机侧对 upper 目录执行文件级 diff（记录相对父快照新增/修改的文件列表）
   - 若使用 qcow2 可写镜像：直接做 qcow2 内部 COW 快照，恢复时可按块级增量叠加
8. 恢复 VM 运行（unfreeze vCPU），整个捕获过程对容器进程透明
9. 将快照元数据写入 Snapshot Store
```

捕获过程中 VM 的暂停时间（step 2~7）通常 < **5ms**（对于 256MB 内存的 MicroVM），对在线业务影响极小。

#### 快照存储格式

```
snapshot/
  <snapshot_id>/
    metadata.json      # 快照元数据（时间戳、父快照ID、大小、VM配置摘要）
    cpu.bin            # CPU 状态（寄存器、MSR、VMCS，通常 < 4KB）
    mem.raw            # 内存镜像（原始格式，支持稀疏文件，mmap 友好）
    mem.bitmap         # 内存页存在位图（用于增量恢复）
    devices.json       # 设备状态序列化（virtio 队列、中断控制器状态）
    fs.diff            # 文件系统可写层增量（可选，基于 overlay diff 或 qcow2）
```

关键设计决策：

- `mem.raw` 使用**稀疏文件**存储，未修改的零页不占磁盘空间。
- 支持**父子快照链**（类似 git commit），子快照只存储相对父快照的增量脏页，大幅减少存储开销。快照链最大深度建议不超过 **8 层**：深度为 N 的增量快照恢复时需串行叠加 N 个增量内存段，每增加一层约增加 2~5ms 恢复延迟；超过 8 层时建议执行**快照合并（squash）**，将快照链压缩为单一全量快照。Manager 在 `PruneSnapshots` 时可按配置自动触发 squash。
- `mem.raw` 设计为 **`mmap` 友好**格式，恢复时直接将文件映射为 VM 的物理内存后端，无需将内存数据 `read()` 到用户态再传给 Hypervisor，节省一次内存拷贝。

#### 快照恢复机制

```
1. 调用 StartFromSnapshot(snapshot_id, sandbox_id)
2. Manager 创建新 VM 实例（不引导内核，直接进入恢复模式）
3. 将 mem.raw mmap 为 VM 物理内存后端
   （使用 userfaultfd 机制：按需缺页加载，恢复立即返回，内存页在首次访问时按需从存储中填入）
4. 加载 cpu.bin，恢复 CPU 寄存器和 VMCS 状态
5. 加载 devices.json，重建 virtio 设备队列和中断状态
6. 恢复 vsock 连接（Manager 重新建立控制通道）
7. 恢复文件系统可写层（挂载 overlay + fs.diff）
8. VMRESUME：VM 从快照捕获点继续执行
9. 向 Manager 上报 SANDBOX_READY
```

**`userfaultfd` 按需加载**是恢复速度的关键优化：VM 的 `VMRESUME` 不需要等待所有内存数据从存储加载完毕，而是立即恢复执行。CPU 访问的内存页若尚未在内存中，触发缺页异常，由后台 I/O 线程按需填入。对于绝大多数 Serverless 函数，工作集（working set）较小，实际访问的内存远少于 VM 分配的总内存，因此恢复后到"可处理请求"的时间极短。

**`userfaultfd` 权限与内核版本要求**：

| 内核版本 | 约束 |
|---------|------|
| < 5.11 | 需要 `CAP_SYS_PTRACE` 或以 root 用户运行 |
| ≥ 5.11 | unprivileged 用户可用，但受 `/proc/sys/vm/unprivileged_userfaultfd` 控制（默认可能为 0）|
| 任意版本 | 与 2MB/1GB 大页（HugePage）结合使用时有额外限制，需内核 ≥ 5.12 |

由于文档明确 Hypervisor 进程应以**非 root 用户**运行，部署时须确保：① 内核版本 ≥ 5.11；② `sysctl vm.unprivileged_userfaultfd=1`；或通过 `CAP_SYS_PTRACE` capability 授权给 Hypervisor 进程。不满足上述条件时，自动降级为同步全量内存加载（恢复延迟增加至 50~200ms，视内存大小而定）。

#### 冷启动与快照启动性能对比

| 指标 | 冷启动（无 standby pool）| 冷启动（有 standby pool）| 快照启动 |
|------|--------------------------|--------------------------|----------|
| VM 启动延迟 | ~35ms | ~8ms | < **5ms** |
| 内核引导 | 需要（~20ms） | 预热完成（0ms）| 无需（直接 VMRESUME）|
| 容器进程初始化 | 需要（应用决定）| 需要（应用决定）| **已完成**（快照捕获后状态）|
| 内存使用 | 按需分配 | 预占（standby pool）| 按需（userfaultfd）|
| 首次请求延迟 | 应用启动时间 + 35ms | 应用启动时间 + 8ms | < **50ms**（含应用 warm）|
| 存储需求 | 无（除镜像外）| 无 | 每快照 ~VM内存大小（稀疏）|

> **数据说明**：上表数值为目标值，基于以下参考环境估算：Firecracker 1.x、Linux 5.15 裁剪内核（< 4MB）、erofs rootfs、256MB MicroVM 内存、NVMe SSD 存储。具体数值随 CPU 型号、内存规格、存储类型、Hypervisor 版本及内核裁剪程度不同而变化，正式发布前需提供实测基准数据。

---

### 三、极简 Sandbox 架构

#### 架构原则

NanoSandbox 的架构建立在以下三条原则上：

1. **没有不必要的进程**：宿主机上不存在任何用于管理单个容器生命周期的常驻守护进程（无 shim，无 agent）。
2. **没有不必要的层**：容器进程直接运行在 MicroVM 内核之上，中间没有额外的运行时（无 runc、无 containerd-task）。
3. **没有不必要的权限**：每个 MicroVM 以最小权限集运行，宿主机内核和其他容器的攻击面被 VM 边界完全隔断。

#### 与传统架构的对比

**传统 Kata Containers 架构**（每 Pod 链路）：

```
kubelet
  └─ containerd (1 个全局守护进程)
       └─ containerd-shim-kata-v2 (每 Pod 1 个进程, ~10MB)
            └─ QEMU/Hypervisor 进程 (每 Pod 1 个, ~50MB)
                 └─ MicroVM
                      └─ systemd / kata-agent (每 Pod 1 个, ~20MB guest 内存)
                           └─ runc (每容器 1 次)
                                └─ container process
```

**NanoSandbox 架构**（每 Pod 链路）：

```
kubelet (CRI)
  └─ NanoSandbox Manager (1 个全局守护进程)
       └─ Hypervisor 进程 (每 Pod 1 个, 按需启动/销毁)
            └─ MicroVM
                 └─ container process (PID 1, 直接 exec)
```

对比分析：

| 维度 | Kata Containers | NanoSandbox |
|------|-----------------|-------------|
| 宿主机进程数（每 Pod）| shim + Hypervisor = 2 | Hypervisor = 1 |
| 宿主机常驻进程开销 | shim ~10MB × Pod数 | 0（Hypervisor 随 VM 生命周期）|
| Guest 内存开销 | systemd + kata-agent ~30MB | 仅裸内核 + 容器进程 |
| 容器启动中间环节 | kata-agent → runc → 容器 | 内核 → 直接 exec 容器 |
| 攻击面 | shim（root进程）+ runc（suid）| 无（Hypervisor 以受限用户运行）|

#### 1:1 VM-per-Container 模型详解

**生命周期管理**

容器（Pod）的全部生命周期操作由 NanoSandbox Manager 通过 Hypervisor API 直接完成：

```
Create  → Hypervisor.CreateVM(config)      # 创建 VM 实例（不启动）
Start   → Hypervisor.StartVM(vm_id)        # 引导内核，exec 容器进程
Pause   → Hypervisor.PauseVM(vm_id)        # 冻结 vCPU
Resume  → Hypervisor.ResumeVM(vm_id)       # 恢复 vCPU
Stop    → Hypervisor.StopVM(vm_id)         # 关闭 VM（SIGKILL 相当）
Delete  → Hypervisor.DestroyVM(vm_id)      # 释放所有资源
```

**网络模型**

1:1 VM-per-Container 使得网络模型大幅简化：

- Pod 的网络命名空间由 VM 本身承载，无需 `pause` 容器。
- 宿主机侧通过 tap 设备或 macvtap 直接连接 VM 的 virtio-net，由 CNI 插件配置 tap 设备。
- 去除了传统模型中 `pause` 容器持有 netns 的设计，减少一个常驻进程。

**存储模型**

- **只读层**（镜像层）：宿主机侧将 OCI 镜像的只读层合并为单一 erofs/ext4 镜像文件，通过 virtio-blk 以只读方式挂载进 VM。
- **可写层**：在宿主机侧为每个 VM 创建一个稀疏文件作为 virtio-blk 可写后端，VM 内部 rootfs 叠加此可写层。容器删除时，直接删除该文件，零残留。
- **共享目录**（可选）：通过 virtiofs 将宿主机指定目录挂载进 VM，用于 volume 绑定挂载场景。

#### 安全性分析

**攻击面对比**

```
传统 runc 容器的逃逸路径（部分）：
  ① 利用 Linux 内核漏洞（共享内核）          ← NanoSandbox 通过 VM 边界封堵
  ② 利用 runc 本身漏洞（CVE-2019-5736等）    ← NanoSandbox 无 runc
  ③ 利用 containerd-shim 漏洞（root 进程）   ← NanoSandbox 无 shim
  ④ 通过 /proc/sysrq-trigger 等 host 路径   ← NanoSandbox VM 内无宿主机 /proc
```

**NanoSandbox 的安全属性**：

- **内核隔离**：每个容器拥有独立的 Linux 内核实例（Guest Kernel），宿主机内核漏洞无法被容器直接利用。
- **无 suid 二进制**：不存在 runc 这类需要 `suid` 或 `CAP_SYS_ADMIN` 的进程，消除了一类提权攻击向量。
- **最小宿主机权限**：Hypervisor 进程以非 root 用户运行（配合 `seccomp`/`cgroup` 限制），即使 Hypervisor 被攻破，攻击者获得的宿主机权限也极为有限。
- **故障无交叉**：MicroVM 崩溃只影响自身，不影响宿主机上的其他容器或 NanoSandbox Manager。

#### 资源效率分析

**内存开销**（对比 Kata Containers）：

```
Kata Containers（每 Pod）:
  shim 进程:     ~10MB（宿主机）
  QEMU 进程:     ~20MB（宿主机，不含 VM 内存）
  kata-agent:    ~15MB（VM 内）
  Guest kernel:  ~20MB（VM 内）
  ─────────────────────
  运行时开销: ~65MB

NanoSandbox（每 Pod）:
  Hypervisor:    ~5MB（宿主机，仅 VMM 核心，无 shim）
  Micro Kernel:  ~8MB（VM 内，裁剪内核）
  ─────────────────────
  运行时开销: ~13MB
```

**进程数开销**：Kata 每 Pod 2 个宿主机进程（shim + Hypervisor），NanoSandbox 每 Pod 1 个（Hypervisor），万 Pod 节点节省 10,000 个宿主机进程。

#### 稳定性与故障隔离分析

- **故障域隔离**：单个 MicroVM 的崩溃（无论是 Guest Kernel panic 还是容器进程崩溃）完全被 VM 边界限制，不会触发宿主机 OOM、不会影响其他 VM。
- **无 shim 单点**：传统架构中 shim 进程是一个单点——shim 崩溃会导致其管理的所有容器失控。NanoSandbox 消除了 shim，VM 的运行时状态（内存、CPU、设备）驻留在 Hypervisor 进程中，Manager 重启不影响运行中的 VM。

  Manager 需要持久化以下轻量运行时映射表（写入 `/run/nanosandbox/state/<sandbox_id>/meta.json`）：
  - `sandbox_id` → `(vm_pid, vsock_cid, hypervisor_type)` 映射
  - 每个 Sandbox 的 stdio FIFO 路径
  - Standby pool 中各预热 VM 的 PID 与 vsock_cid

  Manager 重启后通过扫描 `/run/nanosandbox/state/` 目录、重连 vsock（重建控制通道）、重建 FIFO 监听来恢复对已运行 VM 的管理。孤儿 VM 进程（meta.json 存在但进程已消失）在扫描时自动清理。
- **资源泄漏防护**：VM 销毁时，Hypervisor 进程退出，其持有的所有内存、文件描述符由 OS 自动回收，无传统 shim 模型中因 shim 异常退出导致的资源泄漏。

---

## API 设计

### 设计原则

1. **与 containerd 风格一致**：接口命名、参数结构与 containerd 的 Sandbox API（`containerd/containerd/api/services/sandbox/v1`）和 Task API（`containerd/containerd/api/runtime/task/v2`）保持一致。
2. **最小化接口集合**：每个 API 只做一件事，不提供过度设计的 "all-in-one" 接口。
3. **同步为主，阻塞直到完成**：Create/Start/Stop 等操作同步返回最终结果（started_at、stopped_at 等时间戳），调用方无需轮询，操作结果即时可用。唯一支持异步模式的操作是快照捕获（`CreateSnapshot` 可配置为后台运行），通过 `GetSnapshotStatus` 轮询完成状态。
4. **幂等设计**：Create/Delete 操作应是幂等的，重复调用不产生副作用。

---

### Sandbox API

Sandbox API 负责 MicroVM（沙箱）本身的生命周期管理。每个 Sandbox 对应一个 MicroVM 实例。

```protobuf
syntax = "proto3";

package nanosandbox.sandbox.v1;

import "google/protobuf/timestamp.proto";
import "google/protobuf/any.proto";

// SandboxService 管理 MicroVM 沙箱的完整生命周期。
service SandboxService {
    // CreateSandbox 创建一个新的 MicroVM 沙箱实例。
    // 对应 containerd SandboxStore.Create。
    // 此调用仅创建 VM 配置并分配资源，不引导内核。
    rpc CreateSandbox(CreateSandboxRequest) returns (CreateSandboxResponse);

    // StartSandbox 启动已创建的沙箱（引导 MicroVM 内核）。
    // 仅用于冷启动路径：触发内核引导 → Zero-Init exec → 容器进程就绪。
    // 快照恢复请使用 SnapshotService.StartFromSnapshot，两者路径严格分离。
    // 对应 containerd Controller.Start。
    rpc StartSandbox(StartSandboxRequest) returns (StartSandboxResponse);

    // StopSandbox 停止沙箱。
    // timeout_secs > 0：向 VM 发送 ACPI 关机信号（等同于按下电源键），
    //   VM 内核收到 ACPI 事件后执行 graceful shutdown，容器进程收到 SIGTERM。
    //   超时后若 VM 仍未退出，自动升级为强制 poweroff。
    // timeout_secs = 0：直接 poweroff（立即终止 VM，等同于强制断电）。
    //
    // 级联语义：在 1:1 VM-per-Container 模型中，KillTask(SIGKILL) 导致容器 PID 1 退出，
    // 进而触发 Guest Kernel panic 或正常关机，VM 自动进入 STOPPED 状态。
    // 因此 WaitTask 与 WaitSandbox 在此模型下语义等价，均等待 VM 退出。
    // 对应 containerd Controller.Stop。
    rpc StopSandbox(StopSandboxRequest) returns (StopSandboxResponse);

    // WaitSandbox 阻塞直到沙箱退出，返回退出状态。
    // 对应 containerd Controller.Wait。
    rpc WaitSandbox(WaitSandboxRequest) returns (WaitSandboxResponse);

    // PauseSandbox 暂停沙箱（冻结所有 vCPU）。
    // 对应 containerd Controller.Pause。
    rpc PauseSandbox(PauseSandboxRequest) returns (PauseSandboxResponse);

    // ResumeSandbox 恢复已暂停的沙箱。
    // 对应 containerd Controller.Resume。
    rpc ResumeSandbox(ResumeSandboxRequest) returns (ResumeSandboxResponse);

    // DeleteSandbox 删除沙箱并释放所有关联资源。
    // 对应 containerd Controller.Shutdown + SandboxStore.Delete。
    rpc DeleteSandbox(DeleteSandboxRequest) returns (DeleteSandboxResponse);

    // GetSandboxStatus 查询沙箱当前状态。
    // 对应 containerd Controller.Status。
    rpc GetSandboxStatus(GetSandboxStatusRequest) returns (GetSandboxStatusResponse);

    // ListSandboxes 列出所有沙箱实例。
    // 对应 containerd SandboxStore.List。
    rpc ListSandboxes(ListSandboxesRequest) returns (ListSandboxesResponse);

    // WatchSandbox 订阅指定沙箱的状态变更事件（Server-Streaming RPC）。
    // 当容器进程崩溃、VM 异常退出、OOM Kill 等事件发生时，立即推送 SandboxEvent，
    // 无需调用方轮询 GetSandboxStatus。对于 kubelet 及时感知 Pod 失败是核心需求。
    // 流在沙箱进入 EXITED 状态后自动关闭。
    rpc WatchSandbox(WatchSandboxRequest) returns (stream SandboxEvent);
}

// SandboxSpec 描述 MicroVM 的静态配置。
message SandboxSpec {
    // sandbox_id: 全局唯一标识，通常与 Kubernetes Pod UID 对应。
    string sandbox_id = 1;
    // 分配给 VM 的 vCPU 数量。
    uint32 vcpu_count = 2;
    // 分配给 VM 的内存大小（MiB）。
    uint64 memory_mib = 3;
    // 容器 rootfs 的块设备配置（来源：镜像预构建的 erofs/ext4 镜像路径）。
    RootfsConfig rootfs = 4;
    // 网络配置（tap 设备名、MAC 地址等，由 CNI 插件预先配置好传入）。
    NetworkConfig network = 5;
    // 容器启动参数（entrypoint、args、env、workdir、uid/gid）。
    // 冷启动必填；快照启动可选（override_envs 通过 StartFromSnapshotRequest 传入）。
    ProcessSpec process = 6;
    // 扩展字段，用于传递 Hypervisor 实现特有的配置（类型 URL 由实现方定义）。
    google.protobuf.Any hypervisor_options = 7;
    // 可选：沙箱标签，用于过滤和查询。
    map<string, string> labels = 8;
}

message RootfsConfig {
    // 宿主机侧只读 rootfs 镜像文件路径（erofs/ext4 格式）。
    string image_path = 1;
    // 宿主机侧可写层后端文件路径（稀疏文件，由 Manager 创建）。
    string writable_path = 2;
    // 文件系统类型（"erofs" | "ext4"）。
    string fs_type = 3;
}

message NetworkConfig {
    // 宿主机侧 tap 设备名（由 CNI 插件创建）。
    string tap_name = 1;
    // Guest 侧 MAC 地址。
    string mac_address = 2;
    // Guest 侧 IP 地址（CIDR 格式，如 "192.168.1.2/24"）。
    string ip_cidr = 3;
    // 默认网关 IP。
    string gateway = 4;
}

message ProcessSpec {
    // 容器 entrypoint 路径（在 rootfs 内的绝对路径）。
    string entrypoint = 1;
    // 启动参数。
    repeated string args = 2;
    // 环境变量（"KEY=VALUE" 格式）。
    repeated string envs = 3;
    // 工作目录。
    string work_dir = 4;
    // 运行用户的 UID。
    uint32 uid = 5;
    // 运行用户的 GID。
    uint32 gid = 6;
}

// SandboxStatus 描述沙箱的运行时状态。
message SandboxStatus {
    string sandbox_id = 1;
    SandboxState state = 2;
    // VM 进程的宿主机 PID（调试用）。
    uint32 pid = 3;
    google.protobuf.Timestamp created_at = 4;
    google.protobuf.Timestamp started_at = 5;
    google.protobuf.Timestamp exited_at = 6;
    // 退出状态码（仅在 state == EXITED 时有效）。
    int32 exit_status = 7;
    // 若从快照启动，记录来源 snapshot_id。
    string restored_from_snapshot = 8;
}

enum SandboxState {
    SANDBOX_UNKNOWN  = 0;
    SANDBOX_CREATING = 1;  // VM 资源分配中
    SANDBOX_CREATED  = 2;  // VM 已创建，未启动
    SANDBOX_RUNNING  = 3;  // VM 正在运行
    SANDBOX_PAUSED   = 4;  // VM 已暂停（vCPU 冻结）
    SANDBOX_STOPPING = 5;  // VM 正在关闭
    SANDBOX_STOPPED  = 6;  // VM 已停止（可重启）
    SANDBOX_EXITED   = 7;  // VM 已退出（不可恢复）
}

message CreateSandboxRequest  { SandboxSpec spec = 1; }
message CreateSandboxResponse { string sandbox_id = 1; }

message StartSandboxRequest   { string sandbox_id = 1; }
message StartSandboxResponse  { google.protobuf.Timestamp started_at = 1; }

message StopSandboxRequest {
    string sandbox_id = 1;
    // 等待超时（秒）；0 表示立即强制关机。
    uint32 timeout_secs = 2;
}
message StopSandboxResponse   { google.protobuf.Timestamp stopped_at = 1; }

message WaitSandboxRequest    { string sandbox_id = 1; }
message WaitSandboxResponse   { int32 exit_status = 1; google.protobuf.Timestamp exited_at = 2; }

message PauseSandboxRequest   { string sandbox_id = 1; }
message PauseSandboxResponse  {}

message ResumeSandboxRequest  { string sandbox_id = 1; }
message ResumeSandboxResponse {}

message DeleteSandboxRequest  { string sandbox_id = 1; }
message DeleteSandboxResponse {}

message GetSandboxStatusRequest  { string sandbox_id = 1; }
message GetSandboxStatusResponse { SandboxStatus status = 1; }

message ListSandboxesRequest  { map<string, string> filters = 1; }
message ListSandboxesResponse { repeated SandboxStatus sandboxes = 1; }

message WatchSandboxRequest   { string sandbox_id = 1; }
message SandboxEvent {
    string sandbox_id  = 1;
    SandboxState state = 2;
    // 事件类型（如 "oom_kill"、"container_exited"、"vm_panic"、"state_changed"）。
    string event_type  = 3;
    int32  exit_status = 4;
    google.protobuf.Timestamp occurred_at = 5;
    // 附加信息（如 OOM 时的内存限制、panic 时的 Guest 日志片段）。
    string message     = 6;
}
```

---

### Task API

Task API 负责沙箱内容器进程（Task）的生命周期管理。在 NanoSandbox 的 1:1 模型中，每个 Sandbox 只有一个 primary task（容器的 entrypoint 进程）。

**Zero-Init 模型下 ExecTask 和 ListPids 的实现路径**：

`/init` exec 后成为容器进程 PID 1，VM 内不再存在常驻 agent。`ExecTask` 和 `ListPids` 需要特殊机制支持：

- **ExecTask 实现方案**：`/init` 在 exec 前通过 `O_CLOEXEC=false` 保留一个 vsock 控制 fd，该 fd 由容器进程（PID 1）继承。Manager 向此 fd 发送 ExecRequest 后，容器进程通过 `fork()` + `execve()` 在 VM 内发起子进程。此方案要求容器进程实现对应的信号处理逻辑，因此 **ExecTask 在生产 Serverless 场景下标记为可选**（应用通常不需要在线 exec），仅在调试模式或 CRI 兼容模式下要求实现。

- **ListPids 实现方案**：Manager 优先从宿主机侧该 VM 对应的 cgroup（`/sys/fs/cgroup/<vm_cgroup>/cgroup.threads`）读取线程组 ID 列表（宿主机视角的 PID）。由于 1:1 模型下主进程固定为 Guest PID 1，宿主机 cgroup 路径足以满足大多数监控场景。若需要 Guest 内部 PID 映射（多进程容器），则通过上述 vsock 控制通道请求 VM 内枚举 `/proc`。

```protobuf
syntax = "proto3";

package nanosandbox.task.v2;

import "google/protobuf/timestamp.proto";
import "google/protobuf/empty.proto";

// TaskService 管理沙箱内运行的容器进程。
// 接口设计对齐 containerd runtime/v2 Task API。
service TaskService {
    // CreateTask 在指定沙箱内创建主容器任务（幂等，Sandbox 创建时自动完成）。
    // 对于 NanoSandbox，此调用通常在 CreateSandbox + StartSandbox 后自动完成，
    // 显式调用用于更新任务配置（如 stdin/stdout 重定向）。
    rpc CreateTask(CreateTaskRequest) returns (CreateTaskResponse);

    // StartTask 发信号给容器主进程开始执行（在 Zero-Init 模式下，
    // VM 启动后容器进程已处于运行状态，此调用更多作为确认语义使用）。
    rpc StartTask(StartTaskRequest) returns (StartTaskResponse);

    // ExecTask 在已运行的沙箱内执行一个额外的进程（如 exec 进入容器调试）。
    // 对应 containerd Task.Exec。
    rpc ExecTask(ExecTaskRequest) returns (ExecTaskResponse);

    // KillTask 向容器主进程或 exec 进程发送信号。
    // 对应 containerd Task.Kill。
    rpc KillTask(KillTaskRequest) returns (KillTaskResponse);

    // WaitTask 阻塞直到指定进程退出。
    // 对应 containerd Task.Wait。
    rpc WaitTask(WaitTaskRequest) returns (WaitTaskResponse);

    // DeleteTask 清理任务资源（进程退出后调用）。
    // 对应 containerd Task.Delete。
    rpc DeleteTask(DeleteTaskRequest) returns (DeleteTaskResponse);

    // ResizePty 调整 TTY 大小。
    rpc ResizePty(ResizePtyRequest) returns (google.protobuf.Empty);

    // GetTaskStatus 查询任务状态。
    rpc GetTaskStatus(GetTaskStatusRequest) returns (GetTaskStatusResponse);

    // ListPids 列出沙箱内所有进程 PID。
    // 实现：优先读取宿主机侧 VM cgroup（cgroup.threads），无需进入 VM；
    // 若需要 Guest PID 映射，通过 vsock 控制通道请求 VM 内枚举 /proc。
    rpc ListPids(ListPidsRequest) returns (ListPidsResponse);

    // Stats 获取任务资源使用统计（CPU、内存、IO）。
    rpc Stats(StatsRequest) returns (StatsResponse);
}

message TaskSpec {
    // 关联的 sandbox_id。
    string sandbox_id = 1;
    // 主任务 ID（通常与 sandbox_id 相同，或 Kubernetes container name）。
    string task_id = 2;
    // 标准输入/输出/错误的宿主机侧 FIFO 路径（可选）。
    string stdin  = 3;
    string stdout = 4;
    string stderr = 5;
    // 是否分配 TTY。
    bool   terminal = 6;
}

message TaskStatus {
    string sandbox_id = 1;
    string task_id    = 2;
    TaskState state   = 3;
    // 容器主进程在 VM 内的 PID（通常为 1）。
    uint32 pid        = 4;
    int32  exit_status = 5;
    google.protobuf.Timestamp started_at = 6;
    google.protobuf.Timestamp exited_at  = 7;
}

enum TaskState {
    TASK_UNKNOWN  = 0;
    TASK_CREATED  = 1;
    TASK_RUNNING  = 2;
    TASK_PAUSED   = 3;
    TASK_STOPPED  = 4;
}

message CreateTaskRequest  { TaskSpec spec = 1; }
message CreateTaskResponse { string task_id = 1; uint32 pid = 2; }

message StartTaskRequest   { string sandbox_id = 1; string task_id = 2; }
message StartTaskResponse  { uint32 pid = 1; }

message ExecTaskRequest {
    string sandbox_id  = 1;
    string exec_id     = 2;
    // exec 的进程规格（cmd、env、workdir 等）。
    ProcessSpec process = 3;
    string stdin  = 4;
    string stdout = 5;
    string stderr = 6;
    bool   terminal = 7;
}
message ExecTaskResponse   { string exec_id = 1; }

message KillTaskRequest {
    string sandbox_id = 1;
    string task_id    = 2;
    // 信号编号（如 15 for SIGTERM, 9 for SIGKILL）。
    uint32 signal     = 3;
    // 若 exec_id 非空，则 kill 指定的 exec 进程而非主进程。
    string exec_id    = 4;
    // 若为 true，向 VM 内所有进程发送信号。
    bool   all        = 5;
}
message KillTaskResponse   {}

message WaitTaskRequest    { string sandbox_id = 1; string task_id = 2; string exec_id = 3; }
message WaitTaskResponse   { int32 exit_status = 1; google.protobuf.Timestamp exited_at = 2; }

message DeleteTaskRequest  { string sandbox_id = 1; string task_id = 2; }
message DeleteTaskResponse { int32 exit_status = 1; google.protobuf.Timestamp exited_at = 2; }

message ResizePtyRequest   { string sandbox_id = 1; string task_id = 2; uint32 width = 3; uint32 height = 4; }

message GetTaskStatusRequest  { string sandbox_id = 1; string task_id = 2; }
message GetTaskStatusResponse { TaskStatus status = 1; }

message ListPidsRequest    { string sandbox_id = 1; }
message ListPidsResponse   { repeated ProcessInfo processes = 1; }
message ProcessInfo        { uint32 pid = 1; string info = 2; }

message StatsRequest       { string sandbox_id = 1; string task_id = 2; }
message StatsResponse {
    // 数据来源说明：
    // - cpu_usage_ns / memory_usage_kb：来自宿主机侧 VM cgroup（VM 级粒度，含 Hypervisor
    //   进程自身开销，可通过减去 Hypervisor 基础内存得到近似容器用量）。
    //   宿主机 cgroup 无法感知 VM 内部进程级 CPU/内存分布。
    // - rx_bytes / tx_bytes：来自宿主机侧 tap 设备计数（/sys/class/net/<tap>/statistics/）。
    // - read_bytes / write_bytes：来自宿主机侧 virtio-blk 设备 I/O 计数。
    // Kubernetes resource limits（requests/limits）通过宿主机 cgroup 对整个 VM 进程组生效，
    // 精度为 VM 级别而非容器进程级别。
    uint64 cpu_usage_ns    = 1;   // 累计 CPU 使用时间（纳秒，VM 级）
    uint64 memory_usage_kb = 2;   // 当前内存使用（KB，VM 级，含 Guest Kernel）
    uint64 rx_bytes        = 3;   // 网络接收字节数（tap 设备计数）
    uint64 tx_bytes        = 4;   // 网络发送字节数（tap 设备计数）
    uint64 read_bytes      = 5;   // 块设备读字节数
    uint64 write_bytes     = 6;   // 块设备写字节数
}

// ProcessSpec 定义在 nanosandbox.sandbox.v1 包中，此处通过 import 引用，避免重复定义。
// 实现时应：import "nanosandbox/sandbox/v1/sandbox.proto";
// 并使用 nanosandbox.sandbox.v1.ProcessSpec，而非在本包内重新声明。
// 下面的内联定义仅用于文档可读性，代码生成时应从 nanosandbox.common.v1 统一导入。
//
// message ProcessSpec {  // → 使用 nanosandbox.sandbox.v1.ProcessSpec
//     string entrypoint     = 1;
//     repeated string args  = 2;
//     repeated string envs  = 3;
//     string work_dir       = 4;
//     uint32 uid            = 5;
//     uint32 gid            = 6;
// }
```

---

### Snapshot API

Snapshot API 是 NanoSandbox 独有的扩展 API，用于管理 VM 快照的完整生命周期。

```protobuf
syntax = "proto3";

package nanosandbox.snapshot.v1;

import "google/protobuf/timestamp.proto";

// SnapshotService 管理 VM 级快照（含内存、CPU、设备、文件系统状态）。
service SnapshotService {
    // CreateSnapshot 为正在运行的沙箱创建快照。
    // 调用期间 VM 会被短暂暂停（< 5ms），对业务透明。
    rpc CreateSnapshot(CreateSnapshotRequest) returns (CreateSnapshotResponse);

    // StartFromSnapshot 从指定快照创建并启动一个新的沙箱实例。
    // 新沙箱继承快照时刻的完整状态，直接恢复执行。
    // 快照启动的唯一入口，不经过 Sandbox API 的 CreateSandbox/StartSandbox 路径。
    rpc StartFromSnapshot(StartFromSnapshotRequest) returns (StartFromSnapshotResponse);

    // GetSnapshotStatus 查询快照状态（是否可用、大小等）。
    rpc GetSnapshotStatus(GetSnapshotStatusRequest) returns (GetSnapshotStatusResponse);

    // ListSnapshots 列出所有快照（支持按标签、sandbox_id 过滤）。
    rpc ListSnapshots(ListSnapshotsRequest) returns (ListSnapshotsResponse);

    // DeleteSnapshot 删除指定快照及其数据。
    // 若存在子快照依赖此快照，返回错误（需先删除子快照）。
    rpc DeleteSnapshot(DeleteSnapshotRequest) returns (DeleteSnapshotResponse);

    // PruneSnapshots 清理过期或孤立的快照。
    rpc PruneSnapshots(PruneSnapshotsRequest) returns (PruneSnapshotsResponse);
}

// SnapshotSpec 描述快照配置。
message SnapshotSpec {
    // 全局唯一的快照 ID（由调用方提供或由系统生成）。
    string snapshot_id = 1;
    // 源沙箱 ID（被快照的 VM）。
    string source_sandbox_id = 2;
    // 快照存储路径（文件系统路径或对象存储 URI）。
    // 若为空，使用系统默认快照存储路径。
    string storage_path = 3;
    // 快照类型。
    SnapshotType type = 4;
    // 父快照 ID（用于增量快照，仅保存相对父快照的增量脏页）。
    // 若为空，创建全量快照。
    string parent_snapshot_id = 5;
    // 快照描述，便于管理。
    string description = 6;
    // 标签，用于过滤和查询。
    map<string, string> labels = 7;
}

enum SnapshotType {
    // FULL：全量快照，包含完整内存镜像。恢复最快，存储占用最大。
    SNAPSHOT_FULL        = 0;
    // INCREMENTAL：增量快照，仅保存自父快照以来的脏内存页。
    // 存储占用小，恢复时需叠加父快照链。
    SNAPSHOT_INCREMENTAL = 1;
}

// SnapshotInfo 描述已存储的快照信息。
message SnapshotInfo {
    string snapshot_id        = 1;
    string source_sandbox_id  = 2;
    SnapshotType type         = 3;
    string parent_snapshot_id = 4;
    SnapshotState state       = 5;
    // 快照占用的磁盘空间（字节）。
    uint64 size_bytes         = 6;
    // 快照涵盖的 VM 内存总量（字节）。
    uint64 memory_bytes       = 7;
    google.protobuf.Timestamp created_at   = 8;
    google.protobuf.Timestamp captured_at  = 9;  // 捕获 VM 状态的时刻
    string storage_path       = 10;
    string description        = 11;
    map<string, string> labels = 12;
}

enum SnapshotState {
    SNAPSHOT_CREATING   = 0;  // 正在捕获
    SNAPSHOT_READY      = 1;  // 可用于恢复
    SNAPSHOT_INVALID    = 2;  // 数据损坏或不完整
    SNAPSHOT_DELETING   = 3;  // 正在删除
}

message CreateSnapshotRequest {
    SnapshotSpec spec = 1;
    // 若为 true，捕获完成后立即停止源 VM（适合用于迁移场景）。
    bool stop_source_after_capture = 2;
}
message CreateSnapshotResponse {
    string snapshot_id = 1;
    google.protobuf.Timestamp captured_at = 2;
    // 快照占用的磁盘空间（字节）。
    uint64 size_bytes = 3;
}

message StartFromSnapshotRequest {
    // 要从中恢复的快照 ID。
    string snapshot_id = 1;
    // 新沙箱的 ID（由调用方指定，需全局唯一）。
    string new_sandbox_id = 2;
    // 新沙箱的网络配置（恢复时必须重新配置，IP/MAC/tap 均需唯一）。
    NetworkConfig network = 3;
    // 可选：覆盖快照中的环境变量（Serverless 场景下每次调用可能传入不同 env）。
    repeated string override_envs = 4;
    // 扩展配置。
    google.protobuf.Any hypervisor_options = 5;

    // 以下字段用于保证多实例恢复时的身份唯一性：

    // 新沙箱的 hostname（若为空，Manager 自动生成唯一值，如 sandbox_id 前缀）。
    // 快照中的 hostname 会被此字段覆盖，防止多实例 hostname 冲突。
    string hostname = 6;

    // vsock Context Identifier（CID）。每个 VM 实例的 CID 必须全局唯一。
    // Manager 负责在节点范围内分配唯一 CID，不得复用快照中的 CID。
    // 若为 0，由 Manager 自动分配。
    uint32 vsock_cid = 7;

    // 若为 true，Manager 在 VMRESUME 后通过 vsock 向 VM 内注入新的随机熵种子，
    // 强制重置 /dev/urandom 的熵池，防止多个实例共享相同熵状态（密钥生成安全漏洞）。
    // 默认 true，仅在对延迟极敏感且明确不使用密码学操作的场景下可设为 false。
    bool reseed_entropy = 8;
}
message StartFromSnapshotResponse {
    string sandbox_id = 1;
    google.protobuf.Timestamp started_at = 2;
    // 从 StartFromSnapshot 调用到 VM 恢复执行的耗时（毫秒）。
    uint64 restore_latency_ms = 3;
}

message GetSnapshotStatusRequest  { string snapshot_id = 1; }
message GetSnapshotStatusResponse { SnapshotInfo info  = 1; }

message ListSnapshotsRequest {
    // 支持 "source_sandbox_id=<id>"、"label.<key>=<value>" 等过滤条件。
    map<string, string> filters = 1;
    // 最多返回条目数（0 表示不限制）。
    uint32 limit  = 2;
    string cursor = 3;  // 分页游标
}
message ListSnapshotsResponse {
    repeated SnapshotInfo snapshots = 1;
    string next_cursor = 2;
}

message DeleteSnapshotRequest  { string snapshot_id = 1; }
message DeleteSnapshotResponse {}

message PruneSnapshotsRequest {
    // 删除创建时间早于此时刻的快照（Unix 时间戳秒）。
    int64 older_than_unix_sec = 1;
    // 仅删除状态为 INVALID 的快照。
    bool  only_invalid = 2;
    // dry_run = true 时只返回待删除列表，不实际删除。
    bool  dry_run = 3;
}
message PruneSnapshotsResponse {
    repeated string deleted_snapshot_ids = 1;
    uint64 freed_bytes = 2;
}

// NetworkConfig 定义在 nanosandbox.sandbox.v1 包中，此处通过 import 引用，避免重复定义。
// 实现时应：import "nanosandbox/sandbox/v1/sandbox.proto";
// 并使用 nanosandbox.sandbox.v1.NetworkConfig。
// 下面的内联定义仅用于文档可读性。
//
// message NetworkConfig {  // → 使用 nanosandbox.sandbox.v1.NetworkConfig
//     string tap_name    = 1;
//     string mac_address = 2;
//     string ip_cidr     = 3;
//     string gateway     = 4;
// }
//
// 建议：将 ProcessSpec、NetworkConfig、RootfsConfig 等共享类型抽取到
// nanosandbox.common.v1 包，各 API 包统一从该包 import，消除代码生成中的类型转换负担。
```

---

### 核心工作流

#### 工作流一：从零冷启动容器

适用场景：该容器（函数）没有可用快照，或首次部署。

```
调用方 (Kubernetes CRI)          NanoSandbox Manager              Hypervisor
       │                                │                              │
       │ 1. CreateSandbox(spec)         │                              │
       │──────────────────────────────> │                              │
       │                                │ 2. Hypervisor.CreateVM()     │
       │                                │─────────────────────────────>│
       │                                │ <── vm_id                    │
       │ <── sandbox_id                 │                              │
       │                                │                              │
       │ 3. CreateTask(sandbox_id)      │                              │
       │──────────────────────────────> │ (记录 stdio FIFO 路径)       │
       │ <── task_id                    │                              │
       │                                │                              │
       │ 4. StartSandbox(sandbox_id)    │                              │
       │──────────────────────────────> │                              │
       │                                │ 5. Hypervisor.StartVM()      │
       │                                │─────────────────────────────>│
       │                                │     ┌─────────────────────┐  │
       │                                │     │ 内核引导 (~20ms)    │  │
       │                                │     │ Zero-Init exec      │  │
       │                                │     │ 容器进程 PID=1 就绪 │  │
       │                                │     └─────────────────────┘  │
       │                                │ <── VM_READY event           │
       │ <── started_at                 │                              │
       │                                │                              │
       │ 5. [可选] Stats / KillTask /   │                              │
       │    WaitTask ... (运行中)        │                              │
       │                                │                              │
       │ 6. StopSandbox(sandbox_id)     │                              │
       │──────────────────────────────> │ Hypervisor.StopVM()          │
       │ <── stopped_at                 │                              │
       │                                │                              │
       │ 7. DeleteSandbox(sandbox_id)   │                              │
       │──────────────────────────────> │ Hypervisor.DestroyVM()       │
       │ <── OK                         │ 释放所有资源                  │
```

关键时序（无 standby pool）：

```
t=0ms   CreateSandbox 调用
t=5ms   VM 实例创建完成（资源分配）
t=10ms  StartSandbox 调用
t=30ms  内核引导完成
t=35ms  容器进程 PID=1 就绪，Manager 收到 VM_READY
t=35ms  StartSandbox 返回（容器可处理请求）
```

#### 工作流二：从快照热启动容器

适用场景：已有该函数的 Base Snapshot（函数代码和运行时已在快照中完成初始化）。

```
调用方 (Kubernetes CRI)          NanoSandbox Manager              Hypervisor
       │                                │                              │
       │ 1. StartFromSnapshot(          │                              │
       │      snapshot_id,              │                              │
       │      new_sandbox_id,           │                              │
       │      network_config)           │                              │
       │──────────────────────────────> │                              │
       │                                │ 2. 加载快照元数据            │
       │                                │    验证 snapshot.state==READY│
       │                                │                              │
       │                                │ 3. Hypervisor.RestoreVM(     │
       │                                │      mem.raw,    ← mmap      │
       │                                │      cpu.bin,               │
       │                                │      devices.json,          │
       │                                │      network_config)         │
       │                                │─────────────────────────────>│
       │                                │     ┌─────────────────────┐  │
       │                                │     │ VMRESUME (~2ms)     │  │
       │                                │     │ userfaultfd 按需    │  │
       │                                │     │ 加载内存页          │  │
       │                                │     │ 容器进程从快照点    │  │
       │                                │     │ 继续执行 (~3ms)     │  │
       │                                │     └─────────────────────┘  │
       │                                │ <── VM_READY event           │
       │ <── {sandbox_id,               │                              │
       │      restore_latency_ms: 5}    │                              │
       │                                │                              │
       │ 2. [可选] GetTaskStatus()      │                              │
       │──────────────────────────────> │ vsock 查询容器进程状态       │
       │ <── TASK_RUNNING               │                              │
       │                                │                              │
       │ ... (容器处理请求) ...          │                              │
       │                                │                              │
       │ 3. StopSandbox / DeleteSandbox │                              │
       │──────────────────────────────> │ Hypervisor.DestroyVM()       │
       │ <── OK                         │                              │
```

关键时序：

```
t=0ms   StartFromSnapshot 调用
t=1ms   快照元数据加载、验证完成
t=2ms   mem.raw mmap 完成（文件映射到 VM 物理内存地址空间）
t=3ms   cpu.bin / devices.json 加载完成
t=4ms   VMRESUME 执行，容器进程从快照点恢复
t=5ms   Manager 收到 VM_READY（容器进程已在运行中）
t=5ms   StartFromSnapshot 返回（restore_latency_ms = 5）
```

注：内存页由 userfaultfd 在后台按需填入，不阻塞 VMRESUME。

#### 工作流三：为运行中容器创建快照

适用场景：函数完成 warm-up（如模型加载），在接收第一个请求前捕获快照，供后续实例快速恢复。

```
调用方                           NanoSandbox Manager              Hypervisor
       │                                │                              │
       │ [前置：函数已 warm-up 完成]     │                              │
       │                                │                              │
       │ 1. CreateSnapshot({            │                              │
       │      snapshot_id: "fn-v1-warm",│                              │
       │      source_sandbox_id: "s1",  │                              │
       │      type: FULL})              │                              │
       │──────────────────────────────> │                              │
       │                                │ 2. Hypervisor.PauseVM("s1")  │
       │                                │─────────────────────────────>│
       │                                │ <── paused (~1ms)            │
       │                                │                              │
       │                                │ 3. dump cpu.bin             │
       │                                │    dump mem.raw (脏页扫描)  │
       │                                │    dump devices.json        │
       │                                │    snapshot fs.diff         │
       │                                │    (~3ms for 256MB VM)       │
       │                                │                              │
       │                                │ 4. Hypervisor.ResumeVM("s1")│
       │                                │─────────────────────────────>│
       │                                │ <── resumed (~<1ms)          │
       │                                │                              │
       │                                │ 5. 写入 metadata.json        │
       │                                │    更新 Snapshot Store       │
       │ <── {snapshot_id,              │                              │
       │      size_bytes, captured_at}  │                              │
       │                                │                              │
       │ [后续：任意数量的新实例]         │                              │
       │ 2. StartFromSnapshot(          │                              │
       │      "fn-v1-warm", ...)        │                              │
       │──────────────────────────────> │ (见工作流二)                  │
```

---

## 部署模式

NanoSandbox 的调用方不局限于 Kubernetes kubelet，也可以是自研的节点代理（Node Agent）。根据调用方类型，NanoSandbox 支持两种部署模式，在工作流设计和 Task API 最小支持范围上有明显差异。

---

### 模式一：CRI 兼容模式

**适用场景**：调用方为标准 Kubernetes CRI 客户端（kubelet），要求与 containerd CRI Plugin 保持行为兼容，现有 `kubectl exec`、`kubectl top`、HPA、监控等 Kubernetes 生态工具开箱即用。

#### CRI 调用映射

标准 CRI 调用与 NanoSandbox API 的对应关系：

| CRI 调用 | NanoSandbox API | 说明 |
|----------|-----------------|------|
| `RunPodSandbox` | `CreateSandbox` + `StartSandbox` | VM 引导至"内核就绪、等待 exec"中间态；pause 容器配置（infra image/spec）由 CRI Plugin 忽略，NanoSandbox 通过 VM 本身承载 Pod 网络命名空间，无需 pause 容器进程 |
| `StopPodSandbox` | `StopSandbox` | 关闭 VM |
| `RemovePodSandbox` | `DeleteSandbox` | 销毁 VM 及全部资源 |
| `CreateContainer` | `CreateTask` | 注册容器 IO 配置与进程规格 |
| `StartContainer` | `StartTask` | 向 VM 注入进程配置，触发 exec |
| `StopContainer` | `KillTask` | 向容器主进程发送 SIGTERM/SIGKILL |
| `RemoveContainer` | `DeleteTask` | 清理任务资源 |
| `ContainerStatus` | `GetTaskStatus` | 查询容器状态 |
| `ExecSync` / `Exec` | `ExecTask` | `kubectl exec` |
| `ContainerStats` | `Stats` | `kubectl top` / HPA |

#### VM 两阶段启动机制

CRI 标准要求 `RunPodSandbox`（内部触发 `StartSandbox`）在 `CreateContainer`/`StartContainer` 之前完成，但 NanoSandbox 的 Zero-Init 模型要求容器进程规格在 exec 前确定。为此，CRI 兼容模式将 VM 启动拆分为两个阶段：

```
Phase 1：StartSandbox（由 RunPodSandbox 触发）
  ┌───────────────────────────────────────────────────┐
  │  Hypervisor.StartVM()                              │
  │    → 内核引导完成 (~20ms)                          │
  │    → /init 启动，打开 vsock，等待进程配置注入      │  ← 停在此处
  └───────────────────────────────────────────────────┘

Phase 2：StartTask（由 StartContainer 触发）
  ┌───────────────────────────────────────────────────┐
  │  Manager 通过 vsock 向 /init 发送 ProcessSpec      │
  │    （entrypoint、args、envs、uid/gid 等）          │
  │  /init exec → 容器 entrypoint 进程替换 /init       │  ← 容器就绪
  └───────────────────────────────────────────────────┘
```

两阶段设计使 Phase 1 可与 standby pool 结合：pool 内预热 VM 已完成 Phase 1，`StartTask` 调用时直接进入 Phase 2（vsock 注入 + exec ≈ 3ms），总启动延迟约 **3ms**。

#### 工作流：CRI 兼容模式冷启动

```
kubelet (CRI)       containerd CRI Plugin   NanoSandbox Manager        Hypervisor
     │                       │                       │                      │
     │ RunPodSandbox(spec)    │                       │                      │
     │──────────────────────>│                       │                      │
     │                       │ CreateSandbox(vmSpec) │                      │
     │                       │──────────────────────>│                      │
     │                       │                       │ CreateVM()           │
     │                       │                       │─────────────────────>│
     │                       │ <── sandbox_id        │ <── vm_id            │
     │                       │ StartSandbox(id)      │                      │
     │                       │──────────────────────>│                      │
     │                       │                       │ StartVM()            │
     │                       │                       │─────────────────────>│
     │                       │                       │  ┌────────────────┐  │
     │                       │                       │  │ 内核引导~20ms  │  │
     │                       │                       │  │ /init 等待     │  │  ← Phase 1 完成
     │                       │                       │  └────────────────┘  │
     │                       │                       │ <── KERNEL_READY     │
     │                       │ <── started_at        │                      │
     │ <── RunPodSandbox OK   │                       │                      │
     │                       │                       │                      │
     │ CreateContainer(spec)  │                       │                      │
     │──────────────────────>│                       │                      │
     │                       │ CreateTask(taskSpec)  │                      │
     │                       │──────────────────────>│ 记录 IO/ProcessSpec  │
     │                       │ <── task_id           │                      │
     │ <── container_id       │                       │                      │
     │                       │                       │                      │
     │ StartContainer(id)     │                       │                      │
     │──────────────────────>│                       │                      │
     │                       │ StartTask(task_id)    │                      │
     │                       │──────────────────────>│                      │
     │                       │                       │ vsock → /init        │
     │                       │                       │ 注入 ProcessSpec     │
     │                       │                       │ <── exec 完成        │  ← Phase 2 完成
     │                       │ <── pid=1             │                      │
     │ <── StartContainer OK  │                       │                      │
```

关键时序（无 standby pool）：

```
t=0ms   RunPodSandbox
t=5ms   CreateSandbox 完成（VM 资源分配）
t=25ms  StartSandbox 完成（内核就绪，Phase 1 完毕）→ RunPodSandbox 返回
t=25ms  CreateContainer（几乎无延迟，仅记录配置）
t=28ms  StartTask → vsock 注入 ProcessSpec
t=30ms  容器进程 exec 完成 → StartContainer 返回
──────────────────
端到端容器就绪：~30ms
```

#### Task API 最小支持范围（CRI 兼容模式）

CRI 兼容模式下，Task API **所有方法均为必要实现**，Kubernetes 核心功能路径覆盖了几乎全部方法：

| 方法 | 优先级 | 对应 CRI 调用 / 场景 |
|------|--------|----------------------|
| `CreateTask` | **必须** | `CreateContainer`（注册 IO + ProcessSpec）|
| `StartTask` | **必须** | `StartContainer`（触发 Phase 2 exec）|
| `KillTask` | **必须** | `StopContainer`（发送终止信号）|
| `WaitTask` | **必须** | 容器退出等待（kubelet 容器状态同步）|
| `DeleteTask` | **必须** | `RemoveContainer`（资源清理）|
| `GetTaskStatus` | **必须** | `ContainerStatus`（kubelet 轮询）|
| `ExecTask` | **必须** | `ExecSync`/`Exec`（`kubectl exec`）|
| `Stats` | **必须** | `ContainerStats`（`kubectl top`、HPA）|
| `ResizePty` | **必须** | TTY 场景（`kubectl exec -it`）|
| `ListPids` | 建议 | 部分监控/调试组件依赖 |

---

### 模式二：原生 NanoSandbox 模式

**适用场景**：调用方为自研节点代理（Serverless 平台 Node Agent、AI 推理调度器、边缘函数运行时等），无需兼容标准 CRI，可按 NanoSandbox 最优路径调用 API，追求最低延迟与最简调用链路。

#### 设计原则

1. **Sandbox 即 Container**：1:1 VM-per-Container 模型下，Sandbox 与 Task 生命周期完全一致，无需区分两个独立步骤。
2. **ProcessSpec 前置**：容器进程规格（entrypoint、args、envs 等）在 `CreateSandbox` 时一次性传入，`StartSandbox` 完成 VM 引导 + 容器 exec 一步到位，无中间等待状态。
3. **最小化调用次数**：冷启动仅需 **2 次** API 调用（`CreateSandbox` + `StartSandbox`）；快照启动仅需 **1 次**（`StartFromSnapshot`）。

#### 工作流：原生模式冷启动

```
Node Agent                         NanoSandbox Manager              Hypervisor
     │                                      │                            │
     │ CreateSandbox({                       │                            │
     │   sandbox_id,                        │                            │
     │   vcpu, mem,                         │                            │
     │   rootfs, network,                   │                            │
     │   process: {cmd, env, uid, ...}      │  ← 进程规格随 Sandbox 一起传入
     │ })                                   │                            │
     │─────────────────────────────────────>│                            │
     │                                      │ CreateVM(config)           │
     │                                      │───────────────────────────>│
     │ <── sandbox_id                       │ <── vm_id                  │
     │                                      │                            │
     │ StartSandbox(sandbox_id)             │                            │
     │─────────────────────────────────────>│                            │
     │                                      │ StartVM()                  │
     │                                      │───────────────────────────>│
     │                                      │  ┌──────────────────────┐  │
     │                                      │  │ 内核引导 (~20ms)     │  │
     │                                      │  │ /init 读取进程配置   │  │  ← 配置已在 VM 启动参数中
     │                                      │  │ exec 容器进程 PID=1  │  │  ← 一步完成，无需等待
     │                                      │  └──────────────────────┘  │
     │                                      │ <── CONTAINER_READY        │
     │ <── started_at（容器已就绪）          │                            │
     │                                      │                            │
     │ ... 函数执行 ...                      │                            │
     │                                      │                            │
     │ StopSandbox(sandbox_id)              │                            │
     │─────────────────────────────────────>│ StopVM()                   │
     │ <── stopped_at                       │                            │
     │                                      │                            │
     │ DeleteSandbox(sandbox_id)            │                            │
     │─────────────────────────────────────>│ DestroyVM()（资源全回收）  │
     │ <── OK                               │                            │
```

关键时序（无 standby pool）：

```
t=0ms   CreateSandbox（VM 资源分配）
t=5ms   StartSandbox
t=25ms  内核引导完成，容器进程 exec
t=28ms  StartSandbox 返回（容器已就绪）
──────────────────
端到端容器就绪：~28ms
```

#### 工作流：原生模式快照启动

快照启动为单次调用，容器进程恢复后无需任何 Task API 初始化流程：

```
Node Agent                         NanoSandbox Manager              Hypervisor
     │                                      │                            │
     │ StartFromSnapshot({                  │                            │
     │   snapshot_id: "fn-v1-warm",        │                            │
     │   new_sandbox_id: "s2",             │                            │
     │   network: {tap, mac, ip, gw},      │                            │
     │   override_envs: ["REQ_ID=xyz"]     │  ← 仅覆盖每次调用变化的 env │
     │ })                                   │                            │
     │─────────────────────────────────────>│                            │
     │                                      │ 加载快照元数据             │
     │                                      │ RestoreVM(                 │
     │                                      │   mem.raw ← mmap,          │
     │                                      │   cpu.bin,                 │
     │                                      │   devices.json,            │
     │                                      │   network_config)          │
     │                                      │───────────────────────────>│
     │                                      │  ┌──────────────────────┐  │
     │                                      │  │ VMRESUME (~2ms)      │  │
     │                                      │  │ userfaultfd 按需填页 │  │  ← 不阻塞 VMRESUME
     │                                      │  └──────────────────────┘  │
     │                                      │ <── CONTAINER_READY        │
     │ <── {sandbox_id,                     │                            │
     │      restore_latency_ms: 5}          │                            │
     │                                      │                            │
     │ ... 函数立即处理请求（应用已 warm）  │                            │
     │                                      │                            │
     │ DeleteSandbox(sandbox_id)            │                            │
     │─────────────────────────────────────>│ DestroyVM()                │
     │ <── OK                               │                            │
```

注：`StartFromSnapshot` 返回后，容器进程已在快照捕获点恢复执行。Node Agent **无需调用 `CreateTask`/`StartTask`**，Task 状态由 `GetTaskStatus` 直接查询即可。

关键时序：

```
t=0ms   StartFromSnapshot
t=1ms   快照元数据验证完成
t=2ms   mem.raw mmap 完成（物理内存后端建立）
t=3ms   cpu.bin / devices.json 加载，网络配置注入
t=5ms   VMRESUME，容器进程从快照点恢复执行
t=5ms   StartFromSnapshot 返回（restore_latency_ms = 5）
──────────────────
端到端容器就绪：~5ms
```

#### Task API 最小支持范围（原生模式）

原生模式面向 Serverless 函数和 AI Agent，Task 生命周期与 Sandbox 高度重合，Task API 可大幅精简：

| 方法 | 优先级 | 说明 |
|------|--------|------|
| `KillTask` | **必须** | 函数超时强制终止（SIGTERM → SIGKILL）|
| `WaitTask` | **必须** | 等待函数执行完成，获取退出码（计费、状态同步依据）|
| `GetTaskStatus` | **必须** | 查询容器进程状态（RUNNING / STOPPED）|
| `Stats` | **必须** | 函数计费（CPU/内存用量）、节点调度与资源回收依据 |
| `ExecTask` | 可选 | 仅调试模式需要；生产 Serverless 场景可不实现 |
| `CreateTask` | **不需要** | ProcessSpec 已在 `CreateSandbox` 中提供 |
| `StartTask` | **不需要** | 容器 exec 已由 `StartSandbox` / `StartFromSnapshot` 完成 |
| `DeleteTask` | **不需要** | 资源清理由 `DeleteSandbox` 统一完成 |
| `ResizePty` | **不需要** | Serverless 函数无 TTY |
| `ListPids` | **不需要** | 可通过宿主机 cgroup 路径直接读取 |

**MVP 结论**：原生模式最小实现为 4 个方法：`KillTask`、`WaitTask`、`GetTaskStatus`、`Stats`，其余按需扩展。

---

### 两种模式对比

| 维度 | CRI 兼容模式 | 原生 NanoSandbox 模式 |
|------|-------------|----------------------|
| 调用方 | kubelet / 标准 CRI 客户端 | 自研 Node Agent |
| 冷启动 API 调用次数 | 3 步（RunPodSandbox + CreateContainer + StartContainer）| 2 步（CreateSandbox + StartSandbox）|
| 快照启动 API 调用次数 | 3 步（RunPodSandbox + CreateContainer + StartContainer）| 1 步（StartFromSnapshot）|
| VM 两阶段启动 | 需要（Phase 1 在 RunPodSandbox，Phase 2 在 StartContainer）| 不需要（内核引导 + exec 一步完成）|
| 快照恢复后 Task 初始化 | 需要（CreateTask + StartTask）| 不需要（直接查询 GetTaskStatus）|
| 冷启动端到端延迟 | ~30ms | ~28ms（无两阶段切换开销）|
| 快照启动端到端延迟 | ~30ms（VMRESUME ~5ms + CreateTask + StartTask/vsock exec ~25ms；应用 warm-up 优势保留，但时延优势相对原生模式大幅缩减）| ~5ms（直接 VMRESUME，无 exec 步骤）|
| Task API 必须实现数 | 10 个 | 4 个 |
| Kubernetes 生态兼容性 | 完全兼容（kubectl / 监控 / 调度全链路）| 不兼容（需自建运维工具链）|
| 实现复杂度 | 较高（需维护 Phase 1/2 状态机，/init vsock 协议）| 低（Sandbox = Container，生命周期一一对应）|
| 推荐场景 | 存量 K8s 集群改造、需要 kubectl 运维能力 | 新建 Serverless 平台、AI 推理调度、边缘函数运行时 |

---

## 设计细节

### MicroVM 内核裁剪规范

目标：内核体积 < 4MB（压缩后），引导时间 < 20ms。

**必须保留的子系统**：

| 子系统 | 原因 |
|--------|------|
| x86_64 / arm64 架构支持 | 平台依赖 |
| virtio 总线（MMIO 模式）| 设备通信 |
| virtio-blk | rootfs 挂载 |
| virtio-net（可选）| 容器网络 |
| virtio-vsock | 控制通道 |
| virtio-rng | 熵源 |
| ext4 / erofs 文件系统 | rootfs 格式 |
| overlayfs（可选）| 可写层 |
| cgroup v2（`memory`、`cpu`、`pids` controllers）| VM 内进程资源控制（可选，宿主机 cgroup 已覆盖 VM 级资源限制）|
| 进程命名空间（`CONFIG_NAMESPACES`：pid/mount/uts/ipc）| 容器内隔离 |
| 网络栈（minimal）| 容器网络 |
| KVM guest 支持 | 虚拟化 |

**必须移除的子系统**：

- USB、ACPI、PCI 热插拔、蓝牙、音频、打印机等硬件子系统
- devtmpfs 动态设备（改为 static /dev）
- 调试子系统（kgdb、kprobes、ftrace 等）
- 所有非 virtio 网络驱动（e1000、ixgbe 等）
- 文件系统：NFS、CIFS、btrfs、XFS 等（只保留 ext4+erofs+tmpfs+proc+sysfs）
- 模块加载支持（内核完全 built-in，无 `insmod`/`modprobe`）

**内核命令行参数优化**：

```
console=hvc0                  # 最小控制台（可选，生产可关闭）
root=/dev/vda                 # rootfs 设备
rootfstype=erofs              # 文件系统类型
init=/init                    # 容器 entrypoint 包装器
quiet                         # 抑制内核启动日志
ro                            # rootfs 以只读方式挂载（virtio-blk 后端为只读镜像）；可写性由 overlay upper 层在内核挂载后叠加提供
nr_cpus=<vcpu_count>          # 限制 CPU 探测范围
lpj=<precomputed>             # 跳过 BogoMIPS 计算
```

### 根文件系统设计

NanoSandbox 的 rootfs 分为三层，在宿主机侧组装后以 virtio-blk 挂载进 VM：

```
┌─────────────────────────────┐
│      可写层（per-VM）        │  ← 宿主机侧稀疏文件，VM 退出后删除
│   /tmp, /var, 应用写入路径  │
├─────────────────────────────┤
│      容器镜像层（只读）      │  ← OCI 镜像展开的 erofs 镜像，多 VM 共享
│   /app, /lib, /usr, ...     │
├─────────────────────────────┤
│      NanoSandbox base层     │  ← 极简基础层，所有 VM 共享
│  /init, /lib/ld-linux.so,   │
│  /lib/libc.so, /dev（static）│
└─────────────────────────────┘
```

`/init` 是一个极简的 Rust 编写的 wrapper（< 100KB），职责：

1. 通过 vsock 与 Manager 建立控制通道，接收容器配置（env、cmd、uid/gid 等）。
2. 设置 cgroup、namespace（如需要）。
3. `exec()` 为容器的实际 entrypoint，自身被替换，不再占用内存。

### 设备模型最小化

| Hypervisor | 推荐设备配置 |
|------------|-------------|
| Firecracker | virtio-net（tap）、virtio-blk（rootfs）、virtio-vsock；无 PCI 总线；使用 MMIO |
| Cloud Hypervisor | virtio-net、virtio-blk、virtio-vsock、virtio-rng；PCIe 总线（轻量）|
| QEMU（裁剪模式）| `-M microvm`；virtio-net-device、virtio-blk-device、vhost-vsock-device；`-nodefaults -nographic` |
| StratoVirt | virtio-net、virtio-blk、virtio-vsock；轻量 MMIO 总线 |

所有 Hypervisor 配置的共同原则：不启用 USB、不启用 ACPI 热插拔、不模拟 ISA 设备（CMOS、8042 键盘控制器等）。

### 内存管理优化

**宿主机侧内存页预初始化（Pre-zeroed Page Pool）**：Serverless 节点上维护一个已清零内存页池（通过 `madvise(MADV_POPULATE_WRITE)` 或 `hugetlbfs` 预分配实现），当新 VM 创建时直接从池中分配，避免运行时内核按需清零（demand-zero）的延迟。此机制是纯宿主机侧优化，与 `virtio-balloon`（客户机内存回收驱动）无关，VM 内无需安装气球驱动。

**KSM（Kernel Same-page Merging）**：同一函数的多个实例（从相同 Base Snapshot 恢复）在内存中有大量相同页（只读内核代码段、共享库等），KSM 可自动合并这些页，大幅降低多实例场景的内存总开销（目标值：同一函数 100 个实例可节省约 40%~60% 的内存，具体取决于硬件平台与函数特征）。

> **安全权衡**：KSM 是已知的侧信道攻击向量——攻击者可通过观察内存合并/解除合并的时序变化推断其他 VM 的内存内容。**以下场景应禁用 KSM**：① 多租户节点（不同租户的函数实例共存）；② 涉及密钥生成、TLS 握手、密码学运算的函数。KSM 仅适合**同一租户、同一函数**多实例的密度优化场景，且须在部署策略中明确记录此安全权衡。

**HugePage 支持**：为单个 VM 分配 1GB 或 2MB 大页，减少 TLB 压力和内存页表开销，对内存密集型函数（如 AI 推理）有明显收益。

### 快照分级预热策略

在 Serverless 系统中，可按函数的调用频率实施分级快照预热：

```
Level 0 (热路径): 高频函数
  → 常驻 standby pool（预热 VM 常驻内存）
  → 恢复时间: < 5ms

Level 1 (温路径): 中频函数
  → Base Snapshot 存储在节点本地 SSD
  → userfaultfd 按需加载
  → 恢复时间: ~20ms

Level 2 (冷路径): 低频函数
  → Base Snapshot 存储在分布式存储（如 S3）
  → 首次恢复需从远端拉取，后续缓存本地
  → 恢复时间: ~200ms（首次）/ ~30ms（本地缓存命中）
  → ⚠️ 跨节点兼容性约束：快照中的 devices.json 包含 virtio 队列指针、
     中断控制器状态等与 Hypervisor 实现强相关的信息。跨节点恢复要求：
     ① 目标节点 Hypervisor 版本与捕获节点完全一致（建议通过镜像锁定版本）；
     ② CPU 架构相同（x86_64 快照不可在 arm64 节点恢复）；
     ③ mem.raw 中若含有宿主机物理地址相关内容，需 Hypervisor 在恢复时重映射。
     快照元数据（metadata.json）中应记录捕获时的 Hypervisor 版本与 CPU 架构，
     Manager 在恢复前验证兼容性，不匹配时拒绝恢复并返回错误。

Level 3 (无快照): 首次调用
  → 走冷启动路径（见工作流一）
  → 冷启动完成后，自动触发 CreateSnapshot，为下次准备快照
```

---

## 风险与缓解措施

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| 快照捕获时 VM 暂停导致请求超时 | 低（暂停 < 5ms）| 捕获前检查当前请求处理状态；支持异步捕获（后台完成）|
| 快照数据损坏导致恢复失败 | 高 | 写入时 checksum 校验；保留上一个有效快照作为回退；恢复失败自动降级为冷启动 |
| userfaultfd 导致恢复后首次请求延迟抖动 | 中 | 支持 pre-fault（VMRESUME 后后台预取工作集页）；对延迟 SLA 严格的场景禁用 userfaultfd，改为同步全量加载 |
| 1:1 VM 模型单节点 VM 数量上限 | 中 | 每个 MicroVM 内存可低至 64MB；配合 KSM 降低实际内存占用；通过 overcommit 提升密度 |
| 内核裁剪过度导致容器兼容性问题 | 高 | 维护标准内核（全功能）和裁剪内核（极速）两个版本；对有特殊内核需求的函数使用标准内核 |
| 无 shim 导致 containerd CRI 兼容性问题 | 中 | 提供一个轻量 shim 兼容适配层（参见 kuasar/shim），作为过渡方案 |

---

## 方案局限性

1. **不适合有状态长连接服务**：快照启动后网络连接状态丢失，恢复后需重新建立连接，不适用于 WebSocket 长连接、数据库连接池等有状态场景。
2. **不适合需要特殊内核模块的应用**：NanoSandbox 的裁剪内核不支持 `insmod`，对于依赖特定内核模块（如自定义网络驱动）的应用不适用。
3. **快照存储需求**：高频创建快照会消耗大量存储空间，需配合快照生命周期管理（TTL、分级存储）。
4. **多容器 Pod 支持有限**：1:1 VM-per-Container 模型使得 Kubernetes multi-container Pod（sidecar 模式，如 Istio envoy）需要多个 VM 或特殊处理。可选方案：
   - **方案 A（多 VM 共享网络）**：每个容器对应一个独立 MicroVM，Pod 内各容器的 virtio-net 通过 veth pair + bridge 或 macvlan 共享同一网络命名空间。需 CNI 插件配合，增加网络配置复杂度和每容器 ~13MB 的运行时开销。
   - **方案 B（单 VM 多进程）**：允许一个 VM 内运行多个容器进程，引入轻量 mini-agent 管理多进程生命周期。牺牲 Zero-Init 部分收益（多一个常驻进程），但与现有 Kubernetes sidecar 模式完全兼容。

   当前版本以单容器 Pod（Serverless 函数、AI 推理）为首要场景；多容器 Pod 支持列为后续演进目标。

---

## 替代方案讨论

### 替代方案一：保留 containerd-shim，仅优化内核引导

**思路**：不改变 containerd-shim 架构，仅通过裁剪内核和预启动池来加速 Kata Containers。

**缺陷**：shim 进程常驻内存开销（~10MB/Pod）和其带来的启动链路开销（fork shim ~30ms）无法消除；攻击面无法根本收缩；不满足"极简架构"目标。

### 替代方案二：使用 gVisor（runsc）替代 MicroVM

**思路**：用应用内核（gVisor）提供安全隔离，避免 VM 的内存开销。

**缺陷**：gVisor 的系统调用拦截引入额外延迟（I/O 密集型应用性能下降 20%~50%）；仍依赖宿主机内核，隔离强度低于 MicroVM；不支持快照机制。

### 替代方案三：process-level 快照（CRIU）

**思路**：使用 CRIU 在进程级别做快照，而非 VM 级别。

**缺陷**：CRIU 对程序兼容性要求高，许多多线程程序、依赖特定文件描述符的程序无法正确还原；恢复时间通常比 VM 快照慢；无法提供 VM 级别的安全隔离。

### 本方案选择理由

NanoSandbox 选择"VM 级快照 + 极简架构"的组合，是因为它在安全性（VM 硬件隔离）、启动速度（VM 快照 < 50ms）、架构简洁度（无 shim/runc）三个维度上做到了最优平衡，且对应用透明（无需应用适配快照机制）。

---

*文档版本：v0.2 | 最后更新：2026-04-14 | 变更：修复 design_issue.md 所列全部 24 项检视意见*
