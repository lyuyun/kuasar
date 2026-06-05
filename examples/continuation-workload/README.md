# Continuation Workload Example

This example shows a vLLM-style long-lived chat service restored with
`ContinuationSnapshot`.

The point of the example is not raw throughput. It is to show what continuation
preserves:

- in-memory session state
- warm model state
- workload identity used to select the continuation snapshot

The service is intentionally small and uses only the Python standard library.
It behaves like an OpenAI-compatible chat worker with a simulated model backend.
Replace the simulated backend with a real `vllm`/`transformers` integration if
you want to exercise a real model server.

## Layout

| File | Purpose |
|---|---|
| `python/continuation_server.py` | Minimal vLLM-style chat server with in-memory session state |
| `python/continuation.py` | Small helper for loading the demo workload identity from environment variables |
| `python/Dockerfile` | Container image for the example server |
| `k8s/snapshot-pod.yaml` | Pod manifest for creating a continuation snapshot |
| `k8s/restore-pod.yaml` | Pod manifest for restoring from that snapshot |

## Why this workload

This example is a typical AI workload because it mirrors how many online LLM
serving deployments actually behave:

- a model process is warm and expensive to restart
- the server keeps per-session state in memory
- the restored process must resume from a captured memory image
- the workload identity is tracked by restore annotations, not by a fresh task id

In other words, this is the continuation use case rather than warm-fork
injection.

## Build the image

```bash
# From the repo root
docker build -t example/continuation-vllm:latest examples/continuation-workload/python/
```

## Run locally

```bash
cd examples/continuation-workload/python
KUASAR_POD_UID=550e8400-e29b-41d4-a716-446655440000 \
KUASAR_WORKLOAD_GENERATION=0 \
MODEL_NAME=llama3-simulated \
python3 continuation_server.py
```

The server listens on `:8000` by default and exposes:

- `GET /healthz`
- `POST /v1/chat/completions`
- `POST /v1/sessions/<session_id>`
- `GET /v1/sessions/<session_id>`

## Snapshot / restore flow

1. Start the snapshot pod as a normal workload pod.
2. Let it warm up and build in-memory session state.
3. Create the continuation template from that running sandbox with
   `kuasar-ctl template create --snapshot-type continuation --sandbox-id <sandbox-id> --pod-uid <uid> --generation <n>`.
4. Fence the original workload before exposing the restored pod to traffic.
5. Restore a new pod with `kuasar.io/snapshot-type: continuation`,
   `kuasar.io/pod-uid`, and `kuasar.io/workload-generation` annotations.
6. The restored server resumes the session map and model warm state captured in the template.

The snapshot pod itself does not need continuation annotations. The workload
identity is supplied when creating the template and again on the restore pod.
ContinuationSnapshot does not preserve live TCP connections or update
Kubernetes routing automatically.

## Real vLLM integration

The `continuation_server.py` file is intentionally mockable. To use a real vLLM
stack, replace the simulated backend with:

- model load via `vllm` or `transformers`
- session cache and request history in memory
- a readiness gate that indicates the worker is safe to snapshot

This keeps the example lightweight while still showing the continuation
semantics clearly.
