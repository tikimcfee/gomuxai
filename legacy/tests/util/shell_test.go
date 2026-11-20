package util_test

import (
	"testing"

	"github.com/tikimcfee/gomuxai/internal/util"
)

func TestShellJoin(t *testing.T) {
	tests := []struct {
		words []string
		want  string
	}{
		{nil, ""},
		{[]string{"wait", "helper@hub", "--for", "idle,waiting", "--timeout=10m"}, "wait helper@hub --for idle,waiting --timeout=10m"},
		{[]string{"send", "helper", "review auth.go"}, "send helper 'review auth.go'"},
		{[]string{"friction", "add", "it's broken", ""}, `friction add 'it'\''s broken' ''`},
		{[]string{"keys", "s", "$HOME", "a*b"}, "keys s '$HOME' 'a*b'"},
	}
	for _, tt := range tests {
		if got := util.ShellJoin(tt.words); got != tt.want {
			t.Errorf("ShellJoin(%q) = %s, want %s", tt.words, got, tt.want)
		}
	}
}
