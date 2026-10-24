package util_test

import (
	"slices"
	"strconv"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/util"
)

func TestSimilar(t *testing.T) {
	names := []string{"helper", "helper2", "builder", "reviewer", "lead-build", "Helper3"}
	tests := []struct {
		name string
		want []string
	}{
		{"helpr", []string{"helper", "helper2", "Helper3"}},
		{"helper", []string{"helper2", "Helper3"}}, // itself excluded
		{"lead", []string{"lead-build"}},           // a prefix
		{"zzz", nil},
		{"b", nil}, // a one-letter prefix is too loose
	}
	for _, tt := range tests {
		if got := util.Similar(tt.name, names); !slices.Equal(got, tt.want) {
			t.Errorf("Similar(%q) = %q, want %q", tt.name, got, tt.want)
		}
	}
}

func TestOrList(t *testing.T) {
	tests := []struct {
		items []string
		want  string
	}{
		{nil, ""},
		{[]string{"a"}, `"a"`},
		{[]string{"a", "b"}, `"a" or "b"`},
		{[]string{"a", "b", "c"}, `"a", "b", or "c"`},
	}
	for _, tt := range tests {
		if got := util.OrList(tt.items, strconv.Quote); got != tt.want {
			t.Errorf("OrList(%q) = %s, want %s", tt.items, got, tt.want)
		}
	}
	if got := util.OrList([]string{"a", "b"}, nil); got != "a or b" {
		t.Errorf("OrList unquoted = %s", got)
	}
}
