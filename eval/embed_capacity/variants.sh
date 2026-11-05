# variants on 1000 real passages: batch/ubatch, multi-input requests, flash-attn, threads
. $HOME/embed-bench/lib.sh
cd $B
cell() { # label np thr b ub c per extra...
  label=$1; np=$2; thr=$3; b=$4; ub=$5; c=$6; per=$7; shift 7
  guard || { echo abort; return 1; }
  start s1 8081 $np 640 $thr $b $ub "$@" || return 1
  printf "$label np=$np t=$thr b=$b ub=$ub c=$c per=$per "
  nice -n 10 ./ecbench run -url http://127.0.0.1:8081 -in docs.jsonl -n $((1000/per)) -c $c -per $per
  stop s1
}
cell ub512        4 4 512  512  8 1
cell ub2048       4 4 2048 2048 8 1
cell ub4096       4 4 4096 4096 8 1
cell fa_off       4 4 1024 1024 8 1 -fa off
cell threads8     4 8 1024 1024 8 1
cell per8_np8     8 4 1024 1024 2 8
cell per4_np4     4 4 1024 1024 4 4
cell ngl0_cpu     4 8 1024 1024 8 1 -ngl 0
