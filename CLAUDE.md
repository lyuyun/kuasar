# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Kuasar is a multi-sandbox container runtime written in Rust. It implements the containerd Sandbox API and supports four sandboxer types: **vmm** (microVM), **quark** (app kernel), **wasm** (WebAssembly), and **runc**. The key optimization is a 1:N process model—one resident sandboxer process manages N pods—instead of the traditional 1:1 shim-per-pod model.

## Build Commands

```bash
# Build all sandboxers
make all

# Build individual sandboxers
make vmm          # MicroVM sandboxer (default: cloud_hypervisor)
make quark        # Quark app kernel sandboxer
make wasm         # WebAssembly sandboxer (default: wasmedge)
make runc         # runC sandboxer

# Build with specific hypervisor/runtime
make vmm HYPERVISOR=qemu
make vmm HYPERVISOR=stratovirt
make wasm WASM_RUNTIME=wasmtime

# Install binaries to /usr/local/bin and assets to /var/lib/kuasar
make install
```

Build variables: `HYPERVISOR` (cloud_hypervisor|qemu|stratovirt), `WASM_RUNTIME` (wasmedge|wasmtime), `KERNEL_VERSION`, `ARCH`, `GUESTOS_IMAGE`, `ENABLE_YOUKI`.

Note: `vmm-task` is cross-compiled for `x86_64-unknown-linux-musl` since it runs as the init process inside VMs.

## Testing and Quality Checks

```bash
# Run all unit tests (requires sudo for namespace operations)
sudo -E $(command -v cargo) test --all-features

# Run all CI checks (check + clippy + fmt)
make check

# Individual checks
cargo check --workspace --all-targets
cargo clippy --workspace -- -D warnings
cargo +nightly fmt --all -- --check   # formatting requires nightly

# Dependency license/ban checks
cargo deny -L warn --all-features check bans

# E2E tests
make test-e2e
make test-e2e-framework   # no service startup required
make verify-e2e           # verify test environment
```

## Rust Toolchain

The project pins to Rust **1.85** (stable) via `rust-toolchain.toml`. Nightly is additionally required for `rustfmt`. Components: `rustfmt`, `clippy`, `llvm-tools`.

## Code Architecture

### Workspace Members

- **`vmm/sandbox`** — VMM sandboxer (host-side). Manages VM lifecycle. Produces three binaries: `cloud_hypervisor`, `qemu`, `stratovirt` (all installed as `vmm-sandboxer`). Contains hypervisor-specific modules in `src/cloud_hypervisor/`, `src/qemu/`, `src/stratovirt/`.
- **`vmm/task`** — In-VM task service. Runs as PID 1 inside microVMs. Handles container operations via ttrpc over vsock (port 1024). Communicates back to sandboxer over vsock.
- **`vmm/common`** — Shared types/utilities between `vmm/sandbox` and `vmm/task`: ttrpc API definitions, mount helpers, storage types, tracing setup.
- **`quark/`** — Quark sandboxer. Manages QVisor/QKernel lifecycle.
- **`wasm/`** — Wasm sandboxer. Feature-gated for `wasmedge` or `wasmtime` runtimes.
- **`runc/`** — runC sandboxer. Optional `youki` feature replaces runc with the Rust-native libcontainer.
- **`shim/`** — Optional compatibility shim (`containerd-shim-kuasar-v2`). Used when running against official containerd v1.7 instead of the kuasar-io fork.
- **`tools/kuasar-ctl`** — CLI tool for sandboxer management.
- **`tests/e2e`** — E2E test suite (excluded from workspace; has its own Cargo.toml).

### Key Abstractions (vmm/sandbox)

The VMM sandboxer is built around three traits in `vmm/sandbox/src/vm.rs`:

- **`VM`** — Interface for a running VM: `start`, `stop`, `attach`/`hot_attach`/`hot_detach` devices, `ping`, `socket_address`, `wait_channel`, `vcpus`, `pids`.
- **`VMFactory`** — Creates `VM` instances from a config and `SandboxOption`.
- **`Hooks<V>`** — Lifecycle callbacks (`post_create`, `pre_start`, `post_start`, `pre_stop`, `post_stop`) invoked around VM state transitions.

`KuasarSandboxer<F: VMFactory, H: Hooks<F::VM>>` in `vmm/sandbox/src/sandbox.rs` implements the containerd `Sandboxer` trait. Each hypervisor backend provides its own `VMFactory` + `Hooks` impl and a `main.rs` in `src/bin/<hypervisor>/`.

### External Dependencies (Forked)

The project patches crates.io with kuasar-specific forks:
- `containerd-sandbox` / `containerd-shim` from `github.com/kuasar-io/rust-extensions`
- `ttrpc` from `github.com/kuasar-io/ttrpc-rust` (branch `v0.7.1-kuasar`)

### Communication Flow (MicroVM)

1. containerd → `vmm-sandboxer` via Unix socket (Sandbox API / ttrpc)
2. `vmm-sandboxer` launches VM, shares host directories via virtiofs or 9p at `/run/kuasar/state`
3. `vmm-sandboxer` → `vmm-task` (in VM) via vsock ttrpc (port 1024 for Task/Sandbox API, port 1025 for debug console)
4. Container IO exported via vsock or UDS

## Commit Message Format

```
<subsystem>: <what changed>

<why this change was made>

Fixes #N
```

Subject line max 70 characters; body lines wrapped at 80. PRs require approval from two maintainers.
