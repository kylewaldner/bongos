# Source this before any build, test or smoke run:  . script/bongos/env.sh
# All heavy output lives in the workspace, so the machine's main disk is never used.
# The workspace is, in order: $BONGOS_WORKSPACE, else the first line of ~/.config/bongos-dev/workspace.
# There is no other default: a wrong guess would put 100 GB in the wrong place.
# Optional, so temp and scratch files can live on another disk. A value already set wins, else the
# first line of the config file, else the default. A configured directory that is missing or not
# writable is an error, never a silent fallback:
#   BONGOS_TMPDIR         becomes TMPDIR (~/.config/bongos-dev/tmpdir; default $BONGOS_WORKSPACE/scratch/tmp)
#   BONGOS_SMOKE_SCRATCH  holds the per-run smoke directories (~/.config/bongos-dev/smoke-scratch;
#                         default $BONGOS_WORKSPACE/scratch/smoke)
#   BONGOS_LOG_DIR        holds the script logs (see lib.sh; default $BONGOS_WORKSPACE/scratch/logs)
if [ -z "${BONGOS_WORKSPACE:-}" ] && [ -f "$HOME/.config/bongos-dev/workspace" ]; then
  IFS= read -r BONGOS_WORKSPACE <"$HOME/.config/bongos-dev/workspace" || true
fi
if [ -z "${BONGOS_WORKSPACE:-}" ]; then
  echo "bongos: no workspace set. Export BONGOS_WORKSPACE=<dir>, or write the directory on the first" >&2
  echo "bongos: line of ~/.config/bongos-dev/workspace (mkdir -p ~/.config/bongos-dev first)." >&2
  return 1 2>/dev/null || exit 1
fi
export BONGOS_WORKSPACE

if [ ! -d "$BONGOS_WORKSPACE" ]; then
  echo "bongos: workspace $BONGOS_WORKSPACE is not available; is the drive mounted?" >&2
  return 1 2>/dev/null || exit 1
fi

export RUSTUP_HOME="$BONGOS_WORKSPACE/rustup"
export CARGO_HOME="$BONGOS_WORKSPACE/cargo"
# A CARGO_TARGET_DIR already set in the environment wins over the workspace default.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$BONGOS_WORKSPACE/target}"
if [ -z "${BONGOS_TMPDIR:-}" ] && [ -f "$HOME/.config/bongos-dev/tmpdir" ]; then
  IFS= read -r BONGOS_TMPDIR <"$HOME/.config/bongos-dev/tmpdir" || true
  BONGOS_TMPDIR=${BONGOS_TMPDIR%$'\r'}
fi
if [ -z "${BONGOS_SMOKE_SCRATCH:-}" ] && [ -f "$HOME/.config/bongos-dev/smoke-scratch" ]; then
  IFS= read -r BONGOS_SMOKE_SCRATCH <"$HOME/.config/bongos-dev/smoke-scratch" || true
  BONGOS_SMOKE_SCRATCH=${BONGOS_SMOKE_SCRATCH%$'\r'}
fi
# A directory chosen by the environment or a config file must already exist (the drive may be unmounted).
_bongos_need_dir() {
  if [ ! -d "$2" ] || [ ! -w "$2" ]; then
    echo "bongos: $1 $2 is not available or not writable; is the drive mounted?" >&2
    return 1
  fi
}
if [ -n "${BONGOS_TMPDIR:-}" ]; then
  _bongos_need_dir "temp directory" "$BONGOS_TMPDIR" || { unset -f _bongos_need_dir; return 1 2>/dev/null || exit 1; }
fi
if [ -n "${BONGOS_SMOKE_SCRATCH:-}" ]; then
  _bongos_need_dir "smoke scratch directory" "$BONGOS_SMOKE_SCRATCH" || { unset -f _bongos_need_dir; return 1 2>/dev/null || exit 1; }
fi
unset -f _bongos_need_dir
export BONGOS_TMPDIR="${BONGOS_TMPDIR:-$BONGOS_WORKSPACE/scratch/tmp}"
export BONGOS_SMOKE_SCRATCH="${BONGOS_SMOKE_SCRATCH:-$BONGOS_WORKSPACE/scratch/smoke}"
export TMPDIR="$BONGOS_TMPDIR"
export GH_CONFIG_DIR="${GH_CONFIG_DIR:-$HOME/.config/gh}"
mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$TMPDIR"

# Jobs: the lower of 90% of the threads and total memory in GB / 2.5 (linking needs a few GB per
# job), at least 2. About 6 on a 15 GB laptop, about 25 on a 64 GB desktop. CARGO_BUILD_JOBS overrides.
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  _threads=$(nproc 2>/dev/null || echo 2)
  _mem_kb=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null)
  _by_cpu=$((_threads * 9 / 10))
  _by_mem=$(( ${_mem_kb:-0} * 10 / 1024 / 1024 / 25 ))
  CARGO_BUILD_JOBS=$_by_cpu
  [ "$_by_mem" -lt "$CARGO_BUILD_JOBS" ] && CARGO_BUILD_JOBS=$_by_mem
  [ "$CARGO_BUILD_JOBS" -lt 2 ] && CARGO_BUILD_JOBS=2
  unset _threads _mem_kb _by_cpu _by_mem
fi
export CARGO_BUILD_JOBS
# No nice by default. BONGOS_NICE=<n> runs cargo under nice -n <n>.
export PATH="$BONGOS_WORKSPACE/bin:$CARGO_HOME/bin:$HOME/.cargo/bin:$PATH"

# Compiler: clang, as upstream builds with it (g++ 11 failed on C++ dependencies that have since left the build).
export CC=clang CXX=clang++

# Tuning with no manifest diff. Never set RUSTFLAGS: it replaces .cargo/config.toml's flags (cargo uses
# the first of CARGO_ENCODED_RUSTFLAGS, RUSTFLAGS, target.*.rustflags, build.rustflags, and never merges them).
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG=0

# Faster dev builds on Linux, two switches with no manifest diff. Both are for dev and test builds only:
# a caller that builds a release or the reference app exports BONGOS_RELEASE_BUILD=1 before sourcing this
# file (bundle-linux and stock-build do), which skips both and removes a value an earlier sourcing set;
# other build scripts that do not opt out get both. Like BONGOS_UNIFIED_FEATURES, the three switches
# BONGOS_RELEASE_BUILD, BONGOS_NO_MOLD and BONGOS_KEEP_SPLIT_DEBUGINFO count only when set to exactly 1.
#   1. mold links x86_64 Linux builds when `mold` is on PATH: CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER
#      names $BONGOS_WORKSPACE/bin/bongos-mold-linker, a two-line wrapper around $CC -fuse-ld=mold that
#      this file writes there (below). Linking is the slow step of a test build with hundreds of binaries
#      and of every app relink, and cargo never links incrementally. The linker setting is separate from
#      the rustflags, so .cargo/config.toml's flags (symbol-mangling-version=v0, --cfg tokio_unstable)
#      still apply. The path is the same for every checkout on purpose: cargo puts the linker string into
#      the fingerprint of every unit, so a path that differed per checkout would rebuild the whole target
#      directory each time two checkouts alternate on it. A linker already set in the environment wins.
#      Opt out with BONGOS_NO_MOLD=1.
#   2. Dev builds write no separate debug files: CARGO_PROFILE_DEV_SPLIT_DEBUGINFO and the build-override
#      variant are `off`, where Cargo.toml says `unpacked` (one .dwo file per codegen unit, which is a lot of
#      small writes). With line-tables-only the file and line tables stay in the binary, so backtraces and
#      panic locations are unchanged; variable inspection in gdb or lldb was already not available with
#      line-tables-only. The `dbg` profile inherits `dev`, so it gets `off` too: its full debug info goes
#      into the binary. Opt out with BONGOS_KEEP_SPLIT_DEBUGINFO=1.
# The first build after either setting changes (also by the opt-outs) recompiles a target directory once,
# because the linker and the debug-info settings are part of cargo's fingerprint.
# The wrapper text. -fuse-ld=mold goes last because rustc itself passes -fuse-ld=lld (its default linker
# since Rust 1.90) and the driver uses the later of two. ${CC:-cc} is unquoted on purpose, so a CC with
# arguments (for example `ccache clang`) works.
_bongos_mold_linker_text() {
  cat <<'WRAPPER_EOF'
#!/bin/sh
# Written by script/bongos/env.sh; edit it there. Links with mold through the C compiler driver.
# shellcheck disable=SC2086
exec ${CC:-cc} "$@" -fuse-ld=mold
WRAPPER_EOF
}
# Writes the wrapper to $1 only when it differs, through a temp file in the same directory and a rename, so
# several shells sourcing this file at once never see a half-written wrapper and an unchanged one keeps its mtime.
_bongos_write_mold_linker() {
  _bongos_mold_dir=$(dirname "$1")
  mkdir -p "$_bongos_mold_dir" || return 1
  if ! _bongos_mold_linker_text | cmp -s - "$1" || [ ! -x "$1" ]; then
    _bongos_mold_tmp=$(mktemp "$_bongos_mold_dir/.bongos-mold-linker.XXXXXX") || return 1
    if _bongos_mold_linker_text >"$_bongos_mold_tmp" && chmod 755 "$_bongos_mold_tmp" && mv -f "$_bongos_mold_tmp" "$1"; then
      :
    else
      rm -f "$_bongos_mold_tmp"
      return 1
    fi
  fi
}
_bongos_mold_linker="$BONGOS_WORKSPACE/bin/bongos-mold-linker"
if [ "${BONGOS_RELEASE_BUILD:-}" != 1 ] && [ "${BONGOS_NO_MOLD:-}" != 1 ] &&
   [ "$(uname -s 2>/dev/null)" = Linux ] && [ "$(uname -m 2>/dev/null)" = x86_64 ] && command -v mold >/dev/null 2>&1 &&
   _bongos_write_mold_linker "$_bongos_mold_linker"; then
  if [ -z "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-}" ]; then
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$_bongos_mold_linker"
    export BONGOS_SET_MOLD_LINKER="$_bongos_mold_linker"  # marks the value as ours, so opting out later removes only that
  fi
elif [ -n "${BONGOS_SET_MOLD_LINKER:-}" ]; then
  if [ "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-}" = "$BONGOS_SET_MOLD_LINKER" ]; then
    unset CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER
  fi
  unset BONGOS_SET_MOLD_LINKER
fi
unset -f _bongos_mold_linker_text _bongos_write_mold_linker
unset _bongos_mold_linker _bongos_mold_dir _bongos_mold_tmp
if [ "${BONGOS_RELEASE_BUILD:-}" != 1 ] && [ "${BONGOS_KEEP_SPLIT_DEBUGINFO:-}" != 1 ]; then
  export CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=off
  export CARGO_PROFILE_DEV_BUILD_OVERRIDE_SPLIT_DEBUGINFO=off
  export BONGOS_SET_SPLIT_DEBUGINFO=1
elif [ -n "${BONGOS_SET_SPLIT_DEBUGINFO:-}" ]; then
  unset CARGO_PROFILE_DEV_SPLIT_DEBUGINFO CARGO_PROFILE_DEV_BUILD_OVERRIDE_SPLIT_DEBUGINFO BONGOS_SET_SPLIT_DEBUGINFO
fi

# Unified features, off by default. BONGOS_UNIFIED_FEATURES=1 makes every cargo command use the
# workspace-wide feature set, so callers that run many differently shaped cargo commands (different
# -p sets, the app build, the whole-workspace test build) against one target directory share compiled
# dependencies instead of rebuilding them under new artifact hashes per command. Any other value
# turns it off, and clears the cargo settings so a stale environment cannot leave it on. Costs:
#   - The first build after flipping the switch in either direction recompiles everything.
#   - RUSTC_BOOTSTRAP=1 (cargo needs it for the unstable setting) is also read by rustc and by some
#     build scripts, which then enable code meant for nightly compilers (seen in anyhow, thiserror,
#     proc-macro2, cap-primitives, cap-std, io-extras, system-interface).
#   - The app's own dependency features change slightly (gpui gains font-kit and windows-manifest,
#     nix gains a few modules, prettyplease gains verbatim).
#   - `cargo build -p <dependency that is not a workspace member>` silently builds nothing.
# Release bundles and the reference build force it off, and dependency inspections (cargo_plain in
# lib.sh) run without it, so they describe the app as it ships.
if [ "${BONGOS_UNIFIED_FEATURES:-}" = 1 ]; then
  if [ -z "${RUSTC_BOOTSTRAP:-}" ]; then
    export RUSTC_BOOTSTRAP=1
    export BONGOS_SET_RUSTC_BOOTSTRAP=1  # marks the value as ours, so turning the switch off removes only that
  fi
  export CARGO_UNSTABLE_FEATURE_UNIFICATION=true
  export CARGO_RESOLVER_FEATURE_UNIFICATION=workspace
else
  unset CARGO_UNSTABLE_FEATURE_UNIFICATION CARGO_RESOLVER_FEATURE_UNIFICATION
  if [ -n "${BONGOS_SET_RUSTC_BOOTSTRAP:-}" ]; then
    unset RUSTC_BOOTSTRAP BONGOS_SET_RUSTC_BOOTSTRAP
  fi
fi
