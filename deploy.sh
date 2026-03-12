#!/bin/bash
set -e
SSH="ssh -i /Users/jpo/Downloads/telosnex-maps-builder-key.pem -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o ConnectTimeout=5 ubuntu@18.117.159.209"

$SSH 'bash -s' << 'EOF'
set -e
source $HOME/.cargo/env
cd /scratch/solari-src
git fetch
git reset --hard origin/main
cargo build --release -p solari-server 2>&1 | tail -5
kill $(pgrep solari-server) 2>/dev/null || true
sleep 2
cp target/release/solari-server /scratch/solari-server-latest
nohup /scratch/solari-server-latest \
  --base-path /scratch/solari-data/timetable \
  --valhalla-tile-path /scratch/solari-data/transfer_graph_precontract \
  --port 8000 \
  </dev/null >/scratch/solari-latest.log 2>&1 &
disown
sleep 3
pgrep -a solari-server && echo "SERVER UP" || echo "FAILED TO START"
EOF
