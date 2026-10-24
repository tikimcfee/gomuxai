package util

import (
	"sort"
	"strings"
)

// maxSuggestions caps how many close matches Similar returns.
const maxSuggestions = 3

// Similar returns the candidates close to name, closest first: those within
// two edits of it (ignoring case), or that start with it. It returns at most
// three, for "did you mean" suggestions.
func Similar(name string, candidates []string) []string {
	type match struct {
		s    string
		dist int
	}
	lower := strings.ToLower(name)
	var matches []match
	for _, c := range candidates {
		if c == name {
			continue
		}
		lc := strings.ToLower(c)
		d := editDistance(lower, lc)
		if d <= 2 || (len(lower) >= 2 && strings.HasPrefix(lc, lower)) {
			matches = append(matches, match{c, d})
		}
	}
	sort.SliceStable(matches, func(i, j int) bool { return matches[i].dist < matches[j].dist })
	out := make([]string, 0, min(len(matches), maxSuggestions))
	for _, m := range matches[:min(len(matches), maxSuggestions)] {
		out = append(out, m.s)
	}
	return out
}

// editDistance is the Levenshtein distance between a and b.
func editDistance(a, b string) int {
	ra, rb := []rune(a), []rune(b)
	prev := make([]int, len(rb)+1)
	cur := make([]int, len(rb)+1)
	for j := range prev {
		prev[j] = j
	}
	for i := 1; i <= len(ra); i++ {
		cur[0] = i
		for j := 1; j <= len(rb); j++ {
			cost := 1
			if ra[i-1] == rb[j-1] {
				cost = 0
			}
			cur[j] = min(prev[j]+1, cur[j-1]+1, prev[j-1]+cost)
		}
		prev, cur = cur, prev
	}
	return prev[len(rb)]
}

// OrList joins items as "a", "a or b", or "a, b, or c", each formatted by
// quote (e.g. strconv.Quote, or nil to leave them as they are).
func OrList(items []string, quote func(string) string) string {
	q := make([]string, len(items))
	for i, s := range items {
		if quote != nil {
			s = quote(s)
		}
		q[i] = s
	}
	switch len(q) {
	case 0:
		return ""
	case 1:
		return q[0]
	case 2:
		return q[0] + " or " + q[1]
	}
	return strings.Join(q[:len(q)-1], ", ") + ", or " + q[len(q)-1]
}
