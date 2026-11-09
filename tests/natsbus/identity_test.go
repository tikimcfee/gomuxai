package natsbus_test

import (
	"crypto/x509"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// A rename keeps the keys, so peers' pins still match and a peer can check
// the renamed host is the same machine; only the names change.
func TestRenameIdentity_KeepsKeys(t *testing.T) {
	h := newBusHost(t, "old")
	before := h.id.PublicBundle("")

	id, err := natsbus.RenameIdentity(h.dir, "old", "new")
	if err != nil {
		t.Fatal(err)
	}
	after := id.PublicBundle("")
	if id.Host != "new" || after.NKey != before.NKey || after.CertSHA != before.CertSHA {
		t.Errorf("renamed bundle %+v, was %+v: want the same keys under the new name", after, before)
	}
	leaf, err := x509.ParseCertificate(id.Cert.Certificate[0])
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(leaf.DNSNames, []string{"new"}) || leaf.Subject.CommonName != "gomux bus new" {
		t.Errorf("certificate names %v, CN %q; want the new name", leaf.DNSNames, leaf.Subject.CommonName)
	}
	if got := natsbus.PreviousHosts(h.dir); !slices.Equal(got, []string{"old"}) {
		t.Errorf("PreviousHosts = %v", got)
	}
	if _, err := natsbus.LoadIdentity(h.dir, "old"); err == nil || !strings.Contains(err.Error(), "renamed from") {
		t.Errorf("loading under the old name: %v", err)
	}
	for _, f := range []string{"host.nk", "local.nk", "tls.key"} {
		if info, _ := os.Stat(filepath.Join(h.dir, f)); info.Mode().Perm() != 0600 {
			t.Errorf("%s mode %o after rename", f, info.Mode().Perm())
		}
	}
	if left, _ := filepath.Glob(filepath.Join(h.dir, "*.tmp")); len(left) > 0 {
		t.Errorf("left behind %v", left)
	}

	// Renaming back: the name isn't listed as both current and former.
	if _, err := natsbus.RenameIdentity(h.dir, "new", "old"); err != nil {
		t.Fatal(err)
	}
	if got := natsbus.PreviousHosts(h.dir); !slices.Equal(got, []string{"new"}) {
		t.Errorf("after renaming back, PreviousHosts = %v", got)
	}
}

func TestRenameIdentity_Refusals(t *testing.T) {
	h := newBusHost(t, "box")
	peer := newBusHost(t, "peer")
	trust(t, h, peer.id.PublicBundle(""))
	if _, _, err := natsbus.RenamePeer(h.dir, "peer", func() natsbus.PeerBundle {
		b := peer.id.PublicBundle("")
		b.Host = "peer2"
		return b
	}()); err != nil {
		t.Fatal(err)
	}
	for name, want := range map[string]string{
		"box":   "already named",
		"a.b":   "invalid",
		"PEER2": "trusted peer peer2",
		"Peer":  "trusted peer peer2 is (or was) named Peer", // its former name
	} {
		if _, err := natsbus.RenameIdentity(h.dir, "box", name); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("rename to %q: %v, want %q", name, err, want)
		}
	}
	if _, err := natsbus.RenameIdentity(h.dir, "other", "x"); err == nil {
		t.Error("renamed an identity that isn't the named host's")
	}
	if _, err := natsbus.LoadIdentity(h.dir, "box"); err != nil {
		t.Errorf("a refused rename changed the identity: %v", err)
	}
}

func TestUntrustPeer(t *testing.T) {
	h := newBusHost(t, "box")
	trust(t, h, newBusHost(t, "Hub").id.PublicBundle("tls://hub:7422"))
	trust(t, h, newBusHost(t, "leaf").id.PublicBundle(""))

	b, err := natsbus.UntrustPeer(h.dir, "hub") // any case
	if err != nil || b.Host != "Hub" || b.URL != "tls://hub:7422" {
		t.Fatalf("UntrustPeer = %+v, %v", b, err)
	}
	peers, _ := natsbus.LoadPeers(h.dir)
	if len(peers) != 1 || peers[0].Host != "leaf" {
		t.Errorf("peers after untrust: %+v", peers)
	}
	if _, err := natsbus.UntrustPeer(h.dir, "Hub"); err == nil || !strings.Contains(err.Error(), "trusted: leaf") {
		t.Errorf("untrusting it again: %v", err)
	}
}

// Two hosts can't share keys: a bundle with a trusted peer's keys under
// another name is a rename, and needs RenamePeer, which checks the keys.
func TestTrustPeer_SameKeysAndRenames(t *testing.T) {
	h := newBusHost(t, "box")
	peer := newBusHost(t, "peer")
	trust(t, h, peer.id.PublicBundle(""))

	renamed := peer.id.PublicBundle("tls://peer2:7422")
	renamed.Host = "peer2"
	var same *natsbus.SameKeysError
	if err := natsbus.TrustPeer(h.dir, renamed); !errors.As(err, &same) || same.Peer != "peer" || !strings.Contains(err.Error(), "--renames peer") {
		t.Errorf("trusting a renamed peer as new: %v", err)
	}

	stranger := newBusHost(t, "peer2").id.PublicBundle("")
	if _, _, err := natsbus.RenamePeer(h.dir, "peer", stranger); err == nil || !strings.Contains(err.Error(), "isn't peer renamed") {
		t.Errorf("renaming to a bundle with other keys: %v", err)
	}
	if _, _, err := natsbus.RenamePeer(h.dir, "nobody", renamed); err == nil || !strings.Contains(err.Error(), "no trusted peer") {
		t.Errorf("renaming a peer that isn't trusted: %v", err)
	}

	got, replaced, err := natsbus.RenamePeer(h.dir, "PEER", renamed)
	if err != nil || replaced.Host != "peer" {
		t.Fatalf("RenamePeer replaced %+v, %v", replaced, err)
	}
	if !slices.Equal(got.AKA, []string{"peer"}) || !got.Names("Peer") || !got.Names("peer2") || got.Names("box") {
		t.Errorf("renamed bundle %+v", got)
	}
	peers, _ := natsbus.LoadPeers(h.dir)
	if len(peers) != 1 || !reflect.DeepEqual(peers[0], got) {
		t.Errorf("peers after rename: %+v", peers)
	}

	// A plain re-trust (say, to change its url) keeps its former names; a
	// second rename adds one.
	renamed.URL = ""
	trust(t, h, renamed)
	peers, _ = natsbus.LoadPeers(h.dir)
	if len(peers) != 1 || !slices.Equal(peers[0].AKA, []string{"peer"}) || peers[0].URL != "" {
		t.Errorf("re-trusted: %+v", peers)
	}
	third := renamed
	third.Host = "peer3"
	if got, _, err := natsbus.RenamePeer(h.dir, "peer2", third); err != nil || !slices.Equal(got.AKA, []string{"peer", "peer2"}) {
		t.Errorf("second rename: %+v, %v", got, err)
	}

	// Only the case changes: one bundle, under the new spelling, and the
	// old spelling isn't an alias of itself.
	fourth := third
	fourth.Host = "Peer3"
	got, replaced, err = natsbus.RenamePeer(h.dir, "peer3", fourth)
	if err != nil || replaced.Host != "peer3" || got.Host != "Peer3" || !slices.Equal(got.AKA, []string{"peer", "peer2"}) {
		t.Errorf("case-only rename: %+v replacing %+v, %v", got, replaced, err)
	}
	if peers, _ := natsbus.LoadPeers(h.dir); len(peers) != 1 || peers[0].Host != "Peer3" {
		t.Errorf("peers after a case-only rename: %+v", peers)
	}
}

// A renamed leaf links again at once (same keys), but the hub only routes
// requests and events for its new name once it has re-trusted it.
func TestBus_RenamedLeafNeedsTheHubToRetrust(t *testing.T) {
	hub, leaf := newBusHost(t, "hub"), newBusHost(t, "leaf")
	trust(t, hub, leaf.id.PublicBundle(""))
	hubD := hub.start(t, "127.0.0.1:-1", nil)
	hubAddr := hubD.LeafAddr()
	trust(t, leaf, hub.id.PublicBundle("tls://"+hubAddr))
	leafD := leaf.start(t, "", nil)
	waitLinked(t, hubD, 1)

	leafD.Shutdown()
	id, err := natsbus.RenameIdentity(leaf.dir, "leaf", "leaf2")
	if err != nil {
		t.Fatal(err)
	}
	leaf.name, leaf.id = "leaf2", id
	leaf.start(t, "", nil)
	waitLinked(t, hubD, 1)
	leafConn := leaf.connect(t)
	serve(t, leafConn, "leaf2")
	hubConn := hub.connect(t)
	waitInterest(t, leafConn, hubConn, "hub") // leaf's subscriptions have reached the hub, or been refused
	if got, err := requestOnce(hubConn, "leaf2", natsbus.OpStatus); err == nil {
		t.Errorf("hub reached leaf2 before re-trusting it: %q", got)
	}

	// The hub re-trusts it under its new name and restarts.
	if _, _, err := natsbus.RenamePeer(hub.dir, "leaf", leaf.id.PublicBundle("")); err != nil {
		t.Fatal(err)
	}
	hubD.Shutdown()
	hubD = hub.start(t, hubAddr, nil)
	waitLinked(t, hubD, 1)
	hubConn = hub.connect(t)
	if got, err := request(hubConn, "leaf2", natsbus.OpStatus); err != nil || got != "ran gomux.leaf2.rpc.status" {
		t.Errorf("hub -> leaf2 after re-trust: %q, %v", got, err)
	}
	waitInterest(t, hubConn, leafConn, "leaf2") // its events cross under the new name
}

// Untrusting cuts the link from either side once the bus restarts.
func TestBus_UntrustCutsTheLink(t *testing.T) {
	for _, side := range []string{"hub", "leaf"} {
		t.Run("on the "+side, func(t *testing.T) {
			hub, leaf := newBusHost(t, "hub"), newBusHost(t, "leaf")
			trust(t, hub, leaf.id.PublicBundle(""))
			hubD := hub.start(t, "127.0.0.1:-1", nil)
			hubAddr := hubD.LeafAddr()
			trust(t, leaf, hub.id.PublicBundle("tls://"+hubAddr))
			leafD := leaf.start(t, "", nil)
			waitLinked(t, hubD, 1)

			if side == "hub" {
				if _, err := natsbus.UntrustPeer(hub.dir, "leaf"); err != nil {
					t.Fatal(err)
				}
				hubD.Shutdown()
				os.Remove(filepath.Join(hub.dir, "bus.log"))
				hubD = hub.start(t, hubAddr, nil)
				waitLog(t, hub, "Leafnode connection closed") // the leaf tried, and was turned away
			} else {
				if _, err := natsbus.UntrustPeer(leaf.dir, "hub"); err != nil {
					t.Fatal(err)
				}
				leafD.Shutdown()
				leaf.start(t, "", nil)
				waitLinked(t, hubD, 0)
			}
			if n := hubD.LinkedPeers(); n != 0 {
				t.Errorf("%d link(s) after untrusting on the %s", n, side)
			}
		})
	}
}
