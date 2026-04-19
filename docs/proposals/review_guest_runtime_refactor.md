# Code Review: `vmm: core refactoring — GuestRuntime + SandboxProfile abstractions` (19e6324)

**Date:** 2026-04-19
**Branch:** appliance0419
**Files changed:** 18 (+739 / -217)

---

## Overview

This commit introduces two new abstractions — `GuestRuntime` and `SandboxProfile` — that decouple the
sandbox engine from both the host–guest coordination protocol and the device/lifecycle topology.
It also simplifies `VMFactory` by removing `type Config` and `fn new()`, so callers construct
factories themselves before passing them to `KuasarSandboxer::new()`.

---

## Strengths

- `GuestRuntime` is object-safe and held as `Box<dyn GuestRuntime>`, so the sandbox struct stays
  monomorphic in `V: VM` only. Adding a new runtime requires zero changes to `KuasarSandbox`.
- `RuntimeKind` uses `#[serde(default)]` correctly: a legacy `sandbox.json` without the field
  deserialises to `VmmTask`, preserving recovery for existing sandboxes.
- The `SandboxProfile` design (four behaviour methods: `runtime_kind`, `create_runtime`,
  `needs_virtiofsd`, `task_address`) is clean and extensible.
- Clock-sync and event-forwarding logic has been moved out of `sandbox.rs` into `VmmTaskRuntime`,
  which is the right place for protocol-specific behaviour.
- `VMFactory` is now simpler: callers supply a pre-built factory, removing the awkward `fn new(config)`
  on the trait.
- Serde stability tests for `RuntimeKind` guard against accidental renaming breaking recovery.

---

## Issues

### 1. `runtime` field uses `Option` but is always `Some` after `post_create`

**File:** `vmm/sandbox/src/sandbox.rs:143–198`

`KuasarSandbox.runtime` is `Option<Box<dyn GuestRuntime>>`. The field is set to `Some(VmmTaskRuntime)`
in `create()` (line 198), immediately replaced by `hooks.post_create()` (line 205), and then accessed
via `.ok_or_else()` in `start()`. The `None` branch is described as a programming error and can never
be reached in practice.

The `Option` wrapper adds noise at every call site, risks masking future bugs where runtime is
accidentally not set, and forces callers to write defensive `ok_or_else` error paths.

**Suggestion:** Hold `Box<dyn GuestRuntime>` directly. Initialize it in `create()` from the profile
rather than relying on `post_create` to overwrite the placeholder:

```rust
let profile = &self.config.profile;
let sandbox = KuasarSandbox {
    runtime_kind: profile.runtime_kind(),
    runtime: profile.create_runtime(id),
    ...
};
```

This also eliminates the wasted `VmmTaskRuntime::new()` allocation that is thrown away on the
Appliance path.

---

### 2. `start_clock_sync()` wrapper on `KuasarSandbox` is a one-liner that adds no value

**File:** `vmm/sandbox/src/sandbox.rs:563–568`

```rust
pub(crate) fn start_clock_sync(&self) {
    if let Some(rt) = &self.runtime {
        rt.start_sync_clock(&self.id, self.exit_signal.clone());
    }
}
```

This wraps a single `GuestRuntime` method call. It is only called from
`CloudHypervisorHooks::post_start()`. The wrapper also silently does nothing when `runtime` is
`None`, hiding the bug addressed in issue #1.

**Suggestion:** Remove `start_clock_sync` from `KuasarSandbox` and call `rt.start_sync_clock`
directly from the hooks after acquiring `sandbox.runtime`.

---

### 3. `create()` constructs a `VmmTaskRuntime` that is immediately discarded on non-Standard profiles

**File:** `vmm/sandbox/src/sandbox.rs:197–198`

```rust
runtime_kind: RuntimeKind::VmmTask,
runtime: Some(Box::new(VmmTaskRuntime::new())),
```

`post_create` in `CloudHypervisorHooks` (hooks.rs:45–46) immediately overwrites both fields:

```rust
sandbox.runtime_kind = self.profile.runtime_kind();
sandbox.runtime = Some(self.profile.create_runtime(&sandbox.id));
```

On the Appliance path this allocates and drops a `VmmTaskRuntime` for every sandbox creation.
This is minor today but will become confusing as more profiles are added.

**Resolution:** Covered by suggestion in issue #1.

---

### 4. Recovery path re-implements the `RuntimeKind → GuestRuntime` mapping instead of reusing `SandboxProfile`

**File:** `vmm/sandbox/src/sandbox.rs:452–454`

```rust
let mut runtime: Box<dyn GuestRuntime> = match sb.runtime_kind {
    RuntimeKind::VmmTask => Box::new(VmmTaskRuntime::new()),
};
```

Adding a new `RuntimeKind` variant requires updating this match *and* `SandboxProfile::create_runtime`.
The `SandboxProfile::create_runtime` already encodes exactly this mapping.

The disconnect exists because `SandboxProfile` is not persisted in `sandbox.json` — only
`RuntimeKind` is. However, since `RuntimeKind` and `SandboxProfile` are in 1-to-1 correspondence,
`RuntimeKind` could provide a `create_runtime()` method to centralize the mapping:

```rust
impl RuntimeKind {
    pub fn create_runtime(&self, sandbox_id: &str) -> Box<dyn GuestRuntime> {
        match self {
            Self::VmmTask    => Box::new(VmmTaskRuntime::new()),
            Self::Appliance  => Box::new(ApplianceRuntime::new(sandbox_id)),
        }
    }
}
```

Then recovery becomes:

```rust
let mut runtime = sb.runtime_kind.create_runtime(&sb.data.id);
```

---

## Known Limitations (document, don't block)

- `VmmTaskRuntime` is the only concrete implementation in this commit; the abstraction value is
  not yet visible until the Appliance variant lands in the follow-up commit.
- `SandboxProfile` is not persisted in `sandbox.json`. If the persisted `RuntimeKind` ever diverges
  from the configured profile (e.g., config file changed after sandbox creation), recovery will
  silently use the wrong runtime. Worth a comment noting the assumption.

---

## Summary

| Priority    | Item | Status |
|-------------|------|--------|
| Should fix  | `runtime: Option<…>` — use `Box<dyn GuestRuntime>` directly (#1) | **Fixed** — `KuasarSandbox::runtime` is now `Box<dyn GuestRuntime>`; initialized from `self.config.profile` in `create()` |
| Minor       | Remove `start_clock_sync` wrapper (#2) | **Fixed** — wrapper removed; `post_start` calls `sandbox.runtime.start_sync_clock(...)` directly |
| Minor       | Recovery path should use a `create_runtime` method on `RuntimeKind` (#4) | **Fixed** — `RuntimeKind::create_runtime()` added in `guest_runtime/mod.rs`; recovery uses `sb.runtime_kind.create_runtime(...)` |
| Cosmetic    | Wasted `VmmTaskRuntime` allocation in `create()` (#3) | **Fixed** — resolved by #1; no placeholder allocation |
| Document    | Profile/RuntimeKind divergence assumption | Acknowledged in `sandbox.rs` recovery comment |
