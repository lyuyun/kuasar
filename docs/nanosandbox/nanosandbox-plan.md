# NanoSandbox 开发计划与测试计划

<!-- toc -->
- [项目背景](#项目背景)
- [总体原则](#总体原则)
- [架构约束](#架构约束)
- [里程碑规划](#里程碑规划)
  - [M1：基础设施骨架](#m1基础设施骨架)
  - [M2：冷启动核心路径](#m2冷启动核心路径)
  - [M3：Task 管理与可观测性](#m3task-管理与可观测性)
  - [M4：快照子系统](#m4快照子系统)
  - [M5a：冷启动优化](#m5a冷启动优化)
  - [M5b：快照性能优化](#m5b快照性能优化)
  - [M6：CRI 兼容模式](#m6cri-兼容模式)
  - [M7：多 Hypervisor 适配](#m7多-hypervisor-适配)
  - [M8：生产加固与文档](#m8生产加固与文档)
- [里程碑依赖关系](#里程碑依赖关系)
- [测试计划](#测试计划)
  - [单元测试](#单元测试)
  - [集成测试](#集成测试)
  - [性能基准测试](#性能基准测试)
  - [端到端测试](#端到端测试)
  - [回归测试策略](#回归测试策略)
- [风险与缓解措施](#风险与缓解措施)
<!-- /toc -->

---

## 项目背景

NanoSandbox 是面向 Serverless 场景（Function、AI Agent 等）的安全容器方案，详细设计见
[nanosandbox.md](./nanosandbox.md)。本文档描述其实现路径、里程碑规划与测试策略。

---

## 总体原则

- 以 **Cloud Hypervisor** 作为首个适配的 Hypervisor（已有 `api_client` 依赖，现有 CH 适配可复用）
- 以**原生模式（Native Mode）**作为 MVP 路径，CRI 兼容模式在后续里程碑迭代
- **CRI 兼容模式所需的接口扩展点在 M1 阶段预留**，避免后期返工
- 语言：Rust，与现有 Kuasar codebase 保持一致
- 构建入口：`containerd_sandbox::run()`，与现有 `vmm-sandboxer` 模式相同
- 新建 workspace crate：`nanosandbox/sandbox/`（Manager）、`nanosandbox/init/`（Zero-Init）

---

## 架构约束

NanoSandbox CRI 兼容模式需完全兼容当前 Kuasar 架构，以下外部合约不可更改：

| 外部类型 / 函数 | 来源 crate | 约束说明 |
|----------------|-----------|---------|
| `Sandboxer` trait | `containerd-sandbox` | `create`/`start`/`stop`/`delete`/`update`/`sandbox` 方法签名固定 |
| `Sandbox` trait | `containerd-sandbox` | `status`/`append_container`/`remove_container`/`exit_signal`/`get_data` 签名固定 |
| `SandboxStatus` enum | `containerd-sandbox` | 仅 `Created`/`Running(u32)`/`Stopped(u32,i128)`，不可新增变体 |
| `SandboxData` | `containerd-sandbox` | CRI 数据容器，NanoSandbox 从中提取 ProcessSpec |
| `ContainerOption` | `containerd-sandbox` | `append_container` 入参，含 OCI spec |
| `containerd_sandbox::run()` | `containerd-sandbox` | 唯一合法的 gRPC server 入口 |

CRI 兼容模式下两阶段启动（Phase 1 / Phase 2）的状态不暴露给 containerd，由 `NanoSandbox`
内部字段 `boot_phase: BootPhase` 维护；`SandboxStatus` 在 Phase 1 完成后即对外报告
`Running(pid)`，与现有 `KuasarSandbox` 行为一致。

Task API（CreateTask / StartTask / KillTask / WaitTask / Exec 等）由 Manager 进程内嵌
per-sandbox ttrpc server 提供，`socket_address()` 返回
`/run/nanosandbox/state/<sandbox_id>/task.sock`，替代现有 `vmm-task` 的角色。

---

## 里程碑规划

### M1：基础设施骨架

**目标**：建立代码骨架，确定所有接口契约，预留 CRI 兼容模式扩展点。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 1.1 workspace 集成 | 新建 `nanosandbox/sandbox/` 与 `nanosandbox/init/` crate，加入根 `Cargo.toml` `members` |
| 1.2 Proto 定义 | 按 nanosandbox.md 实现三个 proto 包（`sandbox.v1`、`task.v2`、`snapshot.v1`）；共用类型抽取到 `common.v1` |
| 1.3 `NanoVM` trait | 定义 VM 抽象（含 `vsock_control`、`task_socket_path`、`pause`/`resume`、快照方法）；不含指向 in-VM agent 的 `socket_address` 语义 |
| 1.4 `NanoSandboxer` / `NanoSandbox` 骨架 | 实现 `Sandboxer` + `Sandbox` 外部 trait；`NanoSandbox` 含内部字段 `boot_phase: BootPhase` |
| 1.5 Manager 内嵌 Task ttrpc server 框架 | per-sandbox Task socket 注册；全部 handler 返回 `Unimplemented`；作为 M2/M3/M6 的填充目标 |
| 1.6 状态持久化框架 | `meta.json` 读写（serde_json），**必须包含 `schema_version` 字段**；Manager 启动时检测版本，执行自动迁移或拒绝启动；`/run/nanosandbox/state/<sandbox_id>/` 目录管理；孤儿 VM 扫描框架 |
| 1.8 并发模型约束 | 明确使用 tokio 异步运行时；设计 `max_concurrent_vm_ops` 配置项（默认值待定，防止 fd 耗尽或 OOM）；超出上限时返回 `RESOURCE_EXHAUSTED`（不排队等待）；在架构文档中写明 |
| 1.7 日志与追踪 | 集成 `tracing`，复用 `vmm-common::trace` 模式；结构化字段规范（sandbox_id、snapshot_id、latency_ms） |

> **M1 细化说明**：M1 各任务在具体开展时进行详细分析与接口设计，本计划不预设实现细节。
> 重点是确保 M2 可以直接填充 `NanoVM` impl 而不需要修改 trait，M6 可以填充 CRI
> 扩展点而不需要修改 M2 已有逻辑。

**验收标准**：
- `cargo check --workspace` 无错误
- Manager 进程可启动并响应 `GetSandboxStatus`（返回 `NOT_FOUND`）
- `NanoSandboxer` 通过 `containerd_sandbox::run()` 注册成功

---

### M2：冷启动核心路径

**目标**：实现原生模式最小冷启动链路，容器进程 PID=1 可在 VM 内运行。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 2.1 `/init` 二进制（原生模式）| Rust 实现，< 100KB，静态 musl binary；**原生模式**：vsock 建立控制通道 → 接收 `ProcessSpec` → `execve` 替换为容器 entrypoint（PID=1）；CRI 模式扩展点预留但不实现（见 M6） |
| 2.2 Cloud Hypervisor 适配器 | 实现 `NanoVM` for `CloudHypervisorVM`；`vm.create` / `vm.boot` / `vm.power-button` / `vm.shutdown` / `vm.pause` / `vm.resume` REST API 调用；`pids`、`wait_channel`、`vcpus` 实现 |
| 2.3 vsock 控制通道 | Manager 侧 vsock 监听（CID 动态分配，注册表防冲突）；向 `/init` 发送 `ProcessSpec`（Protobuf over vsock）；接收 `CONTAINER_READY` / `CONTAINER_EXITED` 事件 |
| 2.4 Sandbox API 实现 | `CreateSandbox`、`StartSandbox`（原生模式：Phase 1+2 合并）、`StopSandbox`（ACPI + 超时 poweroff）、`DeleteSandbox`、`GetSandboxStatus`、`WaitSandbox` |
| 2.5 Task API（原生模式最小集）| 填充 Task ttrpc server 的 `KillTask`、`WaitTask`、`GetTaskStatus`；其余 handler 保持 Unimplemented |
| 2.6 erofs rootfs 构建工具 | `mkfs.erofs` 封装；将 `/init` 注入 rootfs 根目录；宿主机侧 `mmap` 映射为 virtio-blk 后端 |

**验收标准**：
- 执行 `CreateSandbox` + `StartSandbox` 后容器进程 PID=1 在 VM 内运行
- `KillTask(SIGKILL)` 后容器退出，`WaitTask` 返回退出码
- 端到端冷启动延迟 < 100ms（无 standby pool，Cloud Hypervisor，erofs rootfs；以性能基准测试参考环境为准，见测试计划§性能基准测试）

---

### M3：Task 管理与可观测性

**目标**：补全 Task 生命周期管理，实现监控与事件通知。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 3.1 `Stats` 实现 | CPU/内存：宿主机 cgroup（`/sys/fs/cgroup/<vm_cgroup>/`）；网络：`/sys/class/net/<tap>/statistics/`；磁盘：virtio-blk I/O 计数器 |
| 3.2 `WatchSandbox` 实现 | Server-Streaming RPC；Hypervisor 进程退出监听（`waitpid`）；vsock 断连检测；OOM Kill 检测（`memory.events`）；转为 `SandboxEvent` stream |
| 3.3 `ListSandboxes` | 扫描 state 目录 + 内存注册表；支持 label 过滤 |
| 3.4 `ListPids` | 优先读 `cgroup.threads`；可选 vsock fallback 读取 VM 内 `/proc` |
| 3.5 Manager 重启恢复 | 启动时扫描 `meta.json`；重连 vsock；重建 FIFO 监听；清理孤儿 VM |
| 3.6 `PauseSandbox` / `ResumeSandbox` | 调用 CH `vm.pause` / `vm.resume` API |

**验收标准**：
- Manager 重启后已运行 VM 通过 `GetSandboxStatus` 返回 `RUNNING`
- `WatchSandbox` 事件延迟：**P50 < 50ms，P99 < 1s**（`SANDBOX_EXITED` 事件不用于计费，计费时间由 Manager 内部精确记录）
- `Stats` 数据来源正确（通过 `/sys` 路径验证）

---

### M4：快照子系统

**目标**：实现完整的快照捕获、存储、恢复链路。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 4.1 快照存储模块 | 目录结构（`metadata.json`、`cpu.bin`、`mem.raw`、`mem.bitmap`、`devices.json`、`fs.diff`）；稀疏文件写入；`metadata.json` 含 Hypervisor 版本与 CPU arch 字段 |
| 4.2 `CreateSnapshot` | 调用 CH `vm.snapshot` API；可选 SYNC+fsync 步骤；写 metadata；增量快照父子链管理 |
| 4.3 `StartFromSnapshot` | CH 快照恢复（`vm.restore` API 或启动参数注入）；`mem.raw` mmap 为 VM 物理内存后端；`hostname`/`vsock_cid`/`reseed_entropy` 字段处理；vsock 熵注入 |
| 4.4 userfaultfd 集成 | 检测内核版本与 `vm.unprivileged_userfaultfd`；不满足时降级为同步全量内存加载；后台 I/O 填页线程 |
| 4.5 快照元数据 API | `GetSnapshotStatus`、`ListSnapshots`（分页 cursor）、`DeleteSnapshot`（子依赖检查）、`PruneSnapshots`（dry_run、TTL、auto-squash） |
| 4.6 快照链深度管理 | 深度超过 8 层时自动触发 squash；**原子替换机制**：squash 结果写入临时目录 → `rename()` 原子更新 `metadata.json`（指向新单层）→ 旧层标记 `pending-delete` → 引用归零后异步清理；squash 期间并发 `StartFromSnapshot` 读取旧链，不受影响 |

**验收标准**：
- `StartFromSnapshot` 端到端延迟 < 50ms（userfaultfd 模式，256MB VM，NVMe SSD）
- 快照链深度 ≥ 8 层时自动触发 squash
- metadata 校验失败（Hypervisor 版本/CPU arch 不匹配）时返回明确错误码

---

### M5a：冷启动优化

**目标**：达到 standby pool 冷启动 < 10ms 目标值。**依赖 M2**（不依赖 M4，可与 M4 并行推进）。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 5a.1 Standby Pool | 池大小可配置；后台异步补充线程；`CreateSandbox` 优先从池取出；CID 注册表管理 |
| 5a.2 Pre-zeroed Page Pool | 宿主机侧 `madvise(MADV_POPULATE_WRITE)` 或 `hugetlbfs` 预分配；新 VM 从池分配；纯宿主机侧优化，不依赖 `virtio-balloon`；**要求宿主机内核 ≥ 5.14**（`MADV_POPULATE_WRITE` 于 Linux 5.14 引入）；Manager 启动时检测内核版本，低于 5.14 时打印 `WARN` 并降级为普通 `mmap`，同时下调性能预期 |
| 5a.3 HugePage 支持 | 通过 `hypervisor_options` 扩展字段传递 hugepage 配置 |

**验收标准**：
- 有 standby pool 的冷启动 < 10ms

---

### M5b：快照性能优化

**目标**：达到快照恢复 < 5ms 目标值。**依赖 M4**（基于快照子系统做 userfaultfd 优化）。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 5b.1 userfaultfd 优化 | 后台 I/O 填页线程优化；与 CH `vm.restore` 异步加载对比基准；确认 CH API 是否支持外部 uffd handler 注入 |
| 5b.2 KSM 管理 | 按节点配置（single-tenant / multi-tenant）决定 KSM 开关；`/sys/kernel/mm/ksm/run` 控制；多租户场景强制 off 并告警 |
| 5b.3 基准测试套件 | 冷启动（无 pool / 有 pool）、快照捕获、快照恢复的端到端延迟基准；`criterion` 集成 |

**验收标准**：
- 快照恢复（userfaultfd 优化后）< 5ms
- 多租户配置（`multi_tenant=true`）下 KSM 必须为 off（`/sys/kernel/mm/ksm/run=0`）——**硬性指标**
- 100 个同函数实例 KSM 节省内存 ≥ 30%（测试环境）——**软目标，参考值，不阻断发布**

---

### M6：CRI 兼容模式

**目标**：接入标准 Kubernetes kubelet，支持 `kubectl exec`、`kubectl top`、HPA 全链路。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 6.1 两阶段启动状态机 | `Sandboxer::start()` 完成 Phase 1（内核就绪，`/init` 等待 vsock）；`boot_phase = Phase1Done`；`SandboxStatus = Running(pid)` |
| 6.2 `append_container` 实现 | 对应 CRI `CreateContainer`；解析 `ContainerOption` 提取 `ProcessSpec`；存入 `NanoSandbox` 内存，不调用 VM |
| 6.3 Task ttrpc server 完整实现 | 填充 `CreateTask`（记录 ProcessSpec）、`StartTask`（Phase 2：vsock 注入 ProcessSpec → exec）、`ExecTask`（vsock 转发）、`DeleteTask`、`ResizePty` |
| 6.4 ExecTask vsock 协议与 `/init` 架构决策 | **CRI 模式 `/init` 采用 fork+exec 模式**：父进程 `/init` 留驻持续监听 vsock，处理 `ExecRequest`；子进程 `fork` + `execve` 成为容器业务进程。**此设计意味着 CRI 模式下容器业务进程不再是 PID=1**（父进程 `/init` 占 PID=1），需同步更新 `nanosandbox.md` Zero-Init 设计说明。Manager 侧转发 `ExecRequest` → vsock → `/init`；`/init` fork 子进程执行，返回 exec_id + PID |
| 6.5 CRI 端到端验证 | `crictl runp`、`crictl create`、`crictl start`、`kubectl exec`、`kubectl top` 全链路验证 |
| 6.6 快照启动 CRI 路径 | `StartFromSnapshot` 后 Task ttrpc server 直接返回 `TASK_RUNNING`，跳过 Phase 2 exec |

**验收标准**：
- `kubectl run` 能成功创建并运行 Pod
- `kubectl exec` 能进入容器
- `kubectl top` 返回 CPU/内存数据
- CRI 模式冷启动端到端 < 35ms

> **CRI 测试环境说明**：CRI 模式 E2E 需要 kubelet + containerd（kuasar-io fork）+ kubectl 环境；使用 single-node kubelet 即可，无需完整 K8s 集群；复用现有 `.github/workflows/` 中 Kuasar CRI E2E 测试框架；CI 中 CRI E2E 测试移至每日 CI 专用机（KVM bare-metal），PR CI 只运行单元测试 + 基础集成测试。

---

### M7：多 Hypervisor 适配

**目标**：在 Cloud Hypervisor 基础上扩展 Firecracker、QEMU、StratoVirt 适配器。

**主要任务**：

| 任务 | 说明 |
|------|------|
| 7.1 Firecracker 适配器 | `PUT /snapshot/load` 快照恢复；MMIO 总线；HTTP API over Unix socket |
| 7.2 QEMU 裁剪模式适配器 | `-M microvm`；QMP `loadvm`；`-nodefaults -nographic` |
| 7.3 StratoVirt 适配器 | `restore` 接口；轻量 MMIO 总线 |
| 7.4 Hypervisor 配置规范化 | `hypervisor_options (Any)` 解析规范；各 Hypervisor 快照格式适配层 |

**验收标准**：
- 三种 Hypervisor 均通过 M2 冷启动验收测试
- Firecracker 通过 M4 快照捕获/恢复验收测试

---

### M8：生产加固与文档

**主要任务**：

| 任务 | 说明 |
|------|------|
| 8.1 快照完整性校验 | 写入时 sha256 checksum；恢复前校验；损坏时自动降级冷启动 |
| 8.2 资源泄漏防护 | VM 销毁时确认 Hypervisor 进程退出；CI 集成 LSAN |
| 8.3 seccomp 配置 | Hypervisor 进程 seccomp profile（最小 syscall 白名单）；Manager 以非 root 用户运行 |
| 8.4 部署运维文档 | 节点配置指南（内核参数、`vm.unprivileged_userfaultfd`）；快照分级预热配置；KSM 安全决策树 |
| 8.5 性能调优指南 | 内核裁剪步骤；erofs rootfs 构建；standby pool 大小配置建议 |

---

## 里程碑依赖关系

```
M1（基础设施骨架）
  └─► M2（冷启动，Cloud Hypervisor，原生模式）
        ├─► M3（Task 管理与可观测性）
        │     └─► M6（CRI 兼容模式，填充 M1 预留扩展点，无返工）
        ├─► M5a（冷启动优化：Standby Pool / Pre-zeroed Page Pool / HugePage）
        │     └─► M7（Firecracker / QEMU / StratoVirt 适配）
        │           └─► M8（生产加固与文档）
        └─► M4（快照子系统）
              └─► M5b（快照性能优化：userfaultfd / KSM / 基准测试套件）
                    └─► M7（同上）
```

> M5a 与 M4 可并行推进，均依赖 M2 完成。M5b 依赖 M4（基于快照子系统做 userfaultfd 优化）。

**MVP 范围**（M1 + M2 + M4 原生模式）：可验证核心技术路线（冷启动延迟、快照捕获/恢复），
不依赖 Kubernetes。

---

## 测试计划

### 单元测试

覆盖不依赖 Hypervisor 或内核的纯逻辑模块：

| 测试模块 | 测试用例 |
|----------|---------|
| Proto 序列化 | `ProcessSpec`、`NetworkConfig`、`SnapshotSpec` 序列化/反序列化往返正确 |
| `SandboxStatus` 状态机 | 合法转换（`Created→Running`、`Running→Stopped`）；非法转换返回错误 |
| `BootPhase` 状态机 | `NotStarted→Phase1Done→Complete`；跳过 Phase1（原生模式）直达 Complete |
| 快照链管理 | 深度计算；深度 > 8 触发 squash 标记；子依赖删除检查；父子链遍历 |
| CID 分配 | 注册表无冲突；分配/释放/重用；并发分配无竞争（`tokio::test`）|
| userfaultfd 降级判断 | 内核 < 5.11 且无 `CAP_SYS_PTRACE` → 降级；`vm.unprivileged_userfaultfd=0` → 降级 |
| KSM 决策 | 多租户配置 → KSM off；单租户 → KSM on；告警日志输出 |
| `PruneSnapshots` dry_run | 仅返回待删列表，文件系统无变化 |
| `meta.json` 读写 | 正常写入/读回；文件损坏返回错误（不 panic）；孤儿 VM 识别（进程不存在）|
| vsock 协议编解码 | `ProcessSpec`/`ExecRequest` wire format；消息截断时返回 error |

### 集成测试

需要宿主机 KVM + vsock 环境，可在 CI 虚拟机中运行。

**冷启动路径**

| 场景 | 验证点 |
|------|--------|
| 基本冷启动（无 pool）| 容器 PID=1；端到端 < 100ms；`GetSandboxStatus` 返回 `RUNNING` |
| 冷启动超时 | Hypervisor 启动失败时 30s 超时返回 error；资源全部清理 |
| standby pool 冷启动 | 从 pool 取出后启动 < 10ms；pool 后台补充后 size 恢复 |
| ProcessSpec 注入验证 | 容器 env/workdir/uid/gid 与 `SandboxSpec.process` 一致 |

**Sandbox 生命周期**

| 场景 | 验证点 |
|------|--------|
| `StopSandbox`（graceful）| ACPI 关机信号发出；容器收到 SIGTERM；VM 正常退出 |
| `StopSandbox`（`timeout_secs=0`）| 立即 poweroff；VM 进程退出码非零 |
| `StopSandbox` 超时升级 | 5s 后未退出自动 poweroff |
| `DeleteSandbox` 资源清理 | `meta.json` 删除；tap 设备清理；可写层文件删除；Hypervisor 进程已退出 |
| `PauseSandbox` / `ResumeSandbox` | Pause 后 vsock 无响应；Resume 后 vsock 恢复 |

**WatchSandbox 事件**

| 场景 | 验证点 |
|------|--------|
| 容器正常退出（exit 0）| 收到 `container_exited`，`exit_status=0`；stream 关闭 |
| 容器 OOM Kill | 收到 `oom_kill`；`message` 含内存限制信息 |
| VM Panic | 收到 `vm_panic`；Manager 状态更新为 `EXITED` |
| 多订阅者 | 同一 sandbox_id 多个 `WatchSandbox` 均收到事件 |

**Task API**

| 场景 | 验证点 |
|------|--------|
| `KillTask(SIGTERM)` 后 `WaitTask` | 返回非零 exit_status；exited_at 时间戳有效 |
| `Stats` 数据来源校验 | cpu_usage_ns 单调递增；rx/tx_bytes 随流量变化 |
| `ListPids` 正确性 | 返回的宿主机 PID 在 `cgroup.threads` 中存在 |
| `ExecTask` 正确性 | exec 进程在 VM 内运行；stdout 正确转发；退出后 WaitTask 返回 |

**快照系统**

| 场景 | 验证点 |
|------|--------|
| `CreateSnapshot`（全量）| state == `READY`；`cpu.bin`/`mem.raw`/`devices.json` 均存在 |
| VM 暂停时间 | PauseVM → 捕获 → ResumeVM 全程 < 5ms（256MB VM）|
| `StartFromSnapshot` 基本恢复 | 容器从快照点继续执行；`restore_latency_ms` < 50ms |
| hostname / vsock_cid 唯一性 | 同一快照恢复两实例，hostname 和 CID 均不同 |
| `override_envs` 生效 | 新实例环境变量被覆盖值 |
| `reseed_entropy=true` | VMRESUME 后 `/dev/urandom` 输出不同于快照中值 |
| 增量快照链（深度 5）| 各层 `size_bytes` < 全量大小；恢复结果正确 |
| 快照链 squash（深度 > 8）| `PruneSnapshots` 后链深度归 1；恢复结果与 squash 前一致 |
| 快照损坏降级 | `mem.raw` 截断后 `StartFromSnapshot` 触发冷启动降级 |
| Hypervisor 版本不匹配 | `metadata.json` 版本不匹配时返回 `INCOMPATIBLE_SNAPSHOT` 错误 |
| `DeleteSnapshot` 子依赖检查 | 有子快照时返回 `SNAPSHOT_HAS_CHILDREN` 错误 |

**Manager 重启恢复**

| 场景 | 验证点 |
|------|--------|
| `kill -TERM` 重启后 `GetSandboxStatus` | Manager 主动 flush → 重启后 VM 状态 `RUNNING`；`meta.json` 正确重载 |
| `kill -KILL` 重启后 `GetSandboxStatus` | 崩溃依赖已持久化 meta.json → 重启后 VM 状态 `RUNNING` |
| 重启后 `WatchSandbox` | 重连 vsock 后 Watch stream 恢复 |
| 孤儿 VM 清理 | `meta.json` 存在但进程已退出 → 状态标记 `EXITED`，文件清理 |

### 性能基准测试

参考环境：Cloud Hypervisor latest、Linux 5.15 裁剪内核（< 4MB）、erofs rootfs、256MB VM、NVMe SSD。
使用 `criterion` 框架，每项基准运行 30 次取 p50 / p95 / p99。

| 基准项 | 目标值 | 适用里程碑 |
|--------|--------|-----------|
| 冷启动（无 pool）| < 100ms | M2 |
| 冷启动（有 pool）| < 10ms | M5a |
| 快照恢复（userfaultfd，无 pool 预热）| < 50ms（首次请求就绪，从磁盘加载）| M4 |
| 快照恢复（userfaultfd，pool 预热后）| < 5ms（快照已预热到内存）| M5b |
| 快照恢复（同步全量降级）| < 250ms | M4 |
| 快照捕获 VM 暂停时间 | < 5ms（256MB VM）| M4 |
| 并发恢复 10 实例 | 各实例 < 50ms | M4 |
| `Stats` 查询延迟 | < 5ms（单次 RPC 往返）| M3 |
| Manager 重启恢复时间 | < 2s（100 个 sandbox）| M3 |

### 端到端测试

**原生模式 E2E**

```
1. 节点初始化：启动 Manager，配置 standby_pool_size=5
2. 冷启动：CreateSandbox + StartSandbox，执行 "echo hello" 函数，验证退出码=0
3. 快照创建：对运行中函数调用 CreateSnapshot，捕获 Base Snapshot
4. 快照启动（x10 并发）：10 个并发 StartFromSnapshot，均在 50ms 内返回 CONTAINER_READY
5. 资源清理：DeleteSandbox 后验证 state 目录清空，Hypervisor 进程不存在
6a. Manager 重启恢复（优雅退出）：`kill -TERM <pid>`，Manager 主动 flush meta.json；重启后 GetSandboxStatus 仍返回 RUNNING
6b. Manager 重启恢复（崩溃模拟）：`kill -KILL <pid>`，Manager 依赖已持久化 meta.json；重启后 GetSandboxStatus 仍返回 RUNNING
```

**CRI 兼容模式 E2E**（M6 后执行）

```
1. crictl runp 创建 Pod Sandbox（两阶段启动路径）
2. crictl create + crictl start 启动业务容器
3. kubectl exec 进入容器，验证 ExecTask 路径
4. kubectl top pod 验证 Stats 数据非零
5. 容器异常退出后，kubelet 感知到 Pod Failed 状态（WatchSandbox 事件链路）
6. HPA 扩缩容触发 Pod 创建，验证快照启动路径被利用
```

**安全 E2E**

```
1. Hypervisor 进程以非 root 用户运行，验证 /proc/<pid>/status Uid != 0
2. 同一快照恢复 2 个实例，读取 /dev/urandom 输出，验证二者不同（reseed_entropy 生效）
3. 多租户配置：Manager multi_tenant=true，验证 KSM /sys/kernel/mm/ksm/run 值为 0
```

### 回归测试策略

| 触发时机 | 运行范围 |
|----------|---------|
| 每次 PR 合并 | 全部单元测试 + 冷启动集成测试 + 快照链管理集成测试 |
| 每日 CI | 全部集成测试套件（需 KVM 环境）|
| 版本发布前 | 全部 E2E 测试 + 性能基准（与上一版本比较，退化 > 10% 阻断发布）|

---

## 风险与缓解措施

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Cloud Hypervisor 快照 API 在版本间变化 | M4 适配层返工 | CI 固定 CH 版本（镜像锁定）；适配层做版本分支 |
| userfaultfd 在 CI 环境不可用 | M4 无法测试 uffd 路径 | CI 环境配置 `vm.unprivileged_userfaultfd=1`；降级路径作为独立测试用例 |
| vsock 并发 CID 冲突 | Manager 崩溃 | CID 注册表加 Mutex；并发分配单元测试覆盖 |
| `SandboxStatus` 外部类型无法新增 `KernelReady` | CRI 两阶段状态无法对外表达 | 内部 `BootPhase` 字段管理；对外始终报告 `Running`，与现有 Kuasar 行为一致 |
| CH `vm.restore` 不支持外部 uffd handler 注入 | userfaultfd 优化不可用 | 确认 CH API 能力；若不支持，降级为 CH 原生异步加载，`restore_latency_ms` 仍可观测 |
| erofs 工具链在目标环境缺失 | M2 rootfs 构建失败 | CI Docker 镜像预装 `erofs-utils`；提供 Dockerfile |
| CRI 模式 ExecTask 依赖容器进程实现 vsock 协议，对通用容器不可行 | M6 `kubectl exec` 对任意容器无效，CRI 兼容性存疑 | 采用 fork+exec 模式（父 `/init` 留驻处理 ExecRequest）；明确此架构下容器非 PID=1；如需支持通用容器 exec，评估实现轻量 mini-agent 转发层作为降级方案 |
| `MADV_POPULATE_WRITE` 在宿主机内核 < 5.14 不可用 | M5a Pre-zeroed Page Pool 优化静默失效，性能目标静默不达标 | Manager 启动时检测内核版本；低于 5.14 时打印 `WARN` 并降级为无预热 mmap；下调对应性能预期 |
| KVM CI 环境不可用 | 集成测试和 E2E 测试无法在 PR CI 自动运行 | PR CI 只跑单元测试 + 基础冷启动集成测试；集成/E2E/KVM 测试移至每日 CI bare-metal 专用机；在风险发生时评估 KVM-enabled 云实例成本 |

---

*文档版本：v1.1 | 日期：2026-04-15 | 基于 nanosandbox.md v0.2 | 检视意见 plan-issue.md v1.0 已全部处理*
