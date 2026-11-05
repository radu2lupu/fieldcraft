# alone-queries, media, audio, sleep-idle, and a re-run of the live-server interference check with failure counts
. $HOME/embed-bench/lib.sh
cd $B
U1=http://127.0.0.1:8081
FQ() { nice -n 10 ./ecbench timecmd -reps 20 -- fieldcraft query test; }
start s1 8081 4 640 4 1024 1024 || exit 1
echo "### queries alone (s1, np=4)"; state
for qc in 1 16; do printf "alone qc=$qc: "; ./ecbench mix -qurl $U1 -qin queries_p.jsonl -qc $qc -qreps 3 -bc 0 | sed 's/^query //'; done
echo "### interference rerun"; state
printf "before: "; FQ
( ./ecbench mix -burl $U1 -bin docs.jsonl -bc 8 -qurl $U1 -qin queries_p.jsonl -qc 4 -qreps 6 > mixC.out ) & mp=$!
sleep 4; printf "during-shared: "; FQ; wait $mp; cat mixC.out
printf "after: "; FQ
stop s1
echo "### media (mmproj loaded)"; state
start m1 8083 2 4096 4 4096 4096 --mmproj $MP || exit 1
echo "rss with projector idle: $(rss_mb m1) MB"
curl -s http://127.0.0.1:8083/props | grep -o '"modalities":{[^}]*}'
grep -iE "mmproj|clip|vision|audio|projector|error" m1.log | cut -c1-200 | head -12
printf "image: "; ./ecbench media -url http://127.0.0.1:8083 -dir media -kind image -c 1 -reps 2
echo "rss after images: $(rss_mb m1) MB"
printf "audio: "; ./ecbench media -url http://127.0.0.1:8083 -dir media -kind audio -c 1 -reps 1
echo "rss after audio attempt: $(rss_mb m1) MB"
stop m1
echo "### sleep-idle test"
start m2 8084 2 4096 4 4096 4096 --mmproj $MP --sleep-idle-seconds 5 || exit 1
echo "rss awake: $(rss_mb m2) MB"; sleep 9
curl -s http://127.0.0.1:8084/props | grep -o '"is_sleeping":[a-z]*'
echo "rss asleep: $(rss_mb m2) MB"
curl -s -o /dev/null -w "wake request %{time_total}s code %{http_code}\n" http://127.0.0.1:8084/v1/embeddings -H 'Content-Type: application/json' -d '{"input":"title: none | text: hello","model":"x"}'
echo "rss after wake: $(rss_mb m2) MB"
stop m2; echo DONE
