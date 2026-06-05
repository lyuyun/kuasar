# How to Snapshot and Restore Workloads

In Kuasar, snapshot and restore allows a workload to be frozen at a known-good point in memory and resumed later without repeating expensive initialisation. Cold-start latency caused by model loading, JVM warm-up, or cache fill is paid once at snapshot time; every subsequent restore inherits the already-warm state.

This guide covers two workload-level snapshot/restore modes:

- **WarmFork** — snapshots a workload after initialisation, before it starts serving. The snapshot is restored repeatedly as independent task instances, each receiving its own task identity via an injection protocol.
- **ContinuationSnapshot** — snapshots a workload after it has started serving, preserving guest memory from the captured point in time. The restored pod resumes a controller-provided workload identity, but the application and platform must handle connection re-establishment, routing, and external consistency.

## Status and scope

Snapshot and restore support is experimental and requires end-to-end validation in your environment before production use.

- **WarmFork** is intended for repeatable cold-start acceleration of quiescent workloads. It is not a general-purpose process cloning API; the workload must cooperate with the WarmFork readiness and injection protocol.
- **ContinuationSnapshot** is intended for controlled restore of stateful workloads from a captured memory image. It is not transparent live migration or transparent failover.
- Cross-node restore requires compatible host CPU features, compatible Kuasar and cloud-hypervisor versions, the same workload image and runtime configuration, accessible template storage, and workload-specific handling for identity, service endpoints, and routing.

## How it works

**WarmFork** follows a four-phase startup protocol. The workload loads its model or runtime (Phase 1), reaches a quiescent state (Phase 2), then blocks on an inject socket (Phase 3). The sandboxer takes the snapshot at Phase 3. Each restore prepares an independent memory backend according to the configured memory restore mode, starts a new VM, reseeds entropy, and delivers task identity to the workload (Phase 4).

```
Template creation (once)          Task instances (many)

workload starts                   ┌── instance A (task-0001) ─┐
  Phase 1: load model             │   inject task_id + context │
  Phase 2: quiesce          ──►   ├── instance B (task-0002) ─┤
  Phase 3: wait ◄─snapshot        │   inject task_id + context │
                                  └── instance C (task-0003) ─┘
```

**ContinuationSnapshot** captures the live guest memory image, including the workload's in-memory session map and model warm state. The restored pod carries the workload identity used to select the captured template. Kuasar restores guest memory, but it does not preserve live TCP connections or automatically update Kubernetes Service, Endpoint, DNS, CNI, ingress, or client routing state.

---

## User Guide

### Prerequisites

Before you begin, ensure the following:

- A Kubernetes cluster is running with Kuasar installed and the `kuasar-vmm` RuntimeClass available. See the [Kuasar installation guide](../README.md).
- `kubectl` is configured to access the cluster (`kubectl cluster-info` returns successfully).
- Docker (or another OCI-compatible builder) is available to build example images.
- Rust toolchain (`cargo`) installed if you intend to build cloud-hypervisor from source.

#### Kuasar sandboxer configuration

The sandboxer reads its configuration from `/var/lib/kuasar/config.toml` by default. The source template for cloud-hypervisor is [`vmm/sandbox/config_clh.toml`](../vmm/sandbox/config_clh.toml). Copy it to the default path on each node before starting the sandboxer:

```bash
sudo mkdir -p /var/lib/kuasar
sudo cp vmm/sandbox/config_clh.toml /var/lib/kuasar/config.toml
```

The snapshot/restore behaviour is controlled by the `[sandbox.snapshot]` section:

```toml
[sandbox.snapshot]
enable_warmfork_restore = true       # required for WarmFork
enable_continuation_restore = false  # required for ContinuationSnapshot; set to true
default_memory_restore_mode = "ondemand"  # copy | ondemand | filebackend | externaluffd
```

Template storage is controlled by `[template_pool]`:

```toml
[template_pool]
store_dir = "/var/lib/kuasar/pool"
```

#### cloud-hypervisor with FileBackend and External UFFD support

Kuasar's VMM sandbox uses [cloud-hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) as its hypervisor backend. The standard v52.0 release supports `copy` and `ondemand` (internal UFFD) memory restore. To also use **FileBackend MMAP** (`memory_restore_mode=filebackend`) or **External UFFD** (`memory_restore_mode=externaluffd`) restore, you must apply the Kuasar patch and build a custom binary.

Follow the build and deployment steps in [`vmm/cloud-hypervisor_TwoNewFeatures_patch/README.md`](../vmm/cloud-hypervisor_TwoNewFeatures_patch/README.md). The patch is based on cloud-hypervisor v52.0. After installing the patched binary, update `[hypervisor].path` in `/var/lib/kuasar/config.toml` and set `default_memory_restore_mode` to the desired mode.

| Value | Meaning | Requires patched cloud-hypervisor |
|---|---|---|
| `copy` | Copy all guest memory before resuming the VM. | No |
| `ondemand` | Restore pages on demand with internal userfaultfd support. | No |
| `filebackend` | Use the snapshot memory file as a file-backed mapping. | Yes |
| `externaluffd` | Delegate page faults to an external userfaultfd handler. | Yes |

### Workload snapshot modes at a glance

| | WarmFork | ContinuationSnapshot |
|---|---|---|
| **Use case** | Many independent task instances from one warm template | Controlled restore of an already-serving workload |
| **Snapshot timing** | Before the workload starts serving traffic | After the workload has been serving traffic |
| **In-memory session state** | Not preserved (each instance starts fresh) | Restored from the captured point in time |
| **Workload identity** | Injected per-instance via the WarmFork protocol | Carried by restore annotations |
| **Typical workload** | Batch inference, short-lived requests | Long-lived stateful services |
| **Snapshot pod annotation** | `kuasar.io/warm-fork-ready-protocol-version: "1"` | None required |
| **Restore pod annotation** | `kuasar.io/snapshot-type: warm-fork` | `kuasar.io/snapshot-type: continuation` |

### WarmFork

A WarmFork workload must follow a four-phase structure:

```
Phase 1  Heavy initialisation
         ├─ load model weights, compile/JIT inference graph, fill caches
         └─ threads, I/O, and network connections: all permitted

Phase 2  Reach quiescent state                          ← mandatory before snapshot
         ├─ join/cancel all worker threads
         ├─ close all outbound network connections
         ├─ wait for in-flight async I/O to complete
         ├─ flush writes to persistent storage
         └─ release file locks

Phase 3  Open inject socket and block on accept()       ← snapshot taken here
         └─ no external side effects until STARTED is sent

Phase 4  Post-restore execution
         ├─ apply env_overrides, re-open connections, re-spawn threads
         └─ execute the restored workload instance
```

The sandboxer declares a restore committed only after receiving `STARTED`. Until then the workload must not perform any externally visible action.

#### Injection mode vs autonomous mode

**Injection mode** (`kuasar.io/task-id` present and non-empty): the sandboxer runs the full `CAPABILITIES → PREPARE → READY → COMMIT → STARTED` handshake, delivering task identity before execution begins.

**Autonomous mode** (`kuasar.io/task-id` absent or empty): the sandboxer sends `COMMIT` directly. The workload self-starts without receiving a task identity.

#### WarmFork annotation reference

| Annotation | Pod | Required | Description |
|---|---|---|---|
| `kuasar.io/warm-fork-ready-protocol-version` | Snapshot | Yes | Must be `"1"`. Declares the workload implements the readiness protocol. |
| `kuasar.io/warm-fork-readiness-socket` | Snapshot | No | Override the default readiness socket path (`/run/warmfork-readiness.sock`). |
| `kuasar.io/snapshot-type` | Restore | Yes | Must be `"warm-fork"`. |
| `kuasar.io/template-key` | Restore | No | Business key for template lookup. At least one of `template-key` or `template-id` is required. |
| `kuasar.io/template-id` | Restore | No | Pin restore to a specific template. Can be used alone or with `template-key`; when both are set, both must match. |
| `kuasar.io/task-id` | Restore | No | Non-empty = injection mode. Omit for autonomous mode. |
| `kuasar.io/task-context` | Restore | No | Opaque string passed to the workload in `PREPARE.context`. |
| `kuasar.io/task-env/<NAME>` | Restore | No | Sets `PREPARE.env_overrides[NAME]` for the restored instance. |

### ContinuationSnapshot

ContinuationSnapshot captures the live guest memory image of a running workload. Unlike WarmFork, the workload does not need to implement the WarmFork readiness protocol. The restored pod must carry the same workload identity as the captured template so Kuasar can find the correct continuation snapshot.

Important limitations:

- ContinuationSnapshot does not preserve live TCP connections.
- ContinuationSnapshot does not automatically update Kubernetes Service, Endpoint, DNS, CNI, ingress, load balancer, or client routing state.
- Applications must handle connection re-establishment, request retry, endpoint switching, and external consistency with databases, queues, object stores, and other side-effecting systems.
- Do not run the original pod and the restored pod as active writers at the same time unless the application is explicitly designed for that topology.
- For most stateful workloads, stop or fence the original pod before exposing the restored pod to traffic.

#### ContinuationSnapshot annotation reference

| Annotation | Pod | Required | Description |
|---|---|---|---|
| `kuasar.io/snapshot-type` | Restore | Yes | Must be `"continuation"`. |
| `kuasar.io/pod-uid` | Restore | Yes | The controller-provided workload identity used to look up the stored template. In production this is normally derived from the original Kubernetes Pod UID. |
| `kuasar.io/workload-generation` | Restore | Yes | The generation counter at snapshot time. Must match the value used during template creation. |

### Choosing between WarmFork and ContinuationSnapshot

**Choose WarmFork** when:

- You have many short-lived or independent task instances to run (batch inference, per-request serving).
- The workload can reach a clean quiescent state before starting to serve.
- Each instance needs its own identity delivered via the injection protocol.
- Cold-start latency (model load time) is the bottleneck you want to eliminate.

**Choose ContinuationSnapshot** when:

- The workload is a long-lived service with accumulated in-memory state such as session history, model warm state, or per-user context.
- You need controlled restore of a running pod's guest memory without restarting the workload from scratch.
- Preserving workload identity across restore is a requirement.

**Do not use snapshot/restore** when:

- The workload cannot safely define a snapshot point or tolerate restore from a previous point in time.
- A WarmFork template would contain tenant-specific data, request payloads, access tokens, TLS session keys, one-time nonces, or other state that must not be inherited by every restored instance.
- The application relies on live TCP connections, kernel-side connection state, external locks, or in-flight non-idempotent operations that cannot be rebuilt after restore.
- The source and destination nodes do not have compatible CPU features, Kuasar versions, cloud-hypervisor versions, images, runtime configuration, and template storage access.

---

## Tutorials

### Run a WarmFork workload

This tutorial walks through building the example WarmFork Python LLM server, snapshotting it, and creating multiple task instances from the template.

> **Config:** ensure `enable_warmfork_restore = true` is set in the `[sandbox.snapshot]` section of `/var/lib/kuasar/config.toml` before proceeding.

#### Step 1: Build the workload image

```bash
# From the repo root
docker build -t example/llm-server:latest examples/warmfork-workload/python/
```

If you are pushing to a private registry:

```bash
docker tag example/llm-server:latest registry.example.com/llm-server:v1.0.0
docker push registry.example.com/llm-server:v1.0.0
```

#### Step 2: Deploy the snapshot pod

The snapshot pod runs the workload through Phases 1–3 and then blocks, waiting for the sandboxer to take the snapshot.

```yaml
# examples/warmfork-workload/k8s/snapshot-pod.yaml
apiVersion: v1
kind: Pod
metadata:
  name: warmfork-snapshot
  annotations:
    kuasar.io/warm-fork-ready-protocol-version: "1"
spec:
  runtimeClassName: kuasar-vmm
  restartPolicy: Never
  containers:
    - name: inference
      image: example/llm-server:latest
      env:
        - name: MODEL_NAME
          value: "llama-3-8b"
```

```bash
kubectl apply -f examples/warmfork-workload/k8s/snapshot-pod.yaml
```

Watch the logs to confirm the workload has reached the ready-waiting state:

```bash
kubectl logs -f warmfork-snapshot
```

Look for log lines like these. The real output includes the timestamp and log level added by Python logging.

```
[phase2] quiescent state reached
[phase3] waiting for WarmFork restore on /run/warmfork-readiness.sock ...
```

#### Step 3: Create the WarmFork template

```bash
kuasar-ctl template create \
  --snapshot-type warm_fork \
  --sandbox-id <running-sandbox-id> \
  --key llm-server-v1
```

Find the `sandbox-id` from the sandboxer. `kuasar-ctl sandbox list` shows the current sandbox IDs; keep only the snapshot pod running while following this tutorial, or use your runtime logs/admin tooling to map the pod to its sandbox ID.

```bash
kuasar-ctl sandbox list
```

Then inspect the template:

```bash
kuasar-ctl template list
kuasar-ctl template get --id <template-id>
```

After the template is stored on disk, the snapshot pod can be deleted:

```bash
kubectl delete pod warmfork-snapshot
```

#### Step 4: Deploy task instances from the template

Each restore pod creates one independent task instance. The sandboxer prepares a per-instance memory backend, starts a new VM, reseeds guest entropy, and runs the injection protocol.

```yaml
# examples/warmfork-workload/k8s/restore-pod.yaml
apiVersion: v1
kind: Pod
metadata:
  name: warmfork-task-0001
  annotations:
    kuasar.io/snapshot-type: "warm-fork"
    kuasar.io/template-key: "llm-server-v1"
    kuasar.io/task-id: "req-0001"
    kuasar.io/task-context: '{"user":"alice","prompt":"Explain quantum computing"}'
    kuasar.io/task-env/LOG_LEVEL: "debug"
    kuasar.io/task-env/MAX_TOKENS: "512"
spec:
  runtimeClassName: kuasar-vmm
  restartPolicy: Never
  containers:
    - name: inference
      image: example/llm-server:latest
```

Apply as many restore pods as needed — each gets its own VM and task identity:

```bash
kubectl apply -f examples/warmfork-workload/k8s/restore-pod.yaml
```

---

### Adapting the examples to your own workload

#### WarmFork: integrating the protocol client

The `warmfork/protocol.go` (Go) and `warmfork.py` (Python) files in the example directories are self-contained protocol clients. Copy the appropriate file into your project and call the helper once the workload is ready to be snapshotted:

**Go:**

```go
params, err := warmfork.WaitForInjection(socketPath)
if err != nil {
    log.Fatal(err)
}
params.ApplyEnvOverrides()
// start serving
```

**Python:**

```python
import warmfork
params = warmfork.wait_for_injection()
params.apply_env_overrides()
# start serving
```

Your workload must reach a quiescent state before calling `WaitForInjection` / `wait_for_injection`. The quiescent contract requires: all worker threads joined or cancelled, all outbound network connections closed, in-flight async I/O completed, non-idempotent timers disarmed, pending writes flushed, and file locks released.

#### ContinuationSnapshot: integrating workload identity

The continuation example uses environment variables (`KUASAR_POD_UID`, `KUASAR_WORKLOAD_GENERATION`) to carry the demo workload identity. Copy `continuation.py` into your project and call `load_identity_from_env()` during initialisation to obtain the workload ID and generation for logging and routing. In production, generate these values from Kubernetes metadata or from your workload controller so they cannot diverge from the restore annotations.

### Security and isolation

Each WarmFork restore creates an independent VM and injects a per-instance task identity. Guest entropy is reseeded during restore, but application state present in memory at the snapshot point is inherited by every restored instance.

Do not create a WarmFork template after loading tenant-specific data, request payloads, access tokens, TLS session keys, one-time nonces, or non-idempotent external state. Before Phase 3, close external connections, release locks, disarm non-idempotent timers, and clear temporary credentials that should not be shared.

ContinuationSnapshot templates are tied to a workload identity and must not be reused across tenants. Treat continuation snapshots like live memory dumps: protect template storage with the same access controls you use for secrets and tenant data.

### Troubleshooting

Useful checks:

```bash
kuasar-ctl sandbox list
kuasar-ctl sandbox get --id <sandbox-id>
kuasar-ctl template list
kuasar-ctl template get --id <template-id>
```

WarmFork issues to check:

- The workload never reaches Phase 3: inspect container logs and confirm it opens the configured readiness socket.
- `READY` or `STARTED` is missing: inspect workload logs and the `kuasar-ctl template create` error; the WarmFork ready-check must complete before the template is created.
- The restore pod starts without a task ID: confirm `kuasar.io/task-id` is present and non-empty.
- The template is not found: confirm `kuasar.io/template-key` matches the key used with `kuasar-ctl template create`, or use `kuasar.io/template-id` to pin a specific template.

ContinuationSnapshot issues to check:

- The template is not found: confirm `kuasar.io/pod-uid` and `kuasar.io/workload-generation` match the values passed to `kuasar-ctl template create`.
- Session state is missing: confirm the request that created the state completed before the template was created.
- Clients still reach the old pod: check Pod IP routing, Service endpoints, ingress configuration, and whether the original pod has been fenced.
