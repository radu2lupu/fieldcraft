// Command embed_compare replays logged queries against two fieldcraft servers
// that differ only in their embedding provider (arm A, arm B) and reports how
// much their answers agree and how well each ranks the lessons agents cited.
//
// Input: JSON lines {"log_id":N, "q":"...", "project":"...", "cited":["id",...]}
// where "cited" lists lessons that were served for that logged query and then
// cited (lesson:<id>) in the same session. Queries with an empty "cited" only
// count toward agreement.
//
// Metrics (top 10 per arm):
//
//	overlap@5     |A5 ∩ B5| / 5, mean over all queries
//	MRR@10        1/rank of the first cited lesson, 0 if absent, mean over cited queries
//	hit@1, recall@10 over cited queries
//
// The B−A difference in MRR@10 and hit@1 gets a paired bootstrap 95% CI.
// Caveat: cited lessons were chosen from what the logged (Gemini-era)
// retrieval served, which favours arm A when A is Gemini.
package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"math/rand"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"
)

type query struct {
	LogID   int64    `json:"log_id"`
	Q       string   `json:"q"`
	Project string   `json:"project"`
	Cited   []string `json:"cited"`
}

type hit struct {
	ID string `json:"id"`
}

type result struct {
	LogID     int64    `json:"log_id"`
	Cited     []string `json:"cited,omitempty"`
	A         []string `json:"a"`
	B         []string `json:"b"`
	Overlap5  float64  `json:"overlap5"`
	RRA       float64  `json:"rr_a"`
	RRB       float64  `json:"rr_b"`
	LatencyA  float64  `json:"ms_a"`
	LatencyB  float64  `json:"ms_b"`
	ErrorText string   `json:"error,omitempty"`
}

var client = &http.Client{Timeout: 60 * time.Second}

func main() {
	a := flag.String("a", "http://127.0.0.1:7801", "arm A server")
	b := flag.String("b", "http://127.0.0.1:7802", "arm B server")
	labelA := flag.String("label-a", "A", "arm A name")
	labelB := flag.String("label-b", "B", "arm B name")
	path := flag.String("queries", "", "JSON-lines query file")
	details := flag.String("details", "", "optional JSON-lines file for per-query results (private: holds lesson ids)")
	boot := flag.Int("bootstrap", 10000, "bootstrap resamples")
	flag.Parse()

	qs, err := load(*path)
	if err != nil {
		fail(err)
	}
	ta, err := enroll(*a)
	if err != nil {
		fail(err)
	}
	tb, err := enroll(*b)
	if err != nil {
		fail(err)
	}
	var out *bufio.Writer
	if *details != "" {
		f, err := os.Create(*details)
		if err != nil {
			fail(err)
		}
		defer f.Close()
		out = bufio.NewWriter(f)
		defer out.Flush()
	}

	var results []result
	for i, q := range qs {
		ra, msA, errA := search(*a, ta, q)
		rb, msB, errB := search(*b, tb, q)
		r := result{LogID: q.LogID, Cited: q.Cited, A: ra, B: rb, LatencyA: msA, LatencyB: msB}
		if errA != nil || errB != nil {
			r.ErrorText = fmt.Sprint(errA, " | ", errB)
		} else {
			r.Overlap5 = overlap(ra, rb, 5)
			r.RRA = reciprocalRank(ra, q.Cited)
			r.RRB = reciprocalRank(rb, q.Cited)
		}
		results = append(results, r)
		if out != nil {
			line, _ := json.Marshal(r)
			out.Write(append(line, '\n'))
		}
		if (i+1)%50 == 0 {
			fmt.Fprintf(os.Stderr, "%d/%d\n", i+1, len(qs))
		}
	}
	summary := summarize(results, *labelA, *labelB, *boot)
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	_ = enc.Encode(summary)
}

func summarize(rs []result, la, lb string, boot int) map[string]any {
	var overlaps, rrA, rrB, h1A, h1B, recA, recB, latA, latB []float64
	errors := 0
	for _, r := range rs {
		if r.ErrorText != "" {
			errors++
			continue
		}
		overlaps = append(overlaps, r.Overlap5)
		latA = append(latA, r.LatencyA)
		latB = append(latB, r.LatencyB)
		if len(r.Cited) == 0 {
			continue
		}
		rrA = append(rrA, r.RRA)
		rrB = append(rrB, r.RRB)
		h1A = append(h1A, b2f(r.RRA == 1))
		h1B = append(h1B, b2f(r.RRB == 1))
		recA = append(recA, b2f(r.RRA > 0))
		recB = append(recB, b2f(r.RRB > 0))
	}
	lo, hi := pairedCI(rrA, rrB, boot)
	hlo, hhi := pairedCI(h1A, h1B, boot)
	return map[string]any{
		"queries":             len(rs),
		"errors":              errors,
		"cited_queries":       len(rrA),
		"overlap_at_5":        mean(overlaps),
		"mrr_at_10":           map[string]float64{la: mean(rrA), lb: mean(rrB)},
		"hit_at_1":            map[string]float64{la: mean(h1A), lb: mean(h1B)},
		"recall_at_10":        map[string]float64{la: mean(recA), lb: mean(recB)},
		"mrr_diff_b_minus_a":  mean(rrB) - mean(rrA),
		"mrr_diff_ci95":       []float64{lo, hi},
		"hit1_diff_b_minus_a": mean(h1B) - mean(h1A),
		"hit1_diff_ci95":      []float64{hlo, hhi},
		"latency_ms_p50":      map[string]float64{la: pct(latA, 50), lb: pct(latB, 50)},
		"latency_ms_p95":      map[string]float64{la: pct(latA, 95), lb: pct(latB, 95)},
	}
}

func pairedCI(a, b []float64, n int) (float64, float64) {
	if len(a) == 0 || n <= 0 {
		return 0, 0
	}
	rng := rand.New(rand.NewSource(42))
	diffs := make([]float64, n)
	for i := 0; i < n; i++ {
		s := 0.0
		for j := 0; j < len(a); j++ {
			k := rng.Intn(len(a))
			s += b[k] - a[k]
		}
		diffs[i] = s / float64(len(a))
	}
	sort.Float64s(diffs)
	return diffs[int(0.025*float64(n))], diffs[int(0.975*float64(n))-1]
}

func overlap(a, b []string, k int) float64 {
	set := map[string]bool{}
	for _, id := range head(a, k) {
		set[id] = true
	}
	n := 0
	for _, id := range head(b, k) {
		if set[id] {
			n++
		}
	}
	return float64(n) / float64(k)
}

func reciprocalRank(ids, cited []string) float64 {
	want := map[string]bool{}
	for _, c := range cited {
		want[c] = true
	}
	for i, id := range head(ids, 10) {
		if want[id] {
			return 1 / float64(i+1)
		}
	}
	return 0
}

func head(s []string, k int) []string {
	if len(s) > k {
		return s[:k]
	}
	return s
}

func search(base, token string, q query) ([]string, float64, error) {
	v := url.Values{}
	v.Set("q", q.Q)
	if q.Project != "" {
		v.Set("project", q.Project)
	}
	v.Set("limit", "10")
	req, _ := http.NewRequest("GET", base+"/lessons?"+v.Encode(), nil)
	req.Header.Set("authorization", "Bearer "+token)
	start := time.Now()
	resp, err := client.Do(req)
	if err != nil {
		return nil, 0, err
	}
	body, err := io.ReadAll(resp.Body)
	resp.Body.Close()
	ms := float64(time.Since(start).Microseconds()) / 1000
	if err != nil {
		return nil, ms, err
	}
	if resp.StatusCode != 200 {
		return nil, ms, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	var hits []hit
	if err := json.Unmarshal(body, &hits); err != nil {
		return nil, ms, err
	}
	ids := make([]string, len(hits))
	for i, h := range hits {
		ids[i] = h.ID
	}
	return ids, ms, nil
}

func enroll(base string) (string, error) {
	resp, err := client.Post(base+"/enroll", "application/json", strings.NewReader(`{"label":"embed-compare"}`))
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	var out struct {
		Token string `json:"token"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil || out.Token == "" {
		return "", fmt.Errorf("enroll at %s failed: HTTP %d", base, resp.StatusCode)
	}
	return out.Token, nil
}

func load(path string) ([]query, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var qs []query
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<24)
	for sc.Scan() {
		if strings.TrimSpace(sc.Text()) == "" {
			continue
		}
		var q query
		if err := json.Unmarshal(sc.Bytes(), &q); err != nil {
			return nil, err
		}
		qs = append(qs, q)
	}
	return qs, sc.Err()
}

func mean(xs []float64) float64 {
	if len(xs) == 0 {
		return 0
	}
	s := 0.0
	for _, x := range xs {
		s += x
	}
	return s / float64(len(xs))
}

func pct(xs []float64, p float64) float64 {
	if len(xs) == 0 {
		return 0
	}
	s := append([]float64(nil), xs...)
	sort.Float64s(s)
	i := int(p/100*float64(len(s))+0.5) - 1
	if i < 0 {
		i = 0
	}
	if i >= len(s) {
		i = len(s) - 1
	}
	return s[i]
}

func b2f(b bool) float64 {
	if b {
		return 1
	}
	return 0
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "error:", err)
	os.Exit(1)
}
