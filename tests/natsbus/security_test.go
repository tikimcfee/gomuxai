package natsbus_test

// Adversarial tests for the bus's security model (see "Security model, in
// short" in docs/ARCHITECTURE.md). The hub is trusted; the attackers are an
// unauthenticated network client, a trusted but malicious non-hub peer
// ("mallory"), and other local users.
//
// Tests for known, unfixed weaknesses skip unless GOMUXAI_KNOWN_BUGS=1.

import (
	"bufio"
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
)

func knownBug(t *testing.T, what string) {
	t.Helper()
	if os.Getenv("GOMUXAI_KNOWN_BUGS") != "1" {
		t.Skip("known bug (set GOMUXAI_KNOWN_BUGS=1 to run): " + what)
	}
}

// hubAndPeers starts a hub and one leaf per name, all trusting each other
// through the hub, and returns local connections to each.
func hubAndPeers(t *testing.T, names ...string) (hubConn *nats.Conn, peers map[string]*nats.Conn) {
	t.Helper()
	hub := newBusHost(t, "hub")
	leaves := make([]*busHost, len(names))
	for i, n := range names {
		leaves[i] = newBusHost(t, n)
		trust(t, hub, leaves[i].id.PublicBundle(""))
	}
	hubD := hub.start(t, "127.0.0.1:-1", nil)
	peers = map[string]*nats.Conn{}
	for _, l := range leaves {
		trust(t, l, hub.id.PublicBundle("tls://"+hubD.LeafAddr()))
		l.start(t, "", nil)
	}
	waitLinked(t, hubD, len(names))
	for _, l := range leaves {
		peers[l.name] = l.connect(t)
	}
	return hub.connect(t), peers
}

// A peer may subscribe to replies, but only to its own: RPC replies (capture
// output, status details) between other hosts must not reach it.
func TestSecurity_PeerCantReadOtherHostsReplies(t *testing.T) {
	hubConn, peers := hubAndPeers(t, "alpha", "mallory")
	alpha, mallory := peers["alpha"], peers["mallory"]

	spy, err := mallory.SubscribeSync("_INBOX.>")
	if err != nil {
		t.Fatal(err)
	}
	mallory.Flush()
	time.Sleep(300 * time.Millisecond) // interest propagation

	serveSecret(t, hubConn, "hub", "hub-capture-secret")
	serveSecret(t, alpha, "alpha", "alpha-local-secret")
	if got, err := request(alpha, "hub", natsbus.OpCapture); err != nil || got != "hub-capture-secret" {
		t.Fatalf("alpha -> hub capture: %q, %v", got, err)
	}
	if got, err := request(alpha, "alpha", natsbus.OpCapture); err != nil || got != "alpha-local-secret" {
		t.Fatalf("alpha local capture: %q, %v", got, err)
	}
	if got, err := request(hubConn, "alpha", natsbus.OpCapture); err != nil || got != "alpha-local-secret" {
		t.Fatalf("hub -> alpha capture: %q, %v", got, err)
	}

	for {
		msg, err := spy.NextMsg(500 * time.Millisecond)
		if err != nil {
			break
		}
		t.Errorf("mallory read a reply meant for someone else on %s: %q", msg.Subject, msg.Data)
	}

	// Mallory's own requests still get answers.
	if got, err := request(mallory, "alpha", natsbus.OpStatus); err != nil || got != "alpha-local-secret" {
		t.Errorf("mallory -> alpha status: %q, %v", got, err)
	}
}

// A peer may publish replies only to requests it was sent. Reply subjects
// are random but not secret (they cross the hub in the clear and nats.go
// draws them from a time-seeded math/rand), so a peer that learns one must
// not be able to answer another host's request in place of its target.
func TestSecurity_PeerCantForgeRepliesToOthers(t *testing.T) {
	knownBug(t, "peers may publish to any _INBOX subject")
	hubConn, peers := hubAndPeers(t, "alpha", "mallory")
	alpha, mallory := peers["alpha"], peers["mallory"]

	replySubj := make(chan string, 1)
	sub, err := hubConn.Subscribe(natsbus.SubjectRPC("hub", natsbus.OpStatus), func(m *nats.Msg) {
		replySubj <- m.Reply
		time.Sleep(500 * time.Millisecond)
		m.Respond([]byte("real"))
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { sub.Unsubscribe() })
	hubConn.Flush()
	time.Sleep(300 * time.Millisecond)

	go func() {
		reply := <-replySubj
		mallory.Publish(reply, []byte("forged"))
		mallory.Flush()
	}()
	msg, err := alpha.Request(natsbus.SubjectRPC("hub", natsbus.OpStatus), nil, 2*time.Second)
	if err != nil {
		t.Fatalf("alpha -> hub status: %v", err)
	}
	if string(msg.Data) != "real" {
		t.Errorf("alpha got mallory's reply %q instead of the hub's", msg.Data)
	}

	// Mallory still answers requests sent to it.
	serveSecret(t, mallory, "mallory", "mallory-answer")
	if got, err := request(alpha, "mallory", natsbus.OpStatus); err != nil || got != "mallory-answer" {
		t.Errorf("alpha -> mallory status: %q, %v", got, err)
	}
}

// Wildcards in a published subject must not slip a request past the
// per-host op denials or into local-only subjects.
func TestSecurity_PeerCantReachDeniedOpsOrLocalSubjects(t *testing.T) {
	hubConn, peers := hubAndPeers(t, "alpha", "mallory")
	alpha, mallory := peers["alpha"], peers["mallory"]

	hit := make(chan string, 16)
	for nc, subjects := range map[*nats.Conn][]string{
		hubConn: {natsbus.SubjectRPC("hub", natsbus.OpKeys), natsbus.SubjectLocal("hub", "bus")},
		alpha:   {natsbus.SubjectRPC("alpha", natsbus.OpKeys), natsbus.SubjectLocal("alpha", "bus")},
	} {
		for _, s := range subjects {
			sub, err := nc.Subscribe(s, func(m *nats.Msg) { hit <- m.Subject + " <- " + string(m.Data) })
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { sub.Unsubscribe() })
		}
		nc.Flush()
	}
	time.Sleep(300 * time.Millisecond)

	for _, s := range []string{
		"gomux.hub.rpc.keys", "gomux.alpha.rpc.keys",
		"gomux.alpha.rpc.*", "gomux.*.rpc.keys", "gomux.alpha.rpc.>", "gomux.>", ">",
		"gomux.hub.local.bus", "gomux.alpha.local.bus", "gomux.alpha.local.*", "gomux.*.local.bus",
	} {
		mallory.Publish(s, []byte(s))
	}
	mallory.Flush()
	select {
	case got := <-hit:
		t.Errorf("mallory reached %s", got)
	case <-time.After(500 * time.Millisecond):
	}
}

// A non-hub peer can't act as a second hub by advertising a URL in its
// bundle: hub trust is total (it sees and can answer every request).
func TestSecurity_OnlyOneHub(t *testing.T) {
	hub, alpha, mallory := newBusHost(t, "hub"), newBusHost(t, "alpha"), newBusHost(t, "mallory")
	trust(t, hub, alpha.id.PublicBundle(""))
	trust(t, mallory, alpha.id.PublicBundle(""))
	hubD := hub.start(t, "127.0.0.1:-1", nil)
	malD := mallory.start(t, "127.0.0.1:-1", nil) // mallory listens too
	trust(t, alpha, hub.id.PublicBundle("tls://"+hubD.LeafAddr()))
	trust(t, alpha, mallory.id.PublicBundle("tls://"+malD.LeafAddr()))

	d, err := natsbus.StartDaemon(natsbus.DaemonConfig{Dir: alpha.dir, Host: "alpha", ClientPort: -1})
	if err == nil {
		t.Cleanup(d.Shutdown)
		// Show what mallory gets as a second hub: alpha's requests to others.
		malConn := mallory.connect(t)
		spy, _ := malConn.SubscribeSync(">")
		malConn.Flush()
		waitLinked(t, malD, 1)
		time.Sleep(300 * time.Millisecond)
		alphaConn := alpha.connect(t)
		alphaConn.Request(natsbus.SubjectRPC("hub", natsbus.OpSend), []byte(`{"agent":"x","message":"secret"}`), 500*time.Millisecond)
		if msg, err := spy.NextMsg(500 * time.Millisecond); err == nil {
			t.Errorf("a second hub received %s: %q", msg.Subject, msg.Data)
		}
		t.Fatal("alpha started with two peers advertising hub URLs; v1 supports one hub")
	}
	if !strings.Contains(err.Error(), "hub") {
		t.Errorf("unexpected error: %v", err)
	}
}

// "No JetStream, monitoring, or system account exposure": the system
// account's request subjects must not answer on the bus.
func TestSecurity_NoSystemAccount(t *testing.T) {
	hubConn, peers := hubAndPeers(t, "alpha")
	for name, nc := range map[string]*nats.Conn{"hub": hubConn, "alpha": peers["alpha"]} {
		for _, subj := range []string{"$SYS.REQ.SERVER.PING.CONNZ", "$SYS.REQ.ACCOUNT.PING.CONNZ", "$SYS.REQ.USER.INFO"} {
			if msg, err := nc.Request(subj, nil, 300*time.Millisecond); err == nil {
				t.Errorf("%s: %s answered (%d bytes)", name, subj, len(msg.Data))
			}
		}
	}
}

// An unauthenticated client on the network learns nothing before TLS: the
// hub must not send its plaintext INFO (server name, version, nonce) first.
func TestSecurity_LeafPortSpeaksTLSFirst(t *testing.T) {
	hub := newBusHost(t, "hub")
	d := hub.start(t, "127.0.0.1:-1", nil)

	conn, err := net.DialTimeout("tcp", d.LeafAddr(), time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	conn.SetReadDeadline(time.Now().Add(500 * time.Millisecond))
	line, _ := bufio.NewReader(conn).ReadString('\n')
	if strings.HasPrefix(line, "INFO") {
		t.Errorf("leaf port sent plaintext before TLS: %.120s", line)
	}
}

// Status events carry their host and agent in both the subject and the
// body. The hub only checks the subject, so a body naming another host is a
// forgery and must be rejected, or `agent events --all-hosts` shows it as
// that host's.
func TestSecurity_EventBodyMustMatchSubject(t *testing.T) {
	msg := &nats.Msg{
		Subject: natsbus.SubjectStatus("mallory", "worker"),
		Data:    []byte(`{"host":"hub","agent":"builder","status":"idle"}`),
	}
	if ev, err := natsbus.DecodeStatusEvent(msg); err == nil {
		t.Errorf("event on %s accepted as %s@%s", msg.Subject, ev.Agent, ev.Host)
	}
	msg.Data = []byte(`{"host":"mallory","agent":"worker","status":"idle"}`)
	if _, err := natsbus.DecodeStatusEvent(msg); err != nil {
		t.Errorf("genuine event rejected: %v", err)
	}
}

// --next must not be armed by an event the real status never showed: a
// forged "processing" followed by the agent's still-idle status (its
// previous turn) releases the wait before the agent ever started.
func TestSecurity_ForgedEventCantArmNext(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "idle"} // never leaves idle
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			publish(t, pub, "worker", "processing") // forged
		})
	}
	opts := waitOpts(src, 100*time.Millisecond, "idle")
	opts.Next = true
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if status, err := natsbus.Wait(ctx, nc, opts); err == nil {
		t.Errorf("--next wait released (%q) by a forged event; the agent never left idle", status)
	}
}

// Local clients authenticate to the bus, but nothing authenticates the bus
// to them. If the bus dies without removing bus.url, another local user who
// binds its port gets every hook's events and every CLI request (message
// text included), and can answer them.
func TestSecurity_LocalClientsVerifyTheBus(t *testing.T) {
	h := newBusHost(t, "solo")

	// An impostor that speaks just enough NATS to accept any nkey.
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	received := make(chan string, 100)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		c.Write([]byte(`INFO {"server_id":"x","version":"2.12.5","proto":1,"headers":true,"max_payload":1048576,"nonce":"abc","auth_required":true}` + "\r\n"))
		r := bufio.NewReader(c)
		for {
			line, err := r.ReadString('\n')
			if err != nil {
				return
			}
			if strings.HasPrefix(line, "PING") {
				c.Write([]byte("PONG\r\n"))
			}
			received <- line
		}
	}()
	if err := os.WriteFile(filepath.Join(h.dir, "bus.url"), []byte("nats://"+ln.Addr().String()), 0644); err != nil {
		t.Fatal(err)
	}

	nc, err := natsbus.ConnectLocal(h.dir, "solo")
	if err != nil {
		return // refused
	}
	defer nc.Close()
	t.Error("connected to a server that isn't this host's bus")
	natsbus.PublishStatus(nc, natsbus.StatusEvent{Host: "solo", Agent: "worker", Status: "waiting", Detail: "secret detail"})
	nc.Flush()
	for {
		select {
		case line := <-received:
			if strings.Contains(line, "secret detail") {
				t.Logf("impostor received: %.100s", line)
			}
		case <-time.After(200 * time.Millisecond):
			return
		}
	}
}

// Text from another host is printed in the user's terminal; escape sequences
// in it must not reach the terminal (raw capture excepted: it's asked for).
func TestRPCClient_SanitizesRemoteText(t *testing.T) {
	nc := natstest.Start(t)
	evil := "ok\x1b]0;pwned\x07\x1b[2J\rover\u009bwrite\x00"
	stop, err := natsbus.ServeRPC(nc, "h1", evilHandler{text: evil}, natsbus.AllOps)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	client := natsbus.NewRPCClient(nc, 0)
	ref := natsbus.AgentRef{Name: "w", Host: "h1"}
	ctx := context.Background()

	st, err := client.Status(ctx, ref)
	if err != nil {
		t.Fatal(err)
	}
	if strings.ContainsAny(st.Detail, "\x1b\x07\r\x00\u009b") {
		t.Errorf("status detail not sanitized: %q", st.Detail)
	}
	if out, _ := client.Capture(ctx, ref, 10, false); strings.ContainsAny(out, "\x1b\x07\r") {
		t.Errorf("capture not sanitized: %q", out)
	}
	if out, _ := client.Capture(ctx, ref, 10, true); out != evil {
		t.Errorf("raw capture should be untouched, got %q", out)
	}
	if err := client.Send(ctx, ref, "x"); err == nil || strings.ContainsAny(err.Error(), "\x1b\x07") {
		t.Errorf("remote error not sanitized: %v", err)
	}
}

type evilHandler struct{ text string }

func (h evilHandler) List(context.Context) (*natsbus.ListResponse, error) {
	return &natsbus.ListResponse{Agents: []natsbus.AgentInfo{{Name: h.text}}}, nil
}
func (h evilHandler) Status(context.Context, natsbus.StatusRequest) (*natsbus.StatusResponse, error) {
	return &natsbus.StatusResponse{Status: "idle", Detail: h.text}, nil
}
func (h evilHandler) Capture(context.Context, natsbus.CaptureRequest) (*natsbus.CaptureResponse, error) {
	return &natsbus.CaptureResponse{Output: h.text}, nil
}
func (h evilHandler) Send(context.Context, natsbus.SendRequest) error { return errors.New(h.text) }
func (h evilHandler) Respond(context.Context, natsbus.RespondRequest) error {
	return nil
}
func (h evilHandler) Keys(context.Context, natsbus.KeysRequest) error { return nil }

// A flood of slow requests gets "busy" replies past the in-flight limit
// instead of unbounded concurrent work.
func TestServeRPC_BoundsInFlightRequests(t *testing.T) {
	nc := natstest.Start(t)
	release := make(chan struct{})
	h := &blockingHandler{release: release}
	stop, err := natsbus.ServeRPC(nc, "h1", h, natsbus.AllOps)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	t.Cleanup(func() { close(release) })

	client := natsbus.NewRPCClient(nc, 2*time.Second)
	busy := make(chan bool, natsbus.MaxInFlight+10)
	for i := 0; i < natsbus.MaxInFlight+10; i++ {
		go func() {
			_, err := client.Status(context.Background(), natsbus.AgentRef{Name: "w", Host: "h1"})
			busy <- err != nil && strings.Contains(err.Error(), "busy")
		}()
	}
	deadline := time.After(3 * time.Second)
	got := 0
	for got < 10 {
		select {
		case b := <-busy:
			if b {
				got++
			}
		case <-deadline:
			t.Fatalf("only %d busy replies; want at least 10", got)
		}
	}
	if n := h.peak.Load(); n > natsbus.MaxInFlight {
		t.Errorf("%d requests ran at once; limit is %d", n, natsbus.MaxInFlight)
	}
}

type blockingHandler struct {
	evilHandler
	release <-chan struct{}
	running atomic.Int64
	peak    atomic.Int64
}

func (h *blockingHandler) Status(ctx context.Context, _ natsbus.StatusRequest) (*natsbus.StatusResponse, error) {
	n := h.running.Add(1)
	defer h.running.Add(-1)
	for {
		p := h.peak.Load()
		if n <= p || h.peak.CompareAndSwap(p, n) {
			break
		}
	}
	select {
	case <-h.release:
	case <-ctx.Done():
	}
	return &natsbus.StatusResponse{Status: "idle"}, nil
}
