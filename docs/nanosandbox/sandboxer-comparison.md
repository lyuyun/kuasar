# vmm-sandboxer 与 nanosandbox-sandboxer 架构对比

<!-- toc -->
- [架构图对比](#架构图对比)
  - [vmm-sandboxer：1:N 多容器共享 VM 架构](#vmm-sandboxer1n-多容器共享-vm-架构)
  - [nanosandbox-sandboxer：1:1 VM-per-Container 架构](#nanosandbox-sandboxer11-vm-per-container-架构)
  - [两图关键差异速览](#两图关键差异速览)
- [核心设计维度对比](#核心设计维度对比)
- [历史版本兼容与工作负载收编](#历史版本兼容与工作负载收编)
  - [API 层兼容性](#api-层兼容性)
  - [nanosandbox 自身版本演进兼容](#nanosandbox-自身版本演进兼容)
  - [与 vmm-sandboxer 同节点并存](#与-vmm-sandboxer-同节点并存)
  - [存量 vmm-sandboxer 工作负载收编路径](#存量-vmm-sandboxer-工作负载收编路径)
  - [兼容性测试策略](#兼容性测试策略)
- [VM 模型暴露方式：双 RuntimeClass vs 单 RuntimeClass](#vm-模型暴露方式双-runtimeclass-vs-单-runtimeclass)
  - [技术约束分析](#技术约束分析)
  - [用户体验分析](#用户体验分析)
  - [迁移路径视角](#迁移路径视角)
  - [推荐方案](#推荐方案)
- [单一二进制方案 vs 独立二进制方案](#单一二进制方案-vs-独立二进制方案)
  - [单一二进制：可行形态分析](#单一二进制可行形态分析)
  - [优劣对比](#优劣对比)
- [结论与建议](#结论与建议)
<!-- /toc -->

---

## 架构图对比

### vmm-sandboxer：1:N 多容器共享 VM 架构

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          kubelet  (CRI gRPC)                                  │
└────────────────────────────────────┬─────────────────────────────────────────┘
                                     │
┌────────────────────────────────────▼─────────────────────────────────────────┐
│                             containerd                                         │
│      CRI Plugin ──► Sandbox API ──► Task API ──► Image Service                │
└────────────────────────────────────┬─────────────────────────────────────────┘
                                     │ Unix socket (/run/kuasar/vmm.sock)
┌────────────────────────────────────▼─────────────────────────────────────────┐
│              vmm-sandboxer   KuasarSandboxer<VMFactory, Hooks>                │
│                                                                                │
│  ┌──────────────────────┐   ┌────────────────────────────────────────────┐   │
│  │   Sandboxer trait    │   │           KuasarSandbox<V: VM>             │   │
│  │  create / start /    │   │  vm: CloudHypervisorVM (or QEMU/Strato)    │   │
│  │  stop / delete /     │   │  containers: HashMap<id, KuasarContainer>  │   │
│  │  sandbox             │   │  network: Network (from netns)             │   │
│  └──────────────────────┘   │  storages: Vec<Storage>                   │   │
│                              │  client: SandboxServiceClient ──► in-VM  │   │
│  ┌──────────────────────┐   └──────────────────────┬─────────────────────┘   │
│  │  Container Handler   │                          │                          │
│  │  Chain               │   ┌──────────────────────▼─────────────────────┐   │
│  │  (per append_cont.)  │   │        VMFactory + Hooks                   │   │
│  │  - StorageHandler    │   │  pre_start: 网络配置、设备挂载             │   │
│  │  - MountHandler      │   │  post_start: 确认 vmm-task 就绪            │   │
│  │  - NsHandler         │   │  post_stop:  清理设备、释放网络            │   │
│  │  - IoHandler         │   └──────────────────────┬─────────────────────┘   │
│  │  - SpecHandler       │                          │ Hypervisor API           │
│  └──────────────────────┘                          │ (REST / QMP / StratoVirt)│
│                                                    │                          │
│  State: /run/kuasar/state/<sandbox_id>/            │                          │
│  Task 委托: vsock:1024 ──► in-VM vmm-task          │                          │
└────────────────────────────────────────────────────┼─────────────────────────┘
                                                     │
                              ┌──────────────────────▼──────────────────────────┐
                              │     Hypervisor Process (cloud-hypervisor / QEMU) │
                              │     ~20~50MB per Pod                             │
                              └──────────────────────┬──────────────────────────┘
                                                     │ KVM
┌────────────────────────────────────────────────────▼─────────────────────────┐
│  MicroVM（单 VM 承载 Pod 内所有容器）                                          │
│                                                                                │
│  ┌──────────────────────────────────────────────────────────────────────┐    │
│  │  vmm-task  (PID ~, 常驻 agent)                                        │    │
│  │  vsock:1024  ttrpc Task/Sandbox API server                            │    │
│  │  处理：CreateTask / StartTask / ExecTask / KillTask / Stats / ...     │    │
│  └──────┬──────────────────────┬──────────────────────┬─────────────────┘    │
│         │ fork+exec            │ fork+exec            │ fork+exec            │
│  ┌──────▼──────┐        ┌──────▼──────┐        ┌──────▼──────┐              │
│  │  Container1  │        │  Container2  │        │  Container3  │  ← 1:N     │
│  │  (业务进程)  │        │ (sidecar)    │        │ (init 容器) │              │
│  │  runc exec   │        │  runc exec   │        │  runc exec   │              │
│  └─────────────┘        └─────────────┘        └─────────────┘              │
│                                                                                │
│  virtiofs / 9p 共享目录: /run/kuasar/storage/containers/                     │
│  overlayfs: 每容器独立可写层                                                   │
└────────────────────────────────────────────────────────────────────────────────┘
```

---

### nanosandbox-sandboxer：1:1 VM-per-Container 架构

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          kubelet  (CRI gRPC)                                  │
└────────────────────────────────────┬─────────────────────────────────────────┘
                                     │
┌────────────────────────────────────▼─────────────────────────────────────────┐
│                             containerd                                         │
│      CRI Plugin ──► Sandbox API ──► Task API ──► Image Service                │
└────────────────────────────────────┬─────────────────────────────────────────┘
                                     │ Unix socket (/run/nanosandbox/sandboxer.sock)
┌────────────────────────────────────▼─────────────────────────────────────────┐
│          nanosandbox-sandboxer   NanoSandboxer<V: NanoVM>                     │
│                                                                                │
│  ┌──────────────────────┐   ┌────────────────────────────────────────────┐   │
│  │   Sandboxer trait    │   │              NanoSandbox<V: NanoVM>        │   │
│  │  create / start /    │   │  vm: Option<CloudHypervisorVM>             │   │
│  │  stop / delete /     │   │  boot_phase: BootPhase (内部，不暴露)      │   │
│  │  sandbox             │   │  containers: HashMap (CRI 模式 M6 使用)    │   │
│  └──────────────────────┘   │  state_dir: /run/nanosandbox/state/<id>/   │   │
│                              │  task_server: TaskServerHandle ──────┐    │   │
│  ┌──────────────────────┐   └───────────────────────────────────────┼────┘   │
│  │   Snapshot API       │                                            │        │
│  │  CreateSnapshot      │   ┌────────────────────────────────────── ▼──────┐ │
│  │  StartFromSnapshot   │   │   per-sandbox Task ttrpc server               │ │
│  │  ListSnapshots       │   │   /run/nanosandbox/state/<id>/task.sock       │ │
│  │  PruneSnapshots      │   │   KillTask / WaitTask / Stats / ExecTask ...  │ │
│  └──────────────────────┘   │   （M1：接口注册，handler 全部返回             │ │
│                              │    UNIMPLEMENTED；M3 实现完整逻辑）           │ │
│                              └───────────────────────────────────────────────┘ │
│  ┌──────────────────────┐   ┌──────────────────────────────────────────────┐  │
│  │  CID Registry        │   │           ConcurrencyGuard                   │  │
│  │  全局 vsock CID 分配  │   │  Semaphore(max_concurrent_vm_ops)            │  │
│  │  防冲突              │   │  超出立即返回 RESOURCE_EXHAUSTED             │  │
│  └──────────────────────┘   └──────────────────────────────────────────────┘  │
│                                                                                │
│  State: /run/nanosandbox/state/<sandbox_id>/meta.json                         │
│  Task 处理: Manager 进程内直接执行，通过 vsock 控制通道与 VM 通信             │
└────────────────────────────┬────────────────────────────┬─────────────────────┘
              vsock 控制通道 │                            │ Hypervisor API (REST)
          (ProcessSpec 注入) │                            │
   ┌────────────────────────-▼──┐    ┌───────────────────▼──────────┐
   │  MicroVM-1  (Sandbox-A)    │    │  Hypervisor Process (CH)     │
   │                            │    │  ~5MB per Pod（无 shim）      │
   │  ┌──────────────────────┐  │    └──────────────────────────────┘
   │  │   /init (Zero-Init)  │  │
   │  │   ↓ execve (原生模式) │  │         同结构 × N 个 VM
   │  │   Container Process  │  │    ┌────────────────────────────┐
   │  │   PID 1              │  │    │  MicroVM-2  (Sandbox-B)    │
   │  └──────────────────────┘  │    │  Container Process PID 1   │
   │                            │    └────────────────────────────┘
   │  virtio-blk: erofs rootfs  │
   │  virtio-vsock: 控制通道    │    ┌────────────────────────────┐
   │  virtio-net: tap 直连      │    │  Standby Pool VM (预热中)  │
   │  virtio-rng: 熵源          │    │  等待 ProcessSpec 注入     │
   └────────────────────────────┘    └────────────────────────────┘

   快照存储:
   /var/lib/nanosandbox/snapshots/<snapshot_id>/
     ├── metadata.json   (含 hypervisor_version, cpu_arch)
     ├── cpu.bin
     ├── mem.raw         (稀疏文件, mmap 友好)
     ├── mem.bitmap
     ├── devices.json
     └── fs.diff
```

---

### 两图关键差异速览

```
                    vmm-sandboxer              nanosandbox-sandboxer
                    ─────────────              ─────────────────────

  containerd
      │                   │                              │
  Sandbox API        相同接口                       相同接口
      │                   │                              │
  sandboxer          KuasarSandboxer              NanoSandboxer
  进程内                   │                              │
                    KuasarSandbox                 NanoSandbox
                    (1:N 容器)                    (1:1 容器)
                           │                              │
                    VMFactory                     NanoVM trait
                    + Hooks                       (无 Hooks)
                           │                              │
                    in-VM Task ttrpc          Manager内嵌Task ttrpc
                    (vmm-task, port 1024)     (per-sandbox socket)
                           │                              │
                    VM内 vmm-task              vsock 控制通道
                    fork+exec 每个容器         /init execve PID 1
                           │                              │
                    无 Snapshot API            Snapshot API
                    无 Standby Pool            Standby Pool（M2）/ userfaultfd（M5b）
```

---

## 核心设计维度对比

| 维度 | vmm-sandboxer | nanosandbox-sandboxer |
|------|:------------:|:--------------------:|
| **VM:容器比** | 1:N（Pod 内多容器共享一个 VM） | 1:1（每容器独占一个 VM） |
| **Task API 位置** | in-VM `vmm-task` 进程（vsock:1024） | Manager 进程内嵌（per-sandbox socket） |
| **in-VM 常驻 agent** | vmm-task（~15MB Guest 内存） | **无**（Zero-Init exec 后无 agent） |
| **容器创建路径** | vmm-task → runc → container process | `/init` → `execve` → container process (PID 1) |
| **网络配置来源** | 从 netns 读取（rtnetlink） | 由 CNI 预配后以参数传入（tap_name/mac/ip） |
| **存储模型** | virtiofs/9p 共享目录 + overlayfs | virtio-blk erofs 只读镜像（mmap direct） |
| **快照支持** | 无 | Snapshot API（全量/增量/userfaultfd） |
| **冷启动优化** | 无 Standby Pool | Standby Pool（M2）/ Pre-zeroed Page Pool（M5a） |
| **多容器 Pod** | **原生支持**（设计目标） | 受限（1:1 模型限制，M6 后可通过多 VM 网络共享） |
| **宿主机进程数/Pod** | Hypervisor = 1（纯 Sandboxer 模式）；shim 仅在兼容官方 containerd 非 kuasar-io fork 时使用，非常态 | Hypervisor = 1 |
| **Guest 内存开销** | 裸内核 ~20MB + vmm-task ~15MB | 裸内核 ~8MB（更精简） |
| **Hypervisor 进程开销** | ~20~50MB（QEMU 较大）| ~5MB（CH 轻量模式） |
| **适用场景** | 通用 K8s 工作负载、Sidecar、有状态服务 | Serverless Function、AI Agent、无状态短任务、代码执行沙箱、高隔离多租户场景 |
| **核心 trait** | `VM` + `VMFactory` + `Hooks` | `NanoVM`（无 Hooks，无 attach/hot_attach） |
| **状态目录** | `/run/kuasar/state/` | `/run/nanosandbox/state/` |
| **RuntimeClass handler** | `kuasar-vmm` / `kuasar-qemu` 等（每模型独立 Class；`overhead` ~35MB/Pod） | 多模型共存期：`nanosandbox`（独立 Class，`overhead` ~8MB/Pod）；全量收编终态：接管 `kuasar-vmm` handler（见下节分析） |
| **自身版本兼容** | 协议稳定，无版本迁移机制；vmm-task API 版本由 containerd-shim 协议锁定 | `meta.json` 必含 `schema_version`；Manager 启动时自动迁移旧版状态文件或拒绝启动 |
| **存量 vmm Pod 收编** | 成熟稳定，无需收编 | 目标场景全为单容器 Pod，**全量可收编**；滚动替换后下线 vmm-sandboxer，不保留双 sandboxer 常态并存 |
| **同节点并存** | 不感知 nanosandbox | **不支持**（同 handler 不能双注册）；收编采用节点级 cutover：cordon → 排空 vmm Pod → 停 vmm-sandboxer → 接管同名 handler → 启 nanosandbox → uncordon；存量 Pod spec 无需修改 |

---

## 历史版本兼容与工作负载收编

本节从四个层面说明 nanosandbox-sandboxer 如何兼容历史版本，以及如何渐进收编 vmm-sandboxer 存量工作负载。

### API 层兼容性

nanosandbox-sandboxer 与 vmm-sandboxer 实现**相同的外部 API 契约**，因此在 containerd/kubelet 视角下两者等价：

```
外部契约（只读，不可更改）
├── Sandboxer trait : create / start / stop / delete / update / sandbox
├── Sandbox trait   : status / append_container / remove_container /
│                     exit_signal / get_data
└── Task API (task.v2) : ttrpc server，与 containerd 协议版本对齐

前向兼容目标：存量 Pod 的 runtimeClassName 不变，nanosandbox-sandboxer 接管相同 handler

  迁移前（vmm-sandboxer 运行中）：
    "kuasar-vmm"  → /run/kuasar/vmm.sock            (vmm-sandboxer)

  收编完成（nanosandbox-sandboxer 接管）：
    "kuasar-vmm"  → /run/nanosandbox/sandboxer.sock  (nanosandbox-sandboxer)
```

同一 RuntimeClass handler 在 containerd 中只能映射到一个 socket，**两个 sandboxer 无法同时注册相同的 handler**。收编以节点为单位采用 cutover 方式：先将节点上 vmm Pod 排空、停止 vmm-sandboxer，再启动 nanosandbox-sandboxer 并接管同名 handler；存量 Pod spec 的 `runtimeClassName` 无需修改。

---

### nanosandbox 自身版本演进兼容

nanosandbox 将历经 M1→M8 多轮迭代，状态格式和 proto 字段可能随里程碑演变，需保证跨版本的前向兼容：

| 机制 | 实现 | 说明 |
|------|------|------|
| **meta.json schema_version** | 每个文件含 `schema_version: u32`，当前值 = 1 | Manager 启动时读取版本：`< current` 执行自动迁移；`> current` 拒绝启动并报错 |
| **状态目录布局稳定** | `/run/nanosandbox/state/<id>/` 路径规范在 M1 固定 | 新增字段以可选形式追加，不移除旧字段 |
| **proto3 零值默认** | task.v2 / snapshot.v1 使用 proto3 | 新增字段在旧客户端读取时为零值，不破坏解析；废弃字段保留但标注 `reserved` |
| **NanoVM trait 快照 stub** | M1 的 `create_snapshot` / `restore_from_snapshot` 返回 `UNIMPLEMENTED` | M4 实现后接口签名不变，调用方无需适配 |
| **滚动升级** | meta.json 在 sandboxer 重启间持久化 | 升级 nanosandbox-sandboxer 二进制后，重启进程即可恢复存量 VM 管理权（M1 验收目标） |

---

### 与 vmm-sandboxer 同节点并存

**前向兼容采用节点级 cutover，不存在"同节点并存"阶段。** 由于同一 RuntimeClass handler 只能映射到一个 socket，nanosandbox-sandboxer 接管 vmm-sandboxer 的 handler 名时，两者不能同时在同一 containerd 实例下注册相同的 handler。

cutover 期间节点已 cordon（无新 Pod 调度），已运行的 vmm Pod 自然退出后立即停止 vmm-sandboxer，再启动 nanosandbox-sandboxer，两者不存在运行重叠窗口。

若需在**同节点以不同 handler 名**进行灰度验证（非生产收编路径），资源隔离如下：

```
灰度验证场景（不同 handler 名，不用于生产收编）：

  资源          vmm-sandboxer          nanosandbox-sandboxer（测试 handler）  冲突风险
  ─────────────────────────────────────────────────────────────────────────────────
  Unix socket   /run/kuasar/vmm.sock   /run/nanosandbox/sandboxer.sock        无
  State dir     /run/kuasar/state/     /run/nanosandbox/state/                无
  vsock CID     各自独立注册表          各自独立注册表                         无
                （默认 3–1023）         （建议 1024–65535；通过各自配置文件
                                        vsock_cid_min/vsock_cid_max 指定）
  Hypervisor    独立进程               独立进程                               共享宿主机 KVM，内核隔离
  systemd unit  kuasar-vmm.service     nanosandbox.service                    无
```

注意：灰度验证结束后仍需按 cutover 流程完成生产收编，不能以此替代。

---

### 存量 vmm-sandboxer 工作负载收编路径

**目标场景全为单容器 Pod，所有存量工作负载均可收编，最终节点只保留 nanosandbox-sandboxer。**

收编以**节点为单位**逐批推进，每节点分三个阶段：

```
阶段 1：准备（不影响节点现有工作负载）
  ├── 在节点上安装 nanosandbox-sandboxer 二进制（尚未启动，不注册 handler）
  ├── 配置 nanosandbox（内核路径、资源限额、state 目录等）
  └── 可选：以临时 handler 名（如 "nanosandbox-smoke"）启动 nanosandbox 进行冒烟测试，
           测试完毕后停止，避免占用生产 handler

阶段 2：节点级 cutover（每节点独立执行，建议每批 10%~20% 节点）
  ├── kubectl cordon <node>：停止新 Pod 调度至该节点
  ├── 等待（或主动缩容）该节点上所有 vmm-sandboxer 管理的 Pod 退出
  ├── 确认 vmm Pod 数量为 0（kubectl get pods --field-selector spec.nodeName=<node>）
  ├── systemctl stop kuasar-vmm && systemctl disable kuasar-vmm
  ├── 更新 containerd 配置：将 "kuasar-vmm" handler socket 路径改为
  │     /run/nanosandbox/sandboxer.sock
  ├── systemctl start nanosandbox && systemctl reload containerd
  ├── 验证 nanosandbox 健康（创建测试 Pod，确认启动延迟达标）
  ├── kubectl uncordon <node>：新 Pod 经由 "kuasar-vmm" handler 由 nanosandbox 创建
  └── 每批完成后观察错误率和延迟，再推进下一批

阶段 3：全集群收编确认（终态）
  ├── 所有节点 cutover 完成，RuntimeClass "kuasar-vmm" 定义保持不变（Pod spec 无需修改）
  └── 从节点卸载 vmm-sandboxer 二进制（可选，包管理器统一清理）
```

**终态：节点上只有 nanosandbox-sandboxer 一个 sandboxer 进程，一个 systemd unit，一套配置文件；存量 Pod spec 无需任何修改。**

收编不支持跨 sandboxer 热迁移（运行中 Pod 无法从 vmm 搬移至 nanosandbox），原因如下：
- vmm-sandboxer 的 VM 内存布局、vsock 地址、meta 格式与 nanosandbox 完全不同
- nanosandbox 的 `StartFromSnapshot` 只接受自身格式的快照，无法导入 vmm 的 VM 状态
- containerd 不提供跨 sandboxer 的 Pod 迁移原语

因此收编须等待旧 Pod 自然退出（或主动缩容），无需强制驱逐。

---

### 兼容性测试策略

| 测试场景 | 方法 | 执行频率 |
|---------|------|---------|
| **升级兼容测试** | 用旧版 nanosandbox 创建 sandbox → 写入 meta.json → 升级二进制重启 → 验证存量 VM 重新被纳管 | 每个 Release |
| **schema_version 迁移测试** | 手动构造低版本 meta.json → 启动新版 Manager → 验证自动迁移成功且数据完整 | 每次 schema 变更 |
| **灰度并存验证测试** | vmm-sandboxer（handler="kuasar-vmm"）+ nanosandbox-sandboxer（handler="nanosandbox-smoke"，**不同 handler 名**）同时启动 → 各创建 Pod → 验证互不干扰 → 验证 vsock CID 不冲突 | 每日 CI（仅覆盖不同 handler 名并存场景） |
| **全量收编 + 下线测试** | 用 vmm 启动 N 个 Pod → 滚动替换为 nanosandbox → 验证所有 Pod 健康 → 停止并卸载 vmm-sandboxer → 验证节点运行正常 | 每个 Release |

---

## VM 模型暴露方式：双 RuntimeClass vs 单 RuntimeClass

### 问题背景

1:N 共享 VM（vmm-sandboxer）和 1:1 独占 VM（nanosandbox-sandboxer）是两种不同的 VM 隔离模型。用户通过 `runtimeClassName` 声明所需运行时。核心问题是：

```
方案 A（双 RuntimeClass）：每种模型保有独立标识符
  runtimeClassName: kuasar-vmm   # 1:N 共享 VM，适合多容器 Pod
  runtimeClassName: nanosandbox  # 1:1 独占 VM，适合单容器 Pod

方案 B（单 RuntimeClass）：两种模型合并，由某种机制分发
  runtimeClassName: kuasar       # 自动/透明选择底层模型
```

### 技术约束分析

#### 路由机制约束

containerd 按 handler 名称查找 sandboxer socket，此路由选择**早于**解析 Pod spec 内容（容器数量、注解等）。因此"单 RuntimeClass 内自动分发"的所有变体均面临结构性障碍：

| 统一分发变体 | 实现路径 | 问题 |
|------------|---------|------|
| 按容器数量自动路由 | sandboxer 进程内检查 Pod 容器列表后分发 | 需在 sandboxer 内实现多路复用 dispatcher，本质上将「二进制架构统一」问题前移到 sandboxer 层 |
| 按注解/标签路由 | sandboxer 读取 Kubernetes 对象元数据决策 | sandboxer 不应依赖 k8s API server；containerd 侧不可见 Pod 注解 |
| 独立 router sandboxer | 新建转发进程，内部代理至 vmm 或 nano | 引入额外故障域；路径延迟增加；router 自身成为单点 |

#### 调度精度约束（RuntimeClass Overhead）

`RuntimeClass.overhead.podFixed` 告知调度器每个 Pod 的额外资源消耗，直接影响 bin-packing 和 OOM 风险：

| 模型 | 典型额外内存/Pod | 构成 |
|------|---------------|------|
| vmm-sandboxer（1:N） | ~35 MB | 裸内核 ~20MB + vmm-task ~15MB |
| nanosandbox（1:1） | ~8 MB | 精简内核 ~8MB，无 agent |

两者相差 **~4.4×**。单一 RuntimeClass 的 overhead 只能取一个值：

- 取大值（~35MB）→ nanosandbox Pod 被高估，节点利用率下降
- 取小值（~8MB）→ vmm Pod 被低估，节点可能因内存超分触发 OOM
- 取均值（~22MB）→ 两者都不准，误差最大

#### 节点能力约束（RuntimeClass Scheduling）

| 能力 | vmm-sandboxer | nanosandbox |
|------|--------------|-------------|
| Hypervisor 支持 | Cloud Hypervisor / QEMU / StratoVirt | Cloud Hypervisor 仅 |
| 宿主机内核版本 | ≥ 5.10 | ≥ 5.14（`MADV_POPULATE_WRITE`，M5a） |
| 节点特殊特性 | KVM | KVM + userfaultfd + HugePage（M5a+） |

两个模型对应不同节点能力集合，需通过 `RuntimeClass.scheduling.nodeSelector` 分别引导。**单一 RuntimeClass 无法同时为两个模型指定不同的节点亲和性**，导致 Pod 可能落到能力不匹配的节点。

---

### 用户体验分析

| 维度 | 双 RuntimeClass | 单 RuntimeClass |
|------|:--------------:|:--------------:|
| **认知负担** | 需了解两个 class 含义，选择有摩擦 | 一个名称，零选择成本 |
| **意图表达** | Pod spec 显式声明隔离模型，可读即文档 | 底层模型对用户透明，需额外约定传达意图 |
| **多容器 Pod** | `kuasar-vmm` 语义自然，精确匹配 1:N 需求 | 需分发逻辑感知容器数量，规则对用户不透明 |
| **单容器 Pod** | `nanosandbox` 精确匹配 1:1 模型 | 自动路由正确，但用户看不到实际走了哪条路径 |
| **误用检测** | 多容器 Pod 用 `nanosandbox` → 明确报错，错误可见 | 自动分发可能静默路由，语义错误被隐藏 |
| **可观测性** | RuntimeClass 名称直接出现在事件/日志/metrics 中 | 需额外标签区分实际执行路径，调试成本更高 |
| **变更审计** | 切换模型须显式修改 Pod spec，操作有意识且可追溯 | 模型选择隐式，版本历史中不可见 |
| **节点故障定位** | 按 RuntimeClass 过滤，范围精确 | 调试时无法快速判断"该 Pod 实际用了哪个模型" |

---

### 迁移路径视角

文档「历史版本兼容」章节选择 nanosandbox 接管 vmm-sandboxer 的同名 handler（零 Pod spec 变更）。该策略有一个明确前提：**迁移目标是全量替换 1:N 模型，不保留共存**。在此前提下 RuntimeClass 名称只是历史包袱，终态是"一个名称、一个模型"。

若集群中存在必须保留 1:N 模型的多容器工作负载，两个方案的影响不同：

```
场景：集群长期共存单容器（nano）和多容器（vmm）Pod

  双 RuntimeClass（推荐）：
    runtimeClassName: nanosandbox  → nanosandbox-sandboxer（1:1，overhead 8MB）
    runtimeClassName: kuasar-vmm   → vmm-sandboxer（1:N，overhead 35MB）
    各自精确 overhead；独立节点亲和；用户意图显式；模型间互不干扰

  单 RuntimeClass（统一 "kuasar"）：
    overhead 精度丢失（4.4× 差异无法在单值中体现）
    节点亲和性无法同时满足两个模型的能力需求
    分发逻辑引入额外故障域；误用被静默掩盖
```

---

### 推荐方案

**多模型共存期：双 RuntimeClass（当前方向正确）**

三条硬约束使双 RuntimeClass 在多模型共存阶段成为唯一合理选择：

1. **Overhead 精度**：两模型内存开销相差 4.4×，单值无法同时准确反映，调度器精度下降直接影响集群利用率和 OOM 风险
2. **节点亲和性**：两模型节点能力需求不相交，需独立 `scheduling` 约束保证 Pod 落到正确节点池
3. **误用可见性**：用 1:1 模型运行多容器 Pod 是语义错误，应在 Pod spec 层面报错，而非被静默路由

**全量收编终态：自然收敛为单 RuntimeClass**

当 vmm-sandboxer 完全下线、仅剩 nanosandbox-sandboxer 时，集群内自然只有一个 RuntimeClass（如 `kuasar-vmm`，指向 nanosandbox-sandboxer），「双 vs 单」问题自动消解。此时的单 RuntimeClass 不是主动设计决策，而是全量收编完成的自然结果，需同步更新其 `overhead`（~8MB）和 `scheduling` 约束（nano 节点能力集）。

```
多模型共存阶段（当前 → vmm 完全下线前）
  双 RuntimeClass
  ├── kuasar-vmm  → vmm-sandboxer，overhead ~35MB，节点亲和：vmm 节点池
  └── nanosandbox → nanosandbox-sandboxer，overhead ~8MB，节点亲和：nano 节点池
        ↓ 全量收编完成（vmm-sandboxer 下线）
全量收编终态
  单 RuntimeClass
  └── kuasar-vmm  → nanosandbox-sandboxer，overhead 更新为 ~8MB，scheduling 更新为 nano 节点集
```

---

## 单一二进制方案 vs 独立二进制方案

### 单一二进制：可行形态分析

"单一二进制"并非一个单一方案，有以下三种实现形态，成本和代价各不相同：

#### 形态 A：`--mode vmm|nano` 运行时分发

```
kuasar-sandboxer --mode vmm  --listen /run/kuasar/vmm.sock
kuasar-sandboxer --mode nano --listen /run/nanosandbox/sandboxer.sock

二进制内部：
  main() {
    match args.mode {
      Vmm  => run_vmm_sandboxer(config).await,
      Nano => run_nano_sandboxer(config).await,
    }
  }
```

同一个 Rust binary，包含两套完整代码路径。

#### 形态 B：Cargo feature 编译时选择

```toml
[features]
vmm  = ["dep:qapi", "dep:vmm-task-client", ...]
nano = ["dep:nanosandbox-proto", "dep:snapshot-store", ...]
```

```toml
# Cargo.toml 中为每个 feature 声明独立的 [[bin]] 条目
[[bin]]
name = "kuasar-sandboxer-vmm"
required-features = ["vmm"]

[[bin]]
name = "kuasar-sandboxer-nano"
required-features = ["nano"]
```

```
# 编译 vmm 版（--bin 指定目标，--features 激活条件编译）
cargo build --bin kuasar-sandboxer-vmm  --features vmm
# 编译 nano 版
cargo build --bin kuasar-sandboxer-nano --features nano
```

同一个 crate，不同 feature 激活不同依赖和代码路径，产出不同名称的 binary，代码仍在一处管理。

#### 形态 C：顶层 umbrella crate 依赖两个库 crate

```
kuasar/
├── sandboxer/        ← 新顶层 binary crate
│   ├── Cargo.toml    depends on vmm-sandbox-lib + nanosandbox-sandbox-lib
│   └── src/main.rs   分发逻辑
├── vmm/sandbox/      ← 改为 lib crate（现在已有 lib.rs）
└── nanosandbox/sandbox/ ← lib crate
```

---

### 优劣对比

#### 单一二进制

**优势**

| 优势 | 说明 |
|------|------|
| **部署简化** | 节点上只需分发和更新一个二进制文件，减少包管理和版本对齐复杂度 |
| **共享运行时资源** | tokio worker 线程池、fd limit、内存页缓存可在两种模式间共享，减少整体资源消耗 |
| **统一监控端点** | 一个进程，一套 metrics/health-check 路径，运维工具无需区分两个 sandboxer |
| **统一配置格式** | 一份配置文件同时管理 vmm 和 nano 的 Hypervisor 参数（共享字段如 kernel_path） |
| **跨模式特性复用** | 未来如 vmm 也需要 Standby Pool，可直接复用 nano 的实现而不用跨 repo 同步 |

**劣势**

| 劣势 | 说明 |
|------|------|
| **架构耦合** | `KuasarSandboxer` 的 1:N Handler Chain 与 `NanoSandboxer` 的 1:1 Task-per-process 在同一 crate 内会相互拉拽；任何重构都要兼顾两套逻辑 |
| **依赖集合膨胀** | vmm 需要 `qapi`（QEMU QMP）、`vmm-task` client；nano 需要 `snapshot-store`、`userfaultfd`。合并后所有节点都要带全部依赖，二进制体积可能增大 30%~50% |
| **故障域扩大** | nano 模式的 userfaultfd bug 或 snapshot 磁盘写满可能影响同进程的 vmm 模式 VM；两种业务类型故障会相互感染 |
| **版本耦合** | vmm 路径的修复必须与 nano 一起发版，无法单独热修复某一模式 |
| **测试矩阵膨胀** | CI 需测试所有模式组合（vmm × QEMU/CH/Strato + nano × CH），单 PR 测试时间显著增加 |
| **安全配置冲突** | nano 理想情况下以非 root + 更严格 seccomp 运行；vmm 可能需要更多 capability。统一进程难以应用最小权限原则 |

---

#### 独立二进制

**优势**

| 优势 | 说明 |
|------|------|
| **架构清晰** | 每个二进制只有一套核心抽象（VM trait 或 NanoVM trait），代码路径线性可读，新人上手快 |
| **独立发版** | nano M2 完成可立即发布，不等待 vmm 的修复；vmm 的 hotfix 不影响 nano 的发版节奏 |
| **最小依赖** | vmm binary 无 snapshot 相关依赖；nano binary 无 qapi/virtiofs 相关依赖 |
| **故障隔离** | nano sandboxer 崩溃不影响节点上已运行的 vmm sandboxer 管理的 Pod |
| **差异化安全策略** | nano 可用独立 seccomp profile + 非 root 用户；vmm 保持现有配置；互不干扰 |
| **独立 CI 流水线** | nano 的 KVM E2E（userfaultfd、snapshot 测试）在独立 CI job 中运行，不拖慢 vmm PR |
| **`vmm-common` 已解决代码复用** | trace/signal/panic 已在共享库中，独立二进制不等于代码重复 |

**劣势**

| 劣势 | 说明 |
|------|------|
| **两套部署产物** | 节点初始化脚本需管理两个二进制、两个 systemd unit、两套 RuntimeClass |
| **配置文件分离** | vmm 和 nano 的公共配置（如 kernel_path）需在两个文件中分别维护 |
| **两个监控目标** | 需要分别设置 prometheus scrape、日志采集 agent、告警规则 |
| **跨 crate 功能同步** | 如两者同时需要一个新的网络工具函数，需同步更新 vmm-common 并两侧升级依赖 |

---

## 结论与建议

**双 RuntimeClass + 独立二进制（当前 spec 方向正确）**

两个相互独立的设计维度各自有明确结论：

```
维度 1：VM 模型如何暴露给用户（RuntimeClass 设计）

  两个模型的 overhead 是否相近（<2×）？
  └── 否（~35MB vs ~8MB，相差 4.4×）
       └── 节点能力集是否完全重合？
            └── 否（vmm 支持 QEMU/Strato；nano 需 userfaultfd/HugePage）
                 └── 双 RuntimeClass ✓（全量收编终态自然收敛为单 Class）

维度 2：sandboxer 实现如何组织（二进制架构）

  两种 sandboxer 是否共享核心抽象（VM trait 实现）？
  └── 否（1:N vs 1:1，in-VM task vs 内嵌 task）
       └── 是否存在严重的运维复杂度问题？
            └── 否（vmm-common 已解决代码复用；两个 systemd unit 可接受）
                 └── 独立二进制 ✓
```

**减轻独立二进制劣势的具体措施：**

| 劣势 | 缓解措施 |
|------|---------|
| 配置文件分离 | 定义共享 `[common]` 配置段（kernel_path、log_level 等），两个 sandboxer 的 config 文件 include 同一个公共配置片段 |
| 两套部署产物 | 打进同一个 RPM/DEB 包，包内含两个 binary；Makefile `make install` 同时安装 |
| 两个监控目标 | 两个 sandboxer 使用相同的 metrics 格式和 label 命名规范，上层 Grafana Dashboard 通过 `job="kuasar-vmm"` / `job="kuasar-nano"` 过滤，无需维护两套 Dashboard |

此外，nano 可独立应用更严格的 seccomp profile 并以非 root 用户运行；vmm 则需保留更多 capability 以管理网络设备和 KVM。独立二进制使两者各自应用最小权限原则，这在统一进程模型下难以实现，进一步支持独立二进制的选择。

**何时重新评估：** 若未来两种 sandboxer 在同一节点上部署的比例超过 80%，且运维成本成为瓶颈，可考虑形态 B（Cargo feature 编译时选择），既保持代码同仓库管理，又允许按需裁剪二进制。但在 NanoSandbox 达到 M4 快照子系统成熟之前，不应引入这层复杂性。

---

*文档版本：v1.5 | 日期：2026-04-15 | v1.1 新增历史版本兼容章节；v1.2 明确全量收编策略；v1.3 修正 API 层兼容性描述；v1.4 处理 review 全部 10 条意见；v1.5 新增「VM 模型暴露方式：双 RuntimeClass vs 单 RuntimeClass」章节（技术约束：路由机制/overhead 精度/节点亲和；用户体验对比；迁移视角；结论：多模型共存期双 Class，全量收编后自然收敛单 Class）*
