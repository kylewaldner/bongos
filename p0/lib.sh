# Phase 0 helpers (scratch). Source in a step: . p0/lib.sh
avail_root() { df -m --output=avail / | tail -1 | tr -d ' '; }
avail_mnt() { df -m --output=avail /mnt 2>/dev/null | tail -1 | tr -d ' '; }
free_disk() {
  # the removals that the disk experiment found worth it (updated after Phase 0 results)
  s=$(date +%s); b=$(avail_root)
  sudo rm -rf /usr/local/lib/android /usr/share/dotnet /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL /usr/local/share/boost /usr/share/swift /opt/microsoft /usr/lib/jvm /usr/local/share/powershell /opt/az /usr/lib/google-cloud-sdk
  echo "MEASURE free_disk_seconds=$(( $(date +%s)-s )) freed_root_mb=$(( $(avail_root)-b )) root_avail_mb=$(avail_root) mnt_avail_mb=$(avail_mnt)"
}
timed() { # timed <name> <cmd...>
  local name=$1; shift; local s=$(date +%s) rc=0
  "$@" || rc=$?
  echo "MEASURE step=$name seconds=$(( $(date +%s)-s )) exit=$rc root_avail_mb=$(avail_root) mnt_avail_mb=$(avail_mnt)"
  return 0
}
start_watch() { # background sampler: epoch avail_root avail_mnt mem_used_mb
  ( while true; do echo "$(date +%s) $(avail_root) $(avail_mnt) $(free -m | awk '/^Mem:/{print $3}')" >> "$RUNNER_TEMP/watch.log"; sleep 20; done ) &
  disown
}
report_watch() {
  awk 'NR==1{minr=$2;minm=$3;maxmem=$4} {if($2<minr)minr=$2; if($3!=""&&$3<minm)minm=$3; if($4>maxmem)maxmem=$4} END{print "MEASURE watch min_root_avail_mb=" minr " min_mnt_avail_mb=" minm " peak_mem_used_mb=" maxmem}' "$RUNNER_TEMP/watch.log"
}
