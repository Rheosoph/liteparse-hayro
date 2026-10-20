#!/usr/bin/env bash
#
# run_benchmarks.sh — reproduce the LiteParse markdown benchmark sweep.
#
# Runs LiteParse (and, by default, the model-free competitors) across the three
# tracked benchmarks — olmOCR-bench, opendataloader-bench, ParseBench — and prints
# a comparison table per benchmark. See LAUNCH_BENCHMARKS.md for the published run.
#
# Usage:
#   ./run_benchmarks.sh [options]
#
# Options:
#   --liteparse-only        Only run LiteParse (skip all competitor tools).
#   --competitors-only      Only run the model-free competitors (keep existing LiteParse outputs).
#   --benches=LIST          Comma-separated subset of: olmocr,opendataloader,parsebench
#                           (default: all three).
#   --skip-build            Don't rebuild the lit binary / Python binding.
#   --out=DIR               Where to write logs + the summary (default: bench_results/latest).
#   --ocr=MODE              LiteParse OCR mode: none (default), tesseract, or paddle.
#                           paddle = the HTTP PaddleOCR server (ocr/paddleocr, port 8829;
#                           override with LITEPARSE_PADDLE_OCR_URL). OCR modes run with
#                           num_workers=4 and low harness concurrency — OCR is resource-hungry.
#                           Outputs land under mode-suffixed names (liteparse_tesseract, …) so
#                           all modes coexist and show up together in SUMMARY.md.
#   -h, --help              Show this help.
#
# Competitors (model-free, commercial): markitdown, opendataloader, pdf_inspector.
#   - opendataloader needs Java 11+ (the script auto-detects Homebrew openjdk@11/openjdk).
#   - pdf_inspector needs the `pdf2md` binary (cargo install pdf-inspector).
#   - nutrient is commercial and is NOT re-run here; see LAUNCH_BENCHMARKS.md for its
#     preserved opendataloader-bench numbers.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

LITEPARSE_ONLY=0
COMPETITORS_ONLY=0
SKIP_BUILD=0
BENCHES="olmocr opendataloader parsebench"
OUT="$ROOT/bench_results/latest"
OCR_MODE="none"

for arg in "$@"; do
  case "$arg" in
    --liteparse-only) LITEPARSE_ONLY=1 ;;
    --competitors-only) COMPETITORS_ONLY=1 ;;
    --skip-build)     SKIP_BUILD=1 ;;
    --benches=*)      BENCHES="${arg#*=}"; BENCHES="${BENCHES//,/ }" ;;
    --out=*)          OUT="${arg#*=}" ;;
    --ocr=*)          OCR_MODE="${arg#*=}" ;;
    -h|--help)        sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"
PY="$ROOT/.venv/bin/python"

# --- OCR mode → per-harness LiteParse engine/pipeline names ---------------
case "$OCR_MODE" in
  none)
    OLM_LP="liteparse"; OLM_LP_NAME="liteparse"; ODL_LP="liteparse"; PB_LP="liteparse_markdown"; PB_CONC=6 ;;
  tesseract)
    OLM_LP="liteparse:ocr_engine=tesseract:num_workers=4:name=liteparse_tesseract"; OLM_LP_NAME="liteparse_tesseract"
    ODL_LP="liteparse-tesseract"; PB_LP="liteparse_markdown_tesseract"; PB_CONC=3 ;;
  paddle)
    OLM_LP="liteparse:ocr_engine=paddle:num_workers=4:name=liteparse_paddle"; OLM_LP_NAME="liteparse_paddle"
    ODL_LP="liteparse-paddle"; PB_LP="liteparse_markdown_paddle"; PB_CONC=3
    PURL="${LITEPARSE_PADDLE_OCR_URL:-http://localhost:8829/ocr}"
    curl -s -m 5 -o /dev/null "${PURL%/ocr}/" || echo "WARNING: no PaddleOCR server answering at $PURL (start: cd ocr/paddleocr && uv run server.py)" >&2 ;;
  *) echo "Unknown --ocr mode: $OCR_MODE (none|tesseract|paddle)" >&2; exit 2 ;;
esac
TIMING="$OUT/timings.txt"
stamp() { echo "$(date +%s) $*" >> "$TIMING"; }

# --- Java 11+ detection (needed by the opendataloader competitor) -------------
JAVA11_BIN=""
for cand in /opt/homebrew/opt/openjdk@11/bin /opt/homebrew/opt/openjdk/bin /opt/homebrew/opt/openjdk@25/bin; do
  if [ -x "$cand/java" ]; then JAVA11_BIN="$cand"; break; fi
done
if [ -n "$JAVA11_BIN" ]; then export PATH="$JAVA11_BIN:$PATH"; fi

run_engines() {  # echoes the engine list for a benchmark, honoring --liteparse-only / --competitors-only
  if [ "$LITEPARSE_ONLY" = "1" ]; then echo "$1"; return; fi
  shift
  if [ "$COMPETITORS_ONLY" = "1" ]; then shift; fi
  echo "$@"
}

# === 1. Build artifacts =======================================================
if [ "$COMPETITORS_ONLY" = "1" ]; then SKIP_BUILD=1; fi
if [ "$SKIP_BUILD" = "0" ]; then
  echo "### Building lit binary + Python binding ..."
  cargo build --release --bin lit || { echo "cargo build failed"; exit 1; }
  ( cd packages/python && maturin develop --release ) || { echo "maturin build failed"; exit 1; }
else
  echo "### Skipping build (--skip-build)"
fi

# === 2. olmOCR-bench ==========================================================
if [[ " $BENCHES " == *" olmocr "* ]]; then
  echo "### olmOCR-bench ..."
  DIR="$ROOT/olmocr/olmOCR-bench/bench_data"
  for E in $(run_engines "$OLM_LP" "$OLM_LP"  markitdown opendataloader pdf_inspector); do
    N="$E"; [ "$E" = "$OLM_LP" ] && N="$OLM_LP_NAME"
    [[ "$E" == *:name=* ]] && N="${E##*:name=}"
    echo "  convert $N"; stamp "olmocr_convert_$N start"
    $PY -m olmocr.bench.convert "$E" --dir "$DIR" --parallel 1 --force \
        > "$OUT/olmocr_convert_$N.log" 2>&1
    stamp "olmocr_convert_$N end"
    echo "  benchmark $N"
    $PY -m olmocr.bench.benchmark --dir "$DIR" --candidate "$N" \
        > "$OUT/olmocr_bench_$N.log" 2>&1
  done
fi

# === 3. opendataloader-bench ==================================================
if [[ " $BENCHES " == *" opendataloader "* ]]; then
  echo "### opendataloader-bench ..."
  for E in $(run_engines "$ODL_LP" "$ODL_LP" opendataloader markitdown); do
    echo "  run $E"; stamp "odl_$E start"
    ( cd opendataloader-bench && \
      LITEPARSE_BIN="$ROOT/target/release/lit" \
      ../.venv/bin/python src/run.py --engine "$E" --force ) \
        > "$OUT/odl_$E.log" 2>&1
    stamp "odl_$E end"
  done
fi

# === 4. ParseBench (all 5 groups, rule-based, no API key) =====================
if [[ " $BENCHES " == *" parsebench "* ]]; then
  echo "### ParseBench ..."
  for P in $(run_engines "$PB_LP" "$PB_LP" markitdown opendataloader_markdown pdf_inspector); do
    echo "  run $P"; stamp "parsebench_$P start"
    EXTRA="--force"; [[ "$P" == liteparse* ]] && EXTRA="--force -m $PB_CONC"
    ( cd ParseBench && LITEPARSE_BIN="$ROOT/target/release/lit" \
      ../.venv/bin/parse-bench run "$P" --open_report=False $EXTRA ) \
        > "$OUT/parsebench_$P.log" 2>&1
    stamp "parsebench_$P end"
  done
fi

# === 5. Aggregate + print tables =============================================
echo
echo "### Results (also written to $OUT/SUMMARY.md)"
ROOT="$ROOT" OUT="$OUT" BENCHES="$BENCHES" "$PY" - <<'PYEOF' | tee "$OUT/SUMMARY.md"
import json, os, re, glob
ROOT=os.environ["ROOT"]; OUT=os.environ["OUT"]; BENCHES=os.environ["BENCHES"].split()

def table(rows, hdr):
    out=['| '+' | '.join(hdr)+' |', '|'+'|'.join(['---']*len(hdr))+'|']
    for r in rows: out.append('| '+' | '.join(r)+' |')
    return '\n'.join(out)

print(f"# Benchmark results\n")

# --- olmOCR: parse the benchmark logs ---
if 'olmocr' in BENCHES:
    cats=['baseline','headers_footers','multi_column','table_tests','long_tiny_text',
          'old_scans','arxiv_math','old_scans_math']
    data={}
    for lf in sorted(glob.glob(f"{OUT}/olmocr_bench_*.log")):
        eng=re.search(r'olmocr_bench_(.+)\.log',lf).group(1)
        txt=open(lf,encoding='utf-8',errors='ignore').read()
        m=re.search(r'Average Score:\s*([\d.]+)%\s*(?:±|\()',txt)
        overall=m.group(1) if m else 'NA'
        per={}
        for c in cats:
            mm=re.search(rf'{re.escape(c)}(?:\.jsonl)?\s*:\s*([\d.]+)%\s*\(',txt)
            per[c]=mm.group(1) if mm else 'NA'
        data[eng]={'Overall':overall, **per}
    order=[e for e in ['liteparse','liteparse_tesseract','liteparse_paddle','markitdown','opendataloader','pdf_inspector'] if e in data]
    if order:
        hdr=['Engine','Overall']+cats
        rows=[[e]+[data[e]['Overall']]+[data[e][c] for c in cats] for e in order]
        print("## olmOCR-bench (% pass)\n"); print(table(rows,hdr)); print()

# --- opendataloader: read evaluation.json ---
if 'opendataloader' in BENCHES:
    rows=[]
    for e in ['liteparse','liteparse-tesseract','liteparse-paddle','nutrient','opendataloader','markitdown']:
        f=f"{ROOT}/opendataloader-bench/prediction/{e}/evaluation.json"
        if not os.path.exists(f): continue
        s=json.load(open(f))['metrics']['score']
        rows.append([e,'%.4f'%s['overall_mean'],'%.4f'%s['nid_mean'],'%.4f'%s['teds_mean'],'%.4f'%s['mhs_mean']])
    if rows:
        print("## opendataloader-bench\n")
        print(table(rows,['Engine','Overall','NID','TEDS','MHS']))
        print("\n*nutrient = preserved data (not re-run here).*\n")

# --- ParseBench: per-category canonical metric (matches ParseBench leaderboard) ---
if 'parsebench' in BENCHES:
    cats=[('table',['avg_grits_trm_composite'],'Tables'),
          ('chart',['avg_rule_pass_rate'],'Charts'),
          ('text_content',['avg_content_faithfulness','avg_rule_pass_rate'],'Content_Faithfulness'),
          ('text_formatting',['avg_semantic_formatting','avg_rule_pass_rate'],'Semantic_Formatting'),
          ('layout',['avg_layout_element_rule_pass_rate','avg_rule_pass_rate'],'Visual_Grounding')]
    pipes=[('liteparse_markdown','LiteParse'),('liteparse_markdown_tesseract','LiteParse+tesseract'),
           ('liteparse_markdown_paddle','LiteParse+paddle'),
           ('markitdown','markitdown'),('opendataloader_markdown','opendataloader'),
           ('pdf_inspector','pdf-inspector')]
    rows=[]; lay_n=None
    for p,disp in pipes:
        vals={}
        for grp,keys,col in cats:
            f=f"{ROOT}/ParseBench/output/{p}/{grp}/_evaluation_report.json"
            if os.path.exists(f):
                d=json.load(open(f)); m=d['aggregate_metrics']
                vals[col]=next((m[k] for k in keys if k in m),None)
                if grp=='layout':
                    # The harness averages only over scored docs; a doc it *skipped* (no
                    # layout data emitted) is excluded, while a *failed* one counts as 0.
                    # Count skipped docs as 0 too, so rows with different skip counts are
                    # comparable: element rules over docs that have element GT, otherwise
                    # reading-order rules over every doc.
                    per=d['per_example_results']
                    le=[x['value'] for r in per for x in (r.get('metrics') or []) if x['metric_name']=='layout_element_rule_pass_rate']
                    rp=[x['value'] for r in per for x in (r.get('metrics') or []) if x['metric_name']=='rule_pass_rate']
                    if le: vals[col]=sum(le)/(len(le)+d['skipped'])
                    elif rp: vals[col]=sum(rp)/d['total_examples']
                    if lay_n is None: lay_n=(len(le)+d['skipped'] if le else d['total_examples'], d['total_examples'])
            else: vals[col]=None
        present=[v for v in vals.values() if v is not None]
        if not present: continue
        overall=sum(present)/len(present)
        rows.append([disp,'%.4f'%overall]+['%.4f'%vals[c[2]] if vals[c[2]] is not None else 'NA' for c in cats])
    if rows:
        hdr=['Pipeline','Overall']+[c[2] for c in cats]
        print("## ParseBench (rule-based; per-category canonical metric)\n")
        print(table(rows,hdr))
        print("\n*Visual_Grounding: layout-element rules over the docs that have element ground truth (436/500) when the pipeline emits layout data, else reading-order rules over all 500; a doc with no output counts as 0.*")
PYEOF
echo
echo "### Done. Logs + SUMMARY.md in $OUT"
