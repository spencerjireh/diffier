## What this changes

<!-- One or two sentences. What behavior is different after this merges? -->

## Why

<!-- Link the issue if there is one: Closes #123 -->

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --all-features --locked` passes
- [ ] New behavior has a test, or the change is not testable (say which)
- [ ] If `tests/snapshots/` changed, I read the diff and it is intended
- [ ] If `src/hook.rs` changed, it still exits 0 on every path and writes nothing to stdout
