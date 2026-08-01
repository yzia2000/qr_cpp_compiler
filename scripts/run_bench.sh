#!/usr/bin/env bash
# Full benchmark protocol: data -> validation -> benchmark -> README injection.
set -euo pipefail
cd "$(dirname "$0")/.."

PCAP=data/quotes_1g.pcap
DEV=data/quotes_dev.pcap
mkdir -p data results

[ -f "$DEV" ] || python3 python/gen_pcap.py "$DEV" --size-gb 0.01 --seed 42
[ -f "$PCAP" ] || python3 python/gen_pcap.py "$PCAP" --size-gb 1.0 --seed 42

bash scripts/build_all.sh
python3 python/validate.py "$DEV"
python3 python/bench.py --pcap "$PCAP" --reps "${QR_REPS:-7}"
echo "done - see results/results.md and README.md"
