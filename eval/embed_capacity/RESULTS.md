# EmbeddingGemma 2 capacity on the Mac mini (M4, 10 cores, 16 GB, shared live server)

Date 2026-11-01. Capacity benchmark only, nothing deployed. No lesson text appears here: only counts, token lengths and timings.

## Setup
- llama.cpp release **b12032** (tag exists, `llama-b12032-bin-macos-arm64.tar.gz`, sha256 matches the published digest `00468fe8...6382`). It loads architecture `gemma-embedding2`. Metal GPU offload is on by default.
- Models from `ggml-org/embeddinggemma-2-GGUF`: `embeddinggemma-2-Q8_0.gguf` (309,855,456 B) and `mmproj-embeddinggemma-2-Q8_0.gguf` (554,821,056 B). Both sizes and sha256 match the Hugging Face LFS metadata.
- Server: `llama-server --embeddings --pooling mean --host 127.0.0.1 -np N -c N*640 -b B -ub B -t T`, endpoint `POST /v1/embeddings` (one string or an array per request; multimodal input as `{"content":[{"type":"image_url"|"input_audio",...}]}`). Everything under `nice -n 10`. llama-server is killed after every run.
- Prompts: query `task: search result | query: {q}`, document `title: none | text: {t}`.
- Data: 3,000 random active `retrieval_text` values (raw 109 tokens average, p50 75, p95 278, max 3,251; 24 passages truncated at 500 text tokens, so every prompt is at most 512 tokens) and 200 random `retrieval_log.query_text` values (18 tokens average, p50 12, p95 24, max 855; 1 truncated).
- Hardware state before runs: memory free 73-77 percent, load average 1.7-5.1 (never above 8). A 4 s watchdog would have killed the servers below 40 percent free; it never fired.
- Tools: `cmd/ecbench` (Go, stdlib), `cmd/genmedia`, shell glue `lib.sh grid.sh variants.sh phase2.sh phase3.sh` in this directory.

## 1. Resident memory (text-only, ps RSS)
| point | RSS |
|---|---|
| idle after load, np=1 / np=4 / np=8 | 482 / 485 / about 490 MB |
| after 1,000 requests (np=4) | 509 MB |
| after about 10,000 requests (np=4) | 498 MB |
| second server (np=4, 2 threads), idle / after load | 488 / 504 MB |

Memory is flat; no growth under load.

## 2. Document throughput (real passages, 1,000 per cell; best config 3,000 x 3)
Grid at `-b=-ub=1024`, 4 threads (passages/s, p50/p95 ms per request):

| np | c=1 | c=4 | c=8 | c=16 |
|---|---|---|---|---|
| 1 | 42.8, 19/48 | 43.7, 86/133 | 43.3, 180/246 | 43.5, 361/448 |
| 4 | 42.0, 19/49 | 51.9, 73/121 | 58.3, 133/201 | 57.2, 274/364 |
| 8 | 42.7, 19/48 | 50.9, 74/124 | 55.2, 139/214 | 57.0, 266/413 (one 2.7 s outlier) |

Variants at np=4, c=8 (passages/s): ub 512 55.8 (one request starved 17.9 s: a near-512-token prompt cannot share a batch), ub 2048 56.5, ub 4096 55.8, `-fa off` 50.2, 8 threads 55.8, 4 inputs per request 55.6, np=8 with 8 inputs per request 56.8, **CPU only (`-ngl 0`) 20.6**.

**Best: np=4, `-c 2560`, `-b 1024 -ub 1024`, 4 threads, client concurrency 8.** Three full 3,000-passage runs: 57.6, 57.5, 57.2 passages/s (median 57.5), 6.6k tokens/s, p50 134 ms, p95 207-209 ms, 0 errors. Why: np=1 is 25 percent slower and serializes; np above 4, bigger batches, more threads and multi-input requests add nothing because the GPU is the limit (Metal device utilization 91 percent during the run). `-ub` at least 1024 avoids the starvation seen at 512. Throughput is flat at about 57/s, so concurrency above 8 only adds latency.

## 3. Query latency (200 real queries, 3 passes = 600 requests; ms)
| scenario | qc=1 p50/p95 | qc=4 | qc=16 | qc=32 | qc=64 |
|---|---|---|---|---|---|
| alone (np=4) | 12/17 | not measured | 78/166 | not measured | not measured |
| shared server, bulk c=8 running | 127/203 | 158/244 | 228/319 | 317/412 | 464/651 |
| isolated query server (np=4, 2 threads) while bulk runs on the first | 56/89 | 108/151 | 245/357 | 516/632 | 1032/1192 |

Bulk throughput while queries run: shared 54/s (qc=1) falling to 29/s (qc=64); isolated stays at 47-51/s.

Does isolation fix the tail? Only at low concurrency (qc 1-4: p95 89 and 151 ms versus 203 and 244). At qc of 16 or more it is worse, because both servers use the same GPU and the capped query server has less throughput (about 61 queries/s versus 124/s shared). It cannot beat the GPU limit; a dedicated instance does not buy isolation on one GPU.

## 4. Re-embedding time (from the measured 57.5 passages/s, avg 109 tokens)
- 33,000 lessons: about 574 s, **9.6 min**.
- 100,000 passages: about 1,739 s, **29 min**.
- Utilisation during bulk: GPU 91 percent (ioreg `Device Utilization %`), llama-server about 25-70 percent of one core (top; the rest of the 10 cores is idle), memory flat. GPU use via powermetrics was not available (needs sudo). CPU-only is 2.8 times slower.
- Extrapolation assumes the corpus length distribution matches the 3,000 sample.

## 5. Image path (mmproj loaded, supported in this build)
- `/props` reports `modalities: vision true, video true, audio true`. Images go through `/v1/embeddings` with `image_url` data URIs; all 40 requests (20 synthetic PNG/JPEG, 896 and 640 px, each twice, serial) returned embeddings, 0 errors.
- RSS with projector idle **1,093 MB** (text-only 485), 1,178 MB after images, 1,202 MB after audio.
- Latency: p50 297 ms, p95 751 ms, max 914 ms per image (first requests slowest; per-size split and image token counts not measured). Embedding quality not evaluated.
- Unloading: llama-server cannot unload the projector alone at runtime; it is loaded for the life of the process. `--sleep-idle-seconds N` unloads the model and the projector together: with N=5 RSS fell from 1,091 to 276 MB asleep, and the first request after sleep took 0.55 s and returned 200 (RSS back to 1,097 MB). So always-on image support costs about 600 MB extra, or run a separate on-demand server with a sleep timeout.

## 6. Audio
Yes, in this build. Evidence: `/props` modalities audio true, log line `init_audio: audio input is in experimental stage and may have reduced quality`, and `input_audio` on `/v1/embeddings` accepted a 30 s synthetic 16 kHz WAV and returned an embedding in **569 ms** (one request, so no spread). The server docs mark audio input experimental. Embedding quality not evaluated.

## 7. Interference on the live local server (`fieldcraft query test`, CLI wall time, 20 runs each)
First run, around the bulk plus query load: before p50 2,124 / p95 2,258 ms; during shared mix p50 1,792 / p95 4,450 ms with outliers (max 20.7 s, min 28 ms); during isolated mix p50 1,195 / p95 1,606 ms (max 11.4 s, min 28 ms); after p50 1,397 / p95 2,169 ms. The 28 ms minima were not explained (failures were not counted in that run).
Re-run with failure counting (0 failures in all): before p50 1,335 / p95 2,192; **during shared mix (bulk c=8 plus queries qc=4) p50 1,159 / p95 1,846**; after p50 1,315 / p95 2,073.
Reading: the CLI takes 1-2 s with or without load, so wall time is dominated by something outside the embedding benchmark and noise is large (p95 varies between 1.8 and 4.5 s). The first run's outliers coincide with load, the re-run shows none, so a rare stall cannot be ruled out; this test cannot resolve it. Side effect: these 60 plus 5 test queries wrote rows to the production `retrieval_log` (the query path logs); delete them if they matter.

## 8. Recommendation for the mini
- **One text server, always on**: `llama-server -m embeddinggemma-2-Q8_0.gguf --embeddings --pooling mean --host 127.0.0.1 -np 4 -c 2560 -b 1024 -ub 1024 -t 4`, run niced. Expected resident memory about 0.5 GB (flat).
- Do not add a dedicated query instance: it does not help beyond qc of about 4 and adds 0.5 GB.
- Bulk jobs (re-embed, imports): on demand, client concurrency 8 at most (more only adds latency), taking 57 passages/s and about 91 percent of the GPU; ask clients to cap bulk concurrency at 2-4 so queries keep slots. This throttle was not measured.
- Image and audio: on demand only, a second instance with `--mmproj` and `--sleep-idle-seconds` (for example 300), about 1.1 GB while awake, 0.28 GB asleep, 0.55 s to wake.
- Where query p95 passes 800 ms (queries only, qc means simultaneous clients): shared server with bulk running, measured p95 651 ms at qc=64, so the crossing is above 64 and was not measured (at 124 queries/s a rough estimate is about 90-100); alone, throughput peaks near 187 queries/s so the crossing is likely above 100, not measured; isolated 2-thread server, between qc=32 (632 ms) and 64 (1,192 ms), about 40 by interpolation. Real traffic of a few clients is far below all of these.
- Caveat: the GPU is shared with whatever the live services use; measurements ran at 73-77 percent free memory and 1.7-5 load. Production load at other times may lower these figures.

## Not measured
Per-size image latency and image token counts; embedding quality (any modality); queries alone at qc 4, 32, 64; query p95 crossing beyond qc=64 on shared and alone; bulk-throttle effect; powermetrics-based GPU power; cache or threads effects on query latency. Total wall time ran about 40 minutes, above the 25 minute target, because two scripts needed fixes (a hung `wait`, a missing query-only path) and were rerun.

## Cleanup
`~/embed-bench` on the mini (binaries, models, texts, logs, results) is deleted, the watchdog is stopped, no llama-server, ecbench or watchdog process remains, nothing under `~/.fieldcraft` was modified (only read-only sqlite3 sampling plus the CLI queries noted above), no launchd job or system-wide install was touched, no sudo used. The text samples never left the mini. Local `/tmp` working files were removed. The tools in this directory are uncommitted.
