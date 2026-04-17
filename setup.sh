#!/usr/bin/env bash
set -euxo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

git submodule update --init --recursive

EMSDK_VERSION="3.1.74"
third_party/emsdk/emsdk install "${EMSDK_VERSION}"
third_party/emsdk/emsdk activate "${EMSDK_VERSION}"