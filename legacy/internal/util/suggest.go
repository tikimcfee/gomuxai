package util

import (
	"sort"
	"strings"
)

// maxSuggestions caps how many close matches Similar returns.
const maxSuggestions = 3

// Similar returns the candidates close to name, closest first, ignoring
// case: those within one edit per three letters of the longer word (at least
// one; swapping two neighbors counts as one edit), or that start with name
// when it has three letters or more. So "stauts" suggests "status", while
// "bogus" suggests neither "logs" nor "bus", two edits from a five-letter
// word. It returns at most three, for "did you mean" suggestions.
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
		maxDist := max(1, max(len([]rune(lower)), len([]rune(lc)))/3)
		if d <= maxDist || (len([]rune(lower)) >= 3 && strings.HasPrefix(lc, lower)) {
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

// editDistance is the edit distance between a and b, counting a swap of two
// neighbors ("lsit" for "list") as one edit, like an insertion, deletion, or
// substitution (optimal string alignment).
func editDistance(a, b string) int {
	ra, rb := []rune(a), []rune(b)
	prev2 := make([]int, len(rb)+1) // row i-2, for swaps
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
			if i > 1 && j > 1 && ra[i-1] == rb[j-2] && ra[i-2] == rb[j-1] {
				cur[j] = min(cur[j], prev2[j-2]+1)
			}
		}
		prev2, prev, cur = prev, cur, prev2
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
