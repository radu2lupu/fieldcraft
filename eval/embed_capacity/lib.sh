# Shell glue for the capacity benchmark (runs on the mini inside ~/embed-bench).
B=$HOME/embed-bench
LS=$B/llama/llama-b12032/llama-server
M=$B/models/embeddinggemma-2-Q8_0.gguf
MP=$B/models/mmproj-embeddinggemma-2-Q8_0.gguf
free_pct() { memory_pressure | tail -1 | grep -o "[0-9]*%" | tr -d "%"; }
load1() { uptime | sed "s/.*averages: //" | awk '{print $1}' | tr -d ,; }
state() { echo "state: free=$(free_pct)% load1=$(load1)"; }
guard() {
  f=$(free_pct); l=$(load1)
  if [ "$f" -lt 40 ] || [ "$(echo "$l > 8" | bc)" = 1 ]; then echo "ABORT guard free=$f load=$l"; return 1; fi
}
# start NAME PORT NP CTX_PER_SLOT THREADS B UB [extra flags...]
start() {
  name=$1; port=$2; np=$3; ctxper=$4; thr=$5; b=$6; ub=$7; shift 7
  guard || return 1
  nice -n 10 $LS -m $M --embeddings --pooling mean --host 127.0.0.1 --port $port -np $np -c $((np*ctxper)) -b $b -ub $ub -t $thr -tb $thr "$@" > $B/$name.log 2>&1 &
  echo $! > $B/$name.pid
  for i in $(seq 1 100); do curl -sf http://127.0.0.1:$port/health >/dev/null && return 0; sleep 0.3; done
  echo "server $name failed to start"; tail -5 $B/$name.log; return 1
}
stop() {
  name=$1
  [ -f $B/$name.pid ] && kill $(cat $B/$name.pid) 2>/dev/null
  sleep 1
  [ -f $B/$name.pid ] && kill -9 $(cat $B/$name.pid) 2>/dev/null
  rm -f $B/$name.pid
}
rss_mb() { ps -o rss= -p $(cat $B/$1.pid) | awk '{printf "%.0f", $1/1024}'; }
# watchdog: kills all bench servers if free memory < 40%
watchdog() {
  while true; do
    f=$(free_pct)
    if [ "$f" -lt 40 ]; then
      echo "WATCHDOG free=$f killing" >> $B/watchdog.log
      for p in $B/*.pid; do [ -f $p ] && kill -9 $(cat $p); done
    fi
    sleep 4
  done
}
