# NanoSandbox 设计文档检视意见

文档版本：v0.1 | 检视日期：2026-04-14 | **修复状态：全部已修复（nanosandbox.md v0.2）**

---

## 总体评价

NanoSandbox 的设计思路清晰，围绕"极速冷启动 + 快照瞬时恢复 + 极简架构"三个支柱展开，对 Serverless 场景的痛点分析准确。API 设计与 containerd 风格一致、分层合理。以下按模块列出具体问题，分"设计缺陷/矛盾"、"技术细节不足"、"开放性问题"三类。

> **修复说明**：以下所有问题已在 nanosandbox.md v0.2（2026-04-14）中修复，各条目标注了对应修复方式。

---

## 一、设计缺陷与逻辑矛盾

### 1.1 initrd 与 virtio-blk 描述矛盾 ✅ 已修复

**位置**：§ 一、极速冷启动 > 根文件系统优化

> "内核与 rootfs 合并打包为单一 **initrd**（压缩后 < 8MB），通过 virtio-blk 直接加载"

initrd 是通过内核引导参数 `initrd=` 由 Hypervisor 在启动 VM 时传入内核的，而 virtio-blk 是在内核启动后才可见的块设备。两者不能混用——如果通过 virtio-blk 挂载 rootfs，就不叫 initrd，而是一个独立的磁盘镜像。建议明确区分：

- 方案 A：使用 `initrd`（内核 + initrd 一起传入，Hypervisor 直接指定，不经 virtio-blk）
- 方案 B：使用 virtio-blk 挂载单独 rootfs 镜像（需要 bootloader 或 direct kernel boot，Firecracker/Cloud Hypervisor 均支持）

**修复**：选用方案 B，描述改为"erofs 镜像通过 virtio-blk + direct kernel boot 挂载，无需独立 bootloader"，摘要与 Zero-Init 章节同步更新，并增加注释说明 initrd 与 virtio-blk 互斥。

### 1.2 API 设计原则"异步优先"与 proto 定义矛盾 ✅ 已修复

**位置**：§ API 设计 > 设计原则 / Sandbox API proto

> 设计原则第 3 条："**异步优先**：长时操作（Create、Start、Stop）均返回操作 ID，支持轮询或事件订阅方式获取结果。"

但 proto 定义中，所有 RPC 均为同步调用，直接返回最终结果（如 `StartSandboxResponse` 返回 `started_at`，而非 operation_id）。proto 与设计原则存在根本矛盾，需要二选一并统一。

**修复**：设计原则第 3 条改为"同步为主，阻塞直到完成"，与 proto 行为一致；说明唯一可配置为异步的操作是 `CreateSnapshot`，通过 `GetSnapshotStatus` 轮询。

### 1.3 SandboxSpec.snapshot_id 与 StartFromSnapshot 功能重叠 ✅ 已修复

**位置**：§ Sandbox API > SandboxSpec / § Snapshot API > StartFromSnapshot

`SandboxSpec.snapshot_id` 字段非空时 `StartSandbox` 走快照恢复路径，这与 Snapshot API 的 `StartFromSnapshot` RPC 功能完全重叠，产生两条语义相同的路径，职责不清晰：

- 调用方应使用哪条路径？
- 两条路径在行为上是否完全等价？若有差异，应明确说明区别。

建议将快照启动统一收敛到 Snapshot API，Sandbox API 仅负责冷启动路径，避免混用。

**修复**：从 `SandboxSpec` 中删除 `snapshot_id` 字段（原 field 7，后续字段重编号）；`StartSandbox` 注释明确"仅冷启动路径"；`StartFromSnapshot` 注释明确"快照启动唯一入口，不经 Sandbox API"。

### 1.4 内核命令行 `rw` 与只读挂载矛盾 ✅ 已修复

**位置**：§ 设计细节 > MicroVM 内核裁剪规范 > 内核命令行参数

内核命令行中列有 `rw` 参数（以读写模式挂载 rootfs），但文档多处强调 rootfs 的只读镜像层通过 virtio-blk 以**只读方式**挂载。`rw` 与只读 virtio-blk 后端冲突，应改为 `ro`，可写性由上层 overlay 的可写层提供。

**修复**：内核命令行参数改为 `ro`，并补充注释说明可写性由 overlay upper 层提供。

### 1.5 KSM 与安全容器定位矛盾 ✅ 已修复

**位置**：§ 设计细节 > 内存管理优化

文档强调 NanoSandbox 的核心安全属性之一是"每个容器拥有独立 Guest Kernel"以防止侧信道攻击，但随即推荐启用 KSM（Kernel Same-page Merging）。KSM 是已知的侧信道攻击向量——攻击者可通过内存合并/解除合并的时序变化推断其他 VM 的内存内容（已有学术论文证明）。在主打安全隔离的场景下启用 KSM 存在自相矛盾的问题，至少应在文档中明确标注安全权衡。

**修复**：KSM 段落后增加安全权衡说明，明确多租户场景和密码学操作场景须禁用 KSM，仅适合同一租户同一函数多实例的密度优化场景。

### 1.6 内存气球预分配与设备模型最小化矛盾 ✅ 已修复

**位置**：§ 设计细节 > 内存管理优化 / § 设计细节 > 设备模型最小化

文档在"内存管理优化"中提到"宿主机侧气球（balloon）预分配"，但在"设备模型最小化"的四种 Hypervisor 推荐配置中，均未列出 `virtio-balloon` 设备。气球预分配依赖 `virtio-balloon` 驱动，两处描述存在遗漏。

**修复**："宿主机侧气球预分配"更名为"内存页预初始化（Pre-zeroed Page Pool）"，明确这是纯宿主机侧 `madvise`/`hugetlbfs` 机制，与 `virtio-balloon` 客户机驱动无关，VM 内无需安装气球驱动，设备模型表无需修改。

---

## 二、技术细节不足

### 2.1 关键性能数据缺乏测试基准 ✅ 已修复

**位置**：全文多处（§ 一、极速冷启动、§ 冷启动与快照启动性能对比、§ 资源效率分析等）

文档给出大量精确数字（如"Hypervisor 进程就绪 ~10ms"、"内核引导完成 ~20ms"、"NanoSandbox 运行时开销 ~13MB"、"KSM 可节省 40%~60% 内存"等），但未说明：

- 测试硬件平台（CPU 型号、内存规格、存储类型）
- 所用 Hypervisor 及版本（Firecracker? Cloud Hypervisor?）
- 内核版本及裁剪程度
- 测试方法和样本量

这些数据目前是估算值还是实测值？如为估算，建议明确标注为"目标值"；如为实测，需提供测试环境说明。

**修复**：性能对比表下方增加"数据说明"注释，标注所有数值为目标值，并给出参考测试环境（Firecracker 1.x、Linux 5.15 裁剪内核、erofs rootfs、256MB MicroVM、NVMe SSD）。

### 2.2 VM Template 的 Clone 机制未说明实现原理 ✅ 已修复

**位置**：§ 一、极速冷启动 > Hypervisor 进程启动阶段

> "当新请求到来时，从池中 **clone** 一个实例，注入容器配置后立即启动容器进程"

"clone" 是核心机制，但文档未说明具体实现方式：

- 是对 Hypervisor 进程做 `fork()` 系统调用？（多线程进程 fork 有严重的安全和一致性风险）
- 还是通过对 standby VM 做快照然后恢复？（此时应纳入快照章节统一描述）
- 还是各 Hypervisor 提供的专有 clone API？（需说明每种 Hypervisor 的支持情况）

**修复**：澄清 "clone" 语义为"取出独占"（每个池实例是独立完整 VM，非进程 fork）；补充各 Hypervisor 快照克隆机制对照表（Firecracker: `PUT /snapshot/load`，Cloud Hypervisor: `--restore`，QEMU: `loadvm`，StratoVirt: `restore`）。

### 2.3 Firecracker "直接 fork VM 进程"描述不准确 ✅ 已修复

**位置**：§ 一、极速冷启动 > Hypervisor 进程启动阶段

> "对支持 fork 的 Hypervisor（如 Firecracker 的 `jailer`），复用已初始化的 Hypervisor 进程状态"

Firecracker 的 `jailer` 是安全沙箱工具（通过 `chroot`/`seccomp`/`cgroup` 对 firecracker 进程进行隔离），不是用于 fork 复用进程状态的组件。Firecracker 本身也不提供进程级 fork API。建议修正此描述，或说明具体的实现方案。

**修复**：删除"直接 fork VM 进程"条目，在 VM Template 说明中增加注释："Firecracker 的 `jailer` 是安全沙箱隔离工具，不提供进程克隆或 fork 能力，不应与克隆机制混淆。"

### 2.4 ExecTask 和 ListPids 在 Zero-Init 模型下的实现路径缺失 ✅ 已修复

**位置**：§ Task API > ExecTask / ListPids

在 Zero-Init 模型中，`/init` 在 `exec()` 后被容器进程替换，VM 内不存在任何常驻 agent 进程。文档未说明：

- `ExecTask`：如何在已运行的 VM 内发起一个额外进程？需要什么机制（例如通过 vsock 向容器进程发送特殊信号？还是保留一个后台 mini-agent？）
- `ListPids`：文档注释说"从 guest 侧 /proc 读取"，但没有 agent，Manager 如何访问 VM 内的 /proc？

这两个接口的实现路径是本方案的关键技术挑战，需要详细说明。

**修复**：Task API 前增加实现路径说明：ExecTask 通过 `/init` 在 exec 前以 `O_CLOEXEC=false` 保留并继承 vsock 控制 fd 实现；ListPids 优先读取宿主机 cgroup 路径，无需进入 VM；`ListPids` 注释同步更新。

### 2.5 多实例快照恢复的身份唯一性问题 ✅ 已修复

**位置**：§ 二、基于快照的瞬时启动 > 快照恢复机制

从同一 Base Snapshot 恢复多个实例时，`StartFromSnapshot` 只覆盖了网络配置和 `override_envs`，但以下状态同样被克隆，可能导致问题：

- **随机数种子**：`/dev/random` 和 `/dev/urandom` 的熵池状态相同，多个实例会生成相同的"随机"数，在加密密钥生成、TLS 握手等场景下存在**严重安全漏洞**。
- **Hostname**：VM 内 hostname 相同，可能影响应用逻辑（如某些服务注册场景）。
- **vsock CID**：vsock 的 Context Identifier（CID）需要在每个 VM 实例中唯一，克隆后必须重新分配。

建议在恢复流程中明确说明如何处理上述唯一性问题。

**修复**：`StartFromSnapshotRequest` 新增三个字段：`hostname`（覆盖快照中的 hostname）、`vsock_cid`（强制分配唯一 CID）、`reseed_entropy`（VMRESUME 后注入新熵种子，默认 true）。

### 2.6 Manager 的状态持久化机制未说明 ✅ 已修复

**位置**：§ 三、极简 Sandbox 架构 > 稳定性与故障隔离分析

> "Manager 本身是无状态的（VM 状态在 Hypervisor 进程和快照 store 中），Manager 重启不影响运行中的 VM。"

Manager 实际上需要维护以下运行时状态：

- sandbox_id → (vm_id, vsock_cid, hypervisor_pid) 的映射表
- 每个 Sandbox 的 stdio FIFO 路径和文件描述符
- Standby pool 的 VM 状态列表

Manager 重启后，如何重新接管这些已运行的 VM？需要说明状态持久化（如写入 `/run/nanosandbox/state/` 目录）和恢复（reconnect vsock、重建 FIFO）的机制。

**修复**：将 Manager "无状态"改为"轻量持久化"，说明 `meta.json` 写入路径（`/run/nanosandbox/state/<sandbox_id>/`）、包含字段（vm_pid、vsock_cid、hypervisor_type、FIFO 路径），以及重启后扫描目录、重连 vsock、清理孤儿进程的恢复流程。

### 2.7 宿主机 cgroup 与 VM 内进程的关联方式 ✅ 已修复

**位置**：§ 三、极简 Sandbox 架构 > Zero-Init 机制

> "cgroup 在宿主机由 NanoSandbox Manager 在 VM 外设置"

在 VM 隔离模型中，宿主机只能看到 Hypervisor 进程的 PID，无法直接将 VM 内的容器进程（Guest PID 1）纳入宿主机 cgroup。实际的资源控制粒度是**整个 VM**（通过 Hypervisor 进程的 cgroup）而非容器进程级别。文档需要明确说明：

- `Stats` API 返回的 CPU/内存数据来源（VM 级 cgroup 统计 vs. guest 内 /proc 统计）
- 是否支持 Kubernetes 的 resource limit（requests/limits）精确控制

**修复**：`StatsResponse` 字段注释补充数据来源（cpu/memory 来自宿主机 VM cgroup，为 VM 级粒度；网络来自 tap 设备计数；磁盘来自 virtio-blk 计数）；明确 Kubernetes resource limits 精度为 VM 级别而非容器进程级别。

### 2.8 快照文件系统一致性保障 ✅ 已修复

**位置**：§ 二、基于快照的瞬时启动 > 快照捕获机制

文档描述捕获流程时，在冻结 vCPU 后直接执行文件系统可写层 COW 快照（step 6），但未说明：

- 容器进程正在写文件时，是否保证文件系统处于一致状态（是否需要 `sync` 或 guest 内 freeze）？
- `fs.diff` 具体是基于 overlay 的 `copy_up` 记录，还是 qcow2 的 COW 机制？两种实现的恢复路径差异较大。
- 增量快照的 `fs.diff` 是基于父快照的文件级 diff 还是块级 diff？

**修复**：捕获流程 step 2 前新增"可选 vsock SYNC 信号 + fsync"步骤；step 7 区分 overlay upper 文件级 diff 和 qcow2 块级 COW 两种实现路径，分别说明。

### 2.9 快照链深度与恢复性能的权衡未说明 ✅ 已修复

**位置**：§ 二、基于快照的瞬时启动 > 快照存储格式

文档支持父子快照链，但未说明：

- 链的最大深度限制是多少？
- 深度为 N 的增量快照恢复时需叠加 N 个增量，是否仍能满足 < 50ms 的目标？
- 是否有自动合并（squash）机制？

**修复**：说明建议最大链深度为 8 层（每层增加 2~5ms 恢复延迟），超限时自动触发 squash，`PruneSnapshots` 支持按配置自动合并。

### 2.10 CRI 兼容模式下快照启动延迟优势丧失 ✅ 已修复

**位置**：§ 部署模式 > 两种模式对比

对比表中指出 CRI 兼容模式快照启动延迟为 ~30ms，接近冷启动，而原生模式为 ~5ms。文档对此缺乏充分说明——CRI 兼容模式下快照启动的主要价值已大幅缩减（应用已 warm-up 的优势保留，但时延优势丧失），用户可能因此误解两种模式的适用场景。建议在对比表中增加说明，并探讨是否可在 CRI 兼容模式下通过改进减少这 ~25ms 的额外开销。

**修复**：对比表中该单元格补充说明：VMRESUME ~5ms + CreateTask + StartTask/vsock exec ~25ms，应用 warm-up 优势保留但时延优势相对原生模式大幅缩减。

---

## 三、开放性问题与待明确事项

### 3.1 缺乏事件/Watch 机制 ✅ 已修复

**位置**：§ API 设计 > Sandbox API / Task API

Sandbox API 和 Task API 均无 `Watch`/`Subscribe` 接口。当容器进程意外崩溃或 VM 异常退出时，调用方只能通过轮询 `GetSandboxStatus`/`GetTaskStatus` 感知。对于 Kubernetes kubelet 而言，及时感知 Pod 失败是核心需求。建议考虑增加事件流接口（如 gRPC Server-Streaming 的 `WatchSandbox(sandbox_id) returns (stream SandboxEvent)`）。

**修复**：Sandbox API 新增 `WatchSandbox(WatchSandboxRequest) returns (stream SandboxEvent)` Server-Streaming RPC，以及 `SandboxEvent` 消息（含 event_type、exit_status、occurred_at、message 字段），支持容器崩溃、VM panic、OOM Kill 等事件的主动推送。

### 3.2 KillTask 与 StopSandbox 的级联语义未定义 ✅ 已修复

**位置**：§ Task API > KillTask / § Sandbox API > StopSandbox

在 1:1 VM-per-Container 模型中，容器进程（PID 1）退出会导致整个 Guest 内核 panic 或正常关机，进而触发 VM 退出。需要明确：

- `KillTask(SIGKILL)` → 容器 PID 1 退出 → VM 是否自动进入 STOPPED 状态？
- `StopSandbox` 是向 VM 发 ACPI 关机信号还是直接 poweroff？这两者对容器进程的行为有本质区别（graceful vs. forced）。
- `WaitTask` 和 `WaitSandbox` 的关系：在 1:1 模型中两者是否等价？

**修复**：`StopSandbox` 注释补充完整语义：`timeout_secs > 0` 发 ACPI 关机信号（graceful，超时后 poweroff）；`timeout_secs = 0` 直接 poweroff（forced）；明确 KillTask → PID 1 退出 → VM 自动 STOPPED 的级联行为；确认 1:1 模型下 `WaitTask` 与 `WaitSandbox` 语义等价。

### 3.3 多容器 Pod（Sidecar 模式）的处理方案 ✅ 已修复

**位置**：§ 方案局限性

文档承认"多容器 Pod 支持有限"，但未给出任何解决思路。Istio sidecar（envoy proxy）等场景在生产环境中极为普遍。建议至少讨论以下备选方案：

- 方案 A：每个容器对应一个 MicroVM，Pod 内容器通过 veth pair 或 macvlan 共享网络命名空间（需 CNI 配合）
- 方案 B：允许一个 VM 内运行多个进程（保留最小 agent），牺牲部分 Zero-Init 收益

**修复**：方案局限性第 4 条补充方案 A（多 VM 共享网络，需 CNI 配合）和方案 B（单 VM 多进程，引入轻量 mini-agent）的权衡说明，明确当前版本以单容器 Pod 为首要场景，多容器 Pod 支持列为后续演进目标。

### 3.4 Pause 容器在 CRI 兼容模式下的处理 ✅ 已修复

**位置**：§ 部署模式 > 模式一：CRI 兼容模式

CRI 标准中 `RunPodSandbox` 会包含 pause 容器（infra container）的配置。文档的 CRI 调用映射表将 `RunPodSandbox` 直接映射为 `CreateSandbox + StartSandbox`，但没有说明 pause 容器的 image/spec 如何处理——NanoSandbox 是否完全忽略 pause 容器配置？如果是，是否会影响与某些 CRI 客户端的兼容性？

**修复**：CRI 调用映射表 `RunPodSandbox` 行补充说明：pause 容器 image/spec 由 CRI Plugin 忽略；Pod 网络命名空间由 VM 本身承载，无需 pause 容器进程；对标准 CRI 客户端（kubelet）兼容性无影响。

### 3.5 Proto 中 ProcessSpec 和 NetworkConfig 重复定义问题 ✅ 已修复

**位置**：§ Task API / § Snapshot API

`ProcessSpec` 在 `nanosandbox.sandbox.v1` 和 `nanosandbox.task.v2` 两个 package 中分别完整定义，字段完全相同。`NetworkConfig` 在 `nanosandbox.sandbox.v1` 和 `nanosandbox.snapshot.v1` 中同样重复。这会导致代码生成出两套类型，调用方需要做手动转换。建议抽取公共 package（如 `nanosandbox.common.v1`）统一定义共享类型。

**修复**：Task API 和 Snapshot API 中的重复定义改为注释说明，指向 `nanosandbox.sandbox.v1` 统一导入；新增建议将 `ProcessSpec`/`NetworkConfig`/`RootfsConfig` 抽取到 `nanosandbox.common.v1` 的说明。

### 3.6 快照跨节点迁移的兼容性 ✅ 已修复

**位置**：§ 设计细节 > 快照分级预热策略 > Level 2

Level 2 描述了从分布式存储（S3）拉取快照，但快照中的设备状态（`devices.json`）包含 virtio 队列指针、中断控制器状态等与宿主机 Hypervisor 实现强相关的信息。跨节点恢复时，目标节点的 Hypervisor 版本、设备配置是否必须与源节点完全一致？快照格式是否具有跨版本兼容性？这是分布式快照存储的核心约束，需要明确。

**修复**：Level 2 段落增加跨节点兼容性约束说明（Hypervisor 版本需一致、CPU 架构需相同），`metadata.json` 记录捕获环境，Manager 恢复前验证兼容性。

### 3.7 userfaultfd 的权限与内核版本要求 ✅ 已修复

**位置**：§ 二、基于快照的瞬时启动 > 快照恢复机制

文档将 `userfaultfd` 作为快照恢复的核心优化，但未提及其约束：

- Linux < 5.11：`userfaultfd` 需要 `CAP_SYS_PTRACE` 或 root 权限
- Linux >= 5.11：unprivileged 用户可用，但仍受 `/proc/sys/vm/unprivileged_userfaultfd` 控制
- `userfaultfd` 与 huge page 的兼容性有限制（2MB/1GB 大页需要特别处理）

若 Hypervisor 以非 root 用户运行（文档明确提到这一安全属性），需要说明如何保证 userfaultfd 的可用性。

**修复**：`userfaultfd` 段落后增加内核版本权限对照表，明确部署要求（内核 ≥ 5.11 且 `vm.unprivileged_userfaultfd=1`，或授予 `CAP_SYS_PTRACE`），以及不满足时的降级策略（同步全量内存加载）。

### 3.8 `cgroup v2 (namespaces only)` 描述不精确 ✅ 已修复

**位置**：§ 设计细节 > MicroVM 内核裁剪规范 > 必须保留的子系统

"cgroup v2（namespaces only）"混淆了两个独立特性：

- `CONFIG_CGROUPS`：cgroup 子系统
- `CONFIG_NAMESPACES`：进程命名空间（pid/net/mount 等）

文档需要明确 VM 内部是否需要 cgroup（以及需要哪些 cgroup controllers），还是仅需要 namespace 隔离。

**修复**：内核裁剪表中该行拆分为两行：① `cgroup v2（memory/cpu/pids controllers）`（VM 内进程资源控制，可选）；② 进程命名空间（`CONFIG_NAMESPACES`：pid/mount/uts/ipc），消除两个独立特性的混淆。

---

## 四、建议补充的内容

| 缺失内容 | 建议 |
|----------|------|
| Manager 崩溃恢复流程 | 说明 VM 孤儿进程的检测与接管机制 |
| 节点级资源上限与 overcommit 策略 | 单节点最多支持多少个并发 MicroVM？overcommit 比例如何确定？ |
| 快照的安全传输与加密 | Level 2 快照存储在 S3，传输和静态存储是否加密？ |
| /init 与 Manager 的 vsock 协议规范 | 控制通道的消息格式（protobuf? JSON?）和握手流程 |
| VM 热插拔（vCPU/内存）支持 | Serverless 场景下是否支持运行时调整 VM 资源规格？ |
| 快照版本管理 | 容器镜像升级后，旧快照如何失效？是否支持快照与 rootfs 版本绑定？ |

---

*检视意见作者：Claude Code | 基于 nanosandbox.md v0.1 | 修复确认：nanosandbox.md v0.2（2026-04-14）*
