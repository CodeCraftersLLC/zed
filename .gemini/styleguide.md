# CodeCrafters Zed Fork Code Review Style Guide

Review pull requests for `CodeCraftersLLC/zed` (the Zed / GPUI fork used by Cherry Pick) with a focus on correctness, panic safety, GPUI concurrency discipline, and performance.

## High-Priority Review Focus

### Rust Correctness & Error Handling
- Avoid panicking helpers (`unwrap()`, `expect()` without proven invariants, unchecked indexing) in production code; propagate errors with `?` or handle them explicitly.
- Never silently discard fallible results with `let _ =`; propagate via `?`, log via `.log_err()`, or handle explicitly.
- Every `unsafe` block must document its safety invariant with a clear `// SAFETY:` comment.

### GPUI & Concurrency Discipline
- Never re-entrantly update an `Entity<T>` while it is already being updated (panics at runtime).
- Inside entity/window closures, use the inner `cx` rather than an outer `cx`.
- Spawned `Task<R>` handles must be awaited, stored, or explicitly detached (`.detach()` / `.detach_and_log_err(cx)`); flag accidentally dropped tasks.
- Avoid blocking work, excessive allocations, or cloning large structures inside `Render::render` hot paths.
- In GPUI tests, prefer `cx.background_executor().timer(...)` over untracked timers like `smol::Timer::after(...)` when relying on `run_until_parked()`.

### Cherry Pick Submodule Compatibility
- Watch for breaking public GPUI / editor API or behavioral changes that could regress embedding in Cherry Pick (`cherrypick-ui`), especially around focus, scrolling, text input, windowing, and embedded editor settings.

## What to Skip
- Do not comment on formatting (`rustfmt`) or trivial style nits already checked by CI.
- Do not suggest unrelated refactors outside the PR's scope.
