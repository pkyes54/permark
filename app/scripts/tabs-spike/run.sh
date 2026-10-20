#!/usr/bin/env bash
# THROWAWAY SPIKE (feature/tabs-spike): CI runner for Linux (under xvfb-run) + macOS. Runs each config's scripted
# plan, samples RSS of the process tree (macOS WebContent = XPC, not children: app process only), prints logs.
# usage: run.sh <exe> <outdir> [paths]
set -u
exe="$1"; out="$2"; paths="${3:-}"
mkdir -p "$out"
tree_pids() { # pid + descendants
  local all="$1" kids
  kids=$(pgrep -P "$1" 2>/dev/null || true)
  for k in $kids; do all="$all $(tree_pids "$k")"; done
  echo "$all"
}
rss_mb() { # sum RSS (MB) of the tree
  local pids; pids=$(tree_pids "$1" | tr ' ' ',' | sed 's/,,*/,/g; s/^,//; s/,$//')
  ps -o rss= -p "$pids" 2>/dev/null | awk '{s+=$1} END {printf "%.1f", s/1024}'
}
wait_mark() { # log mark pid seconds
  for _ in $(seq 1 $(( $4 * 5 ))); do
    grep -q "$2" "$1" 2>/dev/null && return 0
    kill -0 "$3" 2>/dev/null || return 1
    sleep 0.2
  done
  return 1
}
echo "config,phase,procs,rss_mb,first_page_load_ms,exit" > "$out/summary.csv"
for cfg in "baseline-main 0 0" "tabs1 1 0" "tabs2 2 0" "tabs4 4 0" "tabs4-lazy 4 1"; do
  set -- $cfg; name=$1; tabs=$2; lazy=$3
  log="$out/$name.log"
  PERMARK_TABS_SPIKE=$tabs PERMARK_TABS_SPIKE_LAZY=$lazy PERMARK_TABS_SPIKE_PLAN=1 PERMARK_TABS_SPIKE_LOG="$log" \
    PERMARK_TABS_SPIKE_PATHS="$paths" "$exe" > "$out/$name.stdout" 2>&1 &
  pid=$!
  for phase in READY CYCLED; do
    [ "$phase" = CYCLED ] && [ "$tabs" -lt 2 ] && continue
    if wait_mark "$log" "MARK $phase" "$pid" 90; then
      sleep 1.5
      echo "$name,$phase,$(tree_pids "$pid" | wc -w),$(rss_mb "$pid"),,," >> "$out/summary.csv"
    else
      echo "$name,$phase-missing,,,,," >> "$out/summary.csv"
    fi
  done
  for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
  kill -9 "$pid" 2>/dev/null
  wait "$pid"; code=$?
  first=$(grep -m1 "page-load finished" "$log" 2>/dev/null | cut -d' ' -f1)
  echo "$name,end,,,$first,$code" >> "$out/summary.csv"
  echo "===== $name (exit $code)"; cat "$log" 2>/dev/null; tail -20 "$out/$name.stdout"
done
echo "===== summary"; cat "$out/summary.csv"
