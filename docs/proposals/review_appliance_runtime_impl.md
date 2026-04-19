# Code Review: `vmm: implement appliance mode for Cloud Hypervisor` (ee543d2)

**Date:** 2026-04-19
**Branch:** appliance0419
**Files changed:** 7 (+315 / -4)

---

## Overview

This commit wires up the full cold-start path for Cloud Hypervisor "appliance" sandboxes — VMs that
manage their own filesystem and signal readiness by connecting back to the host over vsock with a JSON
`READY` message, instead of the traditional `vmm-task` ttrpc handshake.

---

## Strengths

- `ApplianceRuntime` implements the existing `GuestRuntime` trait cleanly with no interface changes.
- `APPLIANCE_VSOCK_PORT` constant is defined in `factory.rs` and used to derive both the vsock device
  path and the `agent_socket` URI — no magic literals.
- Module doc-comments on `appliance.rs` explain the reverse-connection direction clearly; the naming
  convention (`task.vsock_<port>`) is non-obvious and would confuse a reader without it.
- `process_config` is extracted as a standalone `async fn` to keep `pre_start` minimal.
- Test coverage is excellent: 10 focused tests cover happy path, sandbox_id mismatch, wrong message
  type, FATAL propagation, invalid JSON, timeout, and reconnect no-op.
- `#[cfg(test)]` timeout constant (1 s vs 30 s) is a good pattern for keeping tests fast.
- `main.rs` uses a single `KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks>` path
  with a profile match only for observability strings — no code duplication.

---

## Issues

### 1. Console log written to `/tmp` instead of `base_dir`

**File:** `vmm/sandbox/src/cloud_hypervisor/factory.rs:92`

```rust
let console_path = format!("/tmp/{}-task.log", id);
```

The standard `CloudHypervisorVMFactory` path uses `base_dir`-relative paths so that all sandbox
artifacts are co-located and cleaned up together. Using `/tmp` means appliance console logs are
orphaned after sandbox deletion and can accumulate across reboots.

**Suggestion:**

```rust
let console_path = format!("{}/{}-task.log", s.base_dir, id);
```

---

### 2. No early error when `image_path` is empty for appliance mode

**File:** `vmm/sandbox/src/cloud_hypervisor/factory.rs:64–65`

```rust
if !self.vm_config.common.image_path.is_empty() {
    let rootfs_device = Pmem::new("rootfs", &self.vm_config.common.image_path, true);
```

Appliance sandboxes require a pmem rootfs (no virtiofsd). If `image_path` is empty the pmem device
is silently skipped and the VM boots without a root disk. The error surfaces as an opaque hypervisor
failure rather than a clear config validation message.

**Suggestion:** Add an early check in `create_vm` for the Appliance profile:

```rust
if matches!(self.profile, SandboxProfile::Appliance)
    && self.vm_config.common.image_path.is_empty()
{
    return Err(anyhow!(
        "appliance mode requires image_path to be set in hypervisor config"
    ).into());
}
```

---

### 3. `unique_socket_path()` can collide in parallel tests

**File:** `vmm/sandbox/src/guest_runtime/appliance.rs:188–193`

```rust
fn unique_socket_path() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("/tmp/kuasar-appliance-test-{}.sock", nanos)
}
```

`subsec_nanos()` resets every second. Two tests starting within the same second produce the same
path, causing one to fail with "address already in use".

**Suggestion:** Use full nanoseconds since epoch:

```rust
fn unique_socket_path() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/tmp/kuasar-appliance-test-{}.sock", nanos)
}
```

---

## Previously Raised Issues — Status Update

| # | Item | Status |
|---|------|--------|
| – | Duplicated `main.rs` arms | **Resolved** — single `KuasarSandboxer` path, profile match only for strings |
| – | Magic `_1024` suffix | **Resolved** — `APPLIANCE_VSOCK_PORT` constant defined and used |
| – | `SandboxMode` missing `Serialize` | **Not applicable** — no `SandboxMode` type exists; was a review error |

---

## Known Limitations (document, don't block)

- **No AF_VSOCK support yet** — `wait_ready` only handles `unix://` addresses; real vsock
  (`AF_VSOCK`) is noted as a follow-up. The guest-side `kuasar-init` implementation must also exist
  before this path is end-to-end functional.
- **Single-connection design** — `UnixListener` accepts exactly one connection. If the guest
  disconnects before sending READY (e.g., VM restart during handshake) there is no retry. Worth
  noting in the struct-level doc comment.
- **`reconnect` address arg is ignored** — `ApplianceRuntime::reconnect` ignores the address
  argument; liveness for appliance sandboxes is confirmed by the PID check in `vm.recover()`.
  A comment has been added at the call site in `sandbox.rs` to document this intent.

---

## Summary

| Priority    | Item | Status |
|-------------|------|--------|
| Should fix  | Console path in `/tmp` (#1) | **Fixed** — `factory.rs:103` uses `{base_dir}/{id}-task.log` |
| Should fix  | Missing `image_path` validation (#2) | **Fixed** — early `Err` return in `create_vm` when profile is Appliance and `image_path` is empty |
| Minor       | Fix `unique_socket_path` collision risk (#3) | **Fixed** — `.subsec_nanos()` → `.as_nanos()` in `appliance.rs` |
| Document    | AF_VSOCK follow-up, single-connection limitation | Documented above (known limitations) |
| Document    | `reconnect` address arg ignored | **Fixed** — comment added at call site in `sandbox.rs` |
