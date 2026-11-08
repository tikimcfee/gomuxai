package cli

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// frictionHost is a host in a friction test: its own store, served over
// the test bus while it's up.
type frictionHost struct {
	t     *testing.T
	nc    *nats.Conn
	cfg   *config.Config
	store *storage.SQLiteStorage
	stop  func()
}

// frictionHosts sets up the CLI on host "local" (setupStore) and each of
// others with a store of its own, all served on one test bus that the CLI
// reaches. It returns the hosts by name, "local" included.
func frictionHosts(t *testing.T, others ...string) map[string]*frictionHost {
	t.Helper()
	setupStore(t)
	t.Setenv("AGENT_NAME", "")
	nc := natstest.Start(t)
	oldConnect := busConnect
	t.Cleanup(func() { busConnect = oldConnect })
	busConnect = func() (*nats.Conn, error) { return natstest.Connect(t, nc.ConnectedUrl()), nil }

	hosts := map[string]*frictionHost{"local": {t: t, nc: nc, cfg: cfg, store: store.(*storage.SQLiteStorage)}}
	for _, name := range others {
		h := &frictionHost{t: t, nc: nc}
		h.reset(name)
		hosts[name] = h
	}
	for _, h := range hosts {
		h.up()
	}
	t.Cleanup(func() {
		for _, h := range hosts {
			h.down()
		}
	})
	return hosts
}

// reset gives h a new, empty store, as if its .gomuxai were deleted.
func (h *frictionHost) reset(name string) {
	h.t.Helper()
	dir := h.t.TempDir()
	h.cfg = &config.Config{DatabasePath: filepath.Join(dir, "gomuxai.db"), Host: name}
	s, err := storage.NewSQLiteStorage(h.cfg.DatabasePath)
	if err != nil {
		h.t.Fatal(err)
	}
	h.t.Cleanup(func() { s.Close() })
	h.store = s
}

// up serves h's friction op on the bus; down stops it, so h is
// unreachable.
func (h *frictionHost) up() {
	h.t.Helper()
	h.down()
	svc := service.NewAgentService(h.cfg, h.store, nil, backend.NewRegistry())
	stop, err := natsbus.ServeRPC(h.nc, h.cfg.Host, service.NewRPCHandler(svc), []string{natsbus.OpFriction})
	if err != nil {
		h.t.Fatal(err)
	}
	h.stop = stop
}

func (h *frictionHost) down() {
	if h.stop != nil {
		h.stop()
		h.stop = nil
	}
}

// on runs agent args on host h, as a person there.
func (h *frictionHost) on(args ...string) (string, error) {
	h.t.Helper()
	oldCfg, oldStore := cfg, store
	defer func() { cfg, store = oldCfg, oldStore }()
	cfg, store = h.cfg, h.store
	return runAgent(h.t, args...)
}

func (h *frictionHost) must(args ...string) string {
	h.t.Helper()
	out, err := h.on(args...)
	if err != nil {
		h.t.Fatalf("on %s: agent %s: %v", h.cfg.Host, strings.Join(args, " "), err)
	}
	return out
}

func (h *frictionHost) entry(id int64) *storage.Friction {
	h.t.Helper()
	f, err := h.store.GetFriction(context.Background(), id)
	if err != nil || f == nil {
		h.t.Fatalf("on %s: no #%d: %v", h.cfg.Host, id, err)
	}
	return f
}

func (h *frictionHost) links(id int64) []*storage.FrictionLink {
	h.t.Helper()
	ls, err := h.store.ListFrictionLinks(context.Background(), id)
	if err != nil {
		h.t.Fatal(err)
	}
	return ls
}

func wantIn(t *testing.T, what, out string, wants ...string) {
	t.Helper()
	for _, w := range wants {
		if !strings.Contains(out, w) {
			t.Errorf("%s lacks %q:\n%s", what, w, out)
		}
	}
}

func wantNotIn(t *testing.T, what, out string, unwanted ...string) {
	t.Helper()
	for _, w := range unwanted {
		if strings.Contains(out, w) {
			t.Errorf("%s has %q:\n%s", what, w, out)
		}
	}
}

func TestParseFrictionRef(t *testing.T) {
	old := cfg
	defer func() { cfg = old }()
	cfg = &config.Config{Host: "local", DatabasePath: filepath.Join(t.TempDir(), "gomuxai.db")}
	tests := []struct {
		arg    string
		host   string
		id     int64
		remote bool
		bad    bool
	}{
		{arg: "34", host: "local", id: 34},
		{arg: "#34", host: "local", id: 34},
		{arg: "34@h1", host: "h1", id: 34, remote: true},
		{arg: "#34@h1", host: "h1", id: 34, remote: true},
		{arg: "34@LOCAL", host: "local", id: 34},
		{arg: "34@", bad: true},
		{arg: "34@h.1", bad: true},
		{arg: "x@h1", bad: true},
		{arg: "0@h1", bad: true},
	}
	for _, tt := range tests {
		host, id, remote, err := parseFrictionRef(tt.arg)
		if (err != nil) != tt.bad {
			t.Errorf("parseFrictionRef(%q) err = %v, want error %v", tt.arg, err, tt.bad)
			continue
		}
		if !tt.bad && (host != tt.host || id != tt.id || remote != tt.remote) {
			t.Errorf("parseFrictionRef(%q) = %q, %d, %v; want %q, %d, %v", tt.arg, host, id, remote, tt.host, tt.id, tt.remote)
		}
	}
}

func TestFrictionLinkAcrossHosts(t *testing.T) {
	hosts := frictionHosts(t, "h1")
	local, h1 := hosts["local"], hosts["h1"]
	local.must("friction", "add", "a claude agent at its trust prompt shows as unknown", "--kind", "bug")
	h1.must("friction", "add", "spawn hangs on a slow disk")
	h1.must("friction", "add", "trust prompt agent reads unknown, not waiting", "--kind", "bug")
	h1.must("friction", "+1", "2")

	if _, err := local.on("friction", "link", "1", "2"); err == nil || !strings.Contains(err.Error(), "is on this host") {
		t.Errorf("linking two local entries: err = %v", err)
	}
	if _, err := local.on("friction", "link", "1", "9@h1"); err == nil || !strings.Contains(err.Error(), "no friction #9 on h1 (latest is #2)") {
		t.Errorf("linking to a missing entry: err = %v", err)
	}
	if n := len(local.links(1)); n != 0 {
		t.Fatalf("a refused link left %d links here", n)
	}

	out := local.must("friction", "link", "1", "#2@h1", "--note", "same prompt")
	if out != "Linked #1 and #2@h1 (bug, open, +1 there: trust prompt agent reads unknown, not waiting): the same problem; resolving either resolves both.\n" {
		t.Errorf("link printed %q", out)
	}
	mine, theirs := local.links(1), h1.links(2)
	if len(mine) != 1 || mine[0].PeerUID != h1.entry(2).UID || mine[0].ConfirmedAt.IsZero() || mine[0].Note != "same prompt" {
		t.Errorf("this host's half = %+v", mine)
	}
	if len(theirs) != 1 || theirs[0].PeerHost != "local" || theirs[0].PeerID != 1 || theirs[0].PeerUID != local.entry(1).UID ||
		theirs[0].LinkedBy != "user@local" || theirs[0].PeerSummary != "a claude agent at its trust prompt shows as unknown" {
		t.Errorf("h1's half = %+v", theirs)
	}

	show := local.must("friction", "show", "1")
	wantIn(t, "show", show, "Linked (the same problem on other hosts; resolving #1 resolves these too):\n"+
		"  #2@h1 bug, open, +1 there: trust prompt agent reads unknown, not waiting\n"+
		"    linked by user, ")
	wantNotIn(t, "show", show, "!")
	wantIn(t, "show on h1", h1.must("friction", "show", "2"), "  #1@local bug, open, +0 there: a claude agent at its trust prompt")
	wantIn(t, "list", local.must("friction", "list"), "a claude agent at its trust prompt shows as unknown [= #2@h1]")

	var listed []struct {
		ID    int64 `json:"id"`
		Links []struct {
			Ref   string `json:"ref"`
			State string `json:"state"`
			Host  string `json:"host"`
		} `json:"links"`
	}
	if err := json.Unmarshal([]byte(local.must("friction", "list", "--json")), &listed); err != nil {
		t.Fatal(err)
	}
	if len(listed) != 1 || len(listed[0].Links) != 1 || listed[0].Links[0].Ref != "#2@h1" || listed[0].Links[0].State != linkOK || listed[0].Links[0].Host != "h1" {
		t.Errorf("list --json = %+v", listed)
	}

	out = local.must("friction", "resolve", "1", "--note", "status reads the trust prompt (abc123)")
	if out != "Resolved friction #1 (bug).\nResolved #2@h1 too.\n" {
		t.Errorf("resolve printed %q", out)
	}
	if f := h1.entry(2); f.Status != storage.FrictionResolved || f.ResolvedBy != "user@local" || f.Resolution != "status reads the trust prompt (abc123)" {
		t.Errorf("h1's #2 = %+v", f)
	}
	if f := h1.entry(1); f.Status != storage.FrictionOpen {
		t.Errorf("h1's unlinked #1 was closed: %+v", f)
	}
	wantIn(t, "links", local.must("friction", "links"), "#1 resolved = #2@h1: bug, resolved, +1 there")
	if out := local.must("friction", "links", "--dangling"); out != "No dangling links (1 in all: agent friction links).\n" {
		t.Errorf("links --dangling printed %q", out)
	}
	wantIn(t, "export", local.must("friction", "export"), "Linked to #2@h1 (resolved there, ")

	if out := local.must("friction", "unlink", "1", "2@h1"); out != "Unlinked #1 and #2@h1.\n" {
		t.Errorf("unlink printed %q", out)
	}
	if len(local.links(1)) != 0 || len(h1.links(2)) != 0 {
		t.Errorf("after unlink: %d links here, %d on h1", len(local.links(1)), len(h1.links(2)))
	}
	if out := local.must("friction", "links"); out != "No links to other hosts' friction entries on host local.\n" {
		t.Errorf("links with none printed %q", out)
	}
}

// A host that can't be reached: link and resolve still work here, and say
// what's left to do; show and links flag the link until it's done.
func TestFrictionLinkWhileUnreachable(t *testing.T) {
	hosts := frictionHosts(t, "h1")
	local, h1 := hosts["local"], hosts["h1"]
	local.must("friction", "add", "wait --timeout doesn't name the last status", "--kind", "error")
	h1.must("friction", "add", "wait timeout says nothing")

	h1.down()
	out := local.must("friction", "link", "1", "1@h1")
	wantIn(t, "link", out, "Linked #1 to #1@h1 on this host only: couldn't reach h1 (no bus answering for host \"h1\"",
		"once its bus answers: agent friction link 1 1@h1\n", "or on h1: agent friction link 1 1@local\n")
	if l := local.links(1); len(l) != 1 || !l[0].ConfirmedAt.IsZero() || l[0].PeerUID != "" {
		t.Errorf("unconfirmed link = %+v", l)
	}
	show := local.must("friction", "show", "1")
	wantIn(t, "show", show, "  #1@h1 not seen yet: it was linked while h1 was unreachable\n",
		"    ! unreachable: no bus answering for host \"h1\"", "      or on h1: agent friction link 1 1@local\n")
	wantIn(t, "list", local.must("friction", "list"), "[= #1@h1 !unreachable]")

	h1.up()
	wantIn(t, "show once h1 answers", local.must("friction", "show", "1"),
		"  #1@h1 other, open, +0 there: wait timeout says nothing\n", "    ! one-sided: h1 has no link back to #1\n",
		"      agent friction link 1 1@h1\n")
	// Linking from the other side confirms both halves.
	h1.must("friction", "link", "1", "1@local")
	if l := local.links(1); len(l) != 1 || l[0].ConfirmedAt.IsZero() || l[0].PeerUID != h1.entry(1).UID {
		t.Errorf("link confirmed from h1 = %+v", l)
	}
	wantNotIn(t, "show once linked both ways", local.must("friction", "show", "1"), "!")

	h1.down()
	out = local.must("friction", "resolve", "1", "--note", "wait names it")
	wantIn(t, "resolve", out, "Resolved friction #1 (error).\nCouldn't reach h1 to resolve #1@h1: no bus answering",
		"Left to do, once those hosts' buses answer: agent friction resolve 1 --linked\n",
		"  or on h1: agent friction resolve 1 --note 'wait names it'\n")
	wantIn(t, "links --dangling", local.must("friction", "links", "--dangling"), "#1 resolved = #1@h1: unreachable: no bus answering",
		"; last seen open, ")

	h1.up()
	wantIn(t, "links --dangling", local.must("friction", "links", "--dangling"),
		"#1 resolved = #1@h1: out of step: open there, resolved here\n    agent friction resolve 1 --linked\n")
	if _, err := local.on("friction", "resolve", "1", "--linked", "--note", "x"); err == nil || !strings.Contains(err.Error(), "--linked takes no --note") {
		t.Errorf("resolve --linked --note: err = %v", err)
	}
	if out := local.must("friction", "resolve", "1", "--linked"); out != "Resolved #1@h1 too.\n" {
		t.Errorf("resolve --linked printed %q", out)
	}
	if f := h1.entry(1); f.Status != storage.FrictionResolved || f.Resolution != "wait names it" {
		t.Errorf("h1's #1 = %+v", f)
	}
	// h1 heard how it was left here, without asking.
	if l := h1.links(1); len(l) != 1 || l[0].PeerStatus != storage.FrictionResolved {
		t.Errorf("h1's half = %+v", l)
	}
	if out := local.must("friction", "resolve", "1", "--linked"); out != "#1@h1 was already resolved (by user@local).\n" {
		t.Errorf("resolve --linked again printed %q", out)
	}

	local.must("friction", "add", "unrelated", "--new")
	if _, err := local.on("friction", "resolve", "2", "--linked"); err == nil || !strings.Contains(err.Error(), "still open") {
		t.Errorf("resolve --linked on an open entry: err = %v", err)
	}

	h1.down()
	local.must("friction", "link", "2", "1@h1")
	out = local.must("friction", "unlink", "2", "1@h1")
	wantIn(t, "unlink while unreachable", out, "Unlinked #2 from #1@h1 here; h1 still has its half", "or on h1: agent friction unlink 1 2@local\n")
	if _, err := local.on("friction", "unlink", "2", "1@h1"); err == nil || !strings.Contains(err.Error(), "#2 has no link to #1@h1 here") {
		t.Errorf("unlinking twice while unreachable: err = %v", err)
	}
}

// Links whose other end changed without this host: resolved there (two
// agents in parallel), a different entry under the id (a reset store), or
// closed there as a duplicate.
func TestFrictionLinksGoStale(t *testing.T) {
	hosts := frictionHosts(t, "h1")
	local, h1 := hosts["local"], hosts["h1"]
	for _, s := range []string{"capture cuts long lines", "send loses its enter", "status flickers"} {
		local.must("friction", "add", s, "--new")
		h1.must("friction", "add", s, "--new")
	}
	for _, id := range []string{"1", "2", "3"} {
		local.must("friction", "link", id, id+"@h1")
	}

	// Resolved on h1 only.
	h1.must("friction", "resolve", "1", "--only", "--note", "fixed there")
	if f := local.entry(1); f.Status != storage.FrictionOpen {
		t.Fatalf("resolve --only on h1 closed this host's #1: %+v", f)
	}
	wantIn(t, "show", local.must("friction", "show", "1"),
		"    ! out of step: resolved there (by user: fixed there), open here\n",
		"      if the same fix covers it here: agent friction resolve 1 --note 'fixed there'\n",
		"      if it isn't the same problem: agent friction unlink 1 1@h1\n")
	wantIn(t, "list", local.must("friction", "list"), "[= #1@h1 !out of step]")

	// h1 closes its #3 as a duplicate of a new #4: the link follows.
	h1.must("friction", "add", "status flickers between idle and processing", "--new")
	h1.must("friction", "resolve", "3", "--dup", "4")
	show := local.must("friction", "show", "3")
	wantIn(t, "show", show, "  #4@h1 other, open, +1 there: status flickers between idle", "(#3 there, which it links to, is closed as its duplicate)")
	wantNotIn(t, "show", show, "!")
	if l := local.links(3); len(l) != 1 || l[0].PeerID != 4 || l[0].PeerUID != h1.entry(4).UID {
		t.Errorf("link after h1's dup = %+v", l)
	}

	// h1's store is reset: #2 is a different entry, then gone.
	old := h1.store
	h1.reset("h1")
	h1.up()
	wantIn(t, "links", local.must("friction", "links", "--dangling"), "#2 open = #2@h1: missing: h1 has no #2\n    agent friction unlink 2 2@h1\n")
	h1.must("friction", "add", "one")
	h1.must("friction", "add", "two")
	wantIn(t, "show", local.must("friction", "show", "2"), "! missing: #2 on h1 is a different entry now (two)")
	out := local.must("friction", "resolve", "2", "--note", "fixed")
	wantIn(t, "resolve", out, "#2@h1 wasn't resolved: it's a different entry there now (two)")
	if f := h1.entry(2); f.Status != storage.FrictionOpen {
		t.Errorf("resolve closed the entry that reused #2: %+v", f)
	}
	if f, _ := old.GetFriction(context.Background(), 2); f.Status != storage.FrictionOpen {
		t.Errorf("the old store changed: %+v", f)
	}
}

// Resolving follows links through other hosts: local-h1-h2.
func TestFrictionResolveFollowsLinks(t *testing.T) {
	hosts := frictionHosts(t, "h1", "h2")
	local, h1, h2 := hosts["local"], hosts["h1"], hosts["h2"]
	local.must("friction", "add", "logs can't show one turn")
	h1.must("friction", "add", "logs has no --turn")
	h2.must("friction", "add", "no way to see a single turn in logs")
	h2.must("friction", "add", "unrelated", "--new")
	local.must("friction", "link", "1", "1@h1")
	h1.must("friction", "link", "1", "2@h2")
	h2.must("friction", "resolve", "1", "--dup", "2")

	out := local.must("friction", "resolve", "1", "--wontfix", "--note", "use transcript")
	if out != "Closed friction #1 (other) as wontfix.\nResolved #1@h1 too.\nResolved #2@h2 too.\n" {
		t.Errorf("resolve printed %q", out)
	}
	for _, f := range []*storage.Friction{h1.entry(1), h2.entry(2)} {
		if f.Status != storage.FrictionWontfix || f.Resolution != "use transcript" || f.ResolvedBy != "user@local" {
			t.Errorf("linked entry = %+v", f)
		}
	}
}

// A same-host duplicate hands its links to its original.
func TestFrictionDupMovesLinks(t *testing.T) {
	hosts := frictionHosts(t, "h1")
	local, h1 := hosts["local"], hosts["h1"]
	local.must("friction", "add", "keys refuses a read-only client")
	local.must("friction", "add", "keys fails when tmux is attached read-only", "--new")
	h1.must("friction", "add", "read-only attach breaks keys")
	local.must("friction", "link", "2", "1@h1")

	out := local.must("friction", "resolve", "2", "--dup", "1")
	if out != "Closed friction #2 as a duplicate of #1 (now +1).\nIts links to other hosts' entries moved to #1.\n" {
		t.Errorf("resolve --dup printed %q", out)
	}
	// h1 still names #2 here, which stands for #1 now.
	show := h1.must("friction", "show", "1")
	wantIn(t, "show on h1", show, "  #1@local other, open, +1 there: keys refuses", "(#2 there, which it links to, is closed as its duplicate)")
	wantNotIn(t, "show on h1", show, "!")
	if out := h1.must("friction", "resolve", "1", "--note", "pastes past it"); out != "Resolved friction #1 (other).\nResolved #1@local too.\n" {
		t.Errorf("resolve on h1 printed %q", out)
	}
	if f := local.entry(1); f.Status != storage.FrictionResolved {
		t.Errorf("local #1 = %+v", f)
	}
}

// A request over the bus can't claim to come from the serving host itself.
func TestServeFrictionRefusesItsOwnHost(t *testing.T) {
	hosts := frictionHosts(t)
	local := hosts["local"]
	local.must("friction", "add", "x")
	svc := service.NewAgentService(local.cfg, local.store, nil, backend.NewRegistry())
	_, err := svc.ServeFriction(context.Background(), natsbus.FrictionRequest{
		Action: natsbus.FrictionLink, ID: 1, From: &natsbus.FrictionEntry{Host: "LOCAL", ID: 1},
	})
	if err == nil || !strings.Contains(err.Error(), "which is this host") {
		t.Errorf("link from this host over the bus: err = %v", err)
	}
	if n := len(local.links(1)); n != 0 {
		t.Errorf("it recorded %d links", n)
	}
}

// Another host resolves only an entry linked to the one the request comes
// from: a mistyped id, or a link this host never recorded, closes nothing.
func TestFrictionResolveOverTheBusNeedsALinkBack(t *testing.T) {
	hosts := frictionHosts(t, "h1")
	local, h1 := hosts["local"], hosts["h1"]
	local.must("friction", "add", "capture drops the last line")
	h1.must("friction", "add", "capture misses its last line")
	h1.must("friction", "add", "spawn hangs on a slow disk", "--new")
	local.must("friction", "link", "1", "1@h1")

	svc := service.NewAgentService(h1.cfg, h1.store, nil, backend.NewRegistry())
	ctx := context.Background()
	from := &natsbus.FrictionEntry{Host: "local", ID: 1, UID: local.entry(1).UID}

	// No From: refused, whatever the id.
	_, err := svc.ServeFriction(ctx, natsbus.FrictionRequest{Action: natsbus.FrictionResolve, ID: 1, Status: "resolved", Note: "x"})
	if err == nil || !strings.Contains(err.Error(), "names no entry it comes from") {
		t.Errorf("resolve with no from: err = %v", err)
	}
	// A mistyped id, no uid: #2 isn't linked to local's #1.
	_, err = svc.ServeFriction(ctx, natsbus.FrictionRequest{Action: natsbus.FrictionResolve, ID: 2, From: from, Status: "resolved", Note: "x"})
	if err == nil || !strings.Contains(err.Error(), "#2 on h1 has no link to #1@local, so it wasn't resolved: link it from this side first (on h1: agent friction link 2 1@local)") {
		t.Errorf("resolve of an unlinked entry: err = %v", err)
	}
	for _, id := range []int64{1, 2} {
		if f := h1.entry(id); f.Status != storage.FrictionOpen {
			t.Errorf("a refused resolve closed h1's #%d: %+v", id, f)
		}
	}

	// A link recorded here while h1 was down is one-sided: h1 refuses, and
	// resolve passes on what to run instead of listing it as left to do.
	h1.down()
	local.must("friction", "add", "status flickers", "--new")
	local.must("friction", "link", "2", "1@h1")
	h1.up()
	out := local.must("friction", "resolve", "2", "--note", "debounced")
	wantIn(t, "resolve", out, "Resolved friction #2 (other).\n",
		"h1 didn't resolve #1@h1: #1 on h1 has no link to #2@local, so it wasn't resolved: link it from this side first (on h1: agent friction link 1 2@local).\n")
	wantNotIn(t, "resolve", out, "Left to do")
	if f := h1.entry(1); f.Status != storage.FrictionOpen {
		t.Errorf("resolving #2, whose link h1 never recorded, closed h1's #1: %+v", f)
	}

	// The real link still resolves.
	if out := local.must("friction", "resolve", "1", "--note", "fixed"); out != "Resolved friction #1 (other).\nResolved #1@h1 too.\n" {
		t.Errorf("resolve printed %q", out)
	}
}
