#!/usr/bin/env bash
# RUSTC_WRAPPER invoked by cargo for every rustc call in the build graph.
#
# Purpose: inject a profile-aware
# `-L <repo>/target/wasm32-unknown-unknown/<profile>/deps`
# for wasm32 compilations so `extern crate rustc_*;` in `src/lib.rs` and the
# `mir_build_verus` fork resolve against the rustc-rlibs artifacts built in
# *this* profile (rather than leaking rmetas from a sibling profile's
# `deps/`, which cargo would reject as E0464 "multiple candidates").
#
# Kind is omitted (defaults to `all`) — `-L dependency=` only participates
# in transitive rmeta lookups, but `extern crate foo;` without a matching
# `--extern` goes through the `crate` / `all` kinds, so we need `all`.
#
# Derives the profile from rustc's own `--out-dir`, so one script covers
# `make dev` / `make release` / `make test` without `.cargo/config.toml`
# needing to know which profile is in flight.
#
# Pass-through for host compilations (build scripts, proc-macros) and for
# any rustc invocation without `--out-dir` (e.g. `--print cfg`).
set -eu
rustc="$1"
shift

target=""
outdir=""
prev=""
for a in "$@"; do
    case "$prev" in
        --target) target="$a" ;;
        --out-dir) outdir="$a" ;;
    esac
    case "$a" in
        --target=*) target="${a#--target=}" ;;
        --out-dir=*) outdir="${a#--out-dir=}" ;;
    esac
    prev="$a"
done

extra=()
marker="/target/wasm32-unknown-unknown/"
if [[ "$target" == "wasm32-unknown-unknown" && "$outdir" == *"$marker"* ]]; then
    prefix="${outdir%%${marker}*}"
    rest="${outdir#*$marker}"
    profile="${rest%%/*}"
    extra=(-L "${prefix}${marker}${profile}/deps")
fi

# `${extra[@]+"${extra[@]}"}` guard: under `set -u`, expanding an empty
# array as `"${extra[@]}"` is unbound on bash < 4.4 (ships on macOS).
exec "$rustc" "$@" ${extra[@]+"${extra[@]}"}
