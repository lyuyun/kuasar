# NanoSandbox 开发计划检视意见

文档版本：v1.0 | 检视日期：2026-04-15 | **已全部处理，见 nanosandbox-plan.md v1.1**

---

## 处理状态总览

| 类别 | 条目数 | 严重程度 | 状态 |
|------|--------|---------|------|
| 设计与逻辑问题 | 3 | 高 | 全部已处理 |
| 技术细节不足 | 5 | 中 | 全部已处理 |
| 开放性问题/缺失内容 | 6 | 中低 | 全部已处理 |
| 风险表补充 | 3 | — | 全部已添加 |

---

## 一、设计与逻辑问题

### 1.1 M2 2.1 `/init` 的 ExecTask vsock fd 继承描述引入设计矛盾

**原文**：`O_CLOEXEC=false 保留控制 fd 供 ExecTask 继承`

**问题**：M2 是原生模式，ExecTask 属于 M6（CRI 兼容模式）。原生模式下 `/init` 直接 `exec()` 替换为容器 entrypoint（PID=1），exec 之后进程映像被完全替换为通用容器进程（如 `nginx`、`python`），这些进程不实现 NanoSandbox vsock 协议，无法处理 ExecRequest。

M6 6.4 进一步说明"/init exec 后通过继承的 vsock fd 处理 ExecRequest"——这在逻辑上与"exec 后进程已替换"矛盾：exec 之后 `/init` 不再存在，没有任何代码可以处理 vsock 上的 ExecRequest。这是 nanosandbox.md 遗留的 R-2 问题，计划文档未予解决。

**处理结果**：
- M2 2.1 已改为"原生模式：`execve` 替换为容器 entrypoint（PID=1）"，移除 ExecTask fd 继承描述
- M6 6.4 已明确**架构决策**：CRI 模式 `/init` 采用 **fork+exec 模式**，父进程 `/init` 留驻持续监听 vsock 处理 ExecRequest，子进程 fork+execve 成为容器业务进程；**CRI 模式容器不再是 PID=1**；已注明需同步更新 nanosandbox.md Zero-Init 设计说明

---

### 1.2 Standby Pool 不依赖快照子系统，但依赖图强制 M5 位于 M4 之后

**依赖图原文**：`M2 → M4（快照子系统）→ M5（性能优化）→ M7 → M8`

**问题**：M5.1（Standby Pool）和 M5.2（Pre-zeroed Page Pool）是冷启动优化，逻辑上只依赖 M2，与 M4 快照系统无关联。当前依赖关系强制 Standby Pool 等待整个快照子系统完成，无谓地拉长了"达到 <10ms 冷启动目标"的关键路径。

**处理结果**：M5 已拆分为两个并行子里程碑：
- **M5a（冷启动优化）**：Standby Pool + Pre-zeroed Page Pool + HugePage，依赖 M2，可与 M4 并行推进
- **M5b（快照性能优化）**：userfaultfd 优化 + KSM 管理 + 基准测试套件，依赖 M4

依赖图已更新，M5a 与 M4 并行路径已明确。

---

### 1.3 M4 与 M5 快照恢复延迟验收标准相差 10 倍，测试计划性能基准表与 M5 目标矛盾

| 位置 | 快照恢复（userfaultfd）目标 |
|------|---------------------------|
| M4 验收标准 | < 50ms |
| M5 验收标准 | < 5ms |
| 测试计划性能基准表 | < 50ms（首次请求就绪） |

**问题**：性能基准表只写了 <50ms，与 M5 的 <5ms 相差 10 倍，且基准表无里程碑上下文标注，读者无法判断哪个是最终目标。

**处理结果**：性能基准测试表已新增"适用里程碑"列：
- `< 50ms`（无 pool 预热，从磁盘加载）标注适用 **M4**
- `< 5ms`（pool 预热后，快照已在内存）标注适用 **M5b**

---

## 二、技术细节不足

### 2.1 M4 4.6 squash 期间原快照保持可用的原子替换机制未说明

**问题**：squash 将深度 >8 的快照链合并为单层，期间并发 `StartFromSnapshot` 仍需读取旧链。原子替换方案对磁盘空间峰值消耗和并发正确性影响很大，计划未说明。

**处理结果**：M4 4.6 已补充原子替换机制：squash 结果写入临时目录 → `rename()` 原子更新 `metadata.json`（指向新单层）→ 旧层标记 `pending-delete` → 引用归零后异步清理；squash 期间并发 `StartFromSnapshot` 读取旧链，不受影响。

---

### 2.2 M5 5.2 Pre-zeroed Page Pool 依赖 `MADV_POPULATE_WRITE`（Linux ≥ 5.14），未说明内核版本要求

**问题**：`MADV_POPULATE_WRITE` 于 Linux 5.14 引入。若宿主机内核低于该版本，syscall 静默失败（返回 `EINVAL`），Pre-zeroed 预热效果丢失，性能目标静默不达标而无报错。

**处理结果**：M5a.2 已注明"**要求宿主机内核 ≥ 5.14**"；Manager 启动时检测内核版本，低于 5.14 时打印 `WARN` 并降级为普通 `mmap`，同时下调性能预期。风险表已新增对应风险项。

---

### 2.3 M2 验收标准缺乏测试环境规格，与测试计划性能基准环境不一致

**问题**：测试计划性能基准表有完整环境规格（Cloud Hypervisor latest、Linux 5.15 裁剪内核 <4MB、256MB VM、NVMe SSD），但 M2 验收标准未引用，导致验收结果可复现性不足。

**处理结果**：M2 验收标准末尾已补充"以性能基准测试参考环境为准（见测试计划§性能基准测试）"。

---

### 2.4 Manager 并发模型和限流策略全文未说明

**问题**：计划多处涉及并发场景，但全文未指定异步运行时选型、最大并发 VM 操作数、超出上限时的拒绝策略。

**处理结果**：M1 已新增任务 **1.8 并发模型约束**：明确使用 tokio 异步运行时；设计 `max_concurrent_vm_ops` 配置项；超出上限返回 `RESOURCE_EXHAUSTED`（不排队等待）。

---

### 2.5 M3 验收标准 WatchSandbox 1s 延迟在 Serverless 计费场景精度不足

**问题**：Serverless 场景通常按毫秒计费，退出事件延迟直接影响计费精度。仅有 P99=1s 无法评估典型情况下的延迟分布。

**处理结果**：M3 验收标准已改为"**P50 < 50ms，P99 < 1s**"；并注明 `SANDBOX_EXITED` 事件不用于计费——计费时间由 Manager 内部精确记录，事件延迟不影响账单准确性。

---

## 三、开放性问题与缺失内容

### 3.1 M6 ExecTask 对通用容器不可行（继承自 nanosandbox.md R-2）

详见 1.1 节。

**处理结果**：风险表已新增此风险，缓解策略：采用 fork+exec 模式，父 `/init` 留驻处理 ExecRequest；如需支持通用容器 exec，评估实现轻量 mini-agent 转发层作为降级方案。

---

### 3.2 CRI 模式 E2E 测试需要 Kubernetes 环境，但测试计划未说明搭建要求

**问题**：CRI 模式 E2E 需要 kubelet + containerd（kuasar-io fork）+ kubectl 环境，搭建复杂度远高于"KVM + vsock"环境。计划未说明是否需要完整 K8s 集群、是否复用现有 Kuasar CI 框架、CI 如何自动化搭建此环境。

**处理结果**：M6 验收标准后已补充 CRI 测试环境说明：使用 single-node kubelet（无需完整集群）；复用现有 `.github/workflows/` 中 Kuasar CRI E2E 测试框架；CRI E2E 测试移至每日 CI bare-metal 专用机，PR CI 只运行单元测试 + 基础集成测试。

---

### 3.3 KSM 验收标准"≥ 30% 内存节省"受环境影响大，不宜作为硬性发布阻断标准

**问题**：KSM 节省比例高度依赖 VM 镜像内容、函数代码大小、内存压力、KSM 扫描周期配置。将此不稳定指标纳入发布阻断条件，会造成大量虚假阻断。

**处理结果**：M5b 验收标准已区分硬性指标与软目标：
- **硬性指标**：多租户配置下 KSM 必须为 off（`/sys/kernel/mm/ksm/run=0`）
- **软目标（参考值，不阻断发布）**：100 个同函数实例 KSM 节省内存 ≥ 30%

---

### 3.4 E2E 重启恢复测试未区分 SIGTERM 和 SIGKILL

**问题**：SIGTERM（Manager 可主动 flush 状态）和 SIGKILL（只能依赖已持久化 `meta.json`）是两条完全不同的恢复路径，只测一种无法覆盖崩溃场景。

**处理结果**：集成测试"Manager 重启恢复"和原生模式 E2E 步骤 6 均已拆分为两个用例：
- `kill -TERM <pid>`：验证优雅退出 + 主动 flush 恢复路径
- `kill -KILL <pid>`：验证崩溃 + 依赖持久化 meta.json 恢复路径

---

### 3.5 缺少 proto / state 格式的升级兼容策略

**问题**：M1～M8 多轮迭代中 proto 字段和 `meta.json` 结构可能演变，计划未说明 schema 版本和跨版本迁移策略。

**处理结果**：M1 1.6 已新增要求：`meta.json` **必须包含 `schema_version` 字段**；Manager 启动时检测版本，执行自动迁移或拒绝启动。

---

### 3.6 CI/CD 基础设施要求未说明

**问题**：回归测试策略要求"每日 CI 全部集成测试套件（需 KVM 环境）"，但标准 GitHub Actions 不支持嵌套 KVM，现有 Kuasar 是否有 bare-metal self-hosted runner 未说明，KVM CI 环境成本未在风险表中体现。

**处理结果**：
- 回归测试策略已明确 PR CI vs 每日 CI 边界：PR CI 只跑单元测试 + 基础集成测试；KVM 集成/E2E 测试移至每日 CI 专用机
- 风险表已新增"KVM CI 环境不可用"风险项，缓解措施：评估 bare-metal self-hosted runner 或 KVM-enabled 云实例成本

---

## 四、风险表补充

已向风险表新增以下 3 条（原 6 条 → 现 9 条）：

| 风险 | 影响 | 处理结果 |
|------|------|---------|
| ExecTask 依赖容器进程实现 vsock 协议，对通用容器不可行 | M6 `kubectl exec` 对任意容器无效，CRI 兼容性存疑 | 已添加至风险表，缓解策略：fork+exec 模式 + mini-agent 转发层降级方案评估 |
| `MADV_POPULATE_WRITE` 在内核 < 5.14 不可用 | M5a Pre-zeroed Page Pool 优化静默失效，性能目标静默不达标 | 已添加至风险表，缓解策略：启动时内核版本检测 + WARN + 降级 mmap |
| KVM CI 环境不可用 | 集成测试和 E2E 测试无法在 PR CI 自动运行 | 已添加至风险表，缓解策略：PR CI 只跑单元测试，KVM 测试移至每日 CI 专用机 |

---

*检视人：Claude Code | 检视日期：2026-04-15 | 处理完成日期：2026-04-15 | 基于 nanosandbox-plan.md v1.0 → v1.1*
