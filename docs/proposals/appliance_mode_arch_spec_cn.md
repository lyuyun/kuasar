# Appliance Mode 目标架构分析

| 字段 | 值 |
|------|---|
| **类型** | 架构分析文档 |
| **参考提案** | [appliance_mode.md](./appliance_mode.md) |
| **状态** | Draft |
| **分析场景** | 冷启动（Cold Boot） |

---

## 文档导读

本文档面向**负责实现 Appliance Mode 的 Kuasar 开发者**。建议按以下路线阅读：

| 目的 | 建议阅读章节 |
|------|-------------|
| **理解 "为什么这么设计"** | §1（现状）→ §3（架构对比）→ §2.1（核心思想） |
| **理解整体架构和组件关系** | §2.2（三层引擎）→ §2.3（冷启动调用链）→ §3.3（进程模型对比） |
| **决定从哪个文件开始写代码** | §7.4（编码入口指南）→ §5.1（代码映射表） |
| **了解 kuasar-init 需要做什么** | §2.6（init 程序设计）→ §2.7（健康检查）→ §2.8（IO 支持） |
| **了解 K8s 接入的完整 API 需求** | §2.9（CRI / Task API 支持设计） |
| **评估改哪些现有代码、改多少** | §5.2（工作量分析）→ §5.3（耦合点）→ §5.4（风险） |
| **确认 Standard Mode 不会被破坏** | §5.4（风险评估）→ §7.3（M1 测试矩阵） |

**Appliance Mode 的一句话本质：** 将 Kuasar 的 Guest Agent（vmm-task）从 VM 中移除，由应用程序（或轻量的 kuasar-init 包装器）通过一个极简 JSON Lines over vsock 协议自报就绪，从而消除宿主机侧多阶段握手开销，降低 cold start 延迟。

---

## 目录

- [1. 当前架构分析](#1-当前架构分析)
  - [1.1 组件概览](#11-组件概览)
  - [1.2 冷启动调用链](#12-冷启动调用链)
  - [1.3 关键代码路径](#13-关键代码路径)
- [2. Appliance Mode 目标架构](#2-appliance-mode-目标架构)
  - [2.1 核心设计思想](#21-核心设计思想)
  - [2.2 三层引擎架构](#22-三层引擎架构)
  - [2.3 冷启动调用链（Appliance Mode）](#23-冷启动调用链appliance-mode)
  - [2.4 AdmissionController 设计](#24-admissioncontroller-设计)
  - [2.5 Appliance Mode 网络配置传递机制](#25-appliance-mode-网络配置传递机制)
  - [2.6 VM 内 init 程序设计](#26-vm-内-init-程序设计)
  - [2.7 应用健康检查机制](#27-应用健康检查机制)
  - [2.8 标准 IO 支持（stdin / stdout / stderr）](#28-标准-io-支持stdin--stdout--stderr)
  - [2.9 CRI 与 Task API 支持设计](#29-cri-与-task-api-支持设计)
- [3. 架构对比分析](#3-架构对比分析)
  - [3.1 CRI → Sandboxer 层对比](#31-cri--sandboxer-层对比)
  - [3.2 Sandboxer → VMM 层对比](#32-sandboxer--vmm-层对比)
  - [3.3 宿主机进程模型对比](#33-宿主机进程模型对比)
  - [3.4 冷启动时序对比](#34-冷启动时序对比)
  - [3.5 启动开销拆解](#35-启动开销拆解)
- [4. Appliance Mode 二进制分析](#4-appliance-mode-二进制分析)
  - [4.1 当前二进制构建体系](#41-当前二进制构建体系)
  - [4.2 Appliance Mode 的二进制变化](#42-appliance-mode-的二进制变化)
  - [4.3 Guest 镜像的根本差异](#43-guest-镜像的根本差异)
  - [4.4 构建系统变更](#44-构建系统变更)
- [5. 重构可行性评估](#5-重构可行性评估)
  - [5.1 现有代码结构与目标架构的映射](#51-现有代码结构与目标架构的映射)
  - [5.2 各层重构工作量分析](#52-各层重构工作量分析)
  - [5.3 主要耦合点与解耦策略](#53-主要耦合点与解耦策略)
  - [5.4 风险评估](#54-风险评估)
- [6. 向前兼容性分析](#6-向前兼容性分析)
  - [6.1 与 containerd / Sandbox API 的兼容性](#61-与-containerd--sandbox-api-的兼容性)
  - [6.2 与 Kubernetes CRI 的兼容性](#62-与-kubernetes-cri-的兼容性)
  - [6.3 与 VMM 版本演进的兼容性](#63-与-vmm-版本演进的兼容性)
  - [6.4 Appliance 协议的演进兼容性](#64-appliance-协议的演进兼容性)
  - [6.5 向前兼容性总结](#65-向前兼容性总结)
- [7. 结论与建议](#7-结论与建议)
  - [7.1 可行性结论](#71-可行性结论)
  - [7.2 冷启动场景的核心价值](#72-冷启动场景的核心价值)
  - [7.3 建议的实施优先级](#73-建议的实施优先级)
  - [7.4 编码入口指南](#74-编码入口指南)

---

## 1. 当前架构分析

### 1.1 组件概览

当前 Kuasar 的 MicroVM 沙箱运行时由以下组件构成：

```
┌──────────────────────────────────────────────────────────────────┐
│  用户平面（Kubernetes / crictl）                                   │
└───────────────────────────┬──────────────────────────────────────┘
                            │ CRI gRPC
┌───────────────────────────▼──────────────────────────────────────┐
│  containerd                                                      │
│  ├── CRI Plugin (RunPodSandbox / CreateContainer / StartContainer)│
│  └── Sandbox Plugin API (containerd 2.0)                         │
└───────────────────────────┬──────────────────────────────────────┘
                            │ Sandbox API (ttrpc/unix socket)
┌───────────────────────────▼──────────────────────────────────────┐
│  vmm-sandboxer（宿主机进程，每 node 一个）                          │
│  KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> │
│  ├── create()  → CloudHypervisorVM (配置设备、网络)                │
│  ├── start()   → vm.start() → init_client() → setup_sandbox()    │
│  ├── sandbox() → KuasarSandbox (持有 VM + ttrpc client)           │
│  │    ├── append_container() → HandlerChain → virtiofs mount      │
│  │    └── update_container() → ProcessHandler                     │
│  └── stop() / delete()                                           │
│                                                                  │
│  附属进程（每 sandbox）：                                           │
│  ├── virtiofsd   (共享宿主机目录到 VM 内)                           │
│  └── cloud-hypervisor (VM 进程)                                   │
└───────────────────────────┬──────────────────────────────────────┘
                            │ vsock (hvsock://task.vsock:1024)
                            │ 协议：ttrpc (protobuf)
┌───────────────────────────▼──────────────────────────────────────┐
│  vmm-task（VM 内，PID 1）                                          │
│  ttrpc 服务，监听 vsock://-1:1024                                  │
│  ├── SandboxService: check() / setup_sandbox() / sync_clock()    │
│  └── TaskService:   create() / start() / exec() / kill() / wait()│
│                                                                  │
│  容器进程（每 container）：                                          │
│  └── fork+exec（在独立 namespace/cgroup 中运行）                    │
└──────────────────────────────────────────────────────────────────┘
```

### 1.2 冷启动调用链

以下是一次完整的 Pod 冷启动的调用链（`RunPodSandbox` + `CreateContainer` + `StartContainer`）：

```
CRI (kubelet/crictl)
  │
  │ RunPodSandbox
  ▼
containerd CRI Plugin
  │ Sandboxer.Create(id, SandboxOption)
  ▼
KuasarSandboxer::create()
  ├─ SandboxCgroup::create_sandbox_cgroups()          # 宿主机 cgroup
  ├─ factory.create_vm(id, &s)                        # CloudHypervisorVMFactory::create_vm()
  │    ├─ CloudHypervisorVM::new()                    # 初始化 VM 配置
  │    ├─ add_device(Pmem "rootfs")                   # rootfs 作为 pmem 设备
  │    ├─ add_device(Rng)                             # 随机数设备
  │    ├─ add_device(Vsock {guest_socket: "task.vsock"})  # vsock，agent_socket = "hvsock://...task.vsock:1024"
  │    ├─ add_device(Console)                         # 串口
  │    └─ add_device(Fs "kuasar" ← virtiofsd.sock)   # virtio-fs 设备
  ├─ KuasarSandbox::setup_sandbox_files()             # 写 hosts/hostname/resolv.conf 到共享目录
  ├─ hooks.post_create()                              # CloudHypervisorHooks (空操作)
  └─ sandbox.dump()                                   # 序列化到 sandbox.json
  │
  │ Sandboxer.Start(id)
  ▼
KuasarSandboxer::start()
  ├─ hooks.pre_start()                                # 处理 CPU/内存资源限制，写入 vm.config
  ├─ sandbox.prepare_network()                        # 创建 veth pair，配置网络命名空间
  ├─ sandbox.start()
  │    ├─ vm.start()  ← CloudHypervisorVM::start()
  │    │    ├─ create_dir_all(base_dir)
  │    │    ├─ start_virtiofsd()                      # 启动 virtiofsd 子进程（~10-20ms）
  │    │    │    └─ tokio::process::Command::spawn("virtiofsd", ...)
  │    │    ├─ build_cmdline_params()                 # 组装 cloud-hypervisor 命令行参数
  │    │    ├─ set_cmd_netns()                        # 设置网络命名空间
  │    │    └─ Command::spawn("cloud-hypervisor", --kernel ... --disk ... --vsock ...) # 启动 VM
  │    │         # VM 内核启动 → 加载 initramfs → 启动 vmm-task（PID 1）
  │    │         # vmm-task: 挂载 /proc /sys /dev，挂载 virtiofs，监听 vsock:1024
  │    │
  │    ├─ init_client()                               # 连接 vmm-task（最多重试 45s）
  │    │    ├─ new_sandbox_client("hvsock://task.vsock:1024")
  │    │    │    └─ connect_to_hvsocket(): 发送 "CONNECT 1024\n"，等待 "OK\n"
  │    │    │         # 重试循环，每 10ms 一次，直到成功
  │    │    └─ client_check()                         # 调用 check() RPC，等待 vmm-task 就绪（最多 45s）
  │    │         └─ SandboxServiceClient::check()     # ttrpc RPC，重试直到成功
  │    │
  │    ├─ setup_sandbox()                             # 初始化沙箱
  │    │    └─ SandboxServiceClient::setup_sandbox()  # ttrpc RPC，传递网络/DNS 配置给 vmm-task
  │    │         # vmm-task 内：配置网络接口、bind mount DNS 文件
  │    │
  │    └─ forward_events()                            # 订阅 vmm-task 事件，转发给 containerd
  │
  ├─ monitor(sandbox_clone)                           # 启动 VM 监控 goroutine
  ├─ sandbox.add_to_cgroup()                          # 将 VM 进程加入 cgroup
  └─ hooks.post_start()                               # 设置 task_address，启动时钟同步
  │
  │ CreateContainer
  ▼
containerd CRI Plugin
  │ Sandbox.AppendContainer(id, ContainerOption)
  ▼
KuasarSandbox::append_container()
  └─ container_append_handlers() → HandlerChain::handle()
       ├─ MetadataAddHandler: 记录 container 元数据
       ├─ NamespaceHandler: 创建/准备命名空间描述符
       ├─ MountHandler (×N): 挂载 rootfs 层到 virtiofs 共享目录
       │    └─ 将容器 rootfs 层 mount 到 {base_dir}/shared/{container_id}/
       ├─ StorageHandler: 处理额外存储卷
       ├─ IoHandler: 设置 vsock IO 设备（热插拔到 VM）
       └─ SpecHandler: 写 OCI spec 到共享目录
  │
  │ StartContainer (通过 containerd Task API)
  ▼
  # containerd 通过 task_address（ttrpc+hvsock://...）直接连接 vmm-task
  vmm-task::TaskService::create()
       # 在 VM 内：从共享目录读取 OCI spec，创建 namespace/cgroup/mount
  vmm-task::TaskService::start()
       # fork+exec 容器进程
```

### 1.3 关键代码路径

| 步骤 | 代码位置 | 关键说明 |
|------|----------|----------|
| VM 创建 | `vmm/sandbox/src/cloud_hypervisor/factory.rs:43` | 装配设备列表，包括 virtiofs 设备 |
| VM 启动 | `vmm/sandbox/src/cloud_hypervisor/mod.rs:162` | 先启动 virtiofsd，再启动 cloud-hypervisor |
| ttrpc 连接 | `vmm/sandbox/src/client.rs:61` | `new_sandbox_client()`，45s 超时重试 |
| 就绪检测 | `vmm/sandbox/src/client.rs:220` | `client_check()`，轮询 `check()` RPC |
| 沙箱初始化 | `vmm/sandbox/src/sandbox.rs:559` | `setup_sandbox()`，传递网络/DNS |
| 容器挂载 | `vmm/sandbox/src/container/handler/mount.rs` | 将 rootfs 挂载到 virtiofs 共享目录 |
| vmm-task 主入口 | `vmm/task/src/main.rs:204` | 监听 `vsock://-1:1024`，注册 TaskService + SandboxService |

---

## 2. Appliance Mode 目标架构

### 2.1 核心设计思想

Appliance Mode 的核心转变是：**VM 就是应用，应用就是 VM**。

| 维度 | 当前（Standard Mode） | 目标（Appliance Mode） |
|------|----------------------|----------------------|
| Guest Agent | vmm-task（ttrpc 服务器，PID 1） | 应用程序自身（PID 1） |
| 通信协议 | ttrpc over vsock（protobuf） | JSON Lines over vsock（轻量） |
| 就绪信号 | ttrpc `check()` 轮询 | 应用主动发送 `{"type":"READY"}` |
| 共享文件系统 | virtiofsd（9p/virtiofs） | 无 |
| 容器抽象 | 完整 OCI 生命周期 | 无（VM = 单一应用） |
| Rootfs 交付 | virtiofs 共享目录（运行时挂载） | virtio-blk（ext4 镜像，启动前配置） |
| exec/attach | 支持 | exec：不支持（`UNIMPLEMENTED`）；attach：受限（仅 pty 模式，见 §2.9） |

### 2.2 三层引擎架构

```
┌─────────────────────────────────────────────────────────────────┐
│  Layer 3: API 适配层（进程启动时选择一种）                          │
│                                                                 │
│  ┌─────────────────────────┐   ┌──────────────────────────────┐ │
│  │  K8s Adapter            │   │  Direct Adapter              │ │
│  │  impl Sandboxer trait   │   │  native gRPC                 │ │
│  │  impl TaskService       │   │  PauseSandbox/ResumeSandbox  │ │
│  │  containerd 兼容         │   │  AI Agent 平台直接对接        │ │
│  └────────────┬────────────┘   └──────────────┬───────────────┘ │
│               └───────────────┬───────────────┘                 │
│                               ▼                                 │
├─────────────────────────────────────────────────────────────────┤
│  Layer 2: 核心引擎（VMM 和 GuestRuntime 无关）                     │
│                                                                 │
│  SandboxEngine<V: Vmm, R: GuestRuntime>                        │
│  ├── create_sandbox() → vmm.create() → 配置磁盘/网络/vsock       │
│  ├── start_sandbox(StartMode) → vmm.boot() → runtime.wait_ready │
│  ├── stop_sandbox() → runtime.shutdown() → vmm.stop()           │
│  └── AdmissionController (并发限制/预算控制)                      │
│                                                                 │
├─────────────────────────────────────────────────────────────────┤
│  Layer 1: 可插拔后端（进程启动时选择）                               │
│                                                                 │
│  Vmm trait                    GuestRuntime trait                │
│  ┌──────────────────┐         ┌──────────────────────────────┐  │
│  │ CloudHypervisor  │         │ VmmTaskRuntime               │  │
│  │ - create/boot    │   OR    │ - ttrpc → vmm-task           │  │
│  │ - stop/wait_exit │         │ - 现有标准模式               │  │
│  ├──────────────────┤         ├──────────────────────────────┤  │
│  │ Firecracker      │         │ ApplianceRuntime             │  │
│  │ (未来支持)        │         │ - vsock JSON Lines           │  │
│  └──────────────────┘         │ - 等待 READY 消息            │  │
│                               └──────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

**四种合法的运行时组合：**

| VMM | GuestRuntime | API 适配层 | 主要场景 |
|-----|-------------|-----------|----------|
| Cloud Hypervisor | VmmTaskRuntime | K8s Adapter | K8s Pod 隔离（现有） |
| Cloud Hypervisor | ApplianceRuntime | Direct Adapter | AI Agent 沙箱 |
| Firecracker | VmmTaskRuntime | K8s Adapter | K8s Pod 隔离（轻量） |
| Firecracker | ApplianceRuntime | Direct Adapter | Serverless/FaaS |

### 2.3 冷启动调用链（Appliance Mode）

```
AI Agent 平台 / CRI (kubelet)
  │
  │ CreateSandbox (Direct gRPC) / RunPodSandbox (CRI)
  ▼
Direct Adapter / K8s Adapter
  │ engine.create_sandbox(id, config)
  ▼
SandboxEngine::create_sandbox()
  ├─ vmm.create(VmConfig)
  │    ├─ CloudHypervisorVM 配置初始化
  │    ├─ vmm.add_disk({path: rootfs.ext4, readonly: true})  # virtio-blk（非 pmem，非共享）
  │    ├─ vmm.add_network(NetworkConfig)                     # virtio-net
  │    └─ vmm.add_vsock(port=1024)                           # 用于 Appliance 协议
  │    # 注意：无 virtiofsd 设备、无 virtio-fs 设备
  │
  │ StartSandbox (StartMode::Boot)
  ▼
SandboxEngine::start_sandbox(id, StartMode::Boot)
  ├─ admission.check(id)                    # 检查并发限制
  ├─ vmm.boot()  ← CloudHypervisorVM 冷启动
  │    ├─ 构建 cloud-hypervisor 命令行（--disk path=rootfs.ext4 --vsock ...）
  │    │  注意：无 --initramfs 挂载 vmm-task；rootfs 通过 virtio-blk 直接 boot
  │    └─ Command::spawn("cloud-hypervisor", ...)
  │         # VM 启动 → 内核 boot → kuasar-init（PID 1，推荐）或应用程序
  │         # kuasar-init 完成基础初始化、应用就绪后，主动连接 vsock port 1024 并发送 READY
  │         # 注：推荐以 kuasar-init 作为 PID 1，协议实现与应用解耦（详见 §2.6）
  │
  └─ runtime.wait_ready(sandbox_id)  ← ApplianceRuntime::wait_ready()
       ├─ 在 host 侧监听 vsock port 1024
       └─ 等待 JSON 消息: {"type":"READY","sandbox_id":"..."}
            # 超时：ready_timeout_ms（默认 5000ms）
            # 超时则 vmm.stop(force=true) + 返回错误
  │
  返回 StartSandboxResponse { ready_ms, mode_used: Boot }
  │
  │ 后续生命周期操作（Appliance Mode）
  ▼
  # 平台直接与沙箱内的应用交互（业务层面），Kuasar 不介入容器操作
  # StopSandbox → ApplianceRuntime::shutdown() → 发送 {"type":"SHUTDOWN","deadline_ms":30000}
  # DeleteSandbox → vmm.stop() → 清理工作目录
```

> **vsock 通信方向说明（与 Standard Mode 完全相反）**
>
> | 模式 | 连接方向 | Host 侧角色 | Guest 侧角色 |
> |------|---------|------------|------------|
> | Standard Mode | Host → Guest | client（`connect_to_hvsocket()`） | vmm-task 作为 ttrpc server，`bind vsock://-1:1024` |
> | Appliance Mode | Guest → Host | **vsock server**（`AF_VSOCK bind+listen+accept`） | 应用作为 client，主动连接 host |
>
> 这一方向反转意味着 host 侧的 `ApplianceRuntime::wait_ready()` 需要实现全新的 `AF_VSOCK` 服务端逻辑，与现有 `client.rs` 中的 `connect_to_hvsocket()` 客户端代码**无法复用**，需单独实现。

---

### 2.4 AdmissionController 设计

`AdmissionController` 是 `SandboxEngine` 中的并发准入控制组件，目前 Kuasar 中尚无此机制，需新增。

**并发限制语义：**

```rust
struct AdmissionController {
    max_concurrent_boots: usize,    // 同时允许的 boot 操作数（主要限制 VM 启动期间的资源占用）
    current_booting: AtomicUsize,   // 当前正在 boot 的 sandbox 数量
}
```

限制维度为**并发 boot 数量**（而非 sandbox 总数），因为 boot 阶段（VM 内核启动 + 应用初始化）对 CPU、内存带宽、I/O 的压力最大，稳态运行的 sandbox 资源消耗相对稳定。

**失败行为：** 超过并发上限时，`admission.check()` 立即返回 `Err(ResourceExhausted)`，由上层适配器转换为 gRPC `RESOURCE_EXHAUSTED` 错误。不排队等待，避免调用方长时间阻塞。

**与 `start_sandbox()` 的集成点：**

```
start_sandbox():
  1. admission.acquire()       # 占用一个 boot 配额（超限立即返回错误）
  2. vmm.boot()
  3. runtime.wait_ready()      # 等待 READY 消息（或超时）
  4. admission.release()       # 无论成功失败，均释放配额
  # 注：release() 在 wait_ready() 返回后立即释放，不等待 sandbox 整个生命周期结束
```

### 2.5 Appliance Mode 网络配置传递机制

Standard Mode 通过 `setup_sandbox()` ttrpc RPC 将网络配置（IP 地址、网关、DNS 服务器等）传递给 VM 内的 vmm-task，再由 vmm-task 配置网络接口。Appliance Mode 消除了 vmm-task，网络配置需通过其他机制传递。

#### 网络接口配置（IP / 网关 / 子网掩码）

**推荐方案：内核命令行 `ip=` 参数**

Linux 内核支持通过 `ip=` cmdline 参数在 boot 阶段由内核自动配置网络接口，无需用户态代码参与：

```
ip=<client_ip>:<server_ip>:<gw_ip>:<netmask>:<hostname>:<interface>:<autoconf>
# 示例
ip=10.0.0.2::10.0.0.1:255.255.255.0:pod-abc:eth0:off
```

Kuasar 的 `vmm.boot()` 在构建 cloud-hypervisor 命令行时，将从 sandbox 的网络配置中动态生成此参数并附加到 `--cmdline`，无需应用程序做任何额外工作。

**备选方案：Appliance 协议 CONFIG 消息**

对于需要运行时动态下发配置的场景（如热更新 DNS、多网卡），可在 READY 握手完成后由 host 发送 `CONFIG` 消息，应用通过 vsock 接收并自行应用：

```json
{"type":"CONFIG","network":{"ip":"10.0.0.2/24","gateway":"10.0.0.1","dns":["8.8.8.8"]}}
```

此方案要求应用主动处理 CONFIG 消息，对应用侧有代码要求。

#### DNS / hosts / hostname 配置

Standard Mode 中 `/etc/resolv.conf`、`/etc/hosts`、`/etc/hostname` 通过 virtiofs 共享目录注入，Appliance Mode 去除了 virtiofs，替代方案：

| 配置文件 | 推荐方案 | 说明 |
|---------|---------|------|
| `/etc/resolv.conf` | 内核 `ip=` 参数中的 DNS 字段（`ip=...:::...` 扩展格式），或 CONFIG 消息 | 内核 `nameserver` 选项支持有限，CONFIG 消息更灵活 |
| `/etc/hostname` | 内核 `ip=` 参数第 5 段（hostname 字段） | 内核 boot 时自动设置 |
| `/etc/hosts` | 嵌入 Guest rootfs 镜像 | Pod IP 和 hostname 可在镜像构建时模板化，或通过 CONFIG 消息下发 |

**设计结论：** Appliance Mode 推荐以内核 `ip=` cmdline 为主要网络配置机制（零应用代码要求），CONFIG 消息作为可选扩展（支持运行时动态配置）。hosts/resolv.conf 通过 CONFIG 消息在 READY 握手后下发，由应用（或 init 脚本）写入文件系统。

---

### 2.6 VM 内 init 程序设计

#### PID 1 的职责与挑战

Appliance Mode 中，VM 内的 PID 1 不再是 `vmm-task`，而是应用程序自身（或其包装器）。Linux 对 PID 1 有特殊行为，若应用不加处理直接作为 PID 1 运行，会面临以下问题：

| 问题 | 说明 |
|------|------|
| **僵尸进程积累** | 所有孤儿进程的父进程被重新分配给 PID 1。若 PID 1 不调用 `wait()`，孤儿进程变为僵尸占用 PID 表，最终导致 PID 耗尽 |
| **SIGTERM 无默认处理** | 内核对 PID 1 屏蔽了默认信号处理器——SIGTERM 不会自动终止 PID 1，除非进程注册了处理函数。SHUTDOWN 消息经由 `SIGTERM` 转发时若应用未处理，VM 将无法正常关闭 |
| **初始化环境缺失** | 内核 boot 完成后仅挂载了最基本的文件系统（内核 cmdline 指定的 rootfs），`/proc`、`/sys`、`/dev/pts` 等通常需要 PID 1 主动挂载 |
| **Appliance 协议实现负担** | 应用程序需自行实现 vsock 连接和 JSON Lines 协议，与业务逻辑耦合 |

#### 设计选项

| 选项 | PID 1 | 适用场景 | 代码侵入性 |
|------|-------|---------|-----------|
| **A：应用直接作为 PID 1** | 应用程序自身 | 单进程、专为 VM 设计、已处理信号和僵尸收割 | 应用需实现 Appliance 协议 |
| **B：`kuasar-init` 薄 init 层（推荐）** | `kuasar-init`（Kuasar 提供） | 通用场景，零应用修改 | 应用无需修改 |
| **C：init 脚本包装** | shell 脚本 | 简单自定义初始化，需 Guest 镜像含 shell | 编写启动脚本 |

#### 推荐方案：`kuasar-init` 薄 init 层

`kuasar-init` 是一个极轻量的 Rust 静态链接二进制（musl，目标大小 < 1 MiB），编译方式与 `vmm-task` 相同，作为 PID 1 嵌入 Guest 镜像的 `/sbin/init`。其启动流程如下：

```
kuasar-init（PID 1）启动流程
  │
  ├─ 1. 基础环境初始化
  │    ├─ mount /proc, /sys, /dev, /dev/pts（若内核未自动完成）
  │    ├─ 读取 /proc/cmdline，解析 KUASAR_SANDBOX_ID、KUASAR_APP 等参数
  │    └─ 设置 hostname（来自 cmdline 第 5 段，或后续 CONFIG 消息覆盖）
  │
  ├─ 2. 执行 init.d 钩子（可选，按序执行 /etc/kuasar/init.d/ 下的脚本）
  │    ├─ 00-mounts.sh       # 额外挂载（数据盘、tmpfs 等）
  │    ├─ 10-env.sh          # 设置环境变量
  │    └─ 90-readiness.sh    # 自定义就绪检测（可覆盖默认就绪判断逻辑）
  │
  ├─ 3. 启动应用进程（fork + exec，保留应用为子进程，非 exec 替换自身）
  │    └─ 应用进程作为 kuasar-init 的子进程运行（pid > 1）
  │         # 好处：kuasar-init 仍可收割孤儿进程、接收 host 控制消息
  │
  ├─ 4. 就绪检测（可配置，见下节 §2.7）
  │    ├─ 模式 A：fork+exec 成功后立即发送 READY（最快，适合启动即就绪的应用）
  │    ├─ 模式 B：轮询应用监听的 TCP 端口（如 :8080），端口可达后发送 READY
  │    └─ 模式 C：等待应用写入约定的就绪文件（如 /run/app-ready）
  │
  ├─ 5. 向 host 发送 READY，并建立持久 vsock 连接
  │    └─ {"type":"READY","sandbox_id":"<id>","version":"1","init":"kuasar-init"}
  │
  └─ 6. 进入事件循环
       ├─ 定期发送 HEARTBEAT（默认 10s 间隔）
       ├─ 接收并处理 host 消息（SHUTDOWN、CONFIG）
       ├─ 收割僵尸进程（SIGCHLD → waitpid(-1, WNOHANG)）
       └─ 监控应用进程退出 → 若非预期退出，发送 FATAL 消息
```

**`kuasar-init` 处理 SHUTDOWN 的行为：**

```
收到 {"type":"SHUTDOWN","deadline_ms":30000}
  │
  ├─ 向应用进程（及其进程组）发送 SIGTERM
  ├─ 等待应用进程退出（最长 deadline_ms）
  │    ├─ 应用正常退出 → kuasar-init 调用 sync() + reboot(RB_POWER_OFF)
  │    └─ 超时 → 发送 SIGKILL → 强制终止 → reboot(RB_POWER_OFF)
  └─ VM 电源关闭，cloud-hypervisor 进程退出，host 的 vmm.wait_exit() 返回
```

#### 扩展性：init.d 钩子目录

`kuasar-init` 在启动应用之前，按文件名字典序执行 `/etc/kuasar/init.d/` 下的可执行脚本（类似 sysvinit 的 `rc.d` 机制）：

```
/etc/kuasar/init.d/
  ├── 00-hostname.sh       # 覆盖 hostname 设置
  ├── 10-extra-mounts.sh   # 挂载数据盘或额外 tmpfs
  ├── 20-env.sh            # 注入环境变量（如 MODEL_PATH=/data/model）
  ├── 50-warmup.sh         # 预热操作（如加载模型权重到内存）
  └── 90-readiness.sh      # 自定义就绪检测（存在则覆盖内置模式 A/B/C）
```

镜像构建者可以在 Dockerfile/Containerfile 中将脚本 COPY 到此目录，`kuasar-init` 无需修改即可支持任意初始化步骤。这是 Appliance Mode 扩展初始化逻辑的主要机制。

**钩子脚本失败语义：**

任何钩子脚本以**非零退出码**退出时，`kuasar-init` 立即终止启动流程：
1. 通过 vsock 向 host 发送 `FATAL` 消息（在建立连接之前失败时写入串口日志）：
   ```json
   {"type":"FATAL","sandbox_id":"sb-123","exit_code":-1,"reason":"init.d hook failed: 10-env.sh (exit 1)"}
   ```
2. 调用 `reboot(RB_POWER_OFF)` 关机

Host 收到 `FATAL` 后，`start_sandbox()` 返回错误，sandbox 进入 `Failed` 状态。若希望某个钩子失败时**不阻断启动**，镜像构建者应在脚本内部处理错误（如 `cmd || true`），而不依赖外部容错机制。

**`kuasar-init` 的构建位置：**

```
vmm/init/src/main.rs          ← 新建，类比 vmm/task/src/main.rs
Cargo.toml（workspace member）  ← 新增 vmm/init
```

构建命令与 `vmm-task` 相同（musl 静态链接）：

```bash
cargo build --release --target x86_64-unknown-linux-musl --bin kuasar-init
```

---

### 2.7 应用健康检查机制

健康检查的目标是让 host 在 READY 之后持续感知 VM 内应用的存活状态（区别于一次性的 READY 信号）。

#### 机制 1：HEARTBEAT vsock 消息（推荐，主动推送）

由 `kuasar-init`（而非应用本身）定期发送，对应用零侵入：

```json
{"type":"HEARTBEAT","sandbox_id":"sb-123","timestamp_ms":1713216000000}
```

Host 侧 `ApplianceRuntime` 的全局 vsock 监听器维护每个 sandbox 的心跳记录：

```rust
struct HeartbeatTracker {
    last_seen: HashMap<SandboxId, Instant>,
    timeout: Duration,        // 默认 30s
    miss_threshold: u32,      // 连续缺失次数，默认 3
}
```

> **计时起始时机：** `HeartbeatTracker` 在 `wait_ready()` 成功返回（READY 消息已收到）后才开始计时，VM 启动阶段（spawn → 内核 boot → 应用初始化 → READY 发送）不在心跳监控范围内。否则，启动耗时较长的应用（如加载大型模型权重）会在尚未发送首次心跳前触发超时告警。

超过阈值时，`SandboxEngine` 将 sandbox 状态标记为 `Unhealthy`，并根据 `health.action` 配置执行策略：

| action | 行为 |
|--------|------|
| `notify` | 触发 SandboxEvent::HealthChanged，由上层适配器上报（默认） |
| `restart` | 调用 `stop_sandbox()` + `start_sandbox(StartMode::Boot)`（自动重启） |
| `stop` | 调用 `stop_sandbox()` + 标记为终止态，不重启 |

#### 机制 2：FATAL 消息（应用主动上报故障）

`kuasar-init` 监控应用进程。当应用意外退出（退出码非零）时，立即向 host 发送：

```json
{"type":"FATAL","sandbox_id":"sb-123","exit_code":137,"reason":"killed by SIGKILL (OOM)","timestamp_ms":1713216100000}
```

Host 收到 FATAL 后**立即**标记 sandbox 故障并触发 `health.action`，不等待心跳超时（30s），显著缩短故障响应时间。

#### 机制 3：VM 进程监控（兜底，不依赖 vsock）

`SandboxEngine` 在 `start_sandbox()` 成功后启动后台任务，持续监控 cloud-hypervisor 进程（`vmm.wait_exit()`）：

```rust
tokio::spawn(async move {
    let exit_info = vmm.wait_exit().await;
    // VM 进程意外退出 → sandbox 立即进入 Dead 状态
    engine.on_vm_exit(sandbox_id, exit_info).await;
});
```

此机制是健康检查的最后防线：即使 vsock 连接断开（HEARTBEAT 机制失效），VM 进程退出事件也会被捕获。三种机制的检测延迟对比：

| 机制 | 检测延迟 | 依赖 |
|------|---------|------|
| VM 进程监控 | < 1s（OS 级别进程退出通知） | 无（OS 保证） |
| FATAL 消息 | < 1s（应用退出时 kuasar-init 立即发送） | vsock 连接可用 |
| HEARTBEAT 超时 | `heartbeat_timeout_ms`（默认 30s） | vsock 连接可用 |

#### 健康检查配置（kuasar.toml）

host 侧和 Guest 侧的配置属于不同域，生效机制不同，分为两个子块：

```toml
# host 侧配置（kuasar.toml 直接读取，立即生效）
[appliance.health.host]
heartbeat_timeout_ms = 30000    # host 侧超时判定阈值（连续 miss_count_threshold 次未收到心跳则触发 action）
miss_count_threshold = 3        # 连续缺失 N 次心跳后触发 action
action = "notify"               # "notify" | "restart" | "stop"

# Guest 侧配置（通过内核 cmdline 传递给 kuasar-init，不可直接读取宿主机文件）
# vmm.boot() 构建 cloud-hypervisor 命令行时，将此参数转换为：
#   --cmdline "... KUASAR_HEARTBEAT_INTERVAL=10000"
# kuasar-init 启动后从 /proc/cmdline 解析，无需访问宿主机文件系统
[appliance.health.guest]
heartbeat_interval_ms = 10000   # kuasar-init 发送 HEARTBEAT 的间隔（单位 ms）
```

#### 与 K8s 探针的协同

对于通过 K8s Adapter 接入的 Appliance Mode sandbox，kubelet 的 `httpGet`/`tcpSocket` 探针通过 Pod IP 发起网络请求，与上述机制完全独立并互不干扰。

`exec` 探针（在容器内执行命令）不被 Appliance Mode 支持，应在 Pod spec 中改用网络探针——这属于用户文档约束（见 §6.2），而非架构缺陷。

---

### 2.8 标准 IO 支持（stdin / stdout / stderr）

Standard Mode 中，vmm-task 为每个容器创建 IO 通道（virtio-console 或 named pipe），containerd 的 IO proxy 透传 stdin/stdout/stderr。Appliance Mode 消除了 vmm-task，需为应用 IO 提供替代方案。

#### 方案对比

| 方案 | stdout/stderr | stdin | 实现复杂度 | 推荐阶段 |
|------|--------------|-------|-----------|---------|
| **A：串口 console 文件捕获** | 写入宿主机文件，可 tail | 不支持 | 极低 | M2 默认 |
| **B：virtio-console pty** | 宿主机 pty 设备，实时读取 | 支持（pty write） | 低 | 调试场景 |
| **C：vsock IO 协议** | vsock 消息流，结构化 | vsock 消息流 | 中 | M3 扩展 |
| **D：应用自管理（日志文件）** | 写入 rootfs，需额外读取机制 | 不适用 | 无（应用负责） | 生产应用 |

#### 推荐方案 A：串口 console 文件捕获（M2 默认）

Cloud Hypervisor 支持 `--serial file:<path>` 参数，将 VM 串口（ttyS0）输出重定向至宿主机文件。`kuasar-init` 在启动应用前将其 stdout/stderr 通过 fd 继承到串口设备（**不替换自身进程**，以保留事件循环）：

```rust
// kuasar-init 内部（Rust 代码等价）
// 注意：使用 spawn() 而非 exec()，kuasar-init 保持运行，继续执行事件循环（HEARTBEAT/SHUTDOWN 等）
let serial = OpenOptions::new().write(true).open("/dev/ttyS0")?;
// stdout/stderr 通过 fd 继承传递给应用，不设置 O_CLOEXEC
cmd.stdout(serial.try_clone()?).stderr(serial);
let _child = cmd.spawn()?;   // kuasar-init 仍然存在，不被替换
// 继续进入事件循环 → HEARTBEAT 定时发送、SHUTDOWN 接收处理...
```

宿主机侧 cloud-hypervisor 命令行由 `Vmm::boot()` 生成：

```
cloud-hypervisor \
  --serial file:/var/lib/kuasar/sandboxes/sb-123/console.log \
  --disk path=rootfs.ext4,readonly=on \
  --vsock cid=<cid>,socket=/var/lib/kuasar/sandboxes/sb-123/app.vsock \
  ...
```

**优点：** 零协议开销，天然持久化（日志文件可跨 sandbox 重启保留），通过 `tail -f console.log` 即可实时查看。

**缺点：** 无 stdin 支持，日志文件需要 rotation 管理（防止磁盘占满）：

```toml
[appliance.io]
console_mode = "serial-file"   # 默认
serial_log_path = "/var/lib/kuasar/sandboxes/{sandbox_id}/console.log"
max_log_size_mb = 100          # 超过后 rotate（保留最新 N MiB）
```

#### 方案 B：virtio-console pty（交互调试场景）

通过 `--console pty` 参数，cloud-hypervisor 在宿主机侧创建 pty 设备（`/dev/pts/N`），pty 路径通过 CH REST API 查询：

```
cloud-hypervisor \
  --console pty \
  ...
# CH 启动后通过 GET /api/v1/vm.info 获取 pty_path（如 /dev/pts/3）
```

宿主机侧工具可程序化读写该 pty，实现交互式访问：

```rust
// 在 SandboxEngine 中暴露 get_console_pty() 方法
fn get_console_pty(&self, sandbox_id: &str) -> Option<PathBuf>;
```

**pty_path 的获取时机与持久化：**
- **获取时机：** 在 `vmm.boot()` 返回后（cloud-hypervisor 进程已启动，pty 设备已由内核分配）、`runtime.wait_ready()` 等待期间，调用 `GET /api/v1/vm.info` 查询实际 pty 路径（如 `/dev/pts/3`）；pty 路径由内核动态分配，N 不固定
- **存储方式：** 作为 `SandboxPersistData` 的可选字段（`console_pty_path: Option<String>`）写入 `sandbox.json`
- **崩溃恢复：** 若 cloud-hypervisor 进程未重启，恢复后从 `sandbox.json` 读取的 pty_path 仍有效；若 VMM 进程已重启，则需重新调用 `GET /api/v1/vm.info` 查询新 pty_path

此方案适合开发调试（`screen /dev/pts/3`），不适合生产环境大规模部署（每个 sandbox 持有一个 pty fd）。

#### 方案 C：vsock IO 协议（M3 阶段扩展）

在 Appliance 协议中新增 IO 消息类型，由 `kuasar-init` 将应用的 stdout/stderr pipe 读取后通过持久 vsock 连接转发至 host，host 侧按需分发给调用方：

```json
// Guest → Host（应用标准输出片段）
{"type":"IO","stream":"stdout","sandbox_id":"sb-123","seq":42,"data":"SGVsbG8gV29ybGQ="}

// Host → Guest（向应用标准输入写入）
{"type":"IO","stream":"stdin","sandbox_id":"sb-123","seq":1,"data":"aW5wdXQ="}
```

- `data` 字段为 base64 编码的原始字节，支持任意二进制内容
- `seq` 字段单调递增，用于检测消息乱序或重连后的断点续传
- `kuasar-init` 通过 `pipe2(O_NONBLOCK)` 捕获应用的 stdout/stderr fd，异步读取后封装为 IO 消息发送

此方案实现了结构化、可寻址的 IO 流，Direct Adapter 可以将其暴露为 gRPC 流式接口，AI Agent 平台可以实时读取应用输出。代价是 `kuasar-init` 增加了 IO 转发逻辑，以及 base64 编解码的 CPU 开销（对 IO 密集型应用有影响）。

#### IO 方案选择建议

| 使用场景 | 推荐方案 |
|---------|---------|
| 生产应用（AI 推理服务等） | 方案 A（串口文件）+ 应用内部日志框架 |
| 交互式调试 | 方案 B（pty） |
| AI Agent 需实时读取应用输出 | 方案 C（vsock IO 协议，M3） |
| 应用完全自管理日志 | 方案 D（不配置 console） |

---

### 2.9 CRI 与 Task API 支持设计

#### 设计前提：合成容器模型（Synthetic Container）

Appliance Mode 的核心矛盾在于：**K8s 生态（kubelet、containerd）假设 Pod 内存在容器对象**，但 Appliance Mode 中 VM 就是应用本身，不存在容器抽象。若对所有容器操作返回 `UNIMPLEMENTED`，kubelet 会将 Pod 标记为失败状态，调度链路断裂。

解决方案是引入**合成容器（Synthetic Container）**：K8s Adapter 将整个 VM 应用映射为一个虚拟容器记录，使 CRI 调用链正常流转，而不驱动任何真实的容器创建。

```
Pod Spec                       Appliance Mode 内部
─────────────────────          ─────────────────────────────────────
containers:                    合成容器记录（仅存在于 SandboxEngine 内存中）
  - name: my-app       ────►   SyntheticContainer {
    image: myapp:v1              id: "{sandbox_id}-app",
    resources: ...               name: "my-app",
                                 state: mirrors sandbox state,
                                 image: "myapp:v1",   // 存储但不用于拉取
                                 resources: ...       // 用于 VM 配置
                               }
```

**合成容器的状态机：**

| sandbox 状态 | 合成容器状态（CRI ContainerState） |
|-------------|----------------------------------|
| `Creating` | `CONTAINER_CREATED` |
| `Ready`（READY 已收到） | `CONTAINER_RUNNING` |
| `Stopping` | `CONTAINER_RUNNING`（等待终止） |
| `Stopped`（VM 进程退出） | `CONTAINER_EXITED` |

合成容器的 `ExitCode` 来源（按优先级）：
1. kuasar-init 发送的 `FATAL` 消息中的 `exit_code` 字段
2. `vmm.wait_exit()` 返回的 VM 进程退出码
3. 宿主机强制终止时固定为 `137`（SIGKILL）

---

#### CRI RuntimeService API 支持矩阵

> 支持级别说明：✅ 完整支持 | ⚠️ 合成实现（行为符合 CRI 语义，数据来自 VM） | 🔶 受限支持（部分场景可用） | ❌ 不支持（返回 `UNIMPLEMENTED`）

**沙箱生命周期（Sandbox Lifecycle）**

| CRI 方法 | 支持级别 | 实现说明 |
|---------|---------|---------|
| `RunPodSandbox` | ✅ | → `SandboxEngine.create_sandbox()` + `start_sandbox()`；创建合成容器记录 |
| `StopPodSandbox` | ✅ | → `ApplianceRuntime.shutdown()` 发送 `SHUTDOWN`；VM 退出后合成容器进入 `EXITED` |
| `RemovePodSandbox` | ✅ | → `SandboxEngine.delete_sandbox()`；清理合成容器记录和工作目录 |
| `PodSandboxStatus` | ✅ | 状态来自 `SandboxEngine`；network 信息来自 sandbox 配置 |
| `ListPodSandbox` | ✅ | 枚举 `SandboxEngine` 中所有 sandbox 记录 |

**容器生命周期（Container Lifecycle，合成实现）**

| CRI 方法 | 支持级别 | 实现说明 |
|---------|---------|---------|
| `CreateContainer` | ⚠️ | 仅存储合成容器元数据到 `SandboxEngine`，返回合成 ID；不创建任何 namespace/cgroup |
| `StartContainer` | ⚠️ | noop——应用随 VM 启动已运行；将合成容器状态置为 `RUNNING` |
| `StopContainer` | ⚠️ | noop——容器停止须通过 `StopPodSandbox` 触发；直接返回 `Ok` |
| `RemoveContainer` | ⚠️ | 删除合成容器记录；不影响 VM 运行状态 |
| `ListContainers` | ⚠️ | 返回单条合成容器记录（每个 sandbox 对应一个） |
| `ContainerStatus` | ⚠️ | 从 sandbox 状态构造；`Pid` 字段来自 kuasar-init 上报的 `app_pid`（若无则为 0） |
| `UpdateContainerResources` | 🔶 | 映射为 VMM 热插拔：CPU 数量变更→ vCPU 热插拔，内存上限变更→ Memory Balloon；仅 CH 支持的参数生效 |
| `ReopenContainerLog` | 🔶 | 触发 console.log 日志轮转（当 `console_mode=serial-file` 时）；其他模式 noop |

**Exec / Attach / PortForward**

| CRI 方法 | 支持级别 | 实现说明 |
|---------|---------|---------|
| `ExecSync` | ❌ | `UNIMPLEMENTED`——VM 内无执行环境代理 |
| `Exec` | ❌ | `UNIMPLEMENTED` |
| `Attach` | 🔶 | 仅当 `console_mode=pty` 时可用：返回宿主机 pty 路径，供客户端连接；其余模式返回 `UNIMPLEMENTED` |
| `PortForward` | 🔶 | 宿主机侧通过 `socat` 将本地端口转发至 VM IP，不进入 VM 内部；适用于 kubectl port-forward |

**可观测性（Observability）**

| CRI 方法 | 支持级别 | 实现说明 |
|---------|---------|---------|
| `PodSandboxStats` | ✅ | 数据聚合（见下节§数据源设计） |
| `ListPodSandboxStats` | ✅ | 枚举所有 sandbox 的 stats |
| `ContainerStats` | ⚠️ | 与 `PodSandboxStats` 使用同一数据源，包装为单容器视图 |
| `ListContainerStats` | ⚠️ | 返回单条合成容器 stats 记录 |

**运行时管理**

| CRI 方法 | 支持级别 | 实现说明 |
|---------|---------|---------|
| `Status` | ✅ | 返回 K8s Adapter 和 `SandboxEngine` 的运行状态 |
| `UpdateRuntimeConfig` | ✅ | 更新 CNI 网络配置（与 Standard Mode 相同） |

---

#### containerd Task API（shim v2）支持矩阵

> Task API 由 K8s Adapter 实现，containerd 通过 ttrpc 调用；Appliance Mode 无 vmm-task，所有 Task 调用在 K8s Adapter 层处理。

**进程生命周期**

| Task API | 支持级别 | 实现说明 |
|---------|---------|---------|
| `State` | ⚠️ | 返回合成 task 状态；`Pid` 来自 kuasar-init `app_pid`；`Status` 映射自 sandbox 状态 |
| `Create` | ⚠️ | 记录合成 task 元数据；noop（应用已随 VM 启动） |
| `Start` | ⚠️ | noop；返回合成 `app_pid` |
| `Delete` | ⚠️ | 删除合成 task 记录；触发 sandbox 停止（若 VM 仍在运行） |
| `Wait` | ✅ | 阻塞等待 `vmm.wait_exit()` 返回；传递实际退出码 |
| `Kill` | ✅ | `SIGTERM` → 发送 `{"type":"SHUTDOWN"}`；`SIGKILL` → `vmm.stop(force=true)` |
| `Pids` | ⚠️ | 返回 `[{pid: app_pid, info: {name: "app"}}]`；`app_pid` 来自 kuasar-init 上报（无则返回空） |
| `Exec` | ❌ | `UNIMPLEMENTED` |
| `ResizePty` | ❌ | `UNIMPLEMENTED` |
| `CloseIO` | ❌ | `UNIMPLEMENTED` |

**资源与可观测性**

| Task API | 支持级别 | 实现说明 |
|---------|---------|---------|
| `Metrics` | ✅ | 数据来自聚合（见下节）；格式兼容 `containerd/cgroups` Metrics proto |
| `Update` | 🔶 | 映射为 VMM 资源热更新（vCPU 数量、Memory Balloon 大小）；不支持 cgroup 粒度的参数 |

**检查点 / 暂停**

| Task API | 支持级别 | 实现说明 |
|---------|---------|---------|
| `Pause` | 🔶 | 映射为 `vmm.pause()`（VM 级别暂停，CH 支持）；M3 阶段实现 |
| `Resume` | 🔶 | 映射为 `vmm.resume()`；M3 阶段实现 |
| `Checkpoint` | 🔶 | 映射为 VM 快照（CH snapshot API）；M3 阶段实现 |

---

#### Stats 数据源聚合设计

`PodSandboxStats` / `ContainerStats` / `Task.Metrics` 共用同一套数据聚合逻辑，数据来自三个层次：

```
Stats 响应组装（每次调用时聚合）

  ┌─────────────────────────────────────────────────────────────────┐
  │  层次 1：VMM API（宿主机侧，无需 Guest 配合，始终可用）           │
  │                                                                 │
  │  GET /api/v1/vm.counters（Cloud Hypervisor REST API）           │
  │  ├─ cpu.vcpu_cycles         → CpuStats.usage_core_nanoseconds  │
  │  ├─ net.rx_bytes / tx_bytes → NetworkStats                     │
  │  ├─ block.read_bytes /...   → FilesystemStats（virtio-blk）    │
  │  └─ balloon.actual_mb       → MemoryStats.available（估算）    │
  └─────────────────────────────────────────────────────────────────┘
          │ 与层次 2 数据合并（层次 2 优先，层次 1 作为 fallback）
  ┌─────────────────────────────────────────────────────────────────┐
  │  层次 2：kuasar-init METRICS 消息（需 kuasar-init，进程粒度）    │
  │                                                                 │
  │  {"type":"METRICS", "cpu_percent":45.2,                         │
  │   "mem_used_mb":1024, "app_pid":2,                              │
  │   "app_rss_mb":800, "fd_count":128}                            │
  │                                                                 │
  │  ├─ cpu_percent → CpuStats.usage_nano_cores（折算）            │
  │  ├─ mem_used_mb / app_rss_mb → MemoryStats.working_set_bytes   │
  │  └─ fd_count → （附加字段，非 CRI 标准，扩展 annotations 传递） │
  └─────────────────────────────────────────────────────────────────┘
          │ 按需触发（STATUS_QUERY）
  ┌─────────────────────────────────────────────────────────────────┐
  │  层次 3：按需 STATUS_QUERY（实时，精度最高）                       │
  │                                                                 │
  │  用于 metrics 精度要求高的场景（如 VPA 决策）；                   │
  │  host 发送 STATUS_QUERY，等待 STATUS_REPLY（最长 500ms）；       │
  │  超时则回退到层次 2 的缓存值                                      │
  └─────────────────────────────────────────────────────────────────┘
```

**数据可用性矩阵：**

| 指标字段 | 无 kuasar-init | 有 kuasar-init（METRICS 缓存） | 有 kuasar-init（STATUS_QUERY） |
|---------|--------------|------------------------------|-------------------------------|
| vCPU 周期数 | ✅ CH API | ✅ CH API | ✅ CH API |
| Guest CPU 使用率（%） | ❌ | ✅（~10s 延迟） | ✅（实时） |
| Guest 内存 working set | ⚠️ 气球估算 | ✅ 精确（RSS） | ✅ 精确 |
| 应用进程 RSS | ❌ | ✅ | ✅ |
| 文件描述符数 | ❌ | ✅ | ✅ |
| 网络 RX/TX 字节 | ✅ CH API（virtio-net 计数器） | ✅ | ✅ |
| 块设备读写字节 | ✅ CH API（virtio-blk 计数器） | ✅ | ✅ |

---

#### 关键设计决策汇总

**合成容器 ID 的命名规则：**

```
container_id = "{sandbox_id}-app"
// 例：sandbox_id = "sb-abc123" → container_id = "sb-abc123-app"
// 保持全局唯一性，便于 containerd 的 metadata 存储
```

**`ContainerStatus.Pid` 的语义：**

在 Standard Mode 中，`Pid` 是容器内进程在宿主机 PID namespace 中的映射 PID（可用于宿主机侧 nsenter/ptrace）。在 Appliance Mode 中，`app_pid` 是 Guest 内的 PID（VM 内的命名空间，无法直接从宿主机访问）。

设计选择：`Pid` 字段填入 `cloud-hypervisor` 进程在宿主机的 PID（而非 Guest 内的 `app_pid`），语义上代表"容器对应的宿主机进程"，保持字段可用性，避免误导性的 Guest 内 PID。`app_pid` 通过 `ContainerStatus.Annotations["kuasar.io/app-pid"]` 额外传递。

**`exec` 探针的显式拒绝策略：**

`ExecSync` / `Exec` 返回 `UNIMPLEMENTED` 而非静默超时，确保 Pod 配置了 `exec` 探针时快速失败，给运维人员明确的错误信息，而不是等待超时后引发误报。应在 `RuntimeClass` 文档中注明此约束（见 §6.2）。

**合成容器元数据的持久化与崩溃恢复：**

合成容器记录（`SyntheticContainer`）的 minimal 元数据（`container_id`、`image_ref`、`resource_spec`）写入 `SandboxPersistData.containers` HashMap（每个 sandbox 一条记录，即使在 Appliance Mode 下也不为空）。这与普通容器的区别在于：Appliance Mode 的 `containers` 条目仅包含元数据，不包含 OCI spec、namespace 描述符等运行时配置。

崩溃恢复流程：
1. 从 `sandbox.json` 反序列化 `SandboxPersistData`
2. 读取 `containers` 中的合成容器条目，重建 `SyntheticContainer` 对象
3. 将合成容器状态与 VM 当前运行状态同步（VM 仍在运行 → `RUNNING`；VM 进程已退出 → `EXITED`）

---

## 3. 架构对比分析

### 3.1 CRI → Sandboxer 层对比

#### 当前架构

```
CRI gRPC
    │ RunPodSandbox
    ▼
containerd (CRI Plugin)
    │ Sandboxer.Create() ──────────────────────────────────┐
    │ Sandboxer.Start()  ──────────────────────────────────┤
    │                                                      │
    │ Sandbox.AppendContainer()  ──────────────────────────┤
    │   (同步等待所有 handler 完成，包括 virtiofs mount)      │
    │                                                      ▼
    │                                           KuasarSandboxer
    │                                           (实现 Sandboxer trait)
    │                                           (实现 Sandbox trait)
    │
    │ Task API (通过 task_address 直连 vmm-task)
    ▼
vmm-task (in-VM)
    ├─ TaskService::Create()   # 创建容器 namespace/cgroup
    └─ TaskService::Start()    # fork+exec 容器进程
```

**关键特征：**
- containerd 需要感知 Task API 地址（`task_address = "ttrpc+hvsock://..."`），并直连 VM 内的 vmm-task
- Sandboxer（宿主机）和 Task 服务（VM 内）分属不同进程，存在两跳通信
- `AppendContainer` 在宿主机侧完成大量协调工作（virtiofs 目录准备、IO 设备热插拔）

#### 目标架构（Appliance Mode）

```
CRI gRPC           AI Agent Platform gRPC
    │ RunPodSandbox          │ CreateSandbox / StartSandbox
    ▼                        ▼
containerd          Direct Adapter
(K8s Adapter)       (native gRPC)
    │                    │
    └────────┬───────────┘
             ▼
  SandboxEngine<CloudHypervisorVmm, ApplianceRuntime>
             │
             │ 容器操作由 GuestRuntime 决定行为：
             │ ApplianceRuntime: Create→noop, Start→noop, Exec→UNIMPLEMENTED
             │ Shutdown → 发送 {"type":"SHUTDOWN"} JSON
             │ Wait → 等待 VM 进程退出
             │
             ▼
      无 Task 服务（VM 内无 vmm-task）
```

**关键特征：**
- K8s Adapter 与 Direct Adapter 共享同一个 `SandboxEngine`，提供两种 API 入口
- containerd 不再需要感知 `task_address`，不再直连 VM 内部
- 所有操作路径通过 `GuestRuntime` trait 统一抽象，无两跳通信

### 3.2 Sandboxer → VMM 层对比

#### 当前：`VM` trait（start() 内部耦合 virtiofsd）

```rust
// 当前 VM trait（vmm/sandbox/src/vm.rs）
trait VM {
    async fn start(&mut self) -> Result<u32>;    // 内部耦合了 virtiofsd 启动
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()>;
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)>;
    async fn hot_detach(&mut self, id: &str) -> Result<()>;
    async fn ping(&self) -> Result<()>;
    fn socket_address(&self) -> String;          // 语义上绑定了 ttrpc/vmm-task 地址
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>>;
    async fn vcpus(&self) -> Result<VcpuThreads>;
    fn pids(&self) -> Pids;
}
```

**针对 Appliance Mode 的具体问题：**
- `start()` 内部硬编码了 `start_virtiofsd()`（`cloud_hypervisor/mod.rs:164`），Appliance Mode 不需要 virtiofsd，无法绕开
- `socket_address()` 返回的是 vmm-task 的 ttrpc 地址（`hvsock://...task.vsock:1024`），语义上与 Standard Mode 绑定，Appliance Mode 不使用 ttrpc
- `start()` 不区分"配置 VM"和"启动 VM"两个阶段，无法支持快照恢复路径（`restore()` + `resume()` 需要在配置后、boot 之前插入）

#### 目标：`Vmm` trait（为 Appliance Mode 所需的最小改动）

针对 Appliance Mode 支持，`Vmm` trait 相比现有 `VM` trait 的核心改动有三处：

```rust
// 目标 Vmm trait（针对 Appliance Mode 的关键变化）
trait Vmm: Send + Sync {
    // 变化 1：将 start() 拆分为 create() + boot()
    // create(): 配置 VM（设备、网络），不启动任何进程
    // boot(): 冷启动 VM（仅 spawn cloud-hypervisor，不含 virtiofsd）
    async fn create(&mut self, config: VmConfig) -> Result<()>;
    async fn boot(&mut self) -> Result<()>;

    // 变化 2：去除 socket_address()，改为语义中立的 vsock_path()
    // 仅返回 vsock 文件路径，不绑定具体协议（ttrpc 或 JSON Lines 均可用）
    fn vsock_path(&self) -> Result<String>;

    // 不变：stop、wait_exit、hot_attach、hot_detach 等保持功能等价
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn wait_exit(&self) -> Result<ExitInfo>;
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;
    fn add_network(&mut self, net: NetworkConfig) -> Result<()>;
    fn capabilities(&self) -> VmmCapabilities;
    // ...
}
```

说明：`restore()`、`resume()`、`pause()`、`snapshot()` 等快照生命周期方法是对 `Vmm` trait 有价值的扩展，但它们同时服务于 Standard Mode 和 Appliance Mode，不是专为 Appliance Mode 引入的改进点，此处不展开。

### 3.3 宿主机进程模型对比

#### 当前（每 sandbox 2 个宿主机附属进程）

```
宿主机
├── vmm-sandboxer              # 1 个/节点
│
└── [per sandbox]
    ├── virtiofsd              # 共享目录守护进程
    └── cloud-hypervisor       # VM 进程
         └── [in VM] vmm-task (PID 1)
              └── [per container] app process
```

#### 目标 Appliance Mode（每 sandbox 1 个宿主机附属进程）

```
宿主机
├── kuasar-engine              # 1 个/节点（新统一二进制，见第 4 节）
│
└── [per sandbox]
    └── cloud-hypervisor       # VM 进程（仅此一个）
         └── [in VM] kuasar-init (PID 1)   ← 轻量 init 包装器，处理协议和僵尸收割
              └── [in VM] app (PID 2+)      ← 应用程序，由 kuasar-init fork+exec
```

**进程数量变化：** 每个 sandbox 宿主机侧附属进程从 2 个（virtiofsd + CH）降为 1 个（仅 CH）。VM 内部由 `kuasar-init`（PID 1）+ 应用进程（PID 2+）组成，替代了原来的 `vmm-task`（PID 1）+ 容器进程（PID 2+）。

### 3.4 冷启动时序对比

#### 当前架构冷启动时序

```
时间轴 ─────────────────────────────────────────────────────────────────────►

│←─ Create() ─→│←────────────────── Start() ───────────────────────────────→│

[cgroup setup  ]
[create VM cfg ]
                [start virtiofsd ]
                                  [spawn cloud-hypervisor                    ]
                                  [VM kernel boot                            ]
                                  [initramfs 加载                             ]
                                  [vmm-task PID1 启动                         ]
                                  [vmm-task 挂载 virtiofs                     ]
                                  [vmm-task 监听 vsock:1024                   ]
                                                           [hvsock 连接+重试  ]
                                                           [check() RPC 轮询  ]
                                                           [setup_sandbox() RPC]
                                                                              Done

│← ~5ms →│     │← ~10-20ms →│    │←───── VM 内核+vmm-task 启动 ~500ms ──────→│← ~30ms →│

CreateContainer: [MetadataH][NsH][MountH×N][StorageH][IoH][SpecH]
Task API (vmm-task): [create container][start process]
```

#### 目标 Appliance Mode 冷启动时序

```
时间轴 ──────────────────────────────────────────────────────────────────────►

│←── create_sandbox() ──→│←────── start_sandbox() ────────────────────────→│

[configure VM            ]
[add disk/net/vsock      ]     [spawn cloud-hypervisor (--disk rootfs.ext4) ]
                               [VM kernel boot                               ]
                               [应用程序 PID1 启动                            ]
                               [应用初始化                                    ]
                               [应用 → vsock:1024 发送 {"type":"READY"}       ]
                                                                              [host 收到 READY]
                                                                              Done

│← ~3ms →│                    │←──────── 主要是 VM+应用启动时间 ─────────────→│← <1ms →│

# 无 CreateContainer、无 StartContainer、无 Task API 调用
```

### 3.5 启动开销拆解

| 开销项 | 当前 Standard Mode | Appliance Mode | 说明 |
|--------|-------------------|----------------|------|
| virtiofsd 启动 | ~10-20ms | **0ms（消除）** | 无共享目录需求 |
| hvsock 连接重试 | ~10-50ms | **0ms（消除）** | 无 ttrpc 连接 |
| `check()` RPC 轮询 | ~10-30ms | **0ms（消除）** | 就绪由应用主动推送 |
| `setup_sandbox()` RPC | ~5-10ms | **0ms（消除）** | 无 vmm-task |
| `CreateContainer` handler chain | ~20-50ms | **0ms（消除）** | 无容器抽象 |
| vmm-task 初始化（挂载 virtiofs + 网络配置，VM 内） | ~50-100ms | **0ms（消除）** | vmm-task 不存在 |
| **可消除总开销** | **~105-260ms** | **0ms** | |
| `kuasar-init` 初始化（挂载 + init.d 钩子，VM 内） | 无此步骤 | ~1-5ms | kuasar-init 自身极轻量；init.d 钩子耗时视内容而定（不计入此处） |
| VM 冷启动（内核 + kuasar-init + 应用启动） | ~300-600ms（内核+vmm-task+应用） | ~300-605ms | kuasar-init 本身仅增加 ~1-5ms，主要耗时仍是内核 boot 和应用初始化 |
| **理论最优冷启动** | ~405-860ms | **~301-605ms** | |

---

## 4. Appliance Mode 二进制分析

### 4.1 当前二进制构建体系

当前 Kuasar 的二进制产物及其构建方式：

```
二进制                    来源                                      运行位置
──────────────────────────────────────────────────────────────────────────
vmm-sandboxer             vmm/sandbox/src/bin/cloud_hypervisor/    宿主机
  （cloud-hypervisor 变体）main.rs
                          cargo build --bin cloud_hypervisor

vmm-sandboxer             vmm/sandbox/src/bin/qemu/main.rs         宿主机
  （qemu 变体）            cargo build --bin qemu

vmm-sandboxer             vmm/sandbox/src/bin/stratovirt/main.rs   宿主机
  （stratovirt 变体）      cargo build --bin stratovirt

vmm-task                  vmm/task/src/main.rs                     VM 内（Standard Mode PID 1）
                          cargo build --target x86_64-unknown-linux-musl
                          # 静态链接 musl，嵌入 guest initramfs/image

kuasar-init               vmm/init/src/main.rs                     VM 内（Appliance Mode PID 1）
  （新增）                cargo build --target x86_64-unknown-linux-musl
                          # 静态链接 musl，嵌入 Appliance Mode guest image

wasm-sandboxer            wasm/src/main.rs                         宿主机
quark-sandboxer           quark/src/main.rs                        宿主机
runc-sandboxer            runc/src/main.rs                         宿主机
```

`vmm-sandboxer` 实际上是按 hypervisor 分开构建的多个二进制（`cloud_hypervisor`、`qemu`、`stratovirt`），在安装时统一重命名为 `vmm-sandboxer`（见 `Makefile install-vmm`）。`vmm-task` 是唯一运行在 VM 内部的 Kuasar 二进制。

### 4.2 Appliance Mode 的二进制变化

#### 宿主机侧：统一入口二进制

提案引入 `kuasar-engine` 作为统一宿主机入口，通过配置文件在进程启动时选择运行模式：

```toml
# kuasar.toml
[engine]
runtime_mode = "appliance"          # "standard" | "appliance"
vmm_type = "cloud-hypervisor"       # "cloud-hypervisor" | "firecracker"

[adapter]
type = "direct"                     # "direct" | "k8s"
listen = "unix:///run/kuasar/engine.sock"
```

对应的进程启动决策（无运行时分支）：

```rust
// cmd/kuasar-engine/main.rs
fn main() {
    let config = load_config();
    match (config.vmm_type, config.runtime_mode) {
        (VmmType::CloudHypervisor, RuntimeMode::Standard) =>
            K8sAdapter::new(SandboxEngine::new(CloudHypervisorVmm::new(), VmmTaskRuntime::new()))
                .serve(),
        (VmmType::CloudHypervisor, RuntimeMode::Appliance) =>
            DirectAdapter::new(SandboxEngine::new(CloudHypervisorVmm::new(), ApplianceRuntime::new()))
                .serve(),
        // ...
    }
}
```

**与现有二进制的关系：**

| 当前二进制 | Appliance Mode 对应 | 关系 |
|-----------|-------------------|------|
| `cloud-hypervisor`（vmm-sandboxer） | `kuasar-engine --vmm cloud-hypervisor --mode standard` | 功能等价，代码重组 |
| `qemu`（vmm-sandboxer） | `kuasar-engine --vmm qemu --mode standard` | 功能等价 |
| *(不存在)* | `kuasar-engine --vmm cloud-hypervisor --mode appliance` | **新增** |

可以选择两种发布策略：
- **方案 A（提案方向）**：`kuasar-engine` 作为新的统一二进制，逐步替代现有的 `cloud-hypervisor`/`qemu`/`stratovirt` 沙箱器二进制
- **方案 B（渐进式）**：在现有 `vmm/sandbox/src/bin/` 下新增 `cloud_hypervisor_appliance/main.rs`，复用 `SandboxEngine` 核心，独立发布

方案 A 长期更优（减少二进制数量），方案 B 对当前代码侵入最小。

#### VM 侧：vmm-task 完全消失

这是 Appliance Mode 最根本的二进制变化：

| 组件 | Standard Mode | Appliance Mode |
|------|--------------|----------------|
| VM 内 PID 1 | `vmm-task`（Kuasar 提供，musl 静态链接） | `kuasar-init`（Kuasar 提供，musl 静态链接，推荐） / 应用程序自身（简单场景） |
| Guest 镜像包含 | `vmm-task` + 精简 Linux 根文件系统 | `kuasar-init` + 应用程序 + 其依赖的根文件系统 |
| Kuasar 对 VM 内部的控制 | ttrpc 全量控制（create/start/exec/kill/wait） | vsock JSON 最小协议（READY/SHUTDOWN/HEARTBEAT/FATAL/IO） |
| 应用对 Kuasar 的侵入性 | 零侵入（应用在容器内，与 vmm-task 解耦） | 零侵入（kuasar-init 处理协议，应用无感知） |

`vmm-task` 的 musl 静态编译仍然必要，**仅用于 Standard Mode**。Appliance Mode 的 VM 镜像中不包含 `vmm-task`，改为包含 `kuasar-init`。两者均是 Kuasar 提供的 musl 静态链接二进制，但职责完全不同：`vmm-task` 是全功能 Guest Agent，`kuasar-init` 是轻量 PID 1 init 包装器。

### 4.3 Guest 镜像的根本差异

Standard Mode 的 Guest 镜像（initramfs 或 ext4 image）：

```
guest image (Standard Mode)
├── /sbin/vmm-task          ← Kuasar 提供，musl 静态链接，PID 1
├── /lib/，/usr/，/etc/     ← 精简 Linux 根文件系统
├── /run/kuasar/state/      ← virtiofs 挂载点（运行时由 vmm-task 挂载）
└── （无应用程序，容器 rootfs 通过 virtiofs 在运行时注入）
```

Appliance Mode 的 Guest 镜像（rootfs.ext4，virtio-blk 直接挂载）：

```
guest image (Appliance Mode)
├── /sbin/init               ← kuasar-init（Kuasar 提供，musl 静态链接，PID 1）
│                               或直接为应用程序二进制（简单单进程场景）
├── /usr/bin/myapp           ← 应用程序（由 kuasar-init exec 启动）
├── /lib/，/usr/，/etc/      ← 应用依赖的根文件系统（可以是最小化镜像）
├── /etc/kuasar/init.d/      ← init 钩子目录（可选，镜像构建者放置初始化脚本）
└── （无 vmm-task，无 kuasar state 挂载点，无 virtiofs 依赖）
```

**推荐**：使用 `kuasar-init` 作为 PID 1（见 §2.6）。`kuasar-init` 负责 Appliance 协议（READY/SHUTDOWN/HEARTBEAT）、僵尸进程收割和 init.d 钩子，对应用零侵入——应用程序无需修改，也无需了解 vsock 协议。

**简单场景**：若应用为单进程且已处理 SIGTERM，可直接以应用二进制作为 PID 1（内核 cmdline 指定 `init=/usr/bin/myapp`），但应用需自行实现 vsock READY 发送。

### 4.4 构建系统变更

对 `Makefile` 和 `Cargo.toml` 的必要变更：

```makefile
# Makefile 新增目标
bin/kuasar-engine:
    @cargo build --release --bin kuasar-engine
    @mkdir -p bin && cp target/release/kuasar-engine bin/kuasar-engine

# kuasar-init 与 vmm-task 相同，需要 musl 静态链接（运行在 VM 内）
bin/kuasar-init:
    @cargo build --release --target x86_64-unknown-linux-musl --bin kuasar-init
    @mkdir -p bin && cp target/x86_64-unknown-linux-musl/release/kuasar-init bin/kuasar-init

# Appliance Mode 宿主机侧二进制 + Guest 侧 init
appliance: bin/kuasar-engine bin/kuasar-init
```

```toml
# vmm/sandbox/Cargo.toml 新增 bin target（或 cmd/kuasar-engine/Cargo.toml 独立 crate）
[[bin]]
name = "kuasar-engine"
path = "src/bin/kuasar_engine/main.rs"
```

```toml
# 根 Cargo.toml —— workspace members 新增 vmm/init
[workspace]
members = [
    "vmm/sandbox",
    "vmm/task",
    "vmm/common",
    "vmm/init",    # ← 新增（kuasar-init crate，Appliance Mode PID 1）
    "wasm",
    "quark",
    "runc",
    "shim",
]
```

**不需要变更的部分：**
- `vmm-task` 构建配置（Standard Mode 仍使用）
- `wasm-sandboxer`、`quark-sandboxer`、`runc-sandboxer`（与本次重构无关）
- Guest 内核构建脚本（`vmm/scripts/kernel/`）

---

## 5. 重构可行性评估

### 5.1 现有代码结构与目标架构的映射

```
当前代码                              →  目标架构位置
─────────────────────────────────────────────────────────────────
vmm/sandbox/src/vm.rs                →  vmm/vmm-trait/src/lib.rs
  trait VM                                trait Vmm（拆分 start，改 socket_address）
  trait VMFactory                         VmmFactory<V: Vmm>（职责：创建 Vmm 实例）
  trait Hooks                             保留，用于 hooks 扩展点
  trait Recoverable                       保留

vmm/sandbox/src/cloud_hypervisor/   →  vmm/cloud_hypervisor/
  CloudHypervisorVM                       impl Vmm for CloudHypervisorVM
  CloudHypervisorVMFactory                VmmFactory<CloudHypervisorVmm>
  CloudHypervisorHooks                    保留（Standard Mode 的 virtiofsd 启动移至此处）
  CloudHypervisorConfig                   保留

vmm/sandbox/src/client.rs           →  vmm/runtime-vmm-task/src/lib.rs
  new_sandbox_client()                    impl GuestRuntime for VmmTaskRuntime
  client_check()                          → wait_ready() 内部
  client_setup_sandbox()                  → wait_ready() 内部
  publish_event()                         移至 K8s Adapter

                                     vmm/runtime-appliance/src/lib.rs  ← 全新
                                         impl GuestRuntime for ApplianceRuntime
                                         JSON Lines vsock 协议

vmm/sandbox/src/sandbox.rs          →  分拆为两部分：
  KuasarSandboxer (Sandboxer trait)       vmm/adapter-k8s/src/sandboxer.rs
  KuasarSandbox (Sandbox trait)           vmm/engine/src/sandbox.rs (SandboxEngine<V,R>)
    vm: V                                   vmm: V
    client: SandboxServiceClient            runtime: R（GuestRuntime trait 对象）
    containers: HashMap<..>                 containers（Standard Mode 有意义；Appliance 为空）

vmm/sandbox/src/container/         →  vmm/adapter-k8s/src/（K8s Adapter 内）
  handler/*                              container handler chain（仅 Standard Mode）
                                         Appliance Mode: append_container() = noop

                                     vmm/adapter-direct/src/  ← 全新
                                         Direct Adapter gRPC 服务器

vmm/common/                         →  保留（两种模式共用的 proto/API 定义）
vmm/task/                           →  保留（Standard Mode 的 guest agent 不变）
```

### 5.2 各层重构工作量分析

#### Layer 1：`Vmm` trait 重构（小-中工作量）

> **`Vmm::create()` 与 `VmmFactory::create_vmm()` 的职责区分：**
> - `VmmFactory::create_vmm(id, config) -> V`：**构造**一个新的 `Vmm` 实例（对应当前 `VMFactory::create_vm()` 的工厂职责）
> - `Vmm::create(&mut self, VmConfig)`：在已有实例上**配置设备**（磁盘、网络、vsock 等），不启动任何进程
> - `Vmm::boot(&mut self)`：**启动** VM 进程（spawn cloud-hypervisor）
>
> 调用顺序：`factory.create_vmm()` → `vmm.create(config)` → `vmm.boot()`。`VmmFactory` 保留在映射表中的原因是它负责实例创建，与 `Vmm::create()` 的配置职责不重叠。

核心改动只有两处，均为代码搬运而非逻辑重写：

1. **拆分 `start()` → `create()` + `boot()`**
   - `create()` ≈ 现有 `CloudHypervisorVMFactory::create_vm()` 的设备装配逻辑（移至实例方法）
   - `boot()` ≈ 现有 `CloudHypervisorVM::start()` 中去掉 virtiofsd 启动后的部分（仅 spawn cloud-hypervisor）

2. **virtiofsd 启动从 `boot()` 中移出**
   - 移至 `CloudHypervisorHooks::pre_start()`（Standard Mode Hooks 负责启动 virtiofsd）
   - `CloudHypervisorHooks` 在 Appliance Mode 下不使用，不需要额外条件判断

3. **`socket_address()` → `vsock_path()`**
   - 调整返回值语义：从 `hvsock://task.vsock:1024` 改为仅返回 vsock 文件路径 `task.vsock`
   - 协议前缀（ttrpc vs JSON Lines）由各 `GuestRuntime` 实现自行处理

#### Layer 1：`GuestRuntime` / `ContainerRuntime` trait（小工作量）

`GuestRuntime` trait 按职责拆分为两个 trait，避免 VM 就绪通信与容器生命周期的语义混合：

```rust
/// VM 就绪通信 —— 所有模式均需实现
trait GuestRuntime: Send + Sync {
    /// host 侧监听 vsock，等待 Guest 报告就绪信号
    async fn wait_ready(&self, vsock_path: &str, timeout_ms: u64) -> Result<()>;
    /// 向 Guest 发送关闭指令并等待 VM 进程退出
    async fn shutdown(&self, deadline_ms: u64) -> Result<()>;
}

/// 容器生命周期操作 —— 仅 Standard Mode 有意义
trait ContainerRuntime: Send + Sync {
    async fn create_container(&self, ...) -> Result<()>;
    async fn start_process(&self, ...) -> Result<u32>;
    async fn exec_process(&self, ...) -> Result<()>;
}
```

`SandboxEngine` 中：Standard Mode 同时持有 `GuestRuntime` + `ContainerRuntime`；Appliance Mode 仅持有 `GuestRuntime`，容器操作调用路径在 K8s Adapter 层以 UNIMPLEMENTED 短路，不需要在 `ApplianceRuntime` 里放 noop 实现。

**`VmmTaskRuntime`（Standard Mode，同时实现两个 trait）：**
- `wait_ready()`: 封装 `new_sandbox_client()` + `client_check()` + `setup_sandbox()`（代码搬运）
- `create_container()` / `start_process()` / `exec_process()`: 转发给 ttrpc 客户端
- 工作量：小

**`ApplianceRuntime`（Appliance Mode，仅实现 `GuestRuntime`）：**
- `wait_ready()`: 在 host 侧实现 `AF_VSOCK bind()+listen()+accept()`，解析 JSON Lines，等待 `READY` 消息

  **host vsock 监听架构：** 每个 sandbox 独立监听器 vs 单一全局监听器。推荐**单一全局监听器**（方案 B），通过 READY 消息中的 `sandbox_id` 字段将连接路由到对应 sandbox 的 `wait_ready()` future：

  ```
  全局 vsock server（port 1024，在 kuasar-engine 启动时创建）
       │ accept() 循环
       ├─ 连接 A → 解析 {"type":"READY","sandbox_id":"sb-001"} → 路由到 sandbox sb-001 的等待通道
       └─ 连接 B → 解析 {"type":"READY","sandbox_id":"sb-002"} → 路由到 sandbox sb-002 的等待通道
  ```

  此方案避免了为每个 sandbox 创建独立的 vsock 监听 fd，且天然支持 sandbox 数量动态变化。

  **vsock 连接持久化语义：** `accept()` 得到的连接 fd 在 READY 消息读取后**不关闭**，而是绑定到 sandbox 对应的会话对象（`ApplianceSession`），后续所有消息（HEARTBEAT、FATAL、SHUTDOWN、STATUS_QUERY / STATUS_REPLY、IO 流）均复用此同一长连接，直到 sandbox 退出。全局监听器持有所有活跃 sandbox 的 session 表：

  ```
  全局 vsock server
    │ session 表: HashMap<SandboxId, ApplianceSession>
    │   ├── sb-001 → ApplianceSession { conn_fd, last_heartbeat, tx_channel, ... }
    │   └── sb-002 → ApplianceSession { conn_fd, last_heartbeat, tx_channel, ... }
  ```

  各消息类型的使用方式：HEARTBEAT/FATAL/STATUS_REPLY 由 kuasar-init 主动发送（host 通过异步读循环接收）；SHUTDOWN/STATUS_QUERY 由 host 写入对应 session 的 conn_fd（通过 `tx_channel` 发送到写任务）。

- `shutdown()`: 通过 sandbox 对应的 `ApplianceSession.tx_channel` 将 `{"type":"SHUTDOWN","deadline_ms":N}` 写入长连接
- 工作量：小（全新但逻辑简单）

#### `kuasar-init`（新增，中工作量）

`kuasar-init` 是全新代码，但逻辑简单，无现有代码可复用：

| 功能模块 | 工作量 | 说明 |
|---------|--------|------|
| 基础 init 逻辑（mount、hostname、env） | 小 | 参考 `vmm-task/src/main.rs` 的 `init_vm_rootfs()` 部分 |
| 僵尸进程收割（SIGCHLD + waitpid） | 小 | 与 `vmm-task` 的信号处理逻辑相同，可直接复用 |
| vsock READY/SHUTDOWN/HEARTBEAT/FATAL 协议 | 小 | JSON Lines over vsock，对应 `ApplianceRuntime` 的对端实现 |
| 应用进程生命周期管理（fork + exec + 监控） | 小 | 标准 UNIX 进程管理，无特殊依赖 |
| init.d 钩子执行 | 小 | 遍历目录、按序执行，标准实现 |
| vsock IO 协议（stdout/stderr 转发） | 中 | 需 pipe 捕获 + 异步读取 + base64 编码，M3 阶段实现 |
| **总计** | **中** | M2 阶段实现前 5 项，M3 实现 IO 协议 |

#### Layer 2：`SandboxEngine` 核心（大工作量）

`KuasarSandbox<V>` 改造为 `SandboxEngine<V: Vmm, R: GuestRuntime>` 是工作量最大的部分：

```
主要改造点：
1. client: Arc<Mutex<Option<SandboxServiceClient>>>
   → runtime: R（GuestRuntime 实现，由进程启动时注入）

2. start() 方法重构：
   - vm.start() → vmm.boot()
   - init_client() + client_check() + setup_sandbox() → runtime.wait_ready()

3. 新增 AdmissionController（目前无此机制）

4. containers 字段处理：
   - Standard Mode: 保留完整 container handler chain，containers 存储完整 OCI 容器元数据
   - Appliance Mode: `append_container()` 将合成容器的 minimal 元数据（`container_id`、`image_ref`、`resource_spec`）写入 `containers` 字段（一条记录），但**不驱动**任何 namespace/cgroup/virtiofs 操作。崩溃恢复时从此字段重建 `SyntheticContainer` 状态，使 `ListContainers` / `ContainerStatus` 在进程重启后仍可返回正确结果

5. sandbox.json 序列化：
   - `runtime: R` 字段不可序列化（vsock 连接状态、ttrpc 客户端均为运行时状态）
   - 需分离持久化数据（`SandboxPersistData`）与运行时状态（在恢复时重建）
   - **新增 `runtime_type` 枚举字段**，用于恢复时路由到正确的 `GuestRuntime` 构造路径（否则从 JSON 反序列化后无法确定应重建 `VmmTaskRuntime` 还是 `ApplianceRuntime`）：

   ```rust
   #[derive(Serialize, Deserialize)]
   enum RuntimeType { Standard, Appliance }

   #[derive(Serialize, Deserialize)]
   struct SandboxPersistData {
       id: String,
       runtime_type: RuntimeType,   // 恢复时据此选择正确的 GuestRuntime 实现
       vmm_config: VmConfig,
       containers: HashMap<String, ContainerMeta>,
       // ...其他持久化字段
       #[serde(skip)]
       runtime: Option<Box<dyn GuestRuntime>>,  // 运行时重建，不序列化
   }
   ```

   旧版本的 `sandbox.json` 若缺少 `runtime_type` 字段，默认视为 `RuntimeType::Standard`（向后兼容）。
```

工作量评估：大（核心数据结构重组，需要仔细处理泛型约束和序列化兼容性）

#### Layer 3：API 适配层（中工作量）

**K8s Adapter（重构自现有 `KuasarSandboxer`）：**
- Sandboxer trait 实现：包装 SandboxEngine 调用，工作量小
- Sandbox trait 中 AppendContainer/UpdateContainer：保留 container handler chain，工作量小
- Appliance Mode 下 TaskService 的 exec/attach 返回 UNIMPLEMENTED，工作量小

**Direct Adapter（全新）：**
- 定义 protobuf 服务（`sandbox.proto`）
- 实现 gRPC/ttrpc 服务器，包装 SandboxEngine
- 工作量：中

### 5.3 主要耦合点与解耦策略

#### 耦合点 1：virtiofsd 与 VM 启动的耦合

**现状：** `CloudHypervisorVM::start()` 第 164 行直接调用 `self.start_virtiofsd()`，virtiofsd 是 VM 启动过程的硬编码步骤。

**解耦策略：** 将 virtiofsd 启动移入 `CloudHypervisorHooks::pre_start()`。Standard Mode 的 Hooks 负责启动 virtiofsd；Appliance Mode 使用空 Hooks（或不同的 Hooks 实现）。现有 `Hooks` trait 机制已为此预留了扩展点，`pre_start()` 在 `KuasarSandboxer::start()` 中已有调用位置（`vmm/sandbox/src/sandbox.rs:214`）。

#### 耦合点 2：`KuasarSandbox` 同时持有 VM 和 ttrpc client

**现状：** `KuasarSandbox<V>` 将 `vm: V`（VMM）和 `client: Arc<Mutex<Option<SandboxServiceClient>>>`（ttrpc）混合在同一结构中。

**解耦策略：** 引入 `GuestRuntime` trait 对象替换 `client` 字段。`VmmTaskRuntime` 内部持有 `SandboxServiceClient`；`ApplianceRuntime` 内部持有 vsock 连接状态。`SandboxEngine<V, R>` 中的 `runtime: R` 完成替换。

#### 耦合点 3：ContainerHandler chain 依赖 virtiofs 共享目录

**现状：** `MountHandler` 将容器 rootfs 挂载到 virtiofs 共享目录（`{base_dir}/shared/`），隐式依赖 virtiofsd 的存在。

**解耦策略：** Appliance Mode 下 `append_container()` 直接返回 `Ok(())`（noop），container handler chain 代码不需要修改，仅在 Standard Mode 的 K8s Adapter 路径下执行。

#### 耦合点 4：`sandbox.json` 包含运行时状态

**现状：** `KuasarSandbox<V>` 通过 `serde_json` 序列化包含 `containers`、`storages` 等字段，用于崩溃恢复，但 ttrpc client 和 vsock 连接等运行时状态已通过 `#[serde(skip)]` 标记排除。

**解耦策略：** 此耦合点现有代码已部分处理（`#[serde(skip)]`）。需要确保 `SandboxEngine<V, R>` 中 `runtime: R` 相关状态同样通过 `#[serde(skip)]` 排除，并在 Recoverable 恢复流程中按模式重建（Standard Mode 重建 ttrpc client，Appliance Mode 重建 vsock 监听器）。

### 5.4 风险评估

| 风险 | 影响 | 概率 | 缓解策略 |
|------|------|------|----------|
| `KuasarSandbox` 重构引入 Standard Mode 回归 | 高 | 中 | M1 以现有所有集成测试全部通过为验收标准，不允许任何行为变化 |
| virtiofsd 解耦导致 Standard Mode 行为变化 | 高 | 低 | virtiofsd 启动移入 Hooks，调用时序与现有代码保持一致 |
| `SandboxEngine<V, R>` 泛型约束过于复杂 | 中 | 中 | 引入类型别名（`type StandardEngine = SandboxEngine<CloudHypervisorVmm, VmmTaskRuntime>`），参考现有 `KuasarSandboxer<F, H>` 模式 |
| `sandbox.json` 序列化格式变更导致恢复不兼容 | 中 | 中 | 保持持久化字段集合不变，仅添加 `#[serde(skip)]` 注解和新的模式标识字段，旧 JSON 文件仍可读取 |
| Appliance 协议（JSON Lines）的应用侧实现门槛 | 低 | 高 | 使用 `kuasar-init` 作为 PID 1，应用零修改；提供多语言 SDK 示例（Rust/Python/Go）作为替代 |
| `kuasar-init` 作为 PID 1 的僵尸收割失效 | 中 | 低 | 单元测试覆盖 SIGCHLD + waitpid 路径；E2E 测试验证多进程应用场景下无僵尸进程残留 |
| `kuasar-init` 与应用进程的 SHUTDOWN 超时协调 | 低 | 低 | SHUTDOWN deadline 由 host 设置，`kuasar-init` 负责转发 SIGTERM → 等待 → SIGKILL → reboot；超时行为与 host 强制 stop 完全独立，不产生竞争 |
| 合成容器模型与 containerd metadata 存储的一致性 | 中 | 中 | K8s Adapter 崩溃重启后，合成容器记录需从 `SandboxEngine` 的 `sandbox.json` 恢复，而非依赖 containerd 自身的 metadata DB；需在 Recoverable 路径中重建合成容器状态 |
| `UpdateContainerResources` 映射为 VMM 热插拔的失败处理 | 低 | 中 | vCPU 热插拔或 Memory Balloon 调整失败时，CRI 返回错误；K8s VPA 的控制循环会重试，不会引发 Pod 重建；但需在错误消息中说明是 VMM 层限制 |

---

## 6. 向前兼容性分析

### 6.1 与 containerd / Sandbox API 的兼容性

**当前状态：** containerd 2.0 的 Sandbox API 仍在演进中，Kuasar 通过 `kuasar-io/rust-extensions` fork 跟踪非稳定版本。

**Standard Mode（K8s Adapter）的影响：**
- `Sandboxer` trait 和 `Sandbox` trait 的变更会直接影响 K8s Adapter 的实现
- containerd 升级时，K8s Adapter 需要同步更新（与现有代码面临相同的维护压力）
- Appliance Mode 本身不引入额外的 containerd API 依赖

**Direct Adapter 的隔离性：**
- Direct Adapter 使用自定义 gRPC/ttrpc 协议，完全不依赖 containerd API
- containerd API 演进不影响 Direct Adapter，AI Agent 平台可以长期稳定地使用 Direct API
- 这是 Direct Adapter 相比 K8s Adapter 的一个重要向前兼容优势

**具体风险：** 若 containerd 2.0 的 Sandbox API 最终稳定版本与当前 fork 差异较大，K8s Adapter 需要适配，但这与 Appliance Mode 无关，是 Standard Mode 已有的维护负担。

### 6.2 与 Kubernetes CRI 的兼容性

**API 兼容性总览：** §2.9 给出了所有 CRI RuntimeService 方法和 containerd Task API 的完整支持矩阵。核心结论是：沙箱生命周期（RunPodSandbox / StopPodSandbox / RemovePodSandbox）和可观测性 API（PodSandboxStats / ContainerStats / Metrics）完整支持；容器生命周期 API 通过合成容器模型在 CRI 语义层面保持兼容；exec / attach（pty 模式除外）不支持。

**exec 探针的问题：**

`ExecSync` 返回 `UNIMPLEMENTED`（快速失败，给出明确错误），不会静默超时。Kubernetes 的健康检查探针：
- `httpGet`/`tcpSocket` 探针：通过 Pod IP 发起网络请求，**与 Appliance Mode 完全兼容**
- `exec` 探针：在容器内执行命令，**Appliance Mode 不支持**——配置了 exec 探针的 Pod 会立即标记 `Unhealthy`

使用 Appliance Mode 的 Pod 必须用网络探针替代 exec 探针。这是**用户文档约束**，而非架构缺陷。

**RuntimeClass 的作用：**

Kubernetes RuntimeClass 机制允许将 Appliance Mode 注册为独立的运行时类（如 `kuasar-appliance`），平台管理员可以在 RuntimeClass 级别记录不支持 exec 的约束，引导用户正确使用：

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kuasar-appliance
handler: kuasar-appliance
# 未来 Kubernetes 若支持 RuntimeClass capabilities 字段，可在此声明不支持 exec
```

**`ContainerStatus.Pid` 的语义偏差（已知约束）：**

CRI 规范中 `ContainerStatus.Pid` 的语义是"容器内初始进程在宿主机 PID namespace 中的 PID"。Appliance Mode 中 Guest 内的 `app_pid` 无法直接映射到宿主机 PID namespace，因此 `Pid` 字段填入 cloud-hypervisor 进程在宿主机的 PID，实际 `app_pid` 通过 `Annotations["kuasar.io/app-pid"]` 附加传递。

影响范围：
- `kubectl describe pod` 显示的 PID 为 cloud-hypervisor 宿主机进程 PID，而非应用进程 PID
- 依赖 `ContainerStatus.Pid` 做 `nsenter` / cgroup 采样的监控工具行为与预期不符，应改用 `kuasar.io/app-pid` annotation
- Kubernetes node 排查工具（如 `crictl inspect`）将此字段解读为容器进程 PID 时可能产生误导

**建议：** 在 RuntimeClass 文档中注明此约束，监控工具和平台运维应通过 `kuasar.io/app-pid` annotation 获取应用进程 PID。

**未来 CRI 演进的兼容性：**

| CRI 演进方向 | 对 Appliance Mode 的影响 |
|-------------|------------------------|
| 新增 Pod 级操作（如 `checkpoint`） | K8s Adapter 需实现或返回 UNIMPLEMENTED，同 exec 处理方式 |
| 废弃 Task API（统一到 Sandbox API） | 有利于 Appliance Mode：减少需要实现的接口 |
| CRI 添加 RuntimeClass 约束声明 | 有利于 Appliance Mode：可以正式声明不支持 exec |
| 容器资源 QoS 细化 | 影响 Standard Mode 和 Appliance Mode：VM 粒度的资源限制，均通过 VMM 配置实现 |

### 6.3 与 VMM 版本演进的兼容性

**Cloud Hypervisor REST API 的稳定性：**

`CloudHypervisorVmm` 通过 `api_client` crate 调用 CH REST API。CH 的 API 在主版本间有时会有非兼容变更。`Vmm` trait 的抽象隔离了上层引擎代码，当 CH API 变更时，只需更新 `CloudHypervisorVmm` 的实现，`SandboxEngine` 和 `ApplianceRuntime` 不受影响。

**Firecracker API 的稳定性：**

Firecracker 的 REST API 相对稳定（遵循 semver），且提案要求 `FirecrackerVmm` 作为独立的 `Vmm` 实现，同样受 `Vmm` trait 隔离保护。

**`VmmCapabilities` 的向前兼容设计：**

当新版 VMM 增加能力时（如 CH 增加了 mmap 式快照恢复），只需在 `VmmCapabilities` 中新增字段（Rust 中新字段默认值为 false 或 None），旧代码不感知新能力，新代码按需查询。这是一个自然的向前兼容设计。

### 6.4 Appliance 协议的演进兼容性

vsock JSON Lines 协议是 Kuasar 与 Appliance Mode 应用之间的接口合约，其向前兼容性尤为重要：

**版本策略：**

协议消息中增加可选的 `version` 字段：

```json
{"type":"READY","sandbox_id":"sb-123","version":"1","app_version":"1.2.3"}
```

- `version` 字段缺失时默认为 `"1"`（向后兼容旧应用）
- 新版 Kuasar 新增消息类型时，旧应用忽略未知消息（JSON 解析跳过未知字段）
- 旧版 Kuasar 收到带未知字段的 READY 消息时正常处理（JSON 解析跳过未知字段）

**消息类型的稳定性分析：**

| 消息 | 方向 | 稳定性评估 |
|------|------|-----------|
| `READY` | Guest → Host | 核心消息，语义固定，长期稳定 |
| `SHUTDOWN` | Host → Guest | 核心消息，语义固定，长期稳定 |
| `HEARTBEAT` | Guest → Host | 可选扩展，需 kuasar-init；不影响核心流程 |
| `FATAL` | Guest → Host | 可选扩展，需 kuasar-init；不影响核心流程 |
| `CONFIG` | Host → Guest | 扩展点，内容自定义，通过 payload 字段版本化 |
| `PING`/`PONG` | 双向 | 可选扩展 |
| `METRICS` | Guest → Host | 可观测性扩展，需 kuasar-init；定期推送，内容随监控需求扩展（通过字段版本化） |
| `STATUS_QUERY` | Host → Guest | 可观测性扩展，需 kuasar-init；按需触发，超时可回退到 METRICS 缓存值 |
| `STATUS_REPLY` | Guest → Host | 配合 `STATUS_QUERY` 使用，语义同 METRICS 但为实时数据；与 METRICS 共享字段结构 |

核心协议（READY + SHUTDOWN）极其简单，长期稳定，应用侧实现成本极低。

**SHUTDOWN 流程完整状态机：**

```
host 调用 stop_sandbox()
  │
  ├─ 情形 A：vsock 连接正常
  │    │ 发送 {"type":"SHUTDOWN","deadline_ms":30000}
  │    ├─ VM 收到后主动调用 shutdown(0) / exit()
  │    │    └─ VM 进程退出 → vmm.wait_exit() 返回 → 成功
  │    └─ 超时（deadline_ms 到期后 VM 仍未退出）
  │         └─ vmm.stop(force=true) 强制终止 VM 进程 → 返回 Ok（超时不视为错误，已尽力通知）
  │
  ├─ 情形 B：vsock 连接已断开（VM 意外退出或应用崩溃）
  │    │ 发送 SHUTDOWN 消息时 write() 返回 EPIPE/ECONNRESET
  │    └─ 跳过发送，直接检查 VM 进程是否已退出
  │         ├─ 已退出 → 记录 warn 日志，返回 Ok
  │         └─ 未退出 → vmm.stop(force=true) 强制终止
  │
  └─ 情形 C：sandbox 尚未完成 wait_ready()（boot 期间收到 stop 请求）
       └─ 取消 wait_ready() future → vmm.stop(force=true) → 清理资源
```

应用收到 SHUTDOWN 消息后**不需要回 ACK**，host 通过监听 VM 进程退出事件（`vmm.wait_exit()`）判断关闭完成，简化协议实现。

**SDK 策略：**

提供官方 Appliance 协议 SDK（Rust crate + Python package + Go module），封装 vsock 连接和消息序列化。应用开发者使用 SDK 而非手写 JSON，协议演进时只需升级 SDK，应用代码不变。

### 6.5 向前兼容性总结

| 维度 | 兼容性评级 | 说明 |
|------|-----------|------|
| containerd API 演进 | 中 | K8s Adapter 需跟踪，与现有代码同等维护压力；Direct Adapter 完全隔离 |
| Kubernetes CRI 演进 | 良好 | exec 不支持是已知约束，通过 RuntimeClass 文档化；网络探针完全兼容 |
| Cloud Hypervisor 版本升级 | 良好 | `Vmm` trait 隔离，CH 变更仅影响 `CloudHypervisorVmm` 实现 |
| Appliance 协议演进 | 优秀 | JSON Lines + 版本字段 + 忽略未知字段，天然向前兼容 |
| Kubernetes 新特性 | 良好 | RuntimeClass 约束声明有利于 Appliance Mode 正式化 |
| Firecracker 接入 | 良好 | `Vmm` trait 隔离，新增 `FirecrackerVmm` 不影响现有代码 |

**最关键的向前兼容结论：** Direct Adapter 完全脱离 containerd API 演进路径，是 AI Agent 等场景最稳定的接入方式。选择通过 K8s Adapter 接入 Appliance Mode 的用户，需承担与 containerd API 演进同步的维护成本，但这与 Standard Mode 用户面临的约束完全相同。

---

## 7. 结论与建议

### 7.1 可行性结论

**架构重构在技术上完全可行。** 主要依据：

1. **泛型抽象基础已有验证**：`KuasarSandboxer<F: VMFactory, H: Hooks>` 的双泛型模式证明团队有此设计经验，新的 `SandboxEngine<V: Vmm, R: GuestRuntime>` 是同一模式的自然延伸。

2. **核心耦合点的解耦路径清晰**：virtiofsd 启动、ttrpc 连接、container handler chain 三处主要耦合，均有具体的代码搬运策略，不需要重写业务逻辑。

3. **Appliance Mode 自身逻辑简单**：`ApplianceRuntime` 的核心实现（JSON Lines over vsock，等待 READY）代码量极少，复杂性集中在引擎层的泛型重构，而非新功能本身。

4. **VM 侧完全解耦**：`vmm-task` 不需要修改，Appliance Mode 的 VM 内完全不涉及 Kuasar 代码，应用程序只需实现一个极简协议。

### 7.2 冷启动场景的核心价值

冷启动场景的架构差异体现在**消除宿主机侧的多阶段握手开销**：

```
当前：VM 启动 → virtiofsd 就绪 → ttrpc 连接 → check() 轮询 → setup_sandbox() → CreateContainer → StartContainer
目标：VM 启动 → 应用就绪 → READY 消息
```

这是**根本性的架构简化**：将"宿主机协调多个组件"模型变为"VM 自报就绪"模型。对于 AI Agent 场景，Kuasar 控制平面开销从 ~100-260ms 降至接近零，瓶颈回归 VM 本身的启动时间。

### 7.3 建议的实施优先级

1. **先完成 M1（核心架构重构，行为不变）**：`Vmm` trait 与 `GuestRuntime` / `ContainerRuntime` trait 的引入，`SandboxEngine<V, R>` 的构建，K8s Adapter 的提取。验收标准：现有所有测试全部通过。

   **M1 最小测试矩阵（合并前硬性门禁）：**

   | 测试类型 | 具体范围 | 验收标准 |
   |---------|---------|---------|
   | 编译检查 | `cargo check --all-features`，无 clippy 警告 | 通过 |
   | `SandboxEngine<V, R>` 单元测试 | mock `Vmm` + mock `GuestRuntime`，覆盖 create/start/stop 生命周期 | 通过 |
   | Standard Mode E2E 测试（完整套件） | `RunPodSandbox` + `CreateContainer` + `StartContainer` + `StopSandbox` | 全部通过，行为与重构前完全一致 |
   | virtiofsd Hooks 集成测试 | Standard Mode `CloudHypervisorHooks::pre_start()` 启动 virtiofsd | virtiofsd 进程正常启动并可挂载 |
   | 崩溃恢复路径 | `sandbox.json` 序列化 → 进程重启 → `recover()` → 容器仍可用 | 恢复后容器状态正确 |
   | `runtime_type` 向后兼容 | 旧格式 `sandbox.json`（无 `runtime_type` 字段）反序列化 | 默认为 Standard，不报错 |

2. **M2 优先实现 `ApplianceRuntime` + Direct Adapter**：Appliance 协议实现简单，可以快速验证端到端路径。Direct Adapter 的 protobuf 定义需要在 M2 早期确定以保证稳定性。

3. **M3（快照路径）需充分基准测试支撑**：CH v50.0 的约束（`--kernel`+`--restore` 冲突、`fill_saved_regions()` 同步读取）需要在进入主分支前完成实测验证。

### 7.4 编码入口指南

本节为开发者提供各模块的**代码入口**和**最小可运行里程碑**，避免阅读完整文档后不知从何下手。

#### M1：核心 trait 重构（不新增功能，只重新组织代码）

**Step 1：定义新 trait（新建文件，无破坏性）**

| 文件 | 操作 | 内容 |
|------|------|------|
| `vmm/sandbox/src/vmm_trait.rs`（新建） | 新增 | `Vmm` trait（拆分自现有 `VM` trait）；`VmmFactory` trait |
| `vmm/sandbox/src/guest_runtime.rs`（新建） | 新增 | `GuestRuntime` trait；`ContainerRuntime` trait |

**Step 2：实现现有代码到新 trait 的迁移**

| 文件 | 操作 | 说明 |
|------|------|------|
| `vmm/sandbox/src/cloud_hypervisor/mod.rs` | 改 | 将 `start()` 拆为 `create()` + `boot()`；将 `start_virtiofsd()` 移至 `CloudHypervisorHooks::pre_start()` |
| `vmm/sandbox/src/client.rs` | 改 | `new_sandbox_client()`+`client_check()`+`setup_sandbox()` 封装为 `VmmTaskRuntime::wait_ready()` |
| `vmm/sandbox/src/sandbox.rs` | 改（最大工作量） | `KuasarSandbox<V>` → `SandboxEngine<V: Vmm, R: GuestRuntime>`；分离 `SandboxPersistData` |

**验收：** `cargo check --all-features` 无错误；现有 E2E 测试全部通过（Standard Mode 行为不变）

#### M2：Appliance Mode 端到端实现

**Step 3：实现 ApplianceRuntime（新建文件，核心协议逻辑）**

```
vmm/sandbox/src/appliance_runtime.rs  ← 新建
  ├── GlobalVsockServer（tokio 异步任务，持有所有 ApplianceSession）
  ├── ApplianceSession { conn_fd, sandbox_id, tx, last_heartbeat }
  ├── ApplianceRuntime::wait_ready()  ← host 侧等待 READY，绑定 session
  ├── ApplianceRuntime::shutdown()    ← 写入 SHUTDOWN 到 session.tx
  └── HeartbeatTracker（在 wait_ready() 返回后启动计时）
```

关键依赖：tokio::net::UnixListener（AF_VSOCK）或 tokio-vsock crate；serde_json 解析 JSON Lines

**Step 4：实现 kuasar-init（新建 crate）**

```
vmm/init/src/main.rs  ← 新建，类比 vmm/task/src/main.rs
  ├── early_init()：mount /proc /sys /dev，解析 /proc/cmdline
  ├── run_init_d_hooks()：遍历 /etc/kuasar/init.d/，非零退出 → 发 FATAL + reboot
  ├── spawn_app()：fork+exec 应用进程（保留 kuasar-init 继续运行）
  ├── wait_ready_check()：端口轮询 or 文件就绪 or 立即
  ├── send_ready()：连接 host vsock（CID=2, port=1024），发送 READY JSON
  └── event_loop()：SIGCHLD 收割 + HEARTBEAT 定时 + 接收 SHUTDOWN/CONFIG
```

`Cargo.toml` (vmm/init): 依赖 `tokio-vsock`、`serde_json`、`nix`；静态链接 musl

**Step 5：实现 K8s Adapter 合成容器逻辑**

```
vmm/sandbox/src/k8s_adapter/synthetic_container.rs  ← 新建
  ├── SyntheticContainer { id, image_ref, resource_spec, state }
  ├── RunPodSandbox  → engine.create_sandbox() + engine.start_sandbox() + insert SyntheticContainer
  ├── CreateContainer → 仅存元数据，返回 "{sandbox_id}-app"
  ├── StartContainer  → noop，设置 state = RUNNING
  ├── ContainerStatus → 从 sandbox 状态映射；Pid = cloud-hypervisor 宿主机 PID
  └── 崩溃恢复       → 从 SandboxPersistData.containers 读取合成容器记录重建
```

**验收：** `crictl runp` + `crictl ps` 能正常返回；sandbox READY 后合成容器显示 Running

#### M3：可选扩展（不阻塞 M2 验收）

| 功能 | 实现位置 | 说明 |
|------|---------|------|
| vsock IO 协议（stdout/stderr 转发） | `vmm/init/src/io_forwarder.rs` | pipe2 + base64 + IO 消息 |
| Direct Adapter（AI Agent gRPC 入口） | `vmm/sandbox/src/direct_adapter/` | 自定义 protobuf + tonic server |
| VM 快照/恢复（StartMode::Restore） | `vmm/sandbox/src/cloud_hypervisor/snapshot.rs` | CH REST API snapshot/restore |
| Pause/Resume | `vmm/sandbox/src/cloud_hypervisor/mod.rs` | CH REST API pause/resume |
