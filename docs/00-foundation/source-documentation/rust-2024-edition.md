# Rust 2024 Edition Reference

> Stabilized in Rust 1.85.0 (2025-02-20). Urd uses `edition = "2024"`.

## Key Language Changes

### RPIT Lifetime Capture (RFC 3498, 3617)

All in-scope generic parameters (including lifetimes) are now implicitly captured
in return-position `impl Trait`. The old `Captures<>` trick and outlives workaround
(`+ 'a`) are no longer needed.

```rust
// 2021: lifetime NOT captured — needed workaround
// 2024: lifetime captured automatically
fn f(x: &str) -> impl Sized { x.len() }

// Opt out with use<..> syntax (stable since 1.82):
fn f<'a>(x: &'a ()) -> impl Sized + use<> { }  // capture nothing
```

### `let` Chains

Chain `let` patterns with `&&` in `if` and `while`:

```rust
if let Some(first) = iter.next()
    && let Some(second) = iter.next()
{
    // use both
}
```

**Urd opportunity:** Useful in pattern-heavy code (config parsing, plan building).

### Tail Expression Temporary Scope

Temporaries in tail expressions drop before local variables. Fixes longstanding
issues where temporary borrows lived too long.

### `unsafe` Changes

- `unsafe extern "C" { }` required (bare `extern` blocks are errors)
- `#[unsafe(no_mangle)]`, `#[unsafe(export_name)]`, `#[unsafe(link_section)]`
- `unsafe_op_in_unsafe_fn` is now warn-by-default
- `static_mut_refs` is now deny-by-default — use `LazyLock`, `OnceLock`, atomics
- `std::env::set_var` and `std::env::remove_var` are now `unsafe`

### Match Ergonomics Restrictions

Redundant `ref`/`ref mut` in patterns with default binding mode are now errors.
Mixing `mut` with inherited binding modes requires fully explicit patterns.

### Reserved Keywords

- `gen` is reserved (future generators)
- `#"foo"#` guarded string syntax is reserved

## Cargo Changes

### Resolver v3 (automatic with edition 2024)

Rust-version-aware dependency resolution. Dependencies whose `rust-version`
exceeds yours are deprioritized in favor of older compatible versions.

### Cargo.toml Key Names

Only hyphenated forms are valid:
- `dev-dependencies` (not `dev_dependencies`)
- `default-features` (not `default_features`)
- `build-dependencies` (not `build_dependencies`)
- `[package]` (not `[project]`)

## Lint Defaults Changed

| Lint | 2021 | 2024 |
|------|------|------|
| `unsafe_op_in_unsafe_fn` | allow | warn |
| `static_mut_refs` | warn | deny |
| `never_type_fallback_flowing_into_unsafe` | warn | deny |
| `missing_unsafe_on_extern` | allow | error |
| `unsafe_attr_outside_unsafe` | allow | error |
| `deprecated_safe_2024` | allow | error |

## Preferred Patterns

- `let` chains over nested `if let`
- `use<..>` bounds for precise RPIT lifetime control
- Interior mutability (`LazyLock`, `OnceLock`, `Mutex`, atomics) over `static mut`
- `&raw const`/`&raw mut` for raw pointers to statics
- Explicit `unsafe { }` blocks inside unsafe functions

## Urd-Specific Notes

Urd already compiles clean on edition 2024. Key opportunities:
- `let` chains can simplify nested option/result matching
- No `unsafe` code in the project (by convention), so most unsafe changes are irrelevant
- Resolver v3 is active — explains the "compatible versions" messages during `cargo build`
- Consider adding `rustfmt.toml` with `style_edition = "2024"` for editor consistency
