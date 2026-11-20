// Command gocompat drives the Go store (legacy/internal/storage) for
// agent-store's compatibility test (tests/compat_test.rs), which builds it in
// a temp copy of the legacy module: Go ignores testdata directories, so this
// never builds in the repo itself.
//
//	gocompat write <db>   write the scenario with the Go store
//	gocompat dump <db>    print everything the Go store reads back
//
// The Rust test writes the same scenario and prints the same dump, line for
// line; keep the two in step.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: gocompat write|dump <db>")
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "write":
		err = write(os.Args[2])
	case "dump":
		err = dump(os.Args[2])
	default:
		err = fmt.Errorf("unknown mode %q", os.Args[1])
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "gocompat:", err)
		os.Exit(1)
	}
}

func write(path string) error {
	s, err := storage.NewSQLiteStorage(path)
	if err != nil {
		return err
	}
	defer s.Close()
	ctx := context.Background()
	loc := time.FixedZone("", -6*3600)
	t0 := time.Date(2026, 11, 1, 9, 30, 15, 123456789, loc)
	sec := func(n int) time.Time { return t0.Add(time.Duration(n) * time.Second) }
	steps := []func() error{
		func() error {
			return s.CreateAgent(ctx, &storage.Agent{Name: "lead", SessionName: "agent-lead", Backend: "claude", TaskFile: "tasks/a.md", LogFile: "/logs/lead.log", Cwd: "/w", CreatedAt: t0})
		},
		func() error {
			return s.CreateAgent(ctx, &storage.Agent{Name: "lead-t1", SessionName: "agent-lead-t1", Backend: "agy", SpawnedBy: "lead@hub", Cwd: "/w/sub", Adopted: true, CreatedAt: sec(1)})
		},
		func() error {
			return s.CreateAgent(ctx, &storage.Agent{Name: "gone", SessionName: "agent-gone", Backend: "claude", CreatedAt: sec(2)})
		},
		func() error {
			return s.SetStates(ctx, []*storage.StateEntry{
				{AgentName: "lead", Key: "_status", Value: "idle", UpdatedAt: sec(3)},
				{AgentName: "lead", Key: "notes", Value: "a && <b>", UpdatedAt: sec(3)},
			})
		},
		func() error {
			return s.RecordReport(ctx, []*storage.StateEntry{
				{AgentName: "lead-t1", Key: "_status", Value: "processing", UpdatedAt: sec(4)},
				{AgentName: "lead-t1", Key: "_transcript", Value: "/t/x.jsonl", UpdatedAt: sec(4)},
			}, "lead-t1", nil, &storage.AgentEvent{At: sec(4), Kind: storage.AgentStatus, Agent: "lead-t1", Status: "processing",
				Event: "PreToolUse", Detail: "d", Tool: "Bash", ToolInput: `{"command":"ls"}`, SessionID: "sid", Transcript: "/t/x.jsonl"})
		},
		func() error {
			return s.UpdateStates(ctx, []*storage.StateEntry{
				{AgentName: "gone", Key: "_status", Value: "ended", UpdatedAt: sec(5)},
				{AgentName: "gone", Key: "_end_reason", Value: "quit", UpdatedAt: sec(5)},
			}, "gone", []string{"notes"})
		},
		func() error { return s.DeleteAgent(ctx, "gone") },
		func() error {
			return s.ReplaceAgent(ctx, &storage.Agent{Name: "lead", SessionName: "agent-lead", Backend: "claude", TaskFile: "tasks/a.md", LogFile: "/logs/lead.log", Cwd: "/w", CreatedAt: sec(6)})
		},
		func() error { return s.UpdateAgentBackend(ctx, "lead-t1", "claude") },
		func() error {
			for _, m := range []*storage.Message{
				{SenderID: "lead", ReceiverID: "lead-t1", Content: "hello", Status: storage.MessageStatusPending, CreatedAt: t0},
				{SenderID: "x", ReceiverID: "lead-t1", Content: "second", Status: storage.MessageStatusPending, CreatedAt: t0},
				{SenderID: "x", ReceiverID: "lead", Content: "long text", Status: storage.MessageStatusHandover, CreatedAt: sec(1)},
			} {
				if err := s.CreateMessage(ctx, m); err != nil {
					return err
				}
			}
			_, err := s.ClaimMessage(ctx, 1)
			return err
		},
		func() error {
			return s.SetTask(ctx, &storage.Task{Name: "review", Content: "# Review\n", SourceFile: "tasks/review.md", CreatedAt: t0})
		},
		func() error {
			if err := s.LogCommand(ctx, &storage.CommandRecord{StartedAt: t0, DurationMS: 12, Host: "hub", Agent: "lead-t1", Cwd: "/w",
				Args: []string{"send", "lead", "a && b"}, ExitCode: 1, Error: "boom", Build: "abc"}); err != nil {
				return err
			}
			return s.LogCommand(ctx, &storage.CommandRecord{StartedAt: sec(1), Host: "hub", Build: "abc"})
		},
		func() error {
			cmd := &storage.AttachedCommand{Args: []string{"wait", "x"}, ExitCode: 2, Error: "timed out", At: t0}
			f1 := &storage.Friction{UID: "hub-1", Host: "hub", FiledBy: "lead", Parent: "boss", Cwd: "/w", Build: "abc", CreatedAt: t0, UpdatedAt: t0,
				Kind: "bug", Summary: "s1", Detail: "d", Command: cmd, Status: storage.FrictionOpen}
			f2 := &storage.Friction{UID: "hub-2", Host: "hub", FiledBy: "user", Build: "abc", CreatedAt: sec(1), UpdatedAt: sec(1),
				Kind: "help", Summary: "s2", Status: storage.FrictionOpen}
			for _, f := range []*storage.Friction{f1, f2} {
				if err := s.AddFriction(ctx, f); err != nil {
					return err
				}
			}
			if err := s.AddFrictionVote(ctx, &storage.FrictionVote{FrictionID: f1.ID, UID: "v1", Host: "hub", Voter: "user", Build: "abc", CreatedAt: t0, Note: "me too", Command: cmd}); err != nil {
				return err
			}
			if err := s.AddFrictionLink(ctx, &storage.FrictionLink{FrictionID: f2.ID, PeerHost: "laptop", PeerID: 5, LinkedBy: "user", LinkedAt: t0}); err != nil {
				return err
			}
			f2.Status, f2.DuplicateOf, f2.ResolvedBy, f2.ResolvedAt, f2.Resolution, f2.UpdatedAt = storage.FrictionDuplicate, f1.ID, "user", t0.Add(time.Hour), "dup", t0.Add(time.Hour)
			if err := s.UpdateFriction(ctx, f2); err != nil {
				return err
			}
			f1.Status, f1.AcceptedBy, f1.AcceptedAt, f1.AcceptNote, f1.UpdatedAt = storage.FrictionAccepted, "lead", t0.Add(2*time.Hour), "soon", t0.Add(2*time.Hour)
			if err := s.UpdateFriction(ctx, f1); err != nil {
				return err
			}
			for _, l := range []*storage.FrictionLink{
				{FrictionID: f1.ID, PeerHost: "laptop", PeerID: 34, PeerUID: "laptop-34", LinkedBy: "lead", LinkedAt: t0, Note: "n",
					ConfirmedAt: t0.Add(time.Minute), PeerStatus: "open", PeerKind: "bug", PeerSummary: "s", PeerVotes: 3, PeerCheckedAt: t0.Add(time.Minute)},
				{FrictionID: f1.ID, PeerHost: "Mac", PeerID: 7, LinkedBy: "lead", LinkedAt: t0},
			} {
				if err := s.AddFrictionLink(ctx, l); err != nil {
					return err
				}
			}
			_, err := s.RenameHost(ctx, "mac", "studio")
			return err
		},
		func() error {
			if err := s.RecordSessionReport(ctx, &storage.SessionReport{SessionID: "s1", Harness: "claude", Status: "idle", Event: "Stop", Transcript: "/t/s1.jsonl", At: t0}); err != nil {
				return err
			}
			return s.RecordSessionReport(ctx, &storage.SessionReport{SessionID: "s2", Harness: "claude", Status: "ended", EndReason: "quit", Event: "SessionEnd", At: sec(1)})
		},
	}
	for i, step := range steps {
		if err := step(); err != nil {
			return fmt.Errorf("step %d: %w", i, err)
		}
	}
	return nil
}

func ts(t time.Time) string { return t.UTC().Format(time.RFC3339Nano) }

func js(v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		return "ERR:" + err.Error()
	}
	return string(b)
}

func line(kind string, fields ...any) {
	parts := make([]string, len(fields))
	for i, f := range fields {
		switch v := f.(type) {
		case time.Time:
			parts[i] = ts(v)
		default:
			parts[i] = fmt.Sprint(v)
		}
	}
	fmt.Printf("%s %s\n", kind, strings.Join(parts, "|"))
}

func agentLine(kind string, a *storage.Agent) {
	line(kind, a.Name, a.SessionName, a.Backend, a.TaskFile, a.LogFile, a.SpawnedBy, a.Cwd, a.Adopted, a.CreatedAt, js(a))
}

func runLine(kind string, r *storage.Run) {
	line(kind, r.ID, r.Name, r.Parent, r.Backend, r.Session, r.Cwd, r.CreatedAt, r.EndedAt, r.Status, r.EndReason, r.Transcript)
}

func stateLine(kind string, e *storage.StateEntry) {
	line(kind, e.AgentName, e.Key, e.Value, e.UpdatedAt, js(e))
}

func dump(path string) error {
	s, err := storage.NewSQLiteStorage(path)
	if err != nil {
		return err
	}
	defer s.Close()
	ctx := context.Background()

	agents, err := s.ListAgents(ctx)
	if err != nil {
		return err
	}
	receivers := map[string]bool{"x": true}
	for _, a := range agents {
		agentLine("agent", a)
		receivers[a.Name] = true
		got, err := s.GetAgent(ctx, a.Name)
		if err != nil || got == nil {
			return fmt.Errorf("get agent %s: %v", a.Name, err)
		}
		agentLine("get", got)
		states, err := s.ListState(ctx, a.Name)
		if err != nil {
			return err
		}
		for _, e := range states {
			stateLine("state", e)
		}
	}
	var names []string
	for r := range receivers {
		names = append(names, r)
	}
	sort.Strings(names)
	for _, r := range names {
		msgs, err := s.GetPendingMessages(ctx, r, 1000)
		if err != nil {
			return err
		}
		for _, m := range msgs {
			line("pending", m.ID, m.SenderID, m.ReceiverID, m.Content, m.Status, m.CreatedAt, js(m))
		}
	}
	for id := int64(1); id <= 20; id++ {
		st, err := s.GetMessageStatus(ctx, id)
		if err != nil {
			continue
		}
		line("msgstatus", id, st)
	}
	tasks, err := s.ListTasks(ctx)
	if err != nil {
		return err
	}
	for _, t := range tasks {
		line("task", t.Name, js(t.Content), t.SourceFile, t.CreatedAt, js(t))
	}
	cmds, err := s.ListCommands(ctx, storage.CommandFilter{})
	if err != nil {
		return err
	}
	for _, c := range cmds {
		line("command", c.ID, c.StartedAt, c.DurationMS, c.Host, c.Agent, c.Parent, c.Cwd, js(c.Args), c.ExitCode, c.Error, c.Build, js(c))
	}
	frictions, err := s.ListFriction(ctx, storage.FrictionFilter{})
	if err != nil {
		return err
	}
	for _, f := range frictions {
		cmd := ""
		if f.Command != nil {
			cmd = js(f.Command)
		}
		line("friction", f.ID, f.UID, f.Host, f.FiledBy, f.Parent, f.Cwd, f.Build, f.CreatedAt, f.UpdatedAt, f.Kind, f.Summary, f.Detail, cmd,
			f.Status, f.AcceptedBy, f.AcceptedAt, f.AcceptNote, f.ResolvedBy, f.ResolvedAt, f.Resolution, f.DuplicateOf, f.Votes, js(f))
		votes, err := s.ListFrictionVotes(ctx, f.ID)
		if err != nil {
			return err
		}
		for _, v := range votes {
			cmd := ""
			if v.Command != nil {
				cmd = js(v.Command)
			}
			line("vote", v.ID, v.FrictionID, v.UID, v.Host, v.Voter, v.Parent, v.Build, v.CreatedAt, v.Note, cmd, js(v))
		}
	}
	last, err := s.LastFrictionID(ctx)
	if err != nil {
		return err
	}
	line("lastfriction", last)
	links, err := s.ListFrictionLinks(ctx, 0)
	if err != nil {
		return err
	}
	for _, l := range links {
		line("link", l.ID, l.FrictionID, l.PeerHost, l.PeerID, l.PeerUID, l.LinkedBy, l.LinkedAt, l.Note, l.ConfirmedAt, l.PeerStatus, l.PeerKind,
			l.PeerSummary, l.PeerVotes, l.PeerResolvedBy, l.PeerResolution, l.PeerCheckedAt, js(l))
	}
	reports, err := s.SessionReports(ctx)
	if err != nil {
		return err
	}
	var ids []string
	for id := range reports {
		ids = append(ids, id)
	}
	sort.Strings(ids)
	for _, id := range ids {
		r := reports[id]
		line("session", r.SessionID, r.Harness, r.Status, r.EndReason, r.Event, r.Detail, r.Transcript, r.At)
	}
	tree, err := s.ReadTree(ctx, true)
	if err != nil {
		return err
	}
	line("tree", tree.Cursor, tree.Events)
	for _, a := range tree.Agents {
		agentLine("tree.agent", a)
	}
	var runNames []string
	for n := range tree.Runs {
		runNames = append(runNames, n)
	}
	sort.Strings(runNames)
	for _, n := range runNames {
		runLine("tree.run", tree.Runs[n])
	}
	for _, e := range tree.State {
		stateLine("tree.state", e)
	}
	for _, r := range tree.Ended {
		runLine("tree.ended", r)
	}
	events, oldest, err := s.AgentEventsAfter(ctx, 0, 1000000)
	if err != nil {
		return err
	}
	line("oldest", oldest)
	for _, ev := range events {
		line("event", ev.ID, ev.At, ev.Kind, ev.Agent, ev.Run, ev.Status, ev.Event, ev.Detail, ev.Tool, js(ev.ToolInput), ev.SessionID, ev.Transcript, ev.EndReason)
		if ev.Run != 0 {
			r, err := s.GetRun(ctx, ev.Run)
			if err != nil {
				return err
			}
			if r != nil {
				runLine("run", r)
			}
		}
	}
	return nil
}
