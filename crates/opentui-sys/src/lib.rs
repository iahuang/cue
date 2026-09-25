//! Raw bindings to `libopentui`, the Zig core of [OpenTUI](https://github.com/sst/opentui).
//!
//! Declarations are generated at build time from the Zig `export fn` signatures
//! and `extern struct` definitions, so they always match the library that is
//! linked. See `build/gen.rs`.
//!
//! Objects are addressed by [`Handle`]s: generation-checked `u32`s where `0` is
//! invalid. Calls with a stale or wrongly-typed handle are ignored natively
//! (returning a zero/false/null result), but the handle registry is not
//! synchronized: all calls must come from one thread at a time.

#![no_std]
#![allow(non_camel_case_types, non_snake_case, clippy::missing_safety_doc)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// The handle value every constructor returns on failure.
pub const INVALID_HANDLE: Handle = 0;
