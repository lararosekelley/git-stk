#!/bin/sh
# git-stk one-line installer - https://github.com/lararosekelley/git-stk
#
# This is the source of truth for the script served at
# https://larakelley.com/sh/git-stk. It is a thin wrapper: it downloads the
# cargo-dist-generated installer (git-stk-installer.sh) attached to the latest
# GitHub release and runs it, forwarding any arguments. The release artifacts
# ship .sha256 checksums if you prefer to download and verify a binary by hand.
set -eu
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/lararosekelley/git-stk/releases/latest/download/git-stk-installer.sh \
  | sh -s -- "$@"
