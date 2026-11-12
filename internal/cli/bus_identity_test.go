package cli

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// setupBusIdentity gives the test CLI host ("local") a bus identity, with
// no bus running and restarts recorded instead of run.
func setupBusIdentity(t *testing.T) (restarts *[]busSettings) {
	t.Helper()
	setupCLI(t)
	if _, err := natsbus.InitIdentity(cfg.BusDir(), cfg.Host); err != nil {
		t.Fatal(err)
	}
	busConnect = func() (*nats.Conn, error) { return nil, natsbus.ErrNoBus }
	var got []busSettings
	oldRestart := restartBusFn
	restartBusFn = func(s busSettings) error { got = append(got, s); return nil }
	t.Cleanup(func() {
		restartBusFn = oldRestart
		busNoRestart, busRenames, busURL = false, "", ""
	})
	return &got
}

// runningBus answers bus status queries as a bus running with info would.
func runningBus(t *testing.T, info busInfo) {
	t.Helper()
	nc := natstest.Start(t)
	sub, err := nc.Subscribe(natsbus.SubjectLocal(cfg.Host, "bus"), func(m *nats.Msg) {
		data, _ := json.Marshal(info)
		m.Respond(data)
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { sub.Unsubscribe() })
	nc.Flush()
	busConnect = func() (*nats.Conn, error) { return natstest.Connect(t, nc.ConnectedUrl()), nil }
}

// peerHost is another machine's bus identity, in its own dir.
func peerIdentity(t *testing.T, name string) (dir string, id *natsbus.Identity) {
	t.Helper()
	dir = t.TempDir()
	id, err := natsbus.InitIdentity(dir, name)
	if err != nil {
		t.Fatal(err)
	}
	return dir, id
}

func writeBundleFile(t *testing.T, b natsbus.PeerBundle) string {
	t.Helper()
	data, _ := json.Marshal(b)
	path := filepath.Join(t.TempDir(), b.Host+".pub.json")
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestBusRename(t *testing.T) {
	setupBusIdentity(t)
	id, _ := natsbus.LoadIdentity(cfg.BusDir(), cfg.Host)
	if _, err := writeBundle(id, "tls://local.lan:7422"); err != nil {
		t.Fatal(err)
	}
	_, hub := peerIdentity(t, "hub")
	if err := natsbus.TrustPeer(cfg.BusDir(), hub.PublicBundle("")); err != nil {
		t.Fatal(err)
	}

	if _, err := stdout(t, func() error { return runBusRename(busRenameCmd, []string{"HUB"}) }); err == nil || !strings.Contains(err.Error(), "trusted peer hub") {
		t.Errorf("renaming to a peer's name: %v", err)
	}
	out, err := stdout(t, func() error { return runBusRename(busRenameCmd, []string{"studio"}) })
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		`from "local" to "studio"`,
		"same key " + id.PublicBundle("").NKey,
		"keep AGENT_HOST=local, which now means studio",
		"on hub: agent bus trust studio.pub.json --renames local",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("output lacks %q:\n%s", want, out)
		}
	}
	if cfg.Host != "studio" {
		t.Errorf("cfg.Host = %q", cfg.Host)
	}
	data, err := os.ReadFile(filepath.Join(cfg.BusDir(), "studio.pub.json"))
	var b natsbus.PeerBundle
	if err != nil || json.Unmarshal(data, &b) != nil || b.Host != "studio" || b.URL != "tls://local.lan:7422" {
		t.Errorf("new bundle %s (%v): want studio's, keeping the old url", data, err)
	}
	if _, err := os.Stat(filepath.Join(cfg.BusDir(), "local.pub.json")); !os.IsNotExist(err) {
		t.Errorf("old bundle still there: %v", err)
	}
	// The old name is this host's still: replies to name@local stay here.
	if ref, remote, err := parseTarget("lead@LOCAL"); err != nil || remote || ref.Name != "lead" {
		t.Errorf("parseTarget(lead@LOCAL) = %+v, remote %v, %v", ref, remote, err)
	}
	if _, err := stdout(t, func() error { return runBusUntrust(busUntrustCmd, []string{"local"}) }); err == nil || !strings.Contains(err.Error(), "this host") {
		t.Errorf("untrusting a former name of this host: %v", err)
	}
}

// A rename stops a running bus and starts it again, as the new name, with
// the settings it ran with; a bus run in the foreground isn't its to stop.
func TestBusRename_RunningBusInTheForeground(t *testing.T) {
	setupBusIdentity(t)
	old := busSession
	busSession = "rpc-cli-gomux-bus-absent"
	t.Cleanup(func() { busSession = old })
	runningBus(t, busInfo{Host: "local", ClientURL: "tls://127.0.0.1:7420"})

	_, err := stdout(t, func() error { return runBusRename(busRenameCmd, []string{"studio"}) })
	if err == nil || !strings.Contains(err.Error(), "agent bus run") {
		t.Errorf("rename with a foreground bus: %v", err)
	}
	if natsbus.PreviousHosts(cfg.BusDir()) != nil || cfg.Host != "local" {
		t.Error("a refused rename renamed")
	}
}

func TestBusTrustRenames(t *testing.T) {
	restarts := setupBusIdentity(t)
	peerDir, peer := peerIdentity(t, "old-mac")
	if err := natsbus.TrustPeer(cfg.BusDir(), peer.PublicBundle("")); err != nil {
		t.Fatal(err)
	}
	if err := saveSSHDests(map[string]string{"old-mac": "me@mac.lan"}); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "helper", SessionName: "rpc-cli-helper", Backend: "claude", SpawnedBy: "lead@old-mac", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}

	// The peer renames itself; its new bundle has the same keys.
	renamed, err := natsbus.RenameIdentity(peerDir, "old-mac", "new-mac")
	if err != nil {
		t.Fatal(err)
	}
	file := writeBundleFile(t, renamed.PublicBundle(""))

	// A plain trust is refused, pointing at --renames.
	_, err = stdout(t, func() error { return runBusTrust(busTrustCmd, []string{file}) })
	if err == nil || !strings.Contains(err.Error(), "agent bus trust "+file+" --renames old-mac") {
		t.Errorf("plain trust of the renamed bundle: %v", err)
	}

	runningBus(t, busInfo{Host: "local", ClientURL: "tls://127.0.0.1:7420", Listen: "0.0.0.0:7422", ClientPort: 7420, RemoteOps: []string{"list", "status"}})
	busRenames = "Old-Mac"
	out, err := stdout(t, func() error { return runBusTrust(busTrustCmd, []string{file}) })
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		`Trusted "new-mac" as old-mac renamed`,
		"the spawned_by of 1 agent",
		"its ssh destination (me@mac.lan)",
		"Restarted the bus (--listen 0.0.0.0:7422 --client-port 7420 --remote-ops list,status)",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("output lacks %q:\n%s", want, out)
		}
	}
	if len(*restarts) != 1 || (*restarts)[0].Listen != "0.0.0.0:7422" {
		t.Errorf("restarts = %+v", *restarts)
	}
	if a, _ := store.GetAgent(ctx, "helper"); a == nil || a.SpawnedBy != "lead@new-mac" {
		t.Errorf("helper = %+v", a)
	}
	if sshDestFor("new-mac") != "me@mac.lan" {
		t.Errorf("ssh destination for new-mac: %q", sshDestFor("new-mac"))
	}
	if got := canonicalHost("OLD-MAC"); got != "new-mac" {
		t.Errorf("canonicalHost(OLD-MAC) = %q, want new-mac", got)
	}

	// --no-restart leaves it to agent bus restart; it only goes with --renames.
	busRenames, busNoRestart = "", true
	if _, err := stdout(t, func() error { return runBusTrust(busTrustCmd, []string{file}) }); err == nil || !strings.Contains(err.Error(), "goes with --renames") {
		t.Errorf("--no-restart without --renames: %v", err)
	}
}

func TestBusUntrust(t *testing.T) {
	restarts := setupBusIdentity(t)
	_, hub := peerIdentity(t, "hub")
	if err := natsbus.TrustPeer(cfg.BusDir(), hub.PublicBundle("tls://hub.lan:7422")); err != nil {
		t.Fatal(err)
	}
	if err := saveSSHDests(map[string]string{"hub": "me@hub.lan"}); err != nil {
		t.Fatal(err)
	}
	if _, err := stdout(t, func() error { return runBusUntrust(busUntrustCmd, []string{"nobody"}) }); err == nil || !strings.Contains(err.Error(), "trusted: hub") {
		t.Errorf("untrusting a host that isn't trusted: %v", err)
	}

	// With the bus running as a leaf: restarted, unless --no-restart.
	runningBus(t, busInfo{Host: "local", ClientURL: "tls://127.0.0.1:7420", ClientPort: 7420, RemoteOps: []string{"list"}})
	busNoRestart = true
	out, err := stdout(t, func() error { return runBusUntrust(busUntrustCmd, []string{"HUB"}) })
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		`Untrusted "hub"`,
		"Removed its ssh destination (me@hub.lan)",
		"hub was the hub this host links to",
		"Restart the bus to apply: agent bus restart",
		"on hub, agent bus untrust local",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("output lacks %q:\n%s", want, out)
		}
	}
	if len(*restarts) != 0 {
		t.Errorf("--no-restart restarted: %+v", *restarts)
	}
	if peers, _ := natsbus.LoadPeers(cfg.BusDir()); len(peers) != 0 {
		t.Errorf("peers left: %+v", peers)
	}
	if d, _ := loadSSHDests(); len(d) != 0 {
		t.Errorf("ssh destinations left: %v", d)
	}

	busNoRestart = false
	_, leaf := peerIdentity(t, "leaf")
	if err := natsbus.TrustPeer(cfg.BusDir(), leaf.PublicBundle("")); err != nil {
		t.Fatal(err)
	}
	if _, err := stdout(t, func() error { return runBusUntrust(busUntrustCmd, []string{"leaf"}) }); err != nil {
		t.Fatal(err)
	}
	if len(*restarts) != 1 || (*restarts)[0].ClientPort != 7420 {
		t.Errorf("restarts = %+v", *restarts)
	}
}

// Restart starts the bus with the settings it reports; one built before
// the reply carried them still says where it listens and its client URL.
func TestBusRestart_Settings(t *testing.T) {
	restarts := setupBusIdentity(t)
	if _, err := stdout(t, func() error { return runBusRestart(busRestartCmd, nil) }); err == nil || !strings.Contains(err.Error(), "nothing to restart") {
		t.Errorf("restart with no bus: %v", err)
	}
	runningBus(t, busInfo{Host: "local", ClientURL: "tls://127.0.0.1:7499", LeafAddr: "0.0.0.0:7422", RemoteOps: []string{"list", "send"}})
	out, err := stdout(t, func() error { return runBusRestart(busRestartCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	want := busSettings{Listen: "0.0.0.0:7422", ClientPort: 7499, RemoteOps: []string{"list", "send"}}
	if len(*restarts) != 1 || (*restarts)[0].String() != want.String() {
		t.Errorf("restarted with %+v, want %+v", *restarts, want)
	}
	if !strings.Contains(out, "Bus restarted in tmux session") || !strings.Contains(out, "--listen 0.0.0.0:7422 --client-port 7499 --remote-ops list,send") {
		t.Errorf("output:\n%s", out)
	}
}

// setFlag sets a flag on cmd as if given on the command line, and puts it
// back when the test ends: tests call the commands' RunE directly, and
// cobra's flags (and their Changed marks) outlive a test.
func setFlag(t *testing.T, cmd *cobra.Command, name, value string) {
	t.Helper()
	f := cmd.Flags().Lookup(name)
	old := f.Value.String()
	if sv, ok := f.Value.(pflag.SliceValue); ok {
		oldSlice := sv.GetSlice()
		t.Cleanup(func() { sv.Replace(oldSlice); f.Changed = false })
	} else {
		t.Cleanup(func() { f.Value.Set(old); f.Changed = false })
	}
	if err := cmd.Flags().Set(name, value); err != nil {
		t.Fatal(err)
	}
}

// Restart saves the settings it restarts with; a flag changes just that
// one; an op the bus doesn't know is refused before anything is stopped.
func TestBusRestart_FlagsAndSaved(t *testing.T) {
	restarts := setupBusIdentity(t)
	runningBus(t, busInfo{Host: "local", ClientURL: "tls://127.0.0.1:7420", Listen: "0.0.0.0:7422", ClientPort: 7420, RemoteOps: []string{"list", "send"}})

	if _, err := stdout(t, func() error { return runBusRestart(busRestartCmd, nil) }); err != nil {
		t.Fatal(err)
	}
	saved, err := loadBusSettings()
	want := busSettings{Listen: "0.0.0.0:7422", ClientPort: 7420, RemoteOps: []string{"list", "send"}}
	if err != nil || saved == nil || saved.String() != want.String() {
		t.Errorf("saved %+v, %v; want %+v", saved, err, want)
	}

	setFlag(t, busRestartCmd, "remote-ops", "list,send,resume")
	out, err := stdout(t, func() error { return runBusRestart(busRestartCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	want.RemoteOps = []string{"list", "send", "resume"}
	if got := (*restarts)[len(*restarts)-1]; got.String() != want.String() {
		t.Errorf("restarted with %+v, want %+v (only the ops changed)", got, want)
	}
	if saved, _ := loadBusSettings(); saved == nil || saved.String() != want.String() {
		t.Errorf("saved %+v, want %+v", saved, want)
	}
	if !strings.Contains(out, "settings saved for agent bus start") {
		t.Errorf("output doesn't say the settings were saved:\n%s", out)
	}

	n := len(*restarts)
	setFlag(t, busRestartCmd, "remote-ops", "list,sned")
	if _, err := stdout(t, func() error { return runBusRestart(busRestartCmd, nil) }); err == nil || !strings.Contains(err.Error(), `unknown op "sned" in --remote-ops: the ops are list, status`) {
		t.Errorf("restart with a typo: %v", err)
	}
	if len(*restarts) != n {
		t.Error("restarted the bus with an op it doesn't know")
	}
}

// Start uses the saved settings for the flags it isn't given, the
// defaults when none were saved, and refuses a broken settings file.
func TestBusStart_Settings(t *testing.T) {
	setupBusIdentity(t)

	s, from, err := startSettings(busStartCmd)
	if err != nil || s.String() != (busSettings{ClientPort: natsbus.DefaultClientPort, RemoteOps: natsbus.DefaultRemoteOps}).String() || from != "" {
		t.Errorf("no saved settings: %+v %q %v, want the defaults", s, from, err)
	}

	if err := saveBusSettings(busSettings{Listen: "0.0.0.0:7422", ClientPort: 7499, RemoteOps: []string{"list", "resume"}}); err != nil {
		t.Fatal(err)
	}
	s, from, err = startSettings(busStartCmd)
	if err != nil || s.String() != "--listen 0.0.0.0:7422 --client-port 7499 --remote-ops list,resume" || from != "; saved settings" {
		t.Errorf("saved settings: %+v %q %v", s, from, err)
	}

	setFlag(t, busStartCmd, "client-port", "7500")
	s, from, err = startSettings(busStartCmd)
	if err != nil || s.String() != "--listen 0.0.0.0:7422 --client-port 7500 --remote-ops list,resume" || !strings.Contains(from, "with the flags given") {
		t.Errorf("a flag over saved settings: %+v %q %v", s, from, err)
	}

	if err := os.WriteFile(busSettingsPath(), []byte("{not json"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, err := startSettings(busStartCmd); err == nil || !strings.Contains(err.Error(), "fix or delete it") {
		t.Errorf("broken settings file: %v", err)
	}
}
