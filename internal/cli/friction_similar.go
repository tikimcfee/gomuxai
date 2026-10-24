package cli

import (
	"slices"
	"strings"
	"unicode"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

// A summary looks like another if they share at least similarMinShared
// words and their Dice coefficient (shared words over average word count)
// reaches similarMinScore. A wrong match costs the filer a turn, so both
// are set to miss rather than over-match.
const (
	similarMinShared = 2
	similarMinScore  = 0.6
	similarMax       = 3 // matches add lists
)

// summaryStopWords carry no topic: function words, and words nearly every
// summary about this tool has.
var summaryStopWords = map[string]bool{}

func init() {
	for _, w := range strings.Fields(`a an the and or but if then than so to of in on at by for from with
		without as is are was were be been being it its this that these those there here when while
		which who what how why do does did doesn don didn not no can cannot could should would will
		just only also too very about into out up any some all my your our their me you we they i
		get gets got agent agents command commands`) {
		summaryStopWords[w] = true
	}
}

// summaryWords returns the topic words of a summary: lowercased, split at
// anything but letters and digits, plurals folded, stop words dropped.
func summaryWords(s string) map[string]bool {
	words := make(map[string]bool)
	for _, w := range strings.FieldsFunc(strings.ToLower(s), func(r rune) bool {
		return !unicode.IsLetter(r) && !unicode.IsDigit(r)
	}) {
		if len(w) > 3 && strings.HasSuffix(w, "s") && !strings.HasSuffix(w, "ss") {
			w = w[:len(w)-1]
		}
		if len(w) > 1 && !summaryStopWords[w] {
			words[w] = true
		}
	}
	return words
}

// summarySimilarity scores how alike two summaries' words are, from 0 to 1.
// Summaries that name different agent commands (e.g. send and queue) are
// about different things, however alike the rest.
func summarySimilarity(a, b map[string]bool) float64 {
	if len(a) == 0 || len(b) == 0 {
		return 0
	}
	shared := 0
	for w := range a {
		if b[w] {
			shared++
		}
	}
	if shared < similarMinShared {
		return 0
	}
	if ca, cb := commandWords(a), commandWords(b); len(ca) > 0 && len(cb) > 0 && !overlaps(ca, cb) {
		return 0
	}
	return 2 * float64(shared) / float64(len(a)+len(b))
}

// commandWords returns the words that name a top-level agent command.
func commandWords(words map[string]bool) map[string]bool {
	out := make(map[string]bool)
	for _, c := range rootCmd.Commands() {
		if words[c.Name()] {
			out[c.Name()] = true
		}
	}
	return out
}

func overlaps(a, b map[string]bool) bool {
	for w := range a {
		if b[w] {
			return true
		}
	}
	return false
}

// similarFriction returns the entries whose summaries look like summary,
// closest first, at most similarMax.
func similarFriction(summary string, entries []*storage.Friction) []*storage.Friction {
	words := summaryWords(summary)
	type match struct {
		f     *storage.Friction
		score float64
	}
	var matches []match
	for _, f := range entries {
		if score := summarySimilarity(words, summaryWords(f.Summary)); score >= similarMinScore {
			matches = append(matches, match{f, score})
		}
	}
	slices.SortStableFunc(matches, func(x, y match) int {
		switch {
		case x.score > y.score:
			return -1
		case x.score < y.score:
			return 1
		}
		return 0
	})
	var out []*storage.Friction
	for _, m := range matches[:min(len(matches), similarMax)] {
		out = append(out, m.f)
	}
	return out
}
