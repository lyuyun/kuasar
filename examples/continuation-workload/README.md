# Continuation Workload Example

This example shows a vLLM-style long-lived chat service restored with
`ContinuationSnapshot`.

The point of the example is not raw throughput. It is to show what continuation
preserves:

- in-memory session state
- request routing state
- warm model state
- original workload identity across a node move

The service is intentionally small and uses only the Python standard library.
It behaves like an OpenAI-compatible chat worker with a simulated model backend.
Replace the simulated backend with a real `vllm`/`transformers` integration if
you want to exercise a real model server.

## Layout

| File | Purpose |
|---|---|
| `python/continuation_server.py` | Minimal vLLM-style chat server with in-memory session state |
| `python/continuation.py` | Small helper for restoring workload identity from pod annotations |
| `python/Dockerfile` | Container image for the example server |
| `k8s/snapshot-pod.yaml` | Pod manifest for creating a continuation snapshot |
| `k8s/restore-pod.yaml` | Pod manifest for restoring from that snapshot |

## Why this workload

This example is a typical AI workload because it mirrors how many online LLM
serving deployments actually behave:

- a model process is warm and expensive to restart
- the server keeps per-session state in memory
- the restored process must continue serving the same logical workload
- the pod identity is tracked by workload annotations, not by a fresh task id

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
   `kuasar-ctl template create --snapshot-type continuation --pod-uid <uid> --generation <n>`.
4. Restore a new pod with `kuasar.io/snapshot-type: continuation`,
   `kuasar.io/pod-uid`, and `kuasar.io/workload-generation` annotations.
5. The restored server continues the same session map and model warm state.

The snapshot pod itself does not need continuation annotations. The workload
identity is supplied when creating the template and again on the restore pod.

## Real vLLM integration

The `continuation_server.py` file is intentionally mockable. To use a real vLLM
stack, replace the simulated backend with:

- model load via `vllm` or `transformers`
- session cache and request history in memory
- a readiness gate that indicates the worker is safe to snapshot

This keeps the example lightweight while still showing the continuation
semantics clearly.
