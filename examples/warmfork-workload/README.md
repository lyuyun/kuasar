# WarmFork Workload Examples

Examples demonstrating how to adapt a workload to the Kuasar WarmFork protocol.
See [`docs/proposals/warmfork_readiness_and_injection_protocol.md`](../../docs/proposals/warmfork_readiness_and_injection_protocol.md)
for the full protocol specification.

These examples intentionally cover the single-container pod mode only. Do not
set `kuasar.io/warm-fork-containers` in the example snapshot or restore pods;
omitting that annotation selects single-container mode.

## What is WarmFork?

WarmFork snapshots a workload after expensive initialisation (model load, JVM
warm-up, cache fill) and restores it many times as independent task instances.
Each restored instance either receives its unique task identity through the
injection protocol or self-starts in autonomous mode, then begins executing
with zero cold-start cost.

```
┌─────────────────────────────────────────────────────────────────┐
│  Template creation (once)          Task instances (many)        │
│                                                                  │
│  workload starts                   ┌── instance A (task-0001) ─┐│
│    Phase 1: load model             │   inject task_id + context ││
│    Phase 2: quiesce          ──►   ├── instance B (task-0002) ─┤│
│    Phase 3: wait ◄─snapshot        │   inject task_id + context ││
│                                    └── instance C (task-0003) ─┘│
└─────────────────────────────────────────────────────────────────┘
```

## Snapshot creation flow

WarmFork always starts from a running workload pod. The snapshot pod is the pod
that reaches the ready-waiting state and is then snapshotted by the sandboxer.

1. Apply `k8s/snapshot-pod.yaml` and wait for the workload to finish Phase 1
   and Phase 2.
2. Confirm that the workload is blocked on the inject socket and the sandboxer
   has accepted it as ready.
3. Create the template from that running sandbox with the admin CLI:

   ```bash
   kuasar-ctl template create \
     --sandbox-id <running-sandbox-id> \
     --key llm-server-v1
   ```

4. After the template exists, apply `k8s/restore-pod.yaml` to create task
   instances from it.

The snapshot pod itself only needs the readiness annotation. The
`kuasar.io/template-key` value is provided when creating the template and is
reused by the restore pod.

## Examples

### `go/` — Go HTTP inference service

A Go HTTP server that loads a model in Phase 1, quiesces in Phase 2, waits for
restore commit in Phase 3, then serves inference requests. In injection mode it
tags responses with the injected `task_id`; in autonomous mode the `task_id`
is empty.

| File | Purpose |
|---|---|
| `warmfork/protocol.go` | Reusable WarmFork protocol client — copy into your project |
| `main.go` | Example HTTP inference service using the client |
| `go.mod` | Go module (requires Go 1.22+, no external dependencies) |
| `Dockerfile` | Two-stage build: compiles on `golang:1.22`, runs on `distroless/static` |

**Build the container image:**

```bash
# From the repo root:
docker build -t example/warmfork-go-service:latest examples/warmfork-workload/go/
```

**Run locally** (for testing Phase 1–3 startup without Kuasar):

```bash
cd examples/warmfork-workload/go
go build -o inference-service .
./inference-service
# Process blocks on /run/warmfork-readiness.sock — use the test script in the
# README to inject a task manually.
```

### `python/` — Python LLM inference server

A Python HTTP inference server modelled on the startup pattern of frameworks
such as vLLM and TorchServe.  Uses only the standard library; replace the
`SimulatedModel` class with a real `AutoModelForCausalLM` or `AsyncLLMEngine`.

| File | Purpose |
|---|---|
| `warmfork.py` | Reusable WarmFork protocol client — copy into your project |
| `llm_server.py` | Example LLM server using the client |
| `Dockerfile` | Single-stage build on `python:3.12-slim`, runs as `nobody` |

**Build the container image** (this is the image referenced by `k8s/*.yaml`):

```bash
# From the repo root:
docker build -t example/llm-server:latest examples/warmfork-workload/python/
```

**Run locally:**

```bash
cd examples/warmfork-workload/python
MODEL_NAME=gpt2-simulated python3 llm_server.py
# Process blocks on /run/warmfork-readiness.sock.
```

**Replacing the simulated model** with a real one (e.g. Hugging Face):

```python
# In llm_server.py, replace SimulatedModel with:
from transformers import AutoTokenizer, AutoModelForCausalLM

class RealModel:
    def __init__(self, name: str) -> None:
        self.name = name

    def load(self) -> None:
        self.tokenizer = AutoTokenizer.from_pretrained(self.name)
        self.model = AutoModelForCausalLM.from_pretrained(
            self.name, device_map="auto"
        )

    def warmup(self) -> None:
        self.generate("warmup", max_tokens=1)

    def generate(self, prompt: str, max_tokens: int = 64) -> str:
        inputs = self.tokenizer(prompt, return_tensors="pt").to(self.model.device)
        output = self.model.generate(**inputs, max_new_tokens=max_tokens)
        return self.tokenizer.decode(output[0], skip_special_tokens=True)
```

Add `transformers accelerate` to your `requirements.txt` and update `FROM` in
the Dockerfile to include `pip install -r requirements.txt`.

### `k8s/` — Kubernetes pod manifests

| File | Purpose |
|---|---|
| `snapshot-pod.yaml` | Running workload pod that reaches the ready-waiting state before snapshot creation |
| `restore-pod.yaml` | Restore pod with WarmFork task annotations (`kuasar.io/snapshot-type: warm-fork`, optional `kuasar.io/task-id`, `kuasar.io/task-context`) |

Both manifests reference `example/llm-server:latest`.  Replace this with your
own registry path before applying, e.g.:

```bash
# Build and push
docker build -t registry.example.com/llm-server:v1.0.0 examples/warmfork-workload/python/
docker push registry.example.com/llm-server:v1.0.0

# Update manifests
sed -i 's|example/llm-server:latest|registry.example.com/llm-server:v1.0.0|g' \
    examples/warmfork-workload/k8s/*.yaml

kubectl apply -f examples/warmfork-workload/k8s/snapshot-pod.yaml
```

## Four-phase workload structure

Every WarmFork workload must follow this structure:

```
Phase 1  Heavy initialisation
         ├─ load model weights from disk / object storage
         ├─ compile/JIT the inference graph
         ├─ fill in-memory caches
         └─ threads, I/O, and network connections: all permitted

Phase 2  Reach quiescent state                          ← mandatory before snapshot
         ├─ join / cancel all worker threads
         ├─ close all outbound network connections
         ├─ wait for in-flight async I/O to complete
         ├─ disarm non-idempotent timers
         ├─ flush writes to persistent storage
         ├─ release file locks
         └─ install deferred signal handlers

Phase 3  Open inject socket and block on accept()       ← snapshot taken here
         ├─ workload is frozen by the hypervisor
         ├─ memory is CoW-copied to a new VM
         ├─ workload resumes waiting in accept()
         └─ NO external side effects until STARTED is sent

Phase 4  Post-restore execution
         ├─ apply env_overrides
         ├─ re-open network connections
         ├─ re-spawn worker threads
         └─ execute the restored workload instance (task_id may be empty)
```

Restore pods can also use autonomous mode. In that case `kuasar.io/task-id`
is omitted and the workload self-starts after `COMMIT`.

## The Commit Point

**STARTED is the commit point.** The sandboxer considers a restore committed
only after receiving STARTED. Until STARTED is sent, the workload must not
perform any externally visible action (open connections, write to storage, send
RPCs).

```
workload                            sandboxer
────────                            ─────────
CAPABILITIES ─────────────────────►
                                ← PREPARE
validate payload
  ↑ no external effects here
READY ────────────────────────────►
                                ← COMMIT
STARTED ──────────────────────────► restore committed
open connections, start serving
```

## Reusing the protocol client

The `warmfork/protocol.go` and `warmfork.py` files are self-contained. To use
them in your own workload:

**Go**: copy the `warmfork/` directory into your module, import it, and call:

```go
params, err := warmfork.WaitForInjection(socketPath)
if err != nil { log.Fatal(err) }
params.ApplyEnvOverrides()
// now start serving
```

**Python**: copy `warmfork.py` next to your application and call:

```python
import warmfork
params = warmfork.wait_for_injection()
params.apply_env_overrides()
# now start serving
```

## Testing the protocol without Kuasar

You can test your workload's Phase 3 behaviour by running the same framed
WarmFork v1 handshake the guest agent uses:

```python
import json, socket, struct

SOCKET = "/run/warmfork-readiness.sock"

def framed(obj):
    body = json.dumps(obj).encode()
    return struct.pack(">I", len(body)) + body

def read_framed(conn):
    n, = struct.unpack(">I", conn.recv(4))
    return json.loads(conn.recv(n))

conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
conn.connect(SOCKET)

caps = read_framed(conn)
print("CAPABILITIES:", caps)

conn.sendall(framed({
    "type": "PREPARE",
    "task_id": "test-001",
    "env_overrides": {"LOG_LEVEL": "debug"},
    "context": "test run",
}))

ready = read_framed(conn)
print("READY:", ready)

conn.sendall(framed({"type": "COMMIT"}))

started = read_framed(conn)
print("STARTED:", started)
conn.close()
```
