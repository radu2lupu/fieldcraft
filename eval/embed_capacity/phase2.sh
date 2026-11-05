# best config: median of 3 full runs, RSS, CPU/GPU; query latency alone / shared / isolated; interference
. $HOME/embed-bench/lib.sh
cd $B
gpu() { ioreg -r -d 1 -c IOAccelerator | grep -o '"Device Utilization %"=[0-9]*' | head -1; }
U1=http://127.0.0.1:8081; U2=http://127.0.0.1:8082
FQ() { nice -n 10 ./ecbench timecmd -reps 20 -- fieldcraft query test; }
echo "### best config np=4 -c=2560 t=4 b=ub=1024"; state
start s1 8081 4 640 4 1024 1024 || exit 1
echo "rss idle: $(rss_mb s1) MB"
printf "warm 1000 req c=8: "; ./ecbench run -url $U1 -in docs.jsonl -n 1000 -c 8
echo "rss after 1000 req: $(rss_mb s1) MB"
for i in 1 2 3; do
  guard || exit 1
  ( sleep 8; gpu > gpu.out; top -l 2 -s 3 -n 5 -stats pid,cpu,mem 2>/dev/null | awk '/PID/{n++} n==2' > top5.out ) & tp=$!
  printf "full3000 run$i c=8: "; nice -n 10 ./ecbench run -url $U1 -in docs.jsonl -n 3000 -c 8
  wait $tp; [ $i = 1 ] && { echo "  gpu during run: $(cat gpu.out)"; echo "  top5 (pid cpu mem) during run:"; cat top5.out | head -7; }
done
echo "rss after 10000 req: $(rss_mb s1) MB"
echo "### queries alone on s1"
state
for qc in 1 16; do printf "alone qc=$qc: "; ./ecbench mix -qurl $U1 -qin queries_p.jsonl -qc $qc -qreps 3 -bc 0 | sed 's/^query //'; done
echo "### queries shared with bulk (bc=8) on s1"
for qc in 1 4 16 32 64; do guard || break; echo "shared qc=$qc:"; ./ecbench mix -burl $U1 -bin docs.jsonl -bc 8 -qurl $U1 -qin queries_p.jsonl -qc $qc -qreps 3; done
echo "### isolated: bulk on s1, queries on s2 (np=4 t=2)"
start s2 8082 4 640 2 1024 1024 || exit 1
echo "rss s2 idle: $(rss_mb s2) MB"
for qc in 1 4 16 32 64; do guard || break; echo "isolated qc=$qc:"; ./ecbench mix -burl $U1 -bin docs.jsonl -bc 8 -qurl $U2 -qin queries_p.jsonl -qc $qc -qreps 3; done
echo "rss s1 $(rss_mb s1) MB, s2 $(rss_mb s2) MB"
echo "### interference: fieldcraft query test x20 on the live server"
state
printf "before: "; FQ
echo "during shared mix (bulk bc=8 + queries qc=4):"
( ./ecbench mix -burl $U1 -bin docs.jsonl -bc 8 -qurl $U1 -qin queries_p.jsonl -qc 4 -qreps 12 > mixA.out ) & mp=$!
sleep 4; printf "during-shared: "; FQ; wait $mp; cat mixA.out
echo "during isolated mix (bulk on s1, queries qc=4 on s2):"
( ./ecbench mix -burl $U1 -bin docs.jsonl -bc 8 -qurl $U2 -qin queries_p.jsonl -qc 4 -qreps 12 > mixB.out ) & mp=$!
sleep 4; printf "during-isolated: "; FQ; wait $mp; cat mixB.out
printf "after: "; FQ
stop s1; stop s2; echo DONE
