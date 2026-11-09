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

# Tuning with no manifest diff. Never set RUSTFLAGS: it replaces .cargo/config.toml's flags.
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG=0
