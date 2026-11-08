package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Links between friction entries on different hosts: the same problem,
// filed on each. Each host keeps its own half (storage.FrictionLink); the
// bus's friction op reads, links and resolves the other host's entry. See
// "The friction box" in docs/ARCHITECTURE.md.

// frictionCheckTimeout bounds each host's answer when show, list and links
// check linked entries; one that doesn't answer in time is unreachable.
const frictionCheckTimeout = 3 * time.Second

var (
	frictionOnly     bool
	frictionLinked   bool
	frictionDangling bool
)

var frictionLinkCmd = &cobra.Command{
	Use:   "link <id> <id@host>",
	Short: "Link a friction entry to the same problem filed on another host",
	Long: `Link records that an entry here and one on another host are the same
problem. Both stay open and count their own votes; show and list name the
other, and resolving either resolves both.

The other host records its half over the bus (its bus must serve the
friction op, which it does by default). If it can't be reached, the link
is recorded here only, and link says what to run there.

Same-host duplicates are closed with agent friction resolve ID --dup OTHER.`,
	Example: `  agent friction link 4 34@monolith1
  agent friction link 4 34@monolith1 --note "same trust prompt, seen with agy"`,
	Args: cobra.ExactArgs(2),
	RunE: runFrictionLink,
}

var frictionUnlinkCmd = &cobra.Command{
	Use:   "unlink <id> <id@host>",
	Short: "Remove a link between friction entries on two hosts",
	Long: `Unlink removes a link that link made, on both hosts: the entries are no
longer the same problem, and resolving one leaves the other open. If the
other host can't be reached, unlink removes this host's half and says
what to run there.`,
	Example: `  agent friction unlink 4 34@monolith1`,
	Args:    cobra.ExactArgs(2),
	RunE:    runFrictionUnlink,
}

var frictionLinksCmd = &cobra.Command{
	Use:   "links",
	Short: "Check every link to other hosts' friction entries",
	Long: `Links lists this host's links to entries on other hosts, each checked
with its host over the bus, and says which have gone stale (--dangling
lists only those), with what to run:

  out of step   one entry is closed and the other still open
  one-sided     the other host has no link back
  missing       the other host has no such entry, or a different one
                under that id now
  unreachable   the other host didn't answer; shows what was last seen`,
	Example: `  agent friction links
  agent friction links --dangling`,
	Args: cobra.NoArgs,
	RunE: runFrictionLinks,
}

func init() {
	frictionLinkCmd.Flags().StringVar(&frictionNote, "note", "", "Why they're the same problem")
	frictionLinksCmd.Flags().BoolVar(&frictionDangling, "dangling", false, "Only links that are out of step, one-sided, missing, or unreachable")
	frictionLinksCmd.Flags().BoolVar(&frictionJSON, "json", false, "Print as JSON")
	frictionResolveCmd.Flags().BoolVar(&frictionOnly, "only", false, "Resolve only this entry, not its linked entries on other hosts")
	frictionResolveCmd.Flags().BoolVar(&frictionLinked, "linked", false, "Pass a closed entry's resolution on to its linked entries")
	frictionCmd.AddCommand(frictionLinkCmd, frictionUnlinkCmd, frictionLinksCmd)
}

// frictionRef formats an entry on host, e.g. "#34@monolith1".
func frictionRef(host string, id int64) string {
	return fmt.Sprintf("#%d@%s", id, host)
}

// frictionKey identifies an entry on host, ignoring the host's case.
func frictionKey(host string, id int64) string {
	return strings.ToLower(host) + "#" + strconv.FormatInt(id, 10)
}

// parseFrictionRef reads an entry argument that may name another host:
// "34", "#34", "34@monolith1" or "#34@monolith1". remote is false for a
// bare id and for this host's own name (in any case).
func parseFrictionRef(arg string) (host string, id int64, remote bool, err error) {
	idPart, host, hasHost := strings.Cut(arg, "@")
	id, err = strconv.ParseInt(strings.TrimPrefix(idPart, "#"), 10, 64)
	if err != nil || id < 1 {
		return "", 0, false, fmt.Errorf("%q isn't a friction entry: want an id like 34, or 34@host for another host's", arg)
	}
	if !hasHost {
		return cfg.Host, id, false, nil
	}
	if err := natsbus.ValidateHost(host); err != nil {
		return "", 0, false, err
	}
	host = canonicalHost(host)
	return host, id, host != cfg.Host, nil
}

// frictionWho is who is running the command, for another host's records:
// "name@host", or "user@host" for a person.
func frictionWho(ctx context.Context) string {
	who, _ := frictionCaller(ctx)
	return who + "@" + cfg.Host
}

// frictionBus sends friction requests to other hosts over one connection
// to this host's bus. With no bus, every request fails with why.
type frictionBus struct {
	client *natsbus.RPCClient
	err    error
	close  func()
}

func openFrictionBus(timeout time.Duration) *frictionBus {
	nc, err := busConnect()
	if err != nil {
		if !errors.Is(err, natsbus.ErrNoBus) {
			err = fmt.Errorf("connecting to the agent bus: %w", err)
		}
		return &frictionBus{err: err, close: func() {}}
	}
	return &frictionBus{client: natsbus.NewRPCClient(nc, timeout), close: nc.Close}
}

func (b *frictionBus) call(ctx context.Context, host string, req natsbus.FrictionRequest) (*natsbus.FrictionResponse, error) {
	if b.err != nil {
		return nil, b.err
	}
	return b.client.Friction(ctx, host, req)
}

// isRefusal reports whether err is the other host's answer (it refused),
// rather than no answer at all.
func isRefusal(err error) bool {
	var re *natsbus.RemoteError
	return errors.As(err, &re)
}

// Link states, as checkFrictionLinks finds them. Every state but linkOK
// is a dangling link.
const (
	linkOK          = "linked"
	linkOutOfStep   = "out of step"
	linkOneSided    = "one-sided"
	linkMissing     = "missing"
	linkUnreachable = "unreachable"
)

// linkView is a link of a local entry, checked against its other end.
type linkView struct {
	*storage.FrictionLink
	Ref     string `json:"ref"`
	State   string `json:"state"`
	Problem string `json:"problem,omitempty"` // what's wrong, for a dangling link
	// Fix lists what to run about a dangling link, one way per line.
	Fix []string `json:"fix,omitempty"`
	// Redirected is the id the link named there, when that entry was
	// closed as a duplicate of Ref's.
	Redirected int64 `json:"redirected,omitempty"`
}

func (v *linkView) dangling() bool { return v.State != linkOK }

// checkFrictionLinks checks links (of the entries in local, by id) with
// their hosts, one request per host, and updates what each link remembers
// of its other end. Links whose entry isn't in local are skipped.
func checkFrictionLinks(ctx context.Context, local map[int64]*storage.Friction, links []*storage.FrictionLink) []*linkView {
	byHost := map[string][]int64{}
	var hosts []string
	for _, l := range links {
		if local[l.FrictionID] == nil {
			continue
		}
		h := strings.ToLower(l.PeerHost)
		if _, ok := byHost[h]; !ok {
			hosts = append(hosts, l.PeerHost)
		}
		if !slices.Contains(byHost[h], l.PeerID) {
			byHost[h] = append(byHost[h], l.PeerID)
		}
	}
	if len(hosts) == 0 {
		return nil
	}

	type answer struct {
		resp *natsbus.FrictionResponse
		err  error
	}
	answers := map[string]answer{}
	bus := openFrictionBus(frictionCheckTimeout)
	defer bus.close()
	var mu sync.Mutex
	var wg sync.WaitGroup
	for _, host := range hosts {
		wg.Add(1)
		go func() {
			defer wg.Done()
			resp, err := bus.call(ctx, host, natsbus.FrictionRequest{Action: natsbus.FrictionGet, IDs: byHost[strings.ToLower(host)]})
			mu.Lock()
			answers[strings.ToLower(host)] = answer{resp, err}
			mu.Unlock()
		}()
	}
	wg.Wait()

	now := time.Now()
	var views []*linkView
	for _, l := range links {
		f := local[l.FrictionID]
		if f == nil {
			continue
		}
		a := answers[strings.ToLower(l.PeerHost)]
		v := judgeLink(f, l, a.resp, a.err, now)
		if a.err == nil && v.State != linkMissing {
			_ = store.UpdateFrictionLink(ctx, l)
		}
		views = append(views, v)
	}
	return views
}

// judgeLink works out the state of f's link l from its host's answer to a
// get (or the error it got instead), and records what was seen in l.
func judgeLink(f *storage.Friction, l *storage.FrictionLink, resp *natsbus.FrictionResponse, err error, now time.Time) *linkView {
	v := &linkView{FrictionLink: l, Ref: frictionRef(l.PeerHost, l.PeerID)}
	if err != nil {
		v.State, v.Problem = linkUnreachable, fmt.Sprintf("unreachable: %v", err)
		if !l.PeerCheckedAt.IsZero() {
			v.Problem += fmt.Sprintf("; last seen %s, %s", l.PeerStatus, stamp(l.PeerCheckedAt))
		}
		if l.ConfirmedAt.IsZero() {
			v.Fix = []string{fmt.Sprintf("once %s's bus answers: agent friction link %d %d@%s", l.PeerHost, f.ID, l.PeerID, l.PeerHost),
				fmt.Sprintf("or on %s: agent friction link %d %d@%s", l.PeerHost, l.PeerID, f.ID, cfg.Host)}
		}
		return v
	}
	unlink := fmt.Sprintf("agent friction unlink %d %d@%s", f.ID, l.PeerID, l.PeerHost)
	var e *natsbus.FrictionEntry
	for i := range resp.Entries {
		c := &resp.Entries[i]
		if c.ID == l.PeerID || (c.Requested != nil && c.Requested.ID == l.PeerID) {
			e = c
			break
		}
	}
	if e == nil {
		v.State, v.Problem, v.Fix = linkMissing, fmt.Sprintf("missing: %s has no #%d", l.PeerHost, l.PeerID), []string{unlink}
		return v
	}
	askedUID := e.UID
	if e.Requested != nil {
		askedUID = e.Requested.UID
	}
	if l.PeerUID != "" && askedUID != l.PeerUID {
		v.State = linkMissing
		v.Problem = fmt.Sprintf("missing: #%d on %s is a different entry now (%s)", l.PeerID, l.PeerHost, clip(e.Summary, 60))
		v.Fix = []string{unlink}
		return v
	}

	if e.ID != l.PeerID {
		v.Redirected = l.PeerID
		unlink = fmt.Sprintf("agent friction unlink %d %d@%s", f.ID, e.ID, l.PeerHost)
	}
	backLinked := slices.ContainsFunc(e.Links, func(r natsbus.FrictionRef) bool {
		return strings.EqualFold(r.Host, cfg.Host) && r.ID == f.ID
	})
	service.SeeFrictionPeer(l, *e, now)
	switch {
	case !backLinked:
		l.ConfirmedAt = time.Time{}
	case l.ConfirmedAt.IsZero():
		l.ConfirmedAt = now
	}
	v.Ref = frictionRef(l.PeerHost, l.PeerID)

	hereOpen, thereOpen := service.FrictionIsOpen(f.Status), service.FrictionIsOpen(e.Status)
	switch {
	case hereOpen && !thereOpen:
		v.State = linkOutOfStep
		v.Problem = fmt.Sprintf("out of step: %s there%s, %s here", e.Status, byNote(e.ResolvedBy, e.Resolution), f.Status)
		v.Fix = []string{
			fmt.Sprintf("if the same fix covers it here: agent friction resolve %d %s", f.ID, resolveFlags(e.Status, e.Resolution)),
			"if it isn't the same problem: " + unlink,
		}
	case !hereOpen && thereOpen:
		v.State = linkOutOfStep
		v.Problem = fmt.Sprintf("out of step: %s there, %s here", e.Status, f.Status)
		v.Fix = []string{fmt.Sprintf("agent friction resolve %d --linked", f.ID)}
		if f.Status == storage.FrictionDuplicate {
			v.Fix = []string{fmt.Sprintf("agent friction resolve %d --linked, once #%d is resolved", f.DuplicateOf, f.DuplicateOf)}
		}
	case !backLinked:
		v.State = linkOneSided
		v.Problem = fmt.Sprintf("one-sided: %s has no link back to #%d", l.PeerHost, f.ID)
		v.Fix = []string{fmt.Sprintf("agent friction link %d %d@%s", f.ID, e.ID, l.PeerHost), "or, if they aren't the same problem: " + unlink}
	default:
		v.State = linkOK
	}
	return v
}

// byNote says who closed an entry and how, e.g. " (by fixer@h1: fixed in
// abc123)".
func byNote(by, note string) string {
	if by == "" && note == "" {
		return ""
	}
	return fmt.Sprintf(" (by %s%s)", by, notePart(clip(note, 80)))
}

// resolveFlags are resolve's flags that close an entry as status with
// note, shell-quoted.
func resolveFlags(status, note string) string {
	if note == "" {
		note = "..."
	}
	if status == storage.FrictionWontfix {
		return "--wontfix --note " + util.ShellQuote(note)
	}
	return "--note " + util.ShellQuote(note)
}

// peerSummary describes a link's other end as last seen, e.g. "bug, open,
// +2 there: summary", or that it hasn't been seen.
func peerSummary(l *storage.FrictionLink) string {
	if l.PeerCheckedAt.IsZero() {
		return "not seen yet: it was linked while " + l.PeerHost + " was unreachable"
	}
	return fmt.Sprintf("%s, %s, +%d there: %s", l.PeerKind, l.PeerStatus, l.PeerVotes, l.PeerSummary)
}

// linksOf returns the links of the entries in entries, by entry id.
func linksOf(ctx context.Context, entries []*storage.Friction) (map[int64][]*linkView, error) {
	all, err := store.ListFrictionLinks(ctx, 0)
	if err != nil || len(all) == 0 {
		return nil, err
	}
	local := map[int64]*storage.Friction{}
	for _, f := range entries {
		local[f.ID] = f
	}
	byEntry := map[int64][]*linkView{}
	for _, v := range checkFrictionLinks(ctx, local, all) {
		byEntry[v.FrictionID] = append(byEntry[v.FrictionID], v)
	}
	return byEntry, nil
}

// formatLinkMarker is list's note on an entry's links, e.g.
// " [= #34@monolith1, #12@hostc !unreachable]".
func formatLinkMarker(links []*linkView) string {
	if len(links) == 0 {
		return ""
	}
	refs := make([]string, 0, len(links))
	for _, v := range links {
		r := v.Ref
		if v.dangling() {
			r += " !" + v.State
		}
		refs = append(refs, r)
	}
	return " [= " + strings.Join(refs, ", ") + "]"
}

// formatFrictionLinks is show's section on an entry's links.
func formatFrictionLinks(f *storage.Friction, links []*linkView) string {
	if len(links) == 0 {
		return ""
	}
	var b strings.Builder
	if service.FrictionIsOpen(f.Status) {
		fmt.Fprintf(&b, "Linked (the same problem on other hosts; resolving #%d resolves these too):\n", f.ID)
	} else {
		fmt.Fprintf(&b, "Linked (the same problem on other hosts):\n")
	}
	for _, v := range links {
		fmt.Fprintf(&b, "  %s %s\n", v.Ref, peerSummary(v.FrictionLink))
		if v.Redirected != 0 {
			fmt.Fprintf(&b, "    (#%d there, which it links to, is closed as its duplicate)\n", v.Redirected)
		}
		fmt.Fprintf(&b, "    linked by %s, %s%s\n", v.LinkedBy, stamp(v.LinkedAt), notePart(v.Note))
		if v.dangling() {
			fmt.Fprintf(&b, "    ! %s\n", v.Problem)
			for _, fix := range v.Fix {
				fmt.Fprintf(&b, "      %s\n", fix)
			}
		}
	}
	return b.String()
}

func runFrictionLink(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	out := cmd.OutOrStdout()
	if f.Status == storage.FrictionDuplicate {
		dup := f.ID
		if f, err = getFriction(ctx, strconv.FormatInt(f.DuplicateOf, 10)); err != nil {
			return err
		}
		fmt.Fprintf(out, "#%d is a duplicate of #%d: linking #%d.\n", dup, f.ID, f.ID)
	}
	host, id, remote, err := parseFrictionRef(args[1])
	if err != nil {
		return err
	}
	if !remote {
		return fmt.Errorf("#%d is on this host: link joins entries on two hosts; close a same-host duplicate with agent friction resolve %d --dup %d (or the other way round)", id, id, f.ID)
	}

	from, err := service.FrictionEntryOf(ctx, store, cfg.Host, f)
	if err != nil {
		return err
	}
	note := strings.TrimSpace(frictionNote)
	who, _ := frictionCaller(ctx)
	now := time.Now()
	l := &storage.FrictionLink{FrictionID: f.ID, PeerHost: host, PeerID: id, LinkedBy: who, LinkedAt: now, Note: note}

	bus := openFrictionBus(0)
	defer bus.close()
	resp, err := bus.call(ctx, host, natsbus.FrictionRequest{
		Action: natsbus.FrictionLink, ID: id, From: &from, By: frictionWho(ctx), Note: note,
	})
	switch {
	case err != nil && isRefusal(err):
		return fmt.Errorf("%s didn't link it: %w", host, err)
	case err != nil:
		if err := store.AddFrictionLink(ctx, l); err != nil {
			return err
		}
		fmt.Fprintf(out, "Linked #%d to %s on this host only: couldn't reach %s (%v).\n", f.ID, frictionRef(host, id), host, err)
		fmt.Fprintf(out, "To link it there too, once its bus answers: agent friction link %d %d@%s\n", f.ID, id, host)
		fmt.Fprintf(out, "or on %s: agent friction link %d %d@%s\n", host, id, f.ID, cfg.Host)
		return nil
	case len(resp.Entries) == 0:
		return fmt.Errorf("%s has no #%d", host, id)
	}
	e := resp.Entries[0]
	service.SeeFrictionPeer(l, e, now)
	l.ConfirmedAt = now
	// Already linked to what the other host's duplicate stands for: keep one.
	if e.Requested != nil {
		fmt.Fprintf(out, "%s is a duplicate of #%d there: linking #%d.\n", frictionRef(host, id), e.ID, e.ID)
	}
	if err := store.AddFrictionLink(ctx, l); err != nil {
		return err
	}
	fmt.Fprintf(out, "Linked #%d and %s (%s): the same problem; resolving either resolves both.\n",
		f.ID, frictionRef(host, e.ID), clip(peerSummary(l), 100))
	if service.FrictionIsOpen(f.Status) != service.FrictionIsOpen(e.Status) {
		fmt.Fprintf(out, "They're out of step: #%d is %s here and %s there (agent friction show %d says what to run).\n",
			f.ID, f.Status, e.Status, f.ID)
	}
	return nil
}

func runFrictionUnlink(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	if f.Status == storage.FrictionDuplicate {
		if f, err = getFriction(ctx, strconv.FormatInt(f.DuplicateOf, 10)); err != nil {
			return err
		}
	}
	host, id, remote, err := parseFrictionRef(args[1])
	if err != nil {
		return err
	}
	if !remote {
		return fmt.Errorf("#%d is on this host: links join entries on two hosts (agent friction show %d lists #%d's)", id, f.ID, f.ID)
	}
	had, err := store.DeleteFrictionLink(ctx, f.ID, host, id)
	if err != nil {
		return err
	}
	ref := frictionRef(host, id)
	bus := openFrictionBus(0)
	defer bus.close()
	_, err = bus.call(ctx, host, natsbus.FrictionRequest{
		Action: natsbus.FrictionUnlink, ID: id, From: &natsbus.FrictionEntry{Host: cfg.Host, ID: f.ID, UID: f.UID}, By: frictionWho(ctx),
	})
	out := cmd.OutOrStdout()
	switch {
	case err != nil && !had:
		return fmt.Errorf("#%d has no link to %s here, and %s didn't unlink its half: %w", f.ID, ref, host, err)
	case err != nil:
		fmt.Fprintf(out, "Unlinked #%d from %s here; %s still has its half: couldn't reach it (%v).\n", f.ID, ref, host, err)
		fmt.Fprintf(out, "Once its bus answers: agent friction unlink %d %d@%s\n", f.ID, id, host)
		fmt.Fprintf(out, "or on %s: agent friction unlink %d %d@%s\n", host, id, f.ID, cfg.Host)
	case !had:
		fmt.Fprintf(out, "#%d had no link to %s here; removed %s's half, if it had one.\n", f.ID, ref, host)
	default:
		fmt.Fprintf(out, "Unlinked #%d and %s.\n", f.ID, ref)
	}
	return nil
}

func runFrictionLinks(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	all, err := store.ListFrictionLinks(ctx, 0)
	if err != nil {
		return err
	}
	local := map[int64]*storage.Friction{}
	for _, l := range all {
		if local[l.FrictionID] == nil {
			f, err := store.GetFriction(ctx, l.FrictionID)
			if err != nil {
				return err
			}
			local[l.FrictionID] = f
		}
	}
	views := []*linkView{}
	for _, v := range checkFrictionLinks(ctx, local, all) {
		if v.dangling() || !frictionDangling {
			views = append(views, v)
		}
	}
	out := cmd.OutOrStdout()
	if frictionJSON {
		return json.NewEncoder(out).Encode(views)
	}
	if len(views) == 0 {
		switch {
		case len(all) == 0:
			fmt.Fprintf(out, "No links to other hosts' friction entries on host %s.\n", cfg.Host)
		default:
			fmt.Fprintf(out, "No dangling links (%d in all: agent friction links).\n", len(all))
		}
		return nil
	}
	fmt.Fprint(out, formatLinksReport(local, views))
	return nil
}

// formatLinksReport is the links command's output: a line per link, and
// what to run under each dangling one.
func formatLinksReport(local map[int64]*storage.Friction, views []*linkView) string {
	var b strings.Builder
	for _, v := range views {
		f := local[v.FrictionID]
		fmt.Fprintf(&b, "#%d %s = %s: ", f.ID, f.Status, v.Ref)
		if v.dangling() {
			fmt.Fprintf(&b, "%s\n", v.Problem)
			for _, fix := range v.Fix {
				fmt.Fprintf(&b, "    %s\n", fix)
			}
			continue
		}
		fmt.Fprintf(&b, "%s\n", clip(peerSummary(v.FrictionLink), 100))
	}
	return b.String()
}

// resolveLinked passes f's resolution on to the entries linked to it,
// following their links in turn (A linked to B linked to C), and prints a
// line for each. Entries on hosts it can't reach are listed with what to
// run.
func resolveLinked(ctx context.Context, out io.Writer, f *storage.Friction) error {
	links, err := store.ListFrictionLinks(ctx, f.ID)
	if err != nil || len(links) == 0 {
		return err
	}
	from, err := service.FrictionEntryOf(ctx, store, cfg.Host, f)
	if err != nil {
		return err
	}
	by := frictionWho(ctx)
	who, _ := frictionCaller(ctx)

	// A target carries the entry whose link led to it: another host
	// resolves only an entry linked to the one a request comes from.
	type target struct {
		host string
		id   int64
		uid  string
		link *storage.FrictionLink // f's own link to it, to update
		via  natsbus.FrictionEntry
	}
	var queue, left []target
	for _, l := range links {
		queue = append(queue, target{l.PeerHost, l.PeerID, l.PeerUID, l, from})
	}
	visited := map[string]bool{frictionKey(cfg.Host, f.ID): true}
	bus := openFrictionBus(0)
	defer bus.close()

	for len(queue) > 0 {
		t := queue[0]
		queue = queue[1:]
		if visited[frictionKey(t.host, t.id)] {
			continue
		}
		visited[frictionKey(t.host, t.id)] = true

		if strings.EqualFold(t.host, cfg.Host) {
			// Another entry here, linked through another host's.
			_, g, err := service.OriginalFriction(ctx, store, t.id)
			if err != nil || g == nil || (t.uid != "" && g.UID != t.uid && g.ID == t.id) {
				continue
			}
			visited[frictionKey(cfg.Host, g.ID)] = true
			if service.FrictionIsOpen(g.Status) {
				if err := service.CloseFriction(ctx, store, g, f.Status, f.Resolution, who); err != nil {
					return err
				}
				fmt.Fprintf(out, "Resolved #%d too (linked through another host's entry).\n", g.ID)
			}
			via, err := service.FrictionEntryOf(ctx, store, cfg.Host, g)
			if err != nil {
				return err
			}
			for _, r := range via.Links {
				queue = append(queue, target{r.Host, r.ID, r.UID, nil, via})
			}
			continue
		}

		ref := frictionRef(t.host, t.id)
		resp, err := bus.call(ctx, t.host, natsbus.FrictionRequest{
			Action: natsbus.FrictionResolve, ID: t.id, UID: t.uid, From: &t.via, By: by, Status: f.Status, Note: f.Resolution,
		})
		if err != nil {
			if isRefusal(err) {
				// Its error says what to run.
				fmt.Fprintf(out, "%s didn't resolve %s: %v.\n", t.host, ref, err)
				continue
			}
			fmt.Fprintf(out, "Couldn't reach %s to resolve %s: %v.\n", t.host, ref, err)
			left = append(left, t)
			continue
		}
		if len(resp.Entries) == 0 {
			fmt.Fprintf(out, "%s is missing: %s has no #%d, so it wasn't resolved (agent friction links shows it).\n", ref, t.host, t.id)
			continue
		}
		e := resp.Entries[0]
		askedUID := e.UID
		if e.Requested != nil {
			askedUID = e.Requested.UID
		}
		if t.uid != "" && askedUID != t.uid {
			fmt.Fprintf(out, "%s wasn't resolved: it's a different entry there now (%s); agent friction links shows it.\n", ref, clip(e.Summary, 60))
			continue
		}
		visited[frictionKey(t.host, e.ID)] = true
		eref := frictionRef(t.host, e.ID)
		if resp.Resolved {
			fmt.Fprintf(out, "Resolved %s too.\n", eref)
		} else {
			fmt.Fprintf(out, "%s was already %s%s.\n", eref, e.Status, byWhomName(e.ResolvedBy))
		}
		if t.link != nil {
			service.SeeFrictionPeer(t.link, e, time.Now())
			_ = store.UpdateFrictionLink(ctx, t.link)
		}
		for _, r := range e.Links {
			queue = append(queue, target{canonicalHost(r.Host), r.ID, r.UID, nil, e})
		}
	}

	if len(left) > 0 {
		fmt.Fprintf(out, "Left to do, once those hosts' buses answer: agent friction resolve %d --linked\n", f.ID)
		for _, t := range left {
			fmt.Fprintf(out, "  or on %s: agent friction resolve %d %s\n", t.host, t.id, resolveFlags(f.Status, f.Resolution))
		}
	}
	return nil
}

// byWhomName is " (by NAME)", or nothing for no name.
func byWhomName(by string) string {
	if by == "" {
		return ""
	}
	return " (by " + by + ")"
}

// formatExportLinks is export's line on an entry's links, from what was
// last seen of them (export doesn't use the bus).
func formatExportLinks(links []*storage.FrictionLink) string {
	if len(links) == 0 {
		return ""
	}
	parts := make([]string, 0, len(links))
	for _, l := range links {
		p := frictionRef(l.PeerHost, l.PeerID)
		if !l.PeerCheckedAt.IsZero() {
			p += fmt.Sprintf(" (%s there, %s)", l.PeerStatus, stamp(l.PeerCheckedAt))
		}
		parts = append(parts, p)
	}
	return "Linked to " + strings.Join(parts, ", ") + ".\n"
}
