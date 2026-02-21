#!/usr/bin/env bash
set -euo pipefail

ROOT="/Users/elifoltyn/Desktop/eli-code"
cd "$ROOT"

MODEL="${1:-arcee-ai/trinity-large-preview:free}"
TS="$(date +%Y%m%d_%H%M%S)"
OUT_DIR="eli_research/perf/smoke_${TS}"
mkdir -p "$OUT_DIR"

run_agent() {
  local name="$1"
  local task="$2"
  local out="$OUT_DIR/agent_${name}.json"
  echo "[agent] $name"
  bin/eli --provider openrouter --model "$MODEL" agent run --task "$task" --max-ms 35000 --out "$out" >/tmp/agent_${name}.stdout 2>/tmp/agent_${name}.stderr || true
}

run_swarm() {
  local name="$1"
  local max_ms="$2"
  local chunks="$3"
  local out="$OUT_DIR/swarm_${name}.json"
  echo "[swarm] $name"
  bin/eli --provider openrouter --model "$MODEL" agent swarm \
    --task "Extract key operational rules" \
    --input AGENTS.md \
    --chunks "$chunks" \
    --max-parallel 3 \
    --max-ms "$max_ms" \
    --max-attempts 2 \
    --out "$out" >/tmp/swarm_${name}.stdout 2>/tmp/swarm_${name}.stderr || true
}

run_agent recession "are we going to have a recession"
run_agent price "what is the price of nvda"
run_agent compare "compare nvda vs amd"
run_agent risk "summarize key risks for apple supply chain"

run_swarm tight 10000 6

python3 - "$OUT_DIR" << 'PY'
import json,glob,os,sys
out_dir=sys.argv[1]
print("\n=== Smoke Summary ===")
for p in sorted(glob.glob(os.path.join(out_dir,'agent_*.json'))):
    name=os.path.basename(p)
    if not os.path.exists(p):
        print(name,{'missing':True})
        continue
    try:
        j=json.load(open(p))
        w=j.get('worker',{})
        print(name,{
            'ok':j.get('ok'),
            'status':w.get('status'),
            'used_model':w.get('used_model'),
            'attempt_count':w.get('attempt_count'),
            'duration_ms':w.get('duration_ms'),
            'report_path':w.get('report_path'),
        })
    except Exception as e:
        print(name,{'parse_error':str(e)})

for p in sorted(glob.glob(os.path.join(out_dir,'swarm_*.json'))):
    name=os.path.basename(p)
    try:
        j=json.load(open(p))
        print(name,{
            'ok':j.get('ok'),
            'usable':j.get('usable'),
            'map_completed':j.get('summary',{}).get('map_completed'),
            'map_failed':j.get('summary',{}).get('map_failed'),
            'reduce_status':j.get('reduce_worker',{}).get('status'),
            'critic_status':j.get('critic_worker',{}).get('status'),
            'final_status':j.get('final_worker',{}).get('status'),
        })
    except Exception as e:
        print(name,{'parse_error':str(e)})
print("Artifacts:",out_dir)
PY
