// Command load_bench measures fieldcraft-server query latency under concurrency
// and checks that two servers answer the same queries identically.
//
// Subcommands:
//
//	stub     a stand-in for the Gemini batchEmbedContents API: deterministic
//	         vectors from a hash of the text, optional fixed delay. Lets the
//	         before/after runs embed queries without an API key or network.
//	run      fire logged queries at one server with N concurrent workers and
//	         print p50/p95/p99/max latency plus the server's /status counters.
//	compare  send the same queries, one at a time, to two servers and report
//	         how many answers differ (ids, order, scores).
//
// Queries come from a JSON-lines file of {"q": ..., "project": ...} objects,
// exported from retrieval_log with sqlite3 (see README.md).
package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"hash/fnv"
	"io"
	"math"
	"math/rand"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strings"
	"sync"
	"time"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: load_bench stub|run|compare [flags]")
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "stub":
		err = stub(os.Args[2:])
	case "run":
		err = run(os.Args[2:])
	case "compare":
		err = compare(os.Args[2:])
	default:
		err = fmt.Errorf("unknown subcommand %q", os.Args[1])
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

// ---------------------------------------------------------------------------
// stub
// ---------------------------------------------------------------------------

func stub(args []string) error {
	fs := flag.NewFlagSet("stub", flag.ExitOnError)
	addr := fs.String("addr", "127.0.0.1:7790", "listen address")
	dim := fs.Int("dim", 768, "vector dimensions")
	delay := fs.Duration("delay", 0, "fixed delay per request")
	_ = fs.Parse(args)

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasSuffix(r.URL.Path, ":batchEmbedContents") {
			http.NotFound(w, r)
			return
		}
		var body struct {
			Requests []struct {
				Content struct {
					Parts []struct {
						Text string `json:"text"`
					} `json:"parts"`
				} `json:"content"`
			} `json:"requests"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		if *delay > 0 {
			time.Sleep(*delay)
		}
		type values struct {
			Values []float64 `json:"values"`
		}
		out := struct {
			Embeddings []values `json:"embeddings"`
		}{}
		for _, req := range body.Requests {
			text := ""
			for _, p := range req.Content.Parts {
				text += p.Text
			}
			out.Embeddings = append(out.Embeddings, values{Values: hashVector(text, *dim)})
		}
		w.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	})
	fmt.Fprintf(os.Stderr, "stub embedder on %s dim=%d delay=%s\n", *addr, *dim, *delay)
	return http.ListenAndServe(*addr, nil)
}

func hashVector(text string, dim int) []float64 {
	h := fnv.New64a()
	_, _ = h.Write([]byte(text))
	rng := rand.New(rand.NewSource(int64(h.Sum64())))
	v := make([]float64, dim)
	for i := range v {
		v[i] = rng.NormFloat64()
	}
	return v
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

type query struct {
	Q       string `json:"q"`
	Project string `json:"project"`
}

func loadQueries(path string, limit int) ([]query, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var out []query
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<24)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" {
			continue
		}
		var q query
		if err := json.Unmarshal([]byte(line), &q); err != nil {
			return nil, fmt.Errorf("bad query line: %w", err)
		}
		if strings.TrimSpace(q.Q) == "" {
			continue
		}
		out = append(out, q)
		if limit > 0 && len(out) >= limit {
			break
		}
	}
	return out, sc.Err()
}

var client = &http.Client{Timeout: 120 * time.Second}

func enroll(base string) (string, error) {
	resp, err := client.Post(base+"/enroll", "application/json", strings.NewReader(`{"label":"load-bench"}`))
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	var out struct {
		Token string `json:"token"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil || out.Token == "" {
		return "", fmt.Errorf("enroll failed: HTTP %d", resp.StatusCode)
	}
	return out.Token, nil
}

type hit struct {
	ID    string  `json:"id"`
	Score float64 `json:"score"`
}

func search(base, token string, q query, limit int) ([]hit, []byte, time.Duration, error) {
	v := url.Values{}
	v.Set("q", q.Q)
	if q.Project != "" {
		v.Set("project", q.Project)
	}
	v.Set("limit", fmt.Sprint(limit))
	req, _ := http.NewRequest("GET", base+"/lessons?"+v.Encode(), nil)
	req.Header.Set("authorization", "Bearer "+token)
	start := time.Now()
	resp, err := client.Do(req)
	if err != nil {
		return nil, nil, time.Since(start), err
	}
	body, err := io.ReadAll(resp.Body)
	resp.Body.Close()
	elapsed := time.Since(start)
	if err != nil {
		return nil, nil, elapsed, err
	}
	if resp.StatusCode != 200 {
		return nil, body, elapsed, fmt.Errorf("HTTP %d: %s", resp.StatusCode, truncate(string(body), 200))
	}
	var hits []hit
	if err := json.Unmarshal(body, &hits); err != nil {
		return nil, body, elapsed, err
	}
	return hits, body, elapsed, nil
}

func status(base, token string) map[string]any {
	req, _ := http.NewRequest("GET", base+"/status", nil)
	req.Header.Set("authorization", "Bearer "+token)
	resp, err := client.Do(req)
	if err != nil {
		return nil
	}
	defer resp.Body.Close()
	var out map[string]any
	_ = json.NewDecoder(resp.Body).Decode(&out)
	delete(out, "rows")
	return out
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n]
}

func percentile(sorted []time.Duration, p float64) time.Duration {
	if len(sorted) == 0 {
		return 0
	}
	idx := int(math.Ceil(p/100*float64(len(sorted)))) - 1
	if idx < 0 {
		idx = 0
	}
	if idx >= len(sorted) {
		idx = len(sorted) - 1
	}
	return sorted[idx]
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

func run(args []string) error {
	fs := flag.NewFlagSet("run", flag.ExitOnError)
	base := fs.String("url", "http://127.0.0.1:7801", "server base URL")
	path := fs.String("queries", "", "JSON-lines query file")
	conc := fs.Int("c", 1, "concurrent workers")
	total := fs.Int("n", 200, "total requests")
	warm := fs.Int("warmup", 5, "sequential warm-up requests (not measured)")
	offset := fs.Int("offset", 0, "skip this many queries first (use fresh queries per run)")
	label := fs.String("label", "", "label printed with the result")
	_ = fs.Parse(args)

	qs, err := loadQueries(*path, 0)
	if err != nil {
		return err
	}
	if len(qs) == 0 {
		return fmt.Errorf("no queries")
	}
	token, err := enroll(*base)
	if err != nil {
		return err
	}
	for i := 0; i < *warm; i++ {
		_, _, _, _ = search(*base, token, qs[i%len(qs)], 10)
	}
	before := status(*base, token)

	jobs := make(chan query)
	var mu sync.Mutex
	var lat []time.Duration
	errs := 0
	var wg sync.WaitGroup
	start := time.Now()
	for w := 0; w < *conc; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for q := range jobs {
				_, _, d, err := search(*base, token, q, 10)
				mu.Lock()
				if err != nil {
					errs++
				} else {
					lat = append(lat, d)
				}
				mu.Unlock()
			}
		}()
	}
	for i := 0; i < *total; i++ {
		jobs <- qs[(*offset+*warm+i)%len(qs)]
	}
	close(jobs)
	wg.Wait()
	wall := time.Since(start)
	after := status(*base, token)

	sort.Slice(lat, func(i, j int) bool { return lat[i] < lat[j] })
	ms := func(d time.Duration) float64 { return float64(d.Microseconds()) / 1000 }
	result := map[string]any{
		"label":       *label,
		"concurrency": *conc,
		"requests":    *total,
		"errors":      errs,
		"p50_ms":      ms(percentile(lat, 50)),
		"p95_ms":      ms(percentile(lat, 95)),
		"p99_ms":      ms(percentile(lat, 99)),
		"max_ms":      ms(percentile(lat, 100)),
		"wall_s":      wall.Seconds(),
		"qps":         float64(len(lat)) / wall.Seconds(),
		"status_diff": statusDiff(before, after),
	}
	out, _ := json.Marshal(result)
	fmt.Println(string(out))
	return nil
}

// statusDiff reports numeric /status counters (nested ones as dotted keys)
// that changed during the run.
func statusDiff(before, after map[string]any) map[string]float64 {
	out := map[string]float64{}
	b, a := map[string]float64{}, map[string]float64{}
	flatten("", before, b)
	flatten("", after, a)
	for k, av := range a {
		if av != b[k] {
			out[k] = av - b[k]
		}
	}
	return out
}

func flatten(prefix string, m map[string]any, out map[string]float64) {
	for k, v := range m {
		switch t := v.(type) {
		case float64:
			out[prefix+k] = t
		case map[string]any:
			flatten(prefix+k+".", t, out)
		}
	}
}

// ---------------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------------

func compare(args []string) error {
	fs := flag.NewFlagSet("compare", flag.ExitOnError)
	a := fs.String("a", "http://127.0.0.1:7801", "server A")
	b := fs.String("b", "http://127.0.0.1:7802", "server B")
	path := fs.String("queries", "", "JSON-lines query file")
	n := fs.Int("n", 500, "number of queries")
	limit := fs.Int("limit", 30, "results per query")
	_ = fs.Parse(args)

	qs, err := loadQueries(*path, *n)
	if err != nil {
		return err
	}
	ta, err := enroll(*a)
	if err != nil {
		return err
	}
	tb, err := enroll(*b)
	if err != nil {
		return err
	}
	same, differ, failed := 0, 0, 0
	var examples []string
	for _, q := range qs {
		ha, _, _, ea := search(*a, ta, q, *limit)
		hb, _, _, eb := search(*b, tb, q, *limit)
		if ea != nil || eb != nil {
			failed++
			continue
		}
		if equalHits(ha, hb) {
			same++
			continue
		}
		differ++
		if len(examples) < 5 {
			examples = append(examples, fmt.Sprintf("q=%q a=%v b=%v", truncate(q.Q, 60), ha, hb))
		}
	}
	out, _ := json.Marshal(map[string]any{
		"queries": len(qs), "identical": same, "different": differ, "failed": failed,
		"examples": examples,
	})
	fmt.Println(string(out))
	if differ > 0 || failed > 0 {
		os.Exit(1)
	}
	return nil
}

func equalHits(a, b []hit) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i].ID != b[i].ID || math.Abs(a[i].Score-b[i].Score) > 1e-6 {
			return false
		}
	}
	return true
}
