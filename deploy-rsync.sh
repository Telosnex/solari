#!/bin/bash
set -e

HOST="${SOLARI_HOST:-3.140.195.186}"
KEY="/Users/jpo/Downloads/telosnex-maps-builder-key.pem"
SSH_OPTS="-i $KEY -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 -o ServerAliveInterval=30"
SSH="ssh $SSH_OPTS ubuntu@$HOST"
REMOTE_SRC="/scratch/solari-src"

echo "=== local check ==="
cd /Users/jpo/dev/solari
cargo check -p solari-server 2>&1 | tail -5
if [ $? -ne 0 ]; then
  echo "FAILED local check, aborting"
  exit 1
fi

echo "=== rsync to $HOST ==="
rsync -az --delete \
  --exclude 'target/' \
  --exclude '.git/' \
  --exclude '_scratch/' \
  --exclude 'vendor/' \
  -e "ssh $SSH_OPTS" \
  /Users/jpo/dev/solari/ \
  ubuntu@$HOST:$REMOTE_SRC/

echo "=== build + restart ==="
$SSH 'bash -s' << 'REMOTE'
set -e
source $HOME/.cargo/env
cd /scratch/solari-src

echo "=== building ==="
T0=$(date +%s)
cargo build --release -p solari-server 2>&1 | tail -10
T1=$(date +%s)
echo "build: $((T1 - T0))s"

echo "=== stopping old server ==="
kill $(pgrep solari-server) 2>/dev/null || true
sleep 2

echo "=== starting with --skip-transfer-graph ==="
cp target/release/solari-server /scratch/solari-server-latest
nohup /scratch/solari-server-latest \
  --base-path /scratch/solari-data/timetable \
  --skip-transfer-graph \
  --port 8000 \
  </dev/null >/scratch/solari-latest.log 2>&1 &
disown

echo "waiting for startup..."
for i in $(seq 1 30); do
  if curl -sf http://127.0.0.1:8000/v1/nearby_stops?lat=34&lon=-118&radius=1000 > /dev/null 2>&1; then
    echo "SERVER UP after ${i}s"
    break
  fi
  sleep 1
done

echo "=== smoke test: LA transit ==="
T0=$(date +%s%3N)
RESULT=$(curl -s -X POST http://127.0.0.1:8000/v1/plan \
  -H "Content-Type: application/json" \
  -d '{"from":{"lat":34.056,"lon":-118.237},"to":{"lat":34.009,"lon":-118.497},"start_at":1773340200000}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['status'], len(d.get('itineraries',[])), 'itin')")
T1=$(date +%s%3N)
echo "LA query: $RESULT ($((T1 - T0))ms)"

echo "=== memory ==="
free -h | head -2
ps -eo pid,rss,cmd | grep solari-server | grep -v grep | awk '{printf "Solari RSS: %.1f MB\n", $2/1024}'
REMOTE
