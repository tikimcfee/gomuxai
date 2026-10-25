package cli

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

var transcriptCmd = &cobra.Command{
	Use:   "transcript <name|session-id|path>",
	Short: "Print one session's events as JSON lines; --follow streams them as they happen",
	Long: `Transcript prints one agent session's events as JSON lines, as
docs/EVENTS.md describes them: a "session" line (the session as agent
sessions --json lists it), then an "event" line for each prompt, thinking
block, reply, tool call and harness record, in transcript order. An event
that changes later (a tool's result lands) is sent again, whole, with the
same id and "op":"update". Displays and scripts read this; agent logs is
the view for a person.

The session is one of:
  - a workgroup agent's name: its current transcript (after cleanup, its
    last one);
  - a session id, or the start of one that only one session's id starts
    with (agent sessions lists them, agent sessions --json their ids);
  - a transcript's path; its harness is told from the path, or from its
    first lines.

--from <byte> resumes after the line that starts at that byte offset: pass
the "at" of the last event you hold. That line's events and those before it
aren't sent again. The ones after have the seq, line and turn a whole read
gives them, and updates to earlier events still come (whole, so you needn't
hold what they update). -1, the default, reads from the start.
--until <byte> leaves out the lines that start at or after it, so --from A
--until B reads the lines after A's and before B.

--follow keeps reading as the transcript grows, until Ctrl-C or until the
reader closes the pipe (both exit 0). It polls the file every 250ms, and
never reads a line that isn't whole yet. Every 2s it reads the session
again, and sends a new session line when anything but its size and times
changed (title, model, status). It goes on after the session ends: a
resumed session writes on.

Claude Code writes a turn's closing reply only after its Stop hook has run,
and the hook has the text already. For a Claude agent of this store,
transcript sends that reply at once, as a provisional say (id
"<turn id>#final", "provisional":true, "at" where its line will land), and
the transcript's own copy as an update of it once it lands. A provisional
reply isn't in the transcript, so it takes no seq of its own: it has the
seq of the event that comes next. Don't resume --from its "at".

--requests adds the request channel: what each model call was sent (Claude
Code's api-request lines). Request events have their own seq, so the
conversation's seq is the same with or without it.

This host only; for another host's agent, agent capture name@host shows its
screen.`,
	Example: `  agent transcript lead-api
  agent transcript lead-api --follow
  agent transcript 3f3a9c --from 18342 --until 40960
  agent transcript ~/.claude/projects/-home-user-shop/3f3a9c1e-7a2b-4c4d-9e0f-1a2b3c4d5e6f.jsonl --requests`,
	Args: cobra.ExactArgs(1),
	RunE: runTranscript,
}

var (
	transcriptFollow   bool
	transcriptFrom     int64
	transcriptUntil    int64
	transcriptRequests bool
)

// How often --follow reads what the transcript appended (and looks for a
// provisional reply), and reads the session's metadata and status again;
// variables for tests.
var (
	transcriptPollInterval    = 250 * time.Millisecond
	transcriptSessionInterval = 2 * time.Second
)

func init() {
	transcriptCmd.Flags().BoolVar(&transcriptFollow, "follow", false, "Keep reading as the transcript grows, until interrupted")
	transcriptCmd.Flags().Int64Var(&transcriptFrom, "from", -1, "Resume after the line starting at this byte offset (the at of the last event you hold)")
	transcriptCmd.Flags().Int64Var(&transcriptUntil, "until", -1, "Leave out the lines starting at or after this byte offset (not with --follow)")
	transcriptCmd.Flags().BoolVar(&transcriptRequests, "requests", false, "Add the request channel: what each model call was sent")
}

func runTranscript(cmd *cobra.Command, args []string) error {
	opts := streamOptions{from: transcriptFrom, until: -1, follow: transcriptFollow, requests: transcriptRequests}
	if opts.from < -1 {
		return fmt.Errorf("--from %d: give the byte offset of a line's start (the at of an event), or -1 for the start", opts.from)
	}
	if cmd.Flags().Changed("until") {
		switch {
		case opts.follow:
			return fmt.Errorf("--until ends the read at an offset, and --follow reads on as the transcript grows: use one")
		case transcriptUntil <= opts.from:
			return fmt.Errorf("--until %d is not after --from %d: the window is the lines after --from's and before --until", transcriptUntil, opts.from)
		}
		opts.until = transcriptUntil
	}
	// A reader closing the pipe is how a display says it's done: the
	// write fails with EPIPE rather than killing the process.
	ctx, stop := signal.NotifyContext(getContext(), os.Interrupt, syscall.SIGTERM, syscall.SIGPIPE)
	defer stop()
	target, err := resolveTranscript(ctx, args[0])
	if err != nil {
		return err
	}
	if opts.follow {
		notice("Following %s (session %s). Ctrl-C stops.", target.path, target.session.SessionID)
	}
	err = streamTranscript(ctx, target, os.Stdout, opts)
	if errors.Is(err, syscall.EPIPE) || ctx.Err() != nil {
		return nil
	}
	return err
}

// transcriptTarget is the transcript agent transcript reads, and its
// session.
type transcriptTarget struct {
	path   string
	reader transcript.Reader
	// session is the session as agent sessions lists it.
	session discovery.Session
	// above holds the transcripts of the sessions above a subagent's,
	// nearest first: its status follows theirs.
	above []string
}

// resolveTranscript finds the transcript arg names: a workgroup agent's
// current one, a session's by id (or the start of one), or a path.
func resolveTranscript(ctx context.Context, arg string) (*transcriptTarget, error) {
	path, err := transcriptPath(ctx, arg)
	if err != nil {
		return nil, err
	}
	scanned := discovery.Scan(sessionDirs())
	if path != "" && sessionAt(scanned, path) < 0 {
		if s, ok := discovery.Probe(path); ok {
			scanned = append(scanned, s)
		}
	}
	sessions, err := resolveSessions(ctx, scanned)
	if err != nil {
		return nil, err
	}
	if path == "" {
		s, err := sessionByID(sessions, arg)
		if err != nil {
			return nil, err
		}
		path = s.Path
	}
	t := &transcriptTarget{path: path}
	if i := sessionAt(sessions, path); i >= 0 {
		t.session = sessions[i]
		t.above = sessionsAbove(sessions, t.session)
	} else if t.session, err = unlistedSession(path); err != nil {
		return nil, err
	}
	if t.reader = harnessReader(t.session.Harness); t.reader == nil {
		head, err := readHead(path)
		if err != nil {
			return nil, err
		}
		t.reader = transcript.Detect(head)
		t.session.Harness = t.reader.Harness()
	}
	return t, nil
}

// transcriptPath returns the transcript a path or a workgroup agent's name
// names, or "" for an argument that can only be a session id.
func transcriptPath(ctx context.Context, arg string) (string, error) {
	if arg == "" {
		return "", fmt.Errorf("give an agent's name, a session id or a transcript's path (agent sessions lists the sessions)")
	}
	if strings.ContainsRune(arg, filepath.Separator) || strings.HasSuffix(arg, ".jsonl") {
		info, err := os.Stat(arg)
		switch {
		case errors.Is(err, fs.ErrNotExist):
			return "", fmt.Errorf("no transcript at %s: there is no such file (agent sessions lists the sessions, and agent sessions --json their paths)", arg)
		case err != nil:
			return "", fmt.Errorf("can't read the transcript at %s: %w", arg, err)
		case info.IsDir():
			return "", fmt.Errorf("%s is a directory: give a transcript file (agent sessions --json lists their paths)", arg)
		}
		return absPath(arg), nil
	}
	ref, remote, err := parseTarget(arg)
	if err != nil {
		return "", err
	}
	if remote {
		return "", fmt.Errorf("transcripts are read per host, and %s's is on %q: agent capture %s shows its screen", ref, ref.Host, ref)
	}
	if service.ValidateAgentName(arg) != nil {
		return "", nil
	}
	if refs := agentService.Transcripts(ctx, arg); len(refs) > 0 {
		return refs[len(refs)-1].Path, nil
	}
	if _, err := agentService.Agent(ctx, arg); err == nil {
		return "", fmt.Errorf("agent %s has no transcript yet: a claude or agy agent's hooks name it at its first prompt, and other backends keep none (agent logs %s --terminal prints its terminal log)", arg, arg)
	}
	return "", nil
}

// sessionByID returns the session whose id is id, or the one session
// whose id starts with it.
func sessionByID(sessions []discovery.Session, id string) (discovery.Session, error) {
	var matches []discovery.Session
	for _, s := range sessions {
		if s.SessionID == id {
			matches = []discovery.Session{s}
			break
		}
		if strings.HasPrefix(s.SessionID, id) {
			matches = append(matches, s)
		}
	}
	switch len(matches) {
	case 0:
		return discovery.Session{}, fmt.Errorf("no agent or session matches %q: agent list lists the agents, and agent sessions --json the sessions and their ids", id)
	case 1:
	default:
		var ids []string
		for _, s := range matches[:min(len(matches), 5)] {
			ids = append(ids, s.SessionID)
		}
		more := ""
		if len(matches) > 5 {
			more = fmt.Sprintf(" and %d more", len(matches)-5)
		}
		return discovery.Session{}, fmt.Errorf("%q starts the ids of %d sessions (%s%s): give more of the id", id, len(matches), strings.Join(ids, ", "), more)
	}
	s := matches[0]
	if s.Path == "" {
		return discovery.Session{}, fmt.Errorf("session %s has no transcript yet (agent %s's CLI hasn't named one; agent logs %s --terminal prints its terminal log)", s.SessionID, s.Workgroup.Name, s.Workgroup.Name)
	}
	return s, nil
}

// sessionAt returns the index of the session whose transcript is path, or
// -1.
func sessionAt(sessions []discovery.Session, path string) int {
	for i, s := range sessions {
		if s.Path != "" && samePath(s.Path, path) {
			return i
		}
	}
	return -1
}

// samePath reports whether a and b name the same file.
func samePath(a, b string) bool {
	if filepath.Clean(a) == filepath.Clean(b) {
		return true
	}
	ai, aerr := os.Stat(a)
	bi, berr := os.Stat(b)
	return aerr == nil && berr == nil && os.SameFile(ai, bi)
}

// sessionsAbove returns the transcripts of the sessions above a
// subagent's, nearest first.
func sessionsAbove(sessions []discovery.Session, s discovery.Session) []string {
	var above []string
	seen := map[string]bool{}
	for p := s.ParentSession; p != nil && !seen[p.SessionID]; {
		seen[p.SessionID] = true
		i := -1
		for j, c := range sessions {
			if c.Harness == s.Harness && c.SessionID == p.SessionID && c.Path != "" {
				i = j
				break
			}
		}
		if i < 0 {
			break
		}
		above = append(above, sessions[i].Path)
		p = sessions[i].ParentSession
	}
	return above
}

// unlistedSession is the session of a transcript discovery can't read (an
// agy log outside a conversation folder): its file, and an unknown status.
func unlistedSession(path string) (discovery.Session, error) {
	info, err := os.Stat(path)
	if err != nil {
		return discovery.Session{}, transcriptGone(path, err)
	}
	return discovery.Session{SessionID: discovery.SessionIDFromPath(path), Harness: discovery.HarnessOf(path), Path: path,
		Size: info.Size(), Mtime: info.ModTime().UTC(), Status: discovery.StatusUnknown}, nil
}

// harnessReader returns the reader of a harness's transcripts, or nil.
func harnessReader(harness string) transcript.Reader {
	switch harness {
	case discovery.HarnessClaude:
		return transcript.Claude{}
	case discovery.HarnessAgy:
		return transcript.Agy{}
	case discovery.HarnessKimi:
		return transcript.Kimi{}
	}
	return nil
}

// transcriptHead is how much of a transcript readHead reads to detect its
// format.
const transcriptHead = 256 << 10

func readHead(path string) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, transcriptGone(path, err)
	}
	defer f.Close()
	head := make([]byte, transcriptHead)
	n, err := io.ReadFull(f, head)
	if err != nil && !errors.Is(err, io.ErrUnexpectedEOF) && !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("reading the transcript %s: %w", path, err)
	}
	return head[:n], nil
}

// transcriptGone is the error for a transcript that can't be opened.
func transcriptGone(path string, err error) error {
	if errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("the transcript %s is gone (agent sessions lists the sessions there are)", path)
	}
	return fmt.Errorf("can't read the transcript %s: %w", path, err)
}

// sessionDirs are the directories agent sessions scans.
func sessionDirs() []discovery.Dir {
	return discovery.Dirs(discovery.Overrides{Claude: cfg.ClaudeProjectsDir, Agy: cfg.AgyBrainDir, Kimi: cfg.KimiSessionsDir})
}

// resolveSessions joins sessions to the workgroup agents and sets their
// status, as agent sessions does. It asks Claude Code what is running only
// when a session is Claude's.
func resolveSessions(ctx context.Context, sessions []discovery.Session) ([]discovery.Session, error) {
	agents, err := workgroupAgents(ctx)
	if err != nil {
		return nil, err
	}
	live := discovery.Live{Now: time.Now()}
	if slices.ContainsFunc(sessions, func(s discovery.Session) bool { return s.Harness == discovery.HarnessClaude }) {
		live, _ = liveNow(ctx)
	}
	return discovery.Resolve(sessions, agents, live), nil
}

// liveNow is what the harnesses say is running on this host now. The error
// says why Claude's sessions are missing from it.
func liveNow(ctx context.Context) (discovery.Live, error) {
	live := discovery.Live{Now: time.Now()}
	statuses, err := claudeSessions(ctx)
	if err != nil {
		return live, err
	}
	live.Claude = map[string]string{}
	for id, st := range statuses {
		live.Claude[id] = st.String()
	}
	return live, nil
}

// streamOptions are what agent transcript's flags choose.
type streamOptions struct {
	// from and until bound the window: the lines after the one at from
	// (-1: all) and before until (-1: no end).
	from, until int64
	follow      bool
	requests    bool
}

// transcriptStream writes one transcript's event stream.
type transcriptStream struct {
	streamOptions
	target *transcriptTarget
	f      *os.File
	p      *transcript.Parser
	finals *transcript.Finals
	w      *bufio.Writer
	enc    *json.Encoder
	buf    []byte
	// pending holds the bytes read but not yet parsed, a line not yet
	// whole; it starts at p.Pos().At.
	pending []byte
	// session is the session line last written.
	session discovery.Session
	// turn is the turn of the latest event, and turnAt when its first
	// event happened.
	turn   *transcript.TurnRef
	turnAt time.Time
	// stopper is the workgroup Claude agent whose Stop replies are sent
	// provisionally, or ""; stopAt, when the latest one looked at was
	// stored.
	stopper string
	stopAt  time.Time
}

// streamTranscript writes target's event stream to out: the session line,
// the events in the window, and with follow, what the transcript appends
// until ctx is done.
func streamTranscript(ctx context.Context, target *transcriptTarget, out io.Writer, opts streamOptions) error {
	f, err := os.Open(target.path)
	if err != nil {
		return transcriptGone(target.path, err)
	}
	defer f.Close()
	popts := transcript.FileOptions(target.reader.Harness(), target.path)
	popts.Requests = opts.requests
	w := bufio.NewWriterSize(out, 64<<10)
	s := &transcriptStream{streamOptions: opts, target: target, f: f, p: target.reader.NewParser(popts),
		finals: transcript.NewFinals(), w: w, enc: json.NewEncoder(w), buf: make([]byte, 1<<20), session: target.session}
	s.enc.SetEscapeHTML(false)
	if g := target.session.Workgroup; g != nil && target.session.Harness == discovery.HarnessClaude && samePath(g.Store, filepath.Dir(cfg.DatabasePath)) {
		s.stopper = g.Name
	}

	// The initial read ends where the session line says the transcript
	// ends.
	end := target.session.Size
	if info, err := f.Stat(); err == nil && (end <= 0 || end > info.Size()) {
		end = info.Size()
	}
	if err := checkLineStart(f, opts.from, end); err != nil {
		return err
	}
	s.session.Size = end
	if err := s.enc.Encode(transcriptSessionLine{"session", s.session}); err != nil {
		return err
	}
	if err := s.read(end); err != nil {
		return err
	}
	if s.p.Lines > 0 && s.p.Skipped == s.p.Lines {
		return fmt.Errorf("the transcript %s is not one agent sessions can read: none of its %d lines is a JSON object", target.path, s.p.Lines)
	}
	if opts.until < 0 {
		if err := s.provisional(ctx); err != nil {
			return err
		}
	}
	if err := w.Flush(); err != nil || !opts.follow {
		return err
	}
	return s.follow(ctx)
}

// transcriptSessionLine is a stream's session line.
type transcriptSessionLine struct {
	Type string `json:"type"`
	discovery.Session
}

// checkLineStart returns an error unless from is -1 or the start of a
// whole line of the transcript's first end bytes, as --from needs.
func checkLineStart(f *os.File, from, end int64) error {
	if from < 0 {
		return nil
	}
	var b [1]byte
	if from >= end {
		return fmt.Errorf("--from %d is past the transcript's end (%d bytes): give the at of an event it sent", from, end)
	}
	if from > 0 {
		if _, err := f.ReadAt(b[:], from-1); err != nil {
			return fmt.Errorf("reading the transcript: %w", err)
		}
		if b[0] != '\n' {
			return fmt.Errorf("--from %d is not the start of a line of the transcript: give the at of an event it sent", from)
		}
	}
	buf := make([]byte, 64<<10)
	for off := from; off < end; {
		n, err := f.ReadAt(buf[:min(int64(len(buf)), end-off)], off)
		if bytes.IndexByte(buf[:n], '\n') >= 0 {
			return nil
		}
		off += int64(n)
		if err != nil && err != io.EOF {
			return fmt.Errorf("reading the transcript: %w", err)
		}
		if n == 0 {
			break
		}
	}
	return fmt.Errorf("--from %d: the line there has no end yet; resume from an earlier event's at", from)
}

// read parses the transcript up to end, or up to until, and writes the
// events in the window.
func (s *transcriptStream) read(end int64) error {
	for off := s.p.Pos().At + int64(len(s.pending)); off < end && !s.pastUntil(); {
		n, err := s.f.ReadAt(s.buf[:min(int64(len(s.buf)), end-off)], off)
		off += int64(n)
		s.pending = append(s.pending, s.buf[:n]...)
		start := s.p.Pos().At
		for _, e := range s.p.Feed(s.pending) {
			if err := s.emit(e); err != nil {
				return err
			}
		}
		s.pending = append(s.pending[:0], s.pending[s.p.Pos().At-start:]...)
		if err != nil && err != io.EOF {
			return fmt.Errorf("reading the transcript %s: %w", s.target.path, err)
		}
		if n == 0 {
			break
		}
	}
	return nil
}

func (s *transcriptStream) pastUntil() bool {
	return s.until >= 0 && s.p.Pos().At >= s.until
}

// emit writes one event of the transcript's, if its line is in the window.
// Every event goes through Finals and the turn, so what the window holds
// is merged and turned as a whole read's is.
func (s *transcriptStream) emit(e transcript.Event) error {
	e = s.finals.Merge(e)
	if e.Kind != transcript.KindRequest && e.Turn != nil && (s.turn == nil || s.turn.ID != e.Turn.ID) {
		s.turn, s.turnAt = e.Turn, e.Time()
	}
	if (s.from >= 0 && e.At <= s.from) || (s.until >= 0 && e.At >= s.until) {
		return nil
	}
	return s.enc.Encode(transcriptEventLine{"event", e})
}

// transcriptEventLine is a stream's event line.
type transcriptEventLine struct {
	Type string `json:"type"`
	transcript.Event
}

// provisional writes the closing reply of the turn the transcript is in,
// if the agent's Stop hook has reported it and the transcript doesn't
// hold it yet. The hook stores it as the agent's _result; a new turn
// empties that.
func (s *transcriptStream) provisional(ctx context.Context) error {
	if s.stopper == "" || s.turn == nil {
		return nil
	}
	entries, err := store.ListState(ctx, s.stopper)
	if err != nil {
		return nil // the next poll asks again
	}
	var result *storage.StateEntry
	current := false
	for _, e := range entries {
		switch e.Key {
		case service.StateKeyResult:
			result = e
		case service.StateKeyTranscript:
			current = samePath(e.Value, s.target.path)
		}
	}
	if result == nil || result.Value == "" || !current || !result.UpdatedAt.After(s.stopAt) {
		return nil
	}
	s.stopAt = result.UpdatedAt
	if result.UpdatedAt.Before(s.turnAt) {
		return nil // an earlier turn's
	}
	pos := s.p.Pos()
	e, ok := s.finals.Provisional(s.turn.ID, result.Value, result.UpdatedAt.UTC().Format(time.RFC3339Nano), pos)
	if !ok {
		return nil
	}
	// Not the transcript's: it shares the seq of the event that comes next.
	e.Seq = pos.Seq
	return s.enc.Encode(transcriptEventLine{"event", e})
}

// follow writes what the transcript appends, and the session line again
// when the session changes, until ctx is done.
func (s *transcriptStream) follow(ctx context.Context) error {
	poll := time.NewTicker(transcriptPollInterval)
	defer poll.Stop()
	sessions := time.NewTicker(transcriptSessionInterval)
	defer sessions.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-poll.C:
			info, err := s.f.Stat()
			if err != nil {
				return fmt.Errorf("reading the transcript %s: %w", s.target.path, err)
			}
			if read := s.p.Pos().At + int64(len(s.pending)); info.Size() < read {
				return fmt.Errorf("the transcript %s shrank from %d to %d bytes: it was rewritten, so the events sent may no longer match it; run agent transcript again",
					s.target.path, read, info.Size())
			}
			if err := s.read(info.Size()); err != nil {
				return err
			}
			if err := s.provisional(ctx); err != nil {
				return err
			}
		case <-sessions.C:
			if err := s.refreshSession(ctx); err != nil {
				return err
			}
		}
		if err := s.w.Flush(); err != nil {
			return err
		}
	}
}

// refreshSession reads the session again and writes its line if anything
// but its size and times changed.
func (s *transcriptStream) refreshSession(ctx context.Context) error {
	var probed []discovery.Session
	for _, p := range append([]string{s.target.path}, s.target.above...) {
		if sess, ok := discovery.Probe(p); ok {
			probed = append(probed, sess)
		}
	}
	sessions, err := resolveSessions(ctx, probed)
	if err != nil {
		return nil // the next refresh tries again
	}
	i := sessionAt(sessions, s.target.path)
	if i < 0 {
		return nil
	}
	if sessionMeta(sessions[i]) == sessionMeta(s.session) {
		return nil
	}
	s.session = sessions[i]
	return s.enc.Encode(transcriptSessionLine{"session", s.session})
}

// sessionMeta is what a session line says, but for the size and times
// that change with every write.
func sessionMeta(s discovery.Session) string {
	s.Size, s.Mtime, s.StatusAt = 0, time.Time{}, time.Time{}
	data, _ := json.Marshal(s)
	return string(data)
}
