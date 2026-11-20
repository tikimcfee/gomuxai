package termd

import (
	"slices"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

const envValueMax = 128 * 1024 // Linux MAX_ARG_STRLEN (PROTOCOL.md "create")

// layer6Reserved are variables termd sets itself in layer 6 and refuses in
// create.env (though create.env_base may carry them).
var layer6Reserved = map[string]bool{
	"TERM":                 true,
	"COLORTERM":            true,
	"TERM_PROGRAM":         true,
	"TERM_PROGRAM_VERSION": true,
	"G3D_TERMD_ID":         true,
	"G3D_TERMD_SOCKET":     true,
	"PWD":                  true,
}

func validEnvName(name string) bool {
	if name == "" {
		return false
	}
	for i := 0; i < len(name); i++ {
		b := name[i]
		if i == 0 {
			if !((b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || b == '_') {
				return false
			}
			continue
		}
		if !((b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || (b >= '0' && b <= '9') || b == '_') {
			return false
		}
	}
	return true
}

func isInternalG3DVar(name string) bool {
	return strings.HasPrefix(name, "G3D_RELAY_") || strings.HasPrefix(name, "G3D_TERMD_")
}

func isShellTransientVar(name string) bool {
	switch name {
	case "SHLVL", "OLDPWD", "PWD", "_":
		return true
	default:
		return false
	}
}

// CallerEnv filters "NAME=VALUE" environment entries to those g3d-termd's
// create.env_base accepts, while scrubbing G3D_RELAY_* capability tokens,
// G3D_TERMD_* internal controls, and shell-transient state so they are never
// transmitted over the termd socket.
func CallerEnv(environ []string) map[string]string {
	out := make(map[string]string, len(environ))
	for _, kv := range environ {
		k, v, ok := strings.Cut(kv, "=")
		if !ok || !validEnvName(k) || isInternalG3DVar(k) || isShellTransientVar(k) {
			continue
		}
		if !utf8.ValidString(k) || !utf8.ValidString(v) || strings.IndexByte(v, 0) >= 0 {
			continue
		}
		if len(k)+len(v)+2 > envValueMax {
			continue
		}
		out[k] = v
	}
	return out
}

func sanitizeRequestEnv(env map[string]string) map[string]string {
	if len(env) == 0 {
		return nil
	}
	out := make(map[string]string, len(env))
	for k, v := range env {
		if !validEnvName(k) || layer6Reserved[k] || isInternalG3DVar(k) || !utf8.ValidString(v) || strings.IndexByte(v, 0) >= 0 {
			continue
		}
		if len(k)+len(v)+2 > envValueMax {
			continue
		}
		out[k] = v
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

type namedKey struct {
	aliases []string
	code    string
	key     string
	text    string
	shift   bool
}

var namedKeys = []namedKey{
	{[]string{"enter", "return", "cr", "kpenter", "numpadenter"}, "Enter", "Enter", "", false},
	{[]string{"tab"}, "Tab", "Tab", "", false},
	{[]string{"btab"}, "Tab", "Tab", "", true},
	{[]string{"escape", "esc"}, "Escape", "Escape", "", false},
	{[]string{"backspace", "bs", "bspace"}, "Backspace", "Backspace", "", false},
	{[]string{"delete", "del", "dc"}, "Delete", "Delete", "", false},
	{[]string{"insert", "ins", "ic"}, "Insert", "Insert", "", false},
	{[]string{"home"}, "Home", "Home", "", false},
	{[]string{"end"}, "End", "End", "", false},
	{[]string{"pageup", "pgup", "ppage"}, "PageUp", "PageUp", "", false},
	{[]string{"pagedown", "pgdn", "pgdown", "npage"}, "PageDown", "PageDown", "", false},
	{[]string{"arrowup", "up"}, "ArrowUp", "ArrowUp", "", false},
	{[]string{"arrowdown", "down"}, "ArrowDown", "ArrowDown", "", false},
	{[]string{"arrowleft", "left"}, "ArrowLeft", "ArrowLeft", "", false},
	{[]string{"arrowright", "right"}, "ArrowRight", "ArrowRight", "", false},
	{[]string{"space"}, "Space", " ", " ", false},
	{[]string{"plus"}, "Equal", "+", "+", true},
}

type usKey struct {
	plain   rune
	shifted rune
	code    string
}

var usLayout = []usKey{
	{'1', '!', "Digit1"},
	{'2', '@', "Digit2"},
	{'3', '#', "Digit3"},
	{'4', '$', "Digit4"},
	{'5', '%', "Digit5"},
	{'6', '^', "Digit6"},
	{'7', '&', "Digit7"},
	{'8', '*', "Digit8"},
	{'9', '(', "Digit9"},
	{'0', ')', "Digit0"},
	{'-', '_', "Minus"},
	{'=', '+', "Equal"},
	{'[', '{', "BracketLeft"},
	{']', '}', "BracketRight"},
	{'\\', '|', "Backslash"},
	{';', ':', "Semicolon"},
	{'\'', '"', "Quote"},
	{',', '<', "Comma"},
	{'.', '>', "Period"},
	{'/', '?', "Slash"},
	{'`', '~', "Backquote"},
}

func parseModName(m string) (string, bool) {
	switch strings.ToLower(m) {
	case "shift", "s":
		return "Shift", true
	case "ctrl", "control", "c":
		return "Control", true
	case "alt", "opt", "option", "m", "meta":
		if strings.EqualFold(m, "m") {
			return "Alt", true // tmux M-x is Escape/Alt
		}
		if strings.EqualFold(m, "meta") {
			return "Meta", true
		}
		return "Alt", true
	case "super", "cmd", "command":
		return "Meta", true
	default:
		return "", false
	}
}

// normalizeTmuxChord rewrites tmux modifier prefixes ("C-c", "M-Left", "C-M-x")
// into "+" chord notation ("ctrl+c", "alt+Left", "ctrl+alt+x"). Strings like
// "-N2" are left untouched so they do not masquerade as modifiers.
func normalizeTmuxChord(in string) string {
	var mods []string
	rest := in
	for len(rest) >= 3 && rest[1] == '-' {
		switch rest[0] {
		case 'C', 'c':
			mods = append(mods, "ctrl")
			rest = rest[2:]
			continue
		case 'M', 'm':
			mods = append(mods, "alt")
			rest = rest[2:]
			continue
		case 'S', 's':
			mods = append(mods, "shift")
			rest = rest[2:]
			continue
		}
		break
	}
	if len(mods) == 0 || rest == "" {
		return in
	}
	return strings.Join(append(mods, rest), "+")
}

// ParseSpecialKey maps a tmux key name ("Enter", "C-c", "BSpace", "Up", "1")
// or g3d chord ("shift+enter", "ctrl+c", "alt+left") to a W3C KeyEvent for
// g3d-termd's "key" op. It returns ok=false when key is not a recognized key
// or chord (e.g. "-N2"), allowing callers to fall back to literal bytes as
// tmux send-keys does.
func ParseSpecialKey(raw string) (KeyEvent, bool) {
	if raw == "" {
		return KeyEvent{}, false
	}
	chord := normalizeTmuxChord(raw)

	var modsPart, keyPart string
	switch {
	case chord == "+":
		keyPart = "+"
	case strings.HasSuffix(chord, "++"):
		modsPart = strings.TrimSuffix(chord, "++")
		keyPart = "+"
	default:
		if idx := strings.LastIndexByte(chord, '+'); idx >= 0 {
			modsPart = chord[:idx]
			keyPart = chord[idx+1:]
		} else {
			keyPart = chord
		}
	}
	if keyPart == "" {
		return KeyEvent{}, false
	}

	var mods []string
	addMod := func(m string) {
		if !slices.Contains(mods, m) {
			mods = append(mods, m)
		}
	}
	if modsPart != "" {
		for part := range strings.SplitSeq(modsPart, "+") {
			if part == "" {
				return KeyEvent{}, false
			}
			w3c, ok := parseModName(part)
			if !ok {
				return KeyEvent{}, false
			}
			addMod(w3c)
		}
	}

	runes := []rune(keyPart)
	if len(runes) == 1 {
		c := runes[0]
		if unicode.IsControl(c) {
			return KeyEvent{}, false
		}
		if c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' {
			if c >= 'A' && c <= 'Z' {
				addMod("Shift")
			}
			upper := unicode.ToUpper(c)
			ch := unicode.ToLower(c)
			if slices.Contains(mods, "Shift") {
				ch = upper
			}
			s := string(ch)
			return KeyEvent{Code: "Key" + string(upper), Key: s, Mods: mods, Text: s}, true
		}
		for _, us := range usLayout {
			if c == us.plain || c == us.shifted {
				if c == us.shifted {
					addMod("Shift")
				}
				ch := us.plain
				if slices.Contains(mods, "Shift") {
					ch = us.shifted
				}
				s := string(ch)
				return KeyEvent{Code: us.code, Key: s, Mods: mods, Text: s}, true
			}
		}
		if c == ' ' {
			return KeyEvent{Code: "Space", Key: " ", Mods: mods, Text: " "}, true
		}
		s := string(c)
		return KeyEvent{Key: s, Mods: mods, Text: s}, true
	}

	norm := strings.NewReplacer("_", "", "-", "").Replace(strings.ToLower(keyPart))
	if strings.HasPrefix(norm, "f") {
		if n, err := strconv.Atoi(norm[1:]); err == nil && n >= 1 && n <= 24 {
			fn := "F" + strconv.Itoa(n)
			return KeyEvent{Code: fn, Key: fn, Mods: mods}, true
		}
	}
	for _, nk := range namedKeys {
		if slices.Contains(nk.aliases, norm) {
			if nk.shift {
				addMod("Shift")
			}
			code := nk.code
			if strings.EqualFold(norm, "numpadenter") || strings.EqualFold(norm, "kpenter") {
				code = "NumpadEnter"
			}
			return KeyEvent{Code: code, Key: nk.key, Mods: mods, Text: nk.text}, true
		}
	}
	return KeyEvent{}, false
}
