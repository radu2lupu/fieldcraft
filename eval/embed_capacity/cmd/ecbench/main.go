// ecbench: capacity benchmark client for llama-server /v1/embeddings.
// Standard library only. Never prints input text; only counts, sizes and timings.
package main

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

var client = &http.Client{Timeout: 120 * time.Second, Transport: &http.Transport{MaxIdleConnsPerHost: 64}}

func post(url string, body any) ([]byte, error) {
	b, _ := json.Marshal(body)
	resp, err := client.Post(url, "application/json", bytes.NewReader(b))
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	out, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 200 {
		n := len(out)
		if n > 160 {
			n = 160
		}
		return nil, fmt.Errorf("status %d: %s", resp.StatusCode, out[:n])
	}
	return out, nil
}

func readJSONL(path string) []string {
	f, err := os.Open(path)
	if err != nil {
		fatal(err)
	}
	defer f.Close()
	var out []string
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<26)
	for sc.Scan() {
		var s string
		if json.Unmarshal(sc.Bytes(), &s) == nil {
			out = append(out, s)
		}
	}
	return out
}

func fatal(err error) { fmt.Fprintln(os.Stderr, "error:", err); os.Exit(1) }

func pct(v []float64, p float64) float64 {
	if len(v) == 0 {
		return 0
	}
	s := append([]float64(nil), v...)
	sort.Float64s(s)
	i := int(p * float64(len(s)-1))
	return s[i]
}

type stats struct {
	N        int     `json:"n"`
	Errors   int64   `json:"errors"`
	Wall     float64 `json:"wall_s"`
	PerSec   float64 `json:"items_per_s"`
	Tokens   int64   `json:"tokens"`
	TokPerS  float64 `json:"tokens_per_s"`
	P50, P95 float64
	P99, Max float64
}

func (s stats) String() string {
	return fmt.Sprintf("{\"n\":%d,\"errors\":%d,\"wall_s\":%.2f,\"items_per_s\":%.1f,\"tokens_per_s\":%.0f,\"p50_ms\":%.0f,\"p95_ms\":%.0f,\"p99_ms\":%.0f,\"max_ms\":%.0f}",
		s.N, s.Errors, s.Wall, s.PerSec, s.TokPerS, s.P50, s.P95, s.P99, s.Max)
}

type embResp struct {
	Data  []json.RawMessage `json:"data"`
	Usage struct {
		Prompt int64 `json:"prompt_tokens"`
	} `json:"usage"`
}

// runPool issues requests through `conc` workers. Each request embeds `per` items taken
// round-robin from items. Stops after n requests or when stop is closed.
func runPool(url string, items []string, conc, n, per int, stop <-chan struct{}, mk func(batch []string) any) stats {
	var next int64 = -1
	var errs, toks int64
	var mu sync.Mutex
	var lat []float64
	var wg sync.WaitGroup
	t0 := time.Now()
	for w := 0; w < conc; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				i := int(atomic.AddInt64(&next, 1))
				if n > 0 && i >= n {
					return
				}
				select {
				case <-stop:
					return
				default:
				}
				batch := make([]string, per)
				for k := 0; k < per; k++ {
					batch[k] = items[(i*per+k)%len(items)]
				}
				t := time.Now()
				out, err := post(url, mk(batch))
				d := float64(time.Since(t).Microseconds()) / 1000
				if err != nil {
					atomic.AddInt64(&errs, 1)
					continue
				}
				var r embResp
				json.Unmarshal(out, &r)
				if len(r.Data) != per {
					atomic.AddInt64(&errs, 1)
					continue
				}
				atomic.AddInt64(&toks, r.Usage.Prompt)
				mu.Lock()
				lat = append(lat, d)
				mu.Unlock()
			}
		}()
	}
	wg.Wait()
	wall := time.Since(t0).Seconds()
	s := stats{N: len(lat), Errors: errs, Wall: wall, Tokens: toks}
	s.PerSec = float64(len(lat)*per) / wall
	s.TokPerS = float64(toks) / wall
	s.P50, s.P95, s.P99 = pct(lat, .5), pct(lat, .95), pct(lat, .99)
	for _, v := range lat {
		if v > s.Max {
			s.Max = v
		}
	}
	return s
}

func textReq(batch []string) any {
	if len(batch) == 1 {
		return map[string]any{"input": batch[0], "model": "x", "encoding_format": "float"}
	}
	return map[string]any{"input": batch, "model": "x", "encoding_format": "float"}
}

func main() {
	if len(os.Args) < 2 {
		fatal(fmt.Errorf("usage: ecbench prep|run|mix|media|get|timecmd|count"))
	}
	cmd := os.Args[1]
	fs := flag.NewFlagSet(cmd, flag.ExitOnError)
	url := fs.String("url", "http://127.0.0.1:8081", "server base url")
	in := fs.String("in", "", "input jsonl of strings (prompts)")
	out := fs.String("out", "", "output jsonl")
	conc := fs.Int("c", 1, "concurrency")
	n := fs.Int("n", 0, "total requests (0 = one pass over input)")
	per := fs.Int("per", 1, "items per request")
	prefix := fs.String("prefix", "", "prefix added when prepping")
	maxTok := fs.Int("maxtok", 500, "max text tokens when prepping")
	// mix
	burl := fs.String("burl", "", "bulk server url")
	bin := fs.String("bin", "", "bulk prompts")
	bc := fs.Int("bc", 8, "bulk concurrency")
	qurl := fs.String("qurl", "", "query server url")
	qin := fs.String("qin", "", "query prompts")
	qc := fs.Int("qc", 1, "query concurrency")
	qreps := fs.Int("qreps", 3, "passes over query set")
	// media
	kind := fs.String("kind", "image", "image|audio")
	dir := fs.String("dir", "", "media dir")
	reps := fs.Int("reps", 1, "repeat count (get/timecmd)")
	fs.Parse(os.Args[2:])

	switch cmd {
	case "prep":
		prep(*url, *in, *out, *prefix, *maxTok)
	case "run":
		items := readJSONL(*in)
		cnt := *n
		if cnt == 0 {
			cnt = len(items) / *per
		}
		s := runPool(*url+"/v1/embeddings", items, *conc, cnt, *per, nil, textReq)
		fmt.Println(s)
	case "mix":
		var bulk []string
		if *bc > 0 {
			bulk = readJSONL(*bin)
		}
		qs := readJSONL(*qin)
		stop := make(chan struct{})
		var bs stats
		var wg sync.WaitGroup
		if *bc > 0 {
			wg.Add(1)
			go func() { defer wg.Done(); bs = runPool(*burl+"/v1/embeddings", bulk, *bc, 0, *per, stop, textReq) }()
			time.Sleep(2 * time.Second) // let bulk reach steady state
		}
		qs0 := runPool(*qurl+"/v1/embeddings", qs, *qc, len(qs)**qreps, 1, nil, textReq)
		close(stop)
		wg.Wait()
		fmt.Printf("query %s\n", qs0)
		if *bc > 0 {
			fmt.Printf("bulk  %s\n", bs)
		}
	case "media":
		media(*url, *dir, *kind, *conc, *reps)
	case "get":
		var lat []float64
		for i := 0; i < *reps; i++ {
			t := time.Now()
			resp, err := client.Get(*url)
			if err != nil {
				fatal(err)
			}
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
			lat = append(lat, float64(time.Since(t).Microseconds())/1000)
		}
		fmt.Printf("{\"n\":%d,\"p50_ms\":%.0f,\"p95_ms\":%.0f,\"max_ms\":%.0f,\"min_ms\":%.0f}\n", len(lat), pct(lat, .5), pct(lat, .95), pct(lat, 1), pct(lat, 0))
	case "timecmd":
		var lat []float64
		fails := 0
		args := fs.Args()
		for i := 0; i < *reps; i++ {
			c := exec.Command(args[0], args[1:]...)
			t := time.Now()
			if err := c.Run(); err != nil { // output discarded
				fails++
			}
			lat = append(lat, float64(time.Since(t).Microseconds())/1000)
		}
		fmt.Printf("{\"failed\":%d,\"n\":%d,\"p50_ms\":%.0f,\"p95_ms\":%.0f,\"max_ms\":%.0f,\"min_ms\":%.0f}\n", fails, len(lat), pct(lat, .5), pct(lat, .95), pct(lat, 1), pct(lat, 0))
	case "count":
		items := readJSONL(*in)
		var l []float64
		for _, s := range items {
			l = append(l, float64(len(s)))
		}
		fmt.Printf("n=%d chars p50=%.0f p95=%.0f max=%.0f\n", len(l), pct(l, .5), pct(l, .95), pct(l, 1))
	}
}

// prep tokenizes with the server, truncates text to maxTok tokens and writes prefix+text.
func prep(url, in, out, prefix string, maxTok int) {
	items := readJSONL(in)
	res := make([]string, len(items))
	ntok := make([]float64, len(items))
	var trunc int64
	var wg sync.WaitGroup
	ch := make(chan int)
	for w := 0; w < 4; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range ch {
				b, err := post(url+"/tokenize", map[string]any{"content": items[i], "add_special": false})
				if err != nil {
					fatal(err)
				}
				var t struct{ Tokens []int }
				json.Unmarshal(b, &t)
				ntok[i] = float64(len(t.Tokens))
				text := items[i]
				if len(t.Tokens) > maxTok {
					b, err = post(url+"/detokenize", map[string]any{"tokens": t.Tokens[:maxTok]})
					if err != nil {
						fatal(err)
					}
					var d struct{ Content string }
					json.Unmarshal(b, &d)
					text = d.Content
					atomic.AddInt64(&trunc, 1)
				}
				res[i] = prefix + text
			}
		}()
	}
	for i := range items {
		ch <- i
	}
	close(ch)
	wg.Wait()
	f, _ := os.OpenFile(out, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0600)
	defer f.Close()
	w := bufio.NewWriter(f)
	for _, s := range res {
		b, _ := json.Marshal(s)
		w.Write(b)
		w.WriteString("\n")
	}
	w.Flush()
	var sum float64
	for _, v := range ntok {
		sum += v
	}
	fmt.Printf("prepped=%d raw_tokens avg=%.0f p50=%.0f p95=%.0f max=%.0f truncated_over_%d=%d\n", len(items), sum/float64(len(items)), pct(ntok, .5), pct(ntok, .95), pct(ntok, 1), maxTok, trunc)
}

// media embeds every file in dir (png/jpg for image, wav for audio) one request at a time per worker.
func media(url, dir, kind string, conc, reps int) {
	ents, err := os.ReadDir(dir)
	if err != nil {
		fatal(err)
	}
	var datas []any
	for _, e := range ents {
		name := e.Name()
		low := strings.ToLower(name)
		b, _ := os.ReadFile(dir + "/" + name)
		enc := base64.StdEncoding.EncodeToString(b)
		switch {
		case kind == "image" && (strings.HasSuffix(low, ".png") || strings.HasSuffix(low, ".jpg")):
			mime := "image/png"
			if strings.HasSuffix(low, ".jpg") {
				mime = "image/jpeg"
			}
			datas = append(datas, map[string]any{"content": []any{map[string]any{"type": "image_url", "image_url": map[string]any{"url": "data:" + mime + ";base64," + enc}}}})
		case kind == "audio" && strings.HasSuffix(low, ".wav"):
			datas = append(datas, map[string]any{"content": []any{map[string]any{"type": "input_audio", "input_audio": map[string]any{"data": enc, "format": "wav"}}}})
		}
	}
	fmt.Printf("files=%d\n", len(datas))
	var lat []float64
	var errMsg string
	var idx int64 = -1
	var mu sync.Mutex
	var wg sync.WaitGroup
	total := len(datas) * reps
	t0 := time.Now()
	for w := 0; w < conc; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				i := int(atomic.AddInt64(&idx, 1))
				if i >= total {
					return
				}
				t := time.Now()
				b, err := post(url+"/v1/embeddings", map[string]any{"input": []any{datas[i%len(datas)]}, "model": "x"})
				d := float64(time.Since(t).Microseconds()) / 1000
				mu.Lock()
				if err != nil {
					errMsg = err.Error()
				} else {
					var r embResp
					json.Unmarshal(b, &r)
					if len(r.Data) == 1 {
						lat = append(lat, d)
					} else {
						errMsg = "no data in response"
					}
				}
				mu.Unlock()
			}
		}()
	}
	wg.Wait()
	fmt.Printf("{\"ok\":%d,\"of\":%d,\"wall_s\":%.2f,\"p50_ms\":%.0f,\"p95_ms\":%.0f,\"max_ms\":%.0f,\"first_error\":%q}\n", len(lat), total, time.Since(t0).Seconds(), pct(lat, .5), pct(lat, .95), pct(lat, 1), errMsg)
}
