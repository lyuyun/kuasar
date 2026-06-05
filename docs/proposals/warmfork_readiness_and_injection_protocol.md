# WarmFork Readiness and Injection Protocol

> **Status: Implemented v1** — the current codebase implements the framed
> `CAPABILITIES/PREPARE/READY/COMMIT/STARTED` restore handshake described here.
> Canonical constants and message types live in
> `vmm/sandbox/src/warmfork/protocol.rs`,
> `vmm/common/src/protos/sandbox.proto`, and
> `vmm/common/src/warmfork/message.rs`.

---

## Overview

This document defines the WarmFork-specific readiness and injection protocol that extends the broader snapshot/restore contract in `snapshot_restore.md`. It describes how a process that has completed expensive initialisation (model loading, JVM warm-up, cache fill) can be snapshotted and restored repeatedly as independent task instances with zero cold-start latency. The design centers on a **readiness standard** that defines when a workload may be snapshotted. A separate **injection protocol** supplies task identity after restore.

The two phases are intentionally asymmetric:

- `kuasar.io/warm-fork-ready-protocol-version` is a **snapshot-pod** readiness assertion. It tells the sandboxer that the workload is blocked on the inject socket and is safe to snapshot.
- `kuasar.io/snapshot-type: warm-fork` plus `kuasar.io/template-key` / `kuasar.io/task-id` are **restore-pod** selectors. They tell the runtime which template to restore and whether to enter injection mode.
- The restore pod does **not** repeat `kuasar.io/warm-fork-ready-protocol-version`; readiness is validated when the snapshot is created, not when the task instance is restored.

Entropy reseed must happen before any guest-side operation that can consume randomness or derive identifiers from the guest entropy pool. In the current restore flow that means reseeding before task injection begins and before any guest-side setup path that depends on the guest's random state.

The protocol has two topologies:

- **Single-container**: one workload container in the Pod joins the injection protocol (default, no extra declaration required).
- **Multi-container**: multiple workload containers join the injection protocol; the sandboxer uses a two-phase pre-commit barrier to prevent partial startup before COMMIT.

Single-container mode is selected by omitting `kuasar.io/warm-fork-containers`.
Multi-container mode is selected by setting `kuasar.io/warm-fork-containers` to
a comma-separated list of workload container names.

The mechanism has two coupled parts:

**Part 1 — Readiness standard: snapshot (create template)**

```
workload process(es)
  ├─ Phase 1: heavy initialisation (load model, warm runtime)
  ├─ Phase 2: reach quiescent state (close connections, stop threads, flush I/O)
  ├─ Phase 3: open inject socket, block on accept()
  │             ↑ snapshot taken here (all workload containers must be ready)
  └─ [process frozen, memory image written to disk, process resumes waiting]
```

**Part 2 — Restore handshake (instantiate as a task)**

Two restore modes are supported, selected by the `kuasar.io/task-id` annotation on the restore pod:

*Injection mode* (`kuasar.io/task-id` present, non-empty): the sandboxer delivers a task identity to the process before execution begins.

```
sandboxer
  ├─ CoW-copy memory from template, start new VM
  ├─ hot-plug an independent network namespace for this instance
  ├─ reseed_guest_entropy()              ← before any guest-side entropy consumers
  └─ run two-phase injection across all workload targets
                      ↓
workload process(es) (restored)
  ├─ Phase 1 (PREPARE): receive task parameters, validate, reply READY; no side effects
  ├─ [barrier: sandboxer waits until all targets have replied READY]
  ├─ Phase 2 (COMMIT): receive COMMIT, send STARTED, close connection
  └─ begin task execution
```

*Autonomous mode* (`kuasar.io/task-id` absent or empty): the sandboxer sends COMMIT directly; the process self-starts without receiving a task identity.

```
sandboxer
  ├─ CoW-copy memory from template, start new VM
  ├─ hot-plug an independent network namespace for this instance
  ├─ reseed_guest_entropy()              ← before any guest-side entropy consumers
  └─ send COMMIT to all workload targets (no PREPARE/READY phase)
                      ↓
workload process(es) (restored)
  ├─ CAPABILITIES → (EOF / direct COMMIT)
  ├─ COMMIT received → STARTED
  └─ begin self-directed task execution (no injected task_id)
```

---

## Transport

| Property | Value |
|---|---|
| Socket type | Unix domain, `SOCK_STREAM` |
| Default path | `/run/warmfork-readiness.sock` |
| Single-container path override | Pod annotation `kuasar.io/warm-fork-readiness-socket` |
| Per-container path override | Pod annotation `kuasar.io/container/<name>/warm-fork-readiness-socket` |
| Creator | Workload (bind + listen before snapshot) |
| Connector | Sandboxer via guest agent, after entering the container's mount namespace |

Each workload container has its own inject socket. The guest agent enters the container's mount namespace via `setns` before connecting, ensuring the socket path is resolved in the correct filesystem view.

At snapshot creation time every workload target must resolve to a CRI container
ID so the guest agent can enter the target container's mount namespace before
connecting. In single-container mode the target is the pod's only running
container. In multi-container mode every declared container name must resolve;
unresolved names are fatal and snapshot creation is aborted.

---

## Message Framing

All messages use a **4-byte big-endian length prefix** followed by a UTF-8 JSON body. Maximum frame body: 4 MiB.

```
[4 bytes big-endian length][JSON body]
```

---

## Message Types

### CAPABILITIES (workload → sandboxer)

The first message sent after every `accept()` call, including the pre-snapshot probe connection.

```json
{
  "type": "CAPABILITIES",
  "protocol_version": "1",
  "supported_features": ["prepare", "commit", "cancel"]
}
```

| Field | Description |
|---|---|
| `protocol_version` | Must be `"1"` |
| `supported_features` | Optional list of extensions the workload supports; unknown values are ignored |

### PREPARE (sandboxer → workload)

```json
{
  "type": "PREPARE",
  "task_id": "<string, non-empty; injection mode only>",
  "env_overrides": { "KEY": "VALUE" },
  "context": "<opaque string>"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `task_id` | string | no | Unique identifier for this task instance. Absent (or empty) in autonomous mode. |
| `env_overrides` | object | no | Environment variable overrides; the workload applies them after COMMIT |
| `context` | string | no | Opaque context for the workload (e.g. serialised prompt, routing key) |

On receiving PREPARE the workload **validates but does not execute**: it parses parameters and checks resources but must not produce any externally-visible side effect. If validation passes it sends READY; if validation fails it sends REJECT.

PREPARE is sent only in injection mode. In autonomous mode the sandboxer skips PREPARE and sends COMMIT immediately after reading CAPABILITIES.

**Unknown fields must be ignored** (forward-compatibility rule).

### READY (workload → sandboxer)

```json
{ "type": "READY" }
```

The workload has validated the PREPARE payload and **commits to producing no externally-visible side effects before this READY is sent**. It will continue blocking until it receives COMMIT or CANCEL. The sandboxer sends COMMIT only after all targets have replied READY.

### COMMIT (sandboxer → workload)

```json
{ "type": "COMMIT" }
```

All workload targets have replied READY; the sandboxer confirms that this restore can be committed and instructs all targets to begin execution. On receiving COMMIT the workload sends STARTED and then starts executing the task (side effects may now be produced).

### STARTED (workload → sandboxer)

```json
{ "type": "STARTED" }
```

The workload has received COMMIT and is acknowledging it. **This is the formal restore commit point**: the sandboxer locks the lease and treats the restore as committed only after receiving STARTED.

STARTED is sent **before** execution begins; the workload closes the connection and then begins task execution. This ordering ensures the sandboxer records the commit before any side effects are produced.

### REJECT (workload → sandboxer)

```json
{
  "type": "REJECT",
  "reason": "parse_error | invalid_task_id | resource_exhausted",
  "message": "<detail>"
}
```

The workload cannot process the PREPARE payload. On receiving REJECT from any target the sandboxer sends CANCEL to all targets that have already replied READY, then initiates VM rollback.

### CANCEL (sandboxer → workload)

```json
{ "type": "CANCEL", "reason": "rollback" }
```

Sent in two situations:
- **In place of PREPARE**: the workload is still blocking in `accept()` and the sandboxer decides to roll back.
- **After PREPARE, before COMMIT**: another target sent REJECT or timed out; the sandboxer cancels all targets that have already replied READY.

**CANCEL is never sent after COMMIT**: once COMMIT has been issued the restore is committed; termination is handled by VM stop.

CANCEL is **best-effort**: if the workload has already left the wait point it may never be received. The real safety boundary is the "no external side effects before READY" invariant combined with VM-level rollback.

### CANCEL_ACK (workload → sandboxer, optional)

```json
{ "type": "CANCEL_ACK" }
```

The workload may optionally send this after receiving CANCEL to confirm it has acknowledged the rollback. Omitting it does not affect the rollback flow.

---

## Full Sequence

### Single-container (injection mode)

```text
Workload (guest)                        Sandboxer (host, via guest agent)
────────────────                        ─────────────────────────────────
bind(inject_socket)
listen(inject_socket)
  │
  │    ─── pre-snapshot probe (CheckInjectSocket RPC) ───
accept() returns  ◄────────────────────  connect(inject_socket)
→ CAPABILITIES ──────────────────────►  read CAPABILITIES, verify, close
(EOF) → loop back to accept()          [probe must not alter task-related state]
  │  ◄── snapshot taken here ─────────  VM paused → memory forked → VM resumed
  │
  │                                     vm.restore() completes
  │                                     guest agent starts
  │                                     reseed_guest_entropy()    ← before inject
  │
  │    ─── Phase 1: PREPARE ───
accept() returns  ◄────────────────────  connect(inject_socket)
→ CAPABILITIES ──────────────────────►  read CAPABILITIES, verify version
                ◄────────────────────   PREPARE(task_id, env, context)
read PREPARE, validate (no execution)
→ READY ─────────────────────────────►  READY received
  │    ─── Phase 2: COMMIT ───
                ◄────────────────────   COMMIT (single-container: sent immediately)
→ STARTED ───────────────────────────►  restore committed, lease locked
close connection, begin task execution
```

**Rollback path (during PREPARE phase):**

```text
accept() returns  ◄────────────────────  connect(inject_socket)
→ CAPABILITIES ──────────────────────►  read CAPABILITIES
                ◄────────────────────   PREPARE(...)
→ REJECT ────────────────────────────►  REJECT received
                ◄────────────────────   CANCEL (best-effort)
clean up, exit                          rollback_vm() → VM stopped
```

### Single-container (autonomous mode)

```text
Workload (guest)                        Sandboxer (host, via guest agent)
────────────────                        ─────────────────────────────────
bind(inject_socket)
listen(inject_socket)
  │
  │    ─── pre-snapshot probe (CheckInjectSocket RPC) ───
accept() returns  ◄────────────────────  connect(inject_socket)
→ CAPABILITIES ──────────────────────►  read CAPABILITIES, verify, close
(EOF) → loop back to accept()          [probe must not alter task-related state]
  │  ◄── snapshot taken here ─────────  VM paused → memory forked → VM resumed
  │
  │                                     vm.restore() completes
  │                                     guest agent starts
  │                                     reseed_guest_entropy()    ← before commit
  │
  │    ─── Direct COMMIT (no PREPARE/READY) ───
accept() returns  ◄────────────────────  connect(inject_socket)
→ CAPABILITIES ──────────────────────►  read CAPABILITIES, verify version
                ◄────────────────────   COMMIT  (task_id is empty; autonomous mode)
→ STARTED ───────────────────────────►  restore committed, lease locked
close connection, begin self-directed task execution
```

No rollback path exists for autonomous mode: COMMIT is sent immediately after CAPABILITIES; there is no PREPARE/READY phase in which the workload can send REJECT.

### Multi-container (two-phase barrier, injection mode)

```text
Container-A (guest)    Container-B (guest)    Guest Agent / Sandboxer
───────────────────    ───────────────────    ───────────────────────
accept()               accept()
                                               ── Phase 1: concurrent PREPARE ──
→ CAPABILITIES ────────────────────────────►
→ CAPABILITIES ──────────────────────────────────────────────────────►
               ◄─────────────────────────── PREPARE(payload-A)
                              ◄──────────────────────────────────── PREPARE(payload-B)
validate (no execution)
→ READY ────────────────────────────────►
                              validate (no execution)
                              → READY ──────────────────────────────►
                                               [barrier: all targets READY]
                                               ── Phase 2: concurrent COMMIT ──
               ◄─────────────────────────── COMMIT
                              ◄──────────────────────────────────── COMMIT
→ STARTED ──────────────────────────────►
                              → STARTED ────────────────────────────►
                                               restore committed (all STARTED)
begin execution        begin execution
```

**Multi-container rollback (any target sends REJECT):**

```text
                              → REJECT ──────────────────────────────►
                                               send CANCEL to already-READY Container-A
               ◄─────────────────────────── CANCEL
clean up, exit                               rollback_vm() → VM stopped
```

---

## Pre-Commit Barrier and Startup Semantics

Multi-container WarmFork provides a **pre-commit all-or-none barrier**, not a strict atomic startup guarantee:

- The sandboxer sends COMMIT to **all** targets only after every target has replied READY.
- Workloads **must not** produce any externally-visible side effect (network connections, file writes, signals, etc.) before sending READY.
- If any target sends REJECT (or times out) before all targets reach READY, the sandboxer sends CANCEL to already-READY targets and rolls back the VM. This is the rollback path: no target has produced side effects.

**Commit path**: once COMMIT has been issued, the restore is on the commit path and will **not** be rolled back. Each target transitions independently from COMMITTING to STARTED.

**Committed-failure path**: if a target fails to reply STARTED after COMMIT has been sent, the sandboxer stops the entire VM. Some containers may already have begun execution. This is not an atomic failure — it is a committed failure handled by VM stop.

**Commit point**: all targets have replied STARTED. If any target times out waiting for STARTED → VM stop (CANCEL is never sent after COMMIT).

## Target Determinism

WarmFork target selection is part of the snapshot identity.

### Readiness-time target rules

- If `kuasar.io/warm-fork-containers` is absent, the template is created in
  single-container mode. The sandbox must have exactly one running container.
  The template stores one target with an empty `container_name`, the resolved
  container ID, and the pod-level socket path.
- If `kuasar.io/warm-fork-containers` is present, it must contain a non-empty
  comma-separated list of unique container names. Empty entries, duplicate
  names, and all-whitespace values are fatal errors.
- Every declared container name must resolve to a running container ID from the
  OCI annotation `io.kubernetes.container.name`. Any unresolved name is a fatal
  template-create error. The runtime must not silently fall back to an empty
  `container_id` in multi-container mode.
- The template stores the snapshot-time target list as `(container_name,
  container_id, socket_path)`.

### Restore-time target rules

- `kuasar.io/task-id` is **optional**.
  - Absent or empty → **autonomous mode**: the sandboxer sends COMMIT directly; the
    workload self-starts without a task identity.
  - Non-empty → **injection mode**: the sandboxer runs the full PREPARE/READY/COMMIT/STARTED
    protocol and delivers `task_id`, `env_overrides`, and `context` to the workload.
- If the template was created in multi-container mode (`warm_fork_targets` is
  non-empty), the restore pod must set `kuasar.io/warm-fork-containers` with
  exactly the same container-name set. Missing, extra, duplicated, empty, or
  mismatched names are fatal restore errors.
- Restore-time target order is not semantically significant, but the name set
  must match exactly. The host overlays each restore target's `container_id`
  from the template's snapshot-time target with the same `container_name`.
- If the template was created in single-container mode, the restore pod should
  omit `kuasar.io/warm-fork-containers`; the injected request contains exactly
  one target. The host overlays its `container_id` from the template before
  calling the guest agent.

### Guest result validation

The host validates both the number of `InjectTaskResponse.results` entries and
the returned `container_id` set. A response with the right count but the wrong
IDs is a protocol error and the restore is treated as failed.

---

## Error Handling

| Situation | Workload behaviour | Sandboxer behaviour |
|---|---|---|
| EOF (probe closes connection) | loop back to `accept()` | expected, not an error |
| REJECT received | — | send CANCEL to already-READY targets, rollback VM |
| PREPARE parse failure | send REJECT | — |
| Workload blocks indefinitely | — | timeout → treated as REJECT, rollback VM |
| READY never arrives | — | Phase-1 timeout → CANCEL already-READY targets, rollback VM |
| STARTED never arrives | — | Phase-2 timeout → VM stop (no CANCEL) |
| `task_id` absent or empty | — | autonomous mode: COMMIT sent immediately, no PREPARE/READY |
| Version mismatch | — | `check_inject_socket` fails → snapshot aborted |
| container name unresolved at snapshot time | — | template-create aborted |
| restore target set mismatches template target set | — | restore rejected before VM starts |
| inject result `container_id` set mismatches request | — | restore treated as protocol failure |
| container_id not found (guest side) | — | TARGET_NOT_FOUND reported to host |
| setns fails (container exited) | — | NAMESPACE_ERROR reported to host |

---

## Host ↔ Guest Agent RPC

The sandboxer reaches the guest agent over ttrpc. This section defines the wire contract for the two RPCs involved in WarmFork.

### CheckInjectSocket RPC

Called once before `vm.snapshot()` to verify the workload is listening and speaking the correct protocol version.

```proto
message CheckInjectSocketRequest {
  // Targets to probe. For single-container mode this has exactly one entry.
  repeated InjectTarget targets = 1;
}

message InjectTarget {
  string container_id   = 1; // CRI container ID; guest agent uses this to enter the mount namespace
  string socket_path    = 2; // absolute path of the inject socket inside the container's mount ns
}

message CheckInjectSocketResponse {
  // Per-target results. Implementations should preserve request order, but the
  // host treats the target identity as the container_id set.
  repeated TargetCheckResult results = 1;
}

message TargetCheckResult {
  string container_id  = 1;
  TargetCheckStatus status = 2;
  string message       = 3; // human-readable detail on non-OK status
}

enum TargetCheckStatus {
  TARGET_CHECK_OK              = 0;
  TARGET_NOT_FOUND             = 1; // container_id unknown to guest agent
  NAMESPACE_ERROR              = 2; // setns into container's mount ns failed
  CONNECT_FAILED               = 3; // unix connect() to socket_path failed
  PROTOCOL_ERROR               = 4; // CAPABILITIES missing, wrong version, or malformed
}
```

The host side treats any non-OK result as a fatal snapshot error and aborts before calling `vm.snapshot()`.

### InjectTask RPC

Called after each `vm.restore()` to deliver per-task parameters to all workload targets via the two-phase protocol.

```proto
message InjectTaskRequest {
  // One entry per workload target.
  repeated ContainerTask tasks = 1;
  // Phase-1 timeout in milliseconds (waiting for all READY replies).
  uint32 prepare_timeout_ms = 2;
  // Phase-2 timeout in milliseconds (waiting for all STARTED replies after COMMIT).
  uint32 commit_timeout_ms  = 3;
}

message ContainerTask {
  string container_id   = 1; // used for mount-namespace entry
  string socket_path    = 2;
  string task_id        = 3; // empty = autonomous mode; non-empty = injection mode (same value for all targets)
  map<string, string> env_overrides = 4; // per-target effective env (pod-level merged with per-container)
  string context        = 5; // per-target effective context
}

message InjectTaskResponse {
  // Per-target results. Implementations should preserve request order. The host
  // validates both result count and the returned container_id set.
  repeated ContainerInjectResult results = 1;
}

// InjectPhase tells the host which phase the result belongs to, so the
// correct action (rollback vs. VM stop) can be chosen without relying on
// enum ordinals. INJECT_TARGET_NOT_FOUND, INJECT_NAMESPACE_ERROR,
// INJECT_CONNECT_FAILED, and INJECT_INTERNAL_ERROR can occur in either phase.
enum InjectPhase {
  INJECT_PHASE_PREPARE = 0; // failure before COMMIT was sent (connect/setns/PREPARE exchange)
  INJECT_PHASE_COMMIT  = 1; // failure after COMMIT was sent (COMMIT exchange / STARTED wait)
}

message ContainerInjectResult {
  string container_id = 1;
  InjectStatus status = 2;
  InjectPhase phase   = 3; // which phase this result belongs to; use this, not enum ordinal
  string message      = 4;
}

enum InjectStatus {
  INJECT_STARTED          = 0; // workload sent STARTED; restore committed
  INJECT_REJECT           = 1; // workload sent REJECT during PREPARE (always PHASE_PREPARE)
  INJECT_CONNECT_FAILED   = 2; // could not connect to inject socket
  INJECT_NAMESPACE_ERROR  = 3; // setns into container's mount ns failed
  INJECT_PROTOCOL_ERROR   = 4; // unexpected message or parse failure
  INJECT_TIMEOUT_PREPARE  = 5; // READY never received within prepare_timeout_ms (always PHASE_PREPARE)
  INJECT_TIMEOUT_COMMIT   = 6; // STARTED never received within commit_timeout_ms (always PHASE_COMMIT)
  INJECT_TARGET_NOT_FOUND = 7; // container_id not found in guest agent
  INJECT_INTERNAL_ERROR   = 8; // any other guest-side error
}
```

**Aggregation rules** — use the `phase` field, not enum ordinals:
- `phase = INJECT_PHASE_PREPARE` and `status != INJECT_STARTED`: the host sends CANCEL to already-READY targets and rolls back the VM. COMMIT has not been sent; no side effects have occurred.
- `phase = INJECT_PHASE_COMMIT` and `status != INJECT_STARTED`: the host stops the VM. COMMIT has already been sent; rollback is not possible.
- `INJECT_TARGET_NOT_FOUND`, `INJECT_NAMESPACE_ERROR`, `INJECT_CONNECT_FAILED`, and `INJECT_INTERNAL_ERROR` can occur in either phase — the guest sets `phase` accordingly; the host must not infer phase from the status code.
- The host reports all per-target statuses to the caller regardless of the action taken.

---

## Snapshot Quiescent Contract

The workload **must** satisfy all of the following conditions at snapshot time. Violating any single condition results in undefined behaviour that the runtime cannot detect.

| # | Requirement | Risk if violated |
|---|---|---|
| 1 | No business worker threads or external-I/O threads (see note below) | Threads resume mid-operation and may corrupt shared data structures |
| 2 | No active outbound network connections | Sockets open at snapshot time are broken after restore → `ECONNRESET` / `EPIPE` |
| 3 | No in-flight async I/O (`io_uring`, `epoll`, `aio_*`) | Events re-delivered after restore may belong to the wrong task instance |
| 4 | No armed non-idempotent timers | Timers fire after restore with an apparent zero deadline |
| 5 | No pending signals | Signals re-delivered after restore may interrupt task execution |
| 6 | All mutable state committed to persistent storage | Two instances fork from the same uncommitted in-memory state |
| 7 | No process-level file locks (`fcntl`/`flock`) | Two instances each hold the same lock, silently violating mutual exclusion |

**Note on threads (condition 1):** JVM, Python runtime, and model-serving frameworks typically keep background threads running (GC, JIT, health-check). These are permitted **if and only if** all three of the following hold: (a) no outbound network connections, (b) no task-specific identity or state, and (c) no non-idempotent state advancement. Business worker threads and any thread that owns an external connection or task identity must be stopped. When in doubt, stop the thread.

The `kuasar.io/warm-fork-ready-protocol-version: 1` annotation is the workload's machine-readable assertion that all conditions above are satisfied. The sandboxer runs the `CheckInjectSocket` probe before `vm.snapshot()` but cannot verify conditions 1–7.

### Sidecar constraints

All processes in a Pod fork from the same memory snapshot, so the Quiescent Contract applies to **every process** simultaneously, whether or not it is declared as a workload target.

- **Sidecars participating in the injection protocol**: declared in `warm-fork-containers`; implement the full PREPARE/READY/COMMIT/STARTED flow and start atomically with the workload.
- **Sidecars not participating in the injection protocol**: not declared in `warm-fork-containers`; must be quiescent before the snapshot and resume autonomously after restore. These sidecars must not hold connections that require task-level identity.
- **Sidecars that cannot meet the Quiescent Contract** (e.g. Envoy holding an xDS connection): must exit before the snapshot and be restarted by the guest agent after restore; or the entire Pod must use `EnvironmentSnapshot` instead.

In practice this means standard service-mesh sidecars are not WarmFork candidates unless they can also satisfy the quiescent contract. The workload is only eligible when every process in the Pod can be either quiescent or explicitly included in the injection protocol.

---

## Workload Adaptation Guide

### Recommended program structure

**Injection mode** (task-id present at restore time):

```
main()
  ├─ Phase 1: heavy initialisation (model load, JVM warm-up, cache fill)
  │     threads, I/O, and network connections are all permitted here
  │
  ├─ Phase 2: reach quiescent state
  │     join all worker threads
  │     close all outbound network connections
  │     wait for all async I/O to complete
  │     disarm non-idempotent timers
  │     flush all pending writes to persistent storage
  │     release all process-level file locks
  │     install signal handlers that defer or discard signals until after COMMIT
  │
  ├─ Phase 3: open inject socket, block waiting    ← snapshot taken here
  │     accept() → CAPABILITIES → PREPARE → READY
  │     → [block waiting for COMMIT or CANCEL]
  │     → COMMIT received → STARTED → close connection
  │     (probe connection: receive EOF, loop back to accept())
  │
  └─ Phase 4: post-injection execution (after STARTED has been sent)
        apply env_overrides
        re-open network connections, re-spawn worker threads
        execute task with injected task_id and context
```

**Key invariant**: no externally-visible side effect may be produced before READY is sent; execution begins only after STARTED is sent.

**Autonomous mode** (task-id absent at restore time):

```
main()
  ├─ Phase 1: heavy initialisation (model load, JVM warm-up, cache fill)
  │     threads, I/O, and network connections are all permitted here
  │
  ├─ Phase 2: reach quiescent state  (same requirements as injection mode)
  │
  ├─ Phase 3: open inject socket, block waiting    ← snapshot taken here
  │     accept() → CAPABILITIES → COMMIT received → STARTED → close connection
  │     (probe connection: receive EOF, loop back to accept())
  │     Note: no PREPARE/READY exchange; task_id and context are empty
  │
  └─ Phase 4: self-directed execution (after STARTED has been sent)
        re-open network connections, re-spawn worker threads
        execute task using self-determined identity (e.g. from environment or config)
```

The same inject socket and CAPABILITIES exchange is used in both modes; the workload implementation only needs to handle COMMIT appearing without a prior PREPARE.

### Example implementations

Complete, runnable examples live in [`examples/warmfork-workload/`](../../examples/warmfork-workload/):

| Directory | Language | What it shows |
|---|---|---|
| `go/` | Go 1.22+ | HTTP inference service: model load, background goroutine lifecycle, deferred signal handler, `net/http` server |
| `python/` | Python 3.12+ | LLM inference server modelled on vLLM / TorchServe: `SimulatedModel`, `MetricsThread`, deferred signals, `http.server` |
| `k8s/` | — | Single-container Pod manifests for snapshot and restore |

The example manifests intentionally omit `kuasar.io/warm-fork-containers`; they
exercise the single-container path only.

Each example includes a self-contained, copyable protocol client:

| File | Copy into your project |
|---|---|
| `go/warmfork/protocol.go` | Drop the `warmfork/` package into your Go module |
| `python/warmfork.py` | Copy next to your Python application |

### Go integration

```go
import "your-module/warmfork"

// Phase 2 — stop all goroutines, close all connections
// ...

// Phase 3 — open inject socket; VM snapshot is taken here.
// Returns after STARTED has been sent (no external effects before READY).
params, err := warmfork.WaitForInjection(socketPath)
if err != nil {
    log.Fatalf("injection failed: %v", err)
}

// Phase 4 — STARTED sent; restore committed.
params.ApplyEnvOverrides()
runTask(params.TaskID, params.Context)
```

`WaitForInjection` handles the injection-mode protocol loop internally: CAPABILITIES → probe-EOF (loops to `accept()`) / PREPARE validation → READY → wait for COMMIT → STARTED; also handles REJECT and CANCEL. Returns after STARTED has been sent. See [`go/main.go`](../../examples/warmfork-workload/go/main.go) for a complete example.

### Python integration

```python
import warmfork

# Phase 2 — stop background threads, close all connections
# ...

# Phase 3 — open inject socket; VM snapshot is taken here.
# Returns after STARTED has been sent (no external effects before READY).
params = warmfork.wait_for_injection(socket_path)

# Phase 4 — STARTED sent; restore committed.
params.apply_env_overrides()
run_task(params.task_id, params.context)
```

`wait_for_injection` handles the injection-mode protocol loop internally: CAPABILITIES → probe-EOF / PREPARE → READY → COMMIT → STARTED; returns after STARTED has been sent. See [`python/llm_server.py`](../../examples/warmfork-workload/python/llm_server.py) for a complete LLM server example. Replace `SimulatedModel` with a real `AutoModelForCausalLM` or `AsyncLLMEngine`.

---

## Forward Compatibility

These rules apply to **all** messages in the protocol:

| Situation | Required behaviour |
|---|---|
| Unknown JSON field in any message | **Must be ignored** (both sides) |
| Unknown `type` value received | Treat as a fatal protocol error; log and close the connection |
| Unknown string in `supported_features` | **Must be ignored** by the receiver |
| `supported_features` entry the sandboxer requires but workload does not advertise | Sandboxer aborts restore with a version mismatch error |

Future protocol revisions introduce new optional capabilities via `supported_features`; `protocol_version` is bumped only when a new capability becomes mandatory.

---

## Template Compatibility

`protocol_version = "1"` is a **necessary but not sufficient** condition for safe restore. A workload restored from the wrong template (different binary, different model weights, different runtime version) will inject successfully yet produce incorrect results. Implementers should record additional metadata in `kuasar.io/template-key` or a sidecar registry to prevent mismatched restores.

The sandboxer does not currently enforce workload binary identity or model digest; that remains the operator's responsibility.

Minimum metadata to track alongside a template:
- `protocol_version` (enforced by `CheckInjectSocket`)
- workload target list: container name, snapshot-time container_id, inject socket path
- Workload binary version or image digest
- Any runtime-specific properties (e.g. model version, cache-warm state tag)

---

## Pod Annotations Reference

### Snapshot pod annotations

| Annotation | Value | Required |
|---|---|---|
| `kuasar.io/snapshot-type` | `warm-fork` | no, restore pod only |
| `kuasar.io/template-key` | any string (no path separators) | yes, restore pod only |
| `kuasar.io/warm-fork-ready-protocol-version` | `1` | yes, snapshot pod only |
| `kuasar.io/warm-fork-containers` | comma-separated container names (pod spec container names) | required for multi-container; may be omitted for single-container |
| `kuasar.io/warm-fork-readiness-socket` | socket path | no, single-container mode only (default `/run/warmfork-readiness.sock`) |
| `kuasar.io/container/<name>/warm-fork-readiness-socket` | socket path | no, per-container override (default `/run/warmfork-readiness.sock`) |

**`warm-fork-containers` parsing rules:**
- Trim whitespace around names.
- Reject empty entries, an empty list, duplicate names, and names not present in the running sandbox.
- Absent with a single-container pod: single-container mode, `warm-fork-readiness-socket` pod annotation applies.
- Set with one or more names: multi-container mode, even if the list contains
  only one name.
- Restore from a multi-container template requires the restore annotation to
  declare exactly the same container-name set as the template.

### Restore pod annotations

| Annotation | Value | Required |
|---|---|---|
| `kuasar.io/task-id` | non-empty string | no — absent or empty → autonomous mode; non-empty → injection mode |
| `kuasar.io/task-context` | any string | no — pod-level fallback; becomes the `context` field for all targets |
| `kuasar.io/task-env/<NAME>` | string | no — pod-level env override; applies to all targets |
| `kuasar.io/container/<name>/task-context` | any string | no — per-container override; takes priority over pod-level |
| `kuasar.io/container/<name>/task-env/<NAME>` | string | no — per-container env override; takes priority over pod-level |
| `kuasar.io/warm-fork-containers` | comma-separated container names | required only when restoring from a multi-container template; must match the template's snapshot-time set exactly |

`kuasar.io/warm-fork-ready-protocol-version` is not part of the restore-pod contract. It is validated only while creating the snapshot template. The restore pod uses `kuasar.io/snapshot-type`, `kuasar.io/template-key`, and `kuasar.io/task-id` to select and drive the restored task instance.

**Priority rule**: `per-container key > pod-level key > hard-coded default`.  
`task_id` is pod-level only and cannot be overridden per-container.

**Env override merge**: the effective `env_overrides` map for a container is built as a key-by-key overlay — it is **not** a whole-map replacement:
1. Start with all `kuasar.io/task-env/<NAME>` pod-level entries.
2. For each `kuasar.io/container/<name>/task-env/<NAME>` entry, set (or overwrite) that `<NAME>` in the map.
3. Per-container entries with different `<NAME>` values are additive; they do not remove pod-level entries with other names.

---

## Configuration

```toml
[sandbox.snapshot]
# Enable restore from a WarmFork template (virtio-blk only).
# Requires end-to-end validation in production before enabling.
enable_warmfork_restore = false
```

---

## Protocol State Machine

### Injection mode (task-id present)

```
READY       workload blocked on accept(); VM is safe to snapshot
   │
   │  sandboxer connects, workload sends CAPABILITIES
   ▼
PROBED      workload sent CAPABILITIES; awaiting PREPARE or EOF
   │
   ├──(sandboxer closes connection / EOF)──► READY  (probe; must not alter
   │                                                  task-related state)
   │  sandboxer sends PREPARE
   ▼
PREPARED    workload validating PREPARE; no external side effects yet
   │
   ├──(workload sends REJECT)──► CANCELLED  (sandboxer rollback)
   │
   │  workload sends READY  [no external effects before this point]
   ▼
HOLDING     workload holds task parameters; blocking for COMMIT or CANCEL
   │
   ├──(sandboxer sends CANCEL)──► CANCELLED  (sandboxer rollback, VM stopped)
   │
   │  sandboxer sends COMMIT (after all targets reach HOLDING)
   ▼
COMMITTING  workload received COMMIT; sends STARTED (before any execution)
   │
   │  workload sends STARTED  [commit point: restore formally committed]
   ▼
STARTED     STARTED sent; connection closed; workload begins task execution

CANCELLED   sandboxer sent CANCEL; workload cleaning up; VM will be stopped
```

**Multi-container barrier location**: the sandboxer sends COMMIT to all targets simultaneously only after every target has entered HOLDING. Each target independently transitions to COMMITTING upon receiving COMMIT.

### Autonomous mode (task-id absent or empty)

```
READY       workload blocked on accept(); VM is safe to snapshot
   │
   │  sandboxer connects, workload sends CAPABILITIES
   ▼
PROBED      workload sent CAPABILITIES; awaiting COMMIT or EOF
   │
   ├──(sandboxer closes connection / EOF)──► READY  (probe; must not alter
   │                                                  task-related state)
   │  sandboxer sends COMMIT immediately (no PREPARE)
   ▼
COMMITTING  workload received COMMIT; sends STARTED (before any execution)
   │
   │  workload sends STARTED  [commit point: restore formally committed]
   ▼
STARTED     STARTED sent; connection closed; workload self-starts
```

There is no PREPARED or HOLDING state in autonomous mode — CANCEL is never sent because there is no PREPARE phase in which a REJECT could occur.
