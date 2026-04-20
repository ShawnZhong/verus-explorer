#!/usr/bin/env bash
# RUSTC_WRAPPER invoked by cargo for every rustc call in the build graph.
#
# Two injections, both scoped by crate attributes that cargo passes on every
# invocation:
#
# 1. For wasm32 compiles: a profile-aware
#    `-L <repo>/target/wasm32-unknown-unknown/<profile>/deps`
#    so `extern crate rustc_*;` in `src/lib.rs` and the `mir_build_verus` fork
#    resolve against the rustc-rlibs artifacts built in *this* profile
#    (rather than leaking rmetas from a sibling profile's `deps/`, which
#    cargo would reject as E0464 "multiple candidates"). Kind is omitted
#    (defaults to `all`) — `-L dependency=` only participates in transitive
#    rmeta lookups, but `extern crate foo;` without a matching `--extern`
#    goes through the `crate` / `all` kinds, so we need `all`.
#
#    Derives the profile from rustc's own `--out-dir`, so one script covers
#    `make dev` / `make release` / `make test` without `.cargo/config.toml`
#    needing to know which profile is in flight.
#
# 2. For the two verus proc-macro rlibs (`verus_builtin_macros`,
#    `verus_state_machines_macros`): `--cfg=verus_keep_ghost`. That switches
#    `cfg_erase()` in `builtin_macros/src/lib.rs` to its smart
#    `expand_expr`-based variant — without it, every `verus!` expansion
#    unconditionally erases ghost bodies (proof blocks, assertions) and VIR
#    sees empty function bodies. We can't apply this globally via
#    `.cargo/config.toml` because the vendored `rustc_mir_build` tree fails
#    to compile under `verus_keep_ghost` (it expects the patched stage1
#    rustc's mut_visit trait shape). Scoping by `--crate-name` sidesteps
#    that.
#
# Pass-through for any rustc invocation without `--out-dir` / unknown crate
# (e.g. `--print cfg`).
set -eu
rustc="$1"
shift

target=""
outdir=""
crate_name=""
prev=""
for a in "$@"; do
    case "$prev" in
        --target) target="$a" ;;
        --out-dir) outdir="$a" ;;
        --crate-name) crate_name="$a" ;;
    esac
    case "$a" in
        --target=*) target="${a#--target=}" ;;
        --out-dir=*) outdir="${a#--out-dir=}" ;;
        --crate-name=*) crate_name="${a#--crate-name=}" ;;
    esac
    prev="$a"
done

extra=()
marker="/wasm32-unknown-unknown/"
if [[ "$target" == "wasm32-unknown-unknown" && "$outdir" == *"$marker"* ]]; then
    prefix="${outdir%%${marker}*}"
    rest="${outdir#*$marker}"
    profile="${rest%%/*}"
    extra=(-L "${prefix}${marker}${profile}/deps")
fi

case "$crate_name" in
    verus_builtin_macros|verus_state_machines_macros)
        extra+=(--cfg=verus_keep_ghost)
        ;;
esac

# `${extra[@]+"${extra[@]}"}` guard: under `set -u`, expanding an empty
# array as `"${extra[@]}"` is unbound on bash < 4.4 (ships on macOS).
exec "$rustc" "$@" ${extra[@]+"${extra[@]}"}
