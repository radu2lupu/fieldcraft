# grid: -np x client concurrency on 1000 real passages (no text printed)
. $HOME/embed-bench/lib.sh
cd $B
stop s1
for np in 1 4 8; do
  echo "== np=$np -c=$((np*640)) t=4 b=ub=1024"; state
  start s1 8081 $np 640 4 1024 1024 || exit 1
  for c in 1 4 8 16; do
    guard || { echo "guard abort"; stop s1; exit 1; }
    ( top -l 2 -s 3 -pid $(cat s1.pid) -stats pid,cpu,mem 2>/dev/null | tail -1 > top.out ) & tp=$!
    printf "np=$np c=$c "; nice -n 10 ./ecbench run -url http://127.0.0.1:8081 -in docs.jsonl -n 1000 -c $c
    wait $tp; echo "   top(cpu%,mem) $(cat top.out)"
  done
  echo "rss after np=$np grid: $(rss_mb s1) MB"
  stop s1
done
