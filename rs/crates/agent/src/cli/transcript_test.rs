//! Go: internal/cli/transcript_test.go and transcript_poll_test.go.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_base::types::Status;
use agent_store::{MESSAGE_STATUS_PENDING, Message};
use agent_transcript::{Claude, KIND_REQUEST};

use super::*;
use crate::cli::logs::logs_test::{hook_transcript, run_words, temp_path, write_test_file};
use crate::cli::sessions::sessions_test::{claude_subagent, claude_transcript, setup_sessions};
use crate::cli::testenv::{TestEnv, temp_dir};

/// The path of a transcript of the conformance corpus, and its bytes (Go:
/// corpusTranscript).
fn corpus_transcript(name: &str) -> (String, Vec<u8>) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-transcript/testdata/corpus/claude")
        .join(name);
    let path = filepath::clean(&path.to_string_lossy());
    let data = std::fs::read(&path).unwrap();
    (path, data)
}

/// A target for the Claude transcript at path that no workgroup owns (Go:
/// fileTarget).
fn file_target(path: &str, size: i64) -> TranscriptTarget {
    TranscriptTarget {
        path: path.to_string(),
        reader: &Claude,
        session: Session {
            session_id: "s".into(),
            harness: discovery::HARNESS_CLAUDE.into(),
            path: path.to_string(),
            size,
            status: discovery::STATUS_ENDED.into(),
            ..Session::default()
        },
        above: Vec::new(),
        unwritten: false,
    }
}

/// The event lines a whole read of data makes, as the stream writes them (Go:
/// wholeStream).
fn whole_stream(path: &str, data: &[u8], requests: bool) -> Vec<String> {
    let mut opts = transcript::file_options("claude", path);
    opts.requests = requests;
    Claude
        .new_parser(opts)
        .feed(data)
        .iter()
        .map(|e| typed_json_line("event", e).unwrap().trim_end().to_string())
        .collect()
}

fn opts(from: i64, until: i64) -> StreamOptions {
    StreamOptions {
        from,
        until,
        ..StreamOptions::default()
    }
}

/// A Ctx for streams that need no termd: a store and config of their own.
fn plain_env() -> TestEnv {
    TestEnv::new()
}

/// Runs a stream that doesn't follow and returns its opening session line and
/// its event lines (Go: streamLines).
fn stream_lines(env: &TestEnv, target: &TranscriptTarget, o: &StreamOptions) -> (String, Vec<String>) {
    let mut out = Vec::new();
    stream_transcript(&env.ctx, &Context::background(), target, &mut out, o).unwrap();
    let out = String::from_utf8(out).unwrap();
    let lines: Vec<String> = out.trim_end_matches('\n').split('\n').map(str::to_string).collect();
    (lines[0].clone(), event_lines(&lines))
}

/// The event lines among a stream's lines (Go: eventLines).
fn event_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l.starts_with(r#"{"type":"event","#))
        .cloned()
        .collect()
}

/// The session lines among a stream's lines, decoded (Go: sessionLines).
fn session_lines(lines: &[String]) -> Vec<Session> {
    lines
        .iter()
        .filter(|l| l.starts_with(r#"{"type":"session","#))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The offset each line of data starts at (Go: lineStarts).
fn line_starts(data: &[u8]) -> Vec<i64> {
    let mut starts = vec![0];
    for (i, &b) in data[..data.len() - 1].iter().enumerate() {
        if b == b'\n' {
            starts.push(i as i64 + 1);
        }
    }
    starts
}

/// The at of an event line (Go: eventAt).
fn event_at(line: &str) -> i64 {
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["at"].as_i64().unwrap_or(0)
}

/// A --until window that ends before the prompt that abandons a branch still
/// sends that branch's events abandoned: every window holds the whole read's
/// events of its lines, with each id the whole read abandons marked, and
/// nothing from past its end.
#[test]
fn test_transcript_window_abandoned() {
    let env = plain_env();
    let (path, data) = corpus_transcript("rewind.jsonl");
    let events = Claude.new_parser(transcript::file_options("claude", &path)).feed(&data);
    let dead: HashSet<String> = events.iter().filter(|e| e.abandoned).map(|e| e.id.clone()).collect();
    assert!(!dead.is_empty(), "the rewind case abandons nothing");
    let target = file_target(&path, data.len() as i64);
    let starts = line_starts(&data);
    let mut marked = 0;
    for from in std::iter::once(-1).chain(starts.iter().copied()) {
        for &until in &starts {
            if until <= from {
                continue;
            }
            let want: Vec<String> = events
                .iter()
                .filter(|e| e.at > from && e.at < until)
                .map(|e| {
                    let mut e = e.clone();
                    e.abandoned = e.abandoned || dead.contains(&e.id);
                    typed_json_line("event", &e).unwrap().trim_end().to_string()
                })
                .collect();
            let (_, got) = stream_lines(&env, &target, &opts(from, until));
            assert_eq!(got.join("\n"), want.join("\n"), "--from {from} --until {until}");
            marked += got
                .iter()
                .filter(|l| l.contains(r#""op":"add""#) && l.contains(r#""abandoned":true"#))
                .count();
        }
    }
    assert!(marked > 0, "no window sent an add abandoned: none ended before its rewind");
}

/// A window holds exactly the whole read's events of its lines, numbered and
/// turned as the whole read numbers them.
#[test]
fn test_transcript_window() {
    let env = plain_env();
    for requests in [false, true] {
        let (path, data) = corpus_transcript("requests.jsonl");
        let whole = whole_stream(&path, &data, requests);
        let target = file_target(&path, data.len() as i64);
        let o = StreamOptions {
            requests,
            ..opts(-1, -1)
        };
        let (session, got) = stream_lines(&env, &target, &o);
        assert!(
            session.starts_with(r#"{"type":"session","session_id":"s","#),
            "session line {session}"
        );
        assert_eq!(got.join("\n"), whole.join("\n"), "whole read (requests {requests})");
        let starts = line_starts(&data);
        for from in std::iter::once(-1).chain(starts.iter().copied()) {
            for until in starts.iter().copied().chain(std::iter::once(-1)) {
                if until >= 0 && until <= from {
                    continue;
                }
                let want: Vec<String> = whole
                    .iter()
                    .filter(|l| {
                        let at = event_at(l);
                        at > from && (until < 0 || at < until)
                    })
                    .cloned()
                    .collect();
                let o = StreamOptions {
                    requests,
                    ..opts(from, until)
                };
                let (_, got) = stream_lines(&env, &target, &o);
                assert_eq!(
                    got.join("\n"),
                    want.join("\n"),
                    "--from {from} --until {until} (requests {requests})"
                );
            }
        }
    }

    // The conversation is numbered the same with or without the request
    // channel, which numbers its own.
    let (path, data) = corpus_transcript("requests.jsonl");
    let seqs = |requests: bool| -> Vec<(String, usize)> {
        whole_stream(&path, &data, requests)
            .iter()
            .map(|l| {
                let e: Event = serde_json::from_str(l).unwrap();
                (e.kind, e.seq)
            })
            .collect()
    };
    let plain: Vec<usize> = seqs(false).into_iter().map(|(_, s)| s).collect();
    let with = seqs(true);
    let conversation: Vec<usize> = with.iter().filter(|(k, _)| k != KIND_REQUEST).map(|(_, s)| *s).collect();
    let req: Vec<usize> = with.iter().filter(|(k, _)| k == KIND_REQUEST).map(|(_, s)| *s).collect();
    assert_eq!(plain, conversation, "seq without requests, and with");
    assert_eq!(req, [0, 1], "requests' seq");
}

/// The stream is what Go's agent transcript writes, byte for byte: the
/// corpus's golden events (written by Go's encoder) are the event lines of a
/// whole read. (tool-results' golden file predates Go sorting an edit's patch
/// hunks' keys, which Go's agent transcript now does, as this stream does.)
#[test]
fn test_transcript_matches_go_events() {
    let env = plain_env();
    for name in ["held-delivery", "requests", "rewind", "notification-and-commands", "basic-turn"] {
        let (path, data) = corpus_transcript(&format!("{name}.jsonl"));
        let golden = std::fs::read_to_string(path.replace(".jsonl", ".events.jsonl")).unwrap();
        let want: Vec<String> = event_lines(&golden.lines().map(str::to_string).collect::<Vec<_>>());
        let (_, got) = stream_lines(&env, &file_target(&path, data.len() as i64), &opts(-1, -1));
        assert_eq!(got.join("\n"), want.join("\n"), "{name}");
    }
}

#[test]
fn test_transcript_from_not_a_line_start() {
    let env = plain_env();
    let (path, data) = corpus_transcript("requests.jsonl");
    let target = file_target(&path, data.len() as i64);
    for (from, want) in [
        (1, "is not the start of a line"),
        (data.len() as i64, "is past the transcript's end"),
        (data.len() as i64 + 10, "is past the transcript's end"),
    ] {
        let mut out = Vec::new();
        let err = stream_transcript(&env.ctx, &Context::background(), &target, &mut out, &opts(from, -1))
            .expect_err("a bad --from");
        assert!(format!("{err:#}").contains(want), "--from {from}: {err:#}, want {want:?}");
        assert!(out.is_empty(), "--from {from} wrote {:?} before failing", String::from_utf8_lossy(&out));
    }

    // A last line not yet whole can't be resumed after.
    let dir = temp_dir("agent-transcript-");
    let partial = temp_path(&dir, "partial.jsonl");
    let last = *line_starts(&data).last().unwrap();
    std::fs::write(&partial, &data[..data.len() - 5]).unwrap();
    let err = stream_transcript(
        &env.ctx,
        &Context::background(),
        &file_target(&partial, data.len() as i64 - 5),
        &mut Vec::new(),
        &opts(last, -1),
    )
    .expect_err("a partial last line");
    assert!(format!("{err:#}").contains("has no end yet"), "--from the partial last line: {err:#}");
}

/// A buffer a stream writes while a test reads it (Go: syncBuffer).
#[derive(Clone, Default)]
struct SyncBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SyncBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SyncBuffer {
    /// The whole lines written so far.
    fn lines(&self) -> Vec<String> {
        let b = self.0.lock().unwrap();
        let s = String::from_utf8_lossy(&b);
        match s.rfind('\n') {
            Some(i) => s[..i].split('\n').map(str::to_string).collect(),
            None => Vec::new(),
        }
    }
}

/// Waits until b holds n event lines, and returns them (Go: waitEvents).
fn wait_events(b: &SyncBuffer, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = event_lines(&b.lines());
        if events.len() >= n {
            assert_eq!(events.len(), n, "stream has {} events, want {n}:\n{}", events.len(), b.lines().join("\n"));
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {n} events on the stream:\n{}",
            b.lines().join("\n")
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A following stream a test runs (Go: follower).
struct Follower {
    out: SyncBuffer,
    cancel: agent_bus::CancelFn,
    done: mpsc::Receiver<anyhow::Result<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The intervals of a test's follow: polling every 5ms and checking an owner's
/// report every 10ms (never in full).
fn fast() -> Intervals {
    Intervals {
        poll: Duration::from_millis(5),
        session: Duration::from_millis(10),
        status: Duration::from_secs(3600),
        status_max: Duration::from_secs(5 * 60),
    }
}

/// Starts a following stream of target in env's Ctx. The stream ends once
/// reader_gone, if set, says so (Go: followStream).
fn follow_stream(env: &TestEnv, target: TranscriptTarget, reader_gone: Option<ReaderGone>) -> Follower {
    let share = env.ctx.share().ok();
    let plain_cfg = env.cfg.clone();
    let plain_store = Arc::clone(env.ctx.store().unwrap());
    let (bctx, cancel) = Context::with_cancel(&Context::background());
    let out = SyncBuffer::default();
    let (tx, done) = mpsc::channel();
    let mut w = out.clone();
    let thread = std::thread::spawn(move || {
        let ctx = match share {
            Some(s) => s.quiet_ctx(),
            None => {
                let ctx = Ctx::new(Box::new(std::io::sink()), Box::new(std::io::sink()));
                ctx.set_cfg(plain_cfg);
                ctx.set_store(plain_store);
                ctx
            }
        };
        let o = StreamOptions {
            from: -1,
            until: -1,
            follow: true,
            reader_gone,
            intervals: fast(),
            ..StreamOptions::default()
        };
        let _ = tx.send(stream_transcript(&ctx, &bctx, &target, &mut w, &o));
    });
    Follower {
        out,
        cancel,
        done,
        thread: Some(thread),
    }
}

impl Follower {
    /// Ends the stream and returns its error.
    fn stop(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        let res = self.done.recv().expect("the stream's result");
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        res
    }

    /// Waits for the stream to end by itself, and returns its error.
    fn wait(&mut self) -> anyhow::Result<()> {
        match self.done.recv_timeout(Duration::from_secs(5)) {
            Ok(res) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
                res
            }
            Err(_) => panic!("the stream didn't end:\n{}", self.out.lines().join("\n")),
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Appends data to the file at path.
fn append_file(path: &str, data: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(data).unwrap();
}

/// A transcript followed as it grows, in chunks that end mid-line, streams
/// what a whole read of it gives.
#[test]
fn test_transcript_follow() {
    let env = plain_env();
    let (src, data) = corpus_transcript("held-delivery.jsonl");
    let whole = whole_stream(&src, &data, false);
    let dir = temp_dir("agent-transcript-");
    let path = temp_path(&dir, "growing.jsonl");
    let starts = line_starts(&data);
    let first = starts[3] + 7; // mid-line: that line isn't read yet
    std::fs::write(&path, &data[..first as usize]).unwrap();
    let mut f = follow_stream(&env, file_target(&path, first), None);

    let up_to = |at: i64| whole.iter().filter(|l| event_at(l) < at).count();
    wait_events(&f.out, up_to(starts[3]));
    let mut off = first as usize;
    while off < data.len() {
        let next = (off + 97).min(data.len());
        append_file(&path, &data[off..next]);
        off = next;
        let complete = if off < data.len() {
            data[..off].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) as i64
        } else {
            data.len() as i64
        };
        wait_events(&f.out, up_to(complete));
    }
    f.stop().unwrap();
    let got = event_lines(&f.out.lines());
    assert_eq!(got.join("\n"), whole.join("\n"), "followed, want the whole read's");
    // Session lines say how far the stream had read.
    let sessions = session_lines(&f.out.lines());
    let last = sessions.last().unwrap();
    assert!(
        last.size > first && last.cwd == "/home/user/src/widget",
        "last session line: size {}, cwd {:?}",
        last.size,
        last.cwd
    );
}

/// A followed transcript that is removed, replaced, or rewritten in place
/// ends the stream with an error that says so.
#[test]
fn test_transcript_follow_file_changes() {
    let env = plain_env();
    let (_, data) = corpus_transcript("held-delivery.jsonl");
    let starts = line_starts(&data);
    type Change = Box<dyn Fn(&str)>;
    let changes: Vec<(&str, Change)> = vec![
        (
            "was removed",
            Box::new(|path: &str| {
                std::fs::remove_file(path).unwrap();
            }),
        ),
        (
            "was replaced by another file",
            Box::new({
                let data = data.clone();
                move |path: &str| {
                    let other = format!("{path}.new");
                    std::fs::write(&other, &data).unwrap();
                    std::fs::rename(&other, path).unwrap();
                }
            }),
        ),
        (
            "shrank",
            Box::new({
                let at = starts[2];
                move |path: &str| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_len(at as u64)
                        .unwrap();
                }
            }),
        ),
        // Truncated and grown past where it was between two polls: the last
        // bytes read are other bytes now.
        (
            "was rewritten",
            Box::new({
                let data = data.clone();
                let (a, b) = (starts[1] as usize, starts[2] as usize);
                move |path: &str| {
                    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                    f.write_all_at(b"XXXX", data.len() as u64 - 10).unwrap();
                    f.write_all_at(&data[a..b], data.len() as u64).unwrap();
                }
            }),
        ),
    ];
    for (name, change) in changes {
        let dir = temp_dir("agent-transcript-");
        let path = temp_path(&dir, "t.jsonl");
        std::fs::write(&path, &data).unwrap();
        let mut f = follow_stream(&env, file_target(&path, data.len() as i64), None);
        wait_events(&f.out, whole_stream(&path, &data, false).len());
        change(&path);
        let err = f.wait().expect_err("the stream ends with an error");
        assert!(format!("{err:#}").contains(name), "{name}: {err:#}");
    }
}

/// The parser's values replace what the opening line had from agent sessions'
/// read of the head and tail, and a known value is never emptied.
#[test]
fn test_transcript_session_meta() {
    let env = plain_env();
    let (path, data) = corpus_transcript("requests.jsonl");
    let mut target = file_target(&path, data.len() as i64);
    target.session.title = "Spin's return".into();
    target.session.model = "older-model".into();
    target.session.project = "stale".into();
    let mut out = Vec::new();
    stream_transcript(&env.ctx, &Context::background(), &target, &mut out, &opts(-1, -1)).unwrap();
    let out = String::from_utf8(out).unwrap();
    let sessions = session_lines(&out.trim().split('\n').map(str::to_string).collect::<Vec<_>>());
    assert_eq!(sessions.len(), 2, "want the opening line and the parser's:\n{out}");
    let s = &sessions[1];
    assert!(
        s.title == "Spin's return"
            && s.model == "claude-opus-5-5"
            && s.project == "widget"
            && s.git_branch == "main"
            && s.size == data.len() as i64,
        "after the read: {s:?}"
    );
    // A window reads part of the transcript: its line stays as it opened.
    let mut window = Vec::new();
    stream_transcript(
        &env.ctx,
        &Context::background(),
        &target,
        &mut window,
        &opts(-1, line_starts(&data)[5]),
    )
    .unwrap();
    let window = String::from_utf8(window).unwrap();
    assert_eq!(window.matches(r#""type":"session""#).count(), 1, "a window's session lines:\n{window}");
}

// held-delivery's turn, and its two final replies.
const HELD_TURN: &str = "389c7d4d-b2a5-5fba-8c0a-ad513ef44a69";
const HELD_FIRST: &str = "Two TODOs: the port should come from the config (cmd/widget/main.go:22), and cmd/legacy/run.go is marked for deletion after v2.";
const HELD_SECOND: &str = "Understood: the only TODO left to act on is reading the port from the config.";

/// The final replies among a stream's event lines (Go: finalSays).
fn final_says(events: &[String]) -> Vec<Event> {
    events
        .iter()
        .map(|l| serde_json::from_str::<Event>(l).unwrap())
        .filter(|e| e.say.as_ref().is_some_and(|s| s.r#final))
        .collect()
}

/// "<id> <op> <the text's start>" of each final reply (Go: finalSayOps).
fn final_say_ops(events: &[String]) -> Vec<String> {
    final_says(events)
        .iter()
        .map(|e| format!("{} {} {}", e.id, e.op, &e.say.as_ref().unwrap().text[..10]))
        .collect()
}

/// Where the line holding text starts in data.
fn line_of(data: &[u8], text: &str) -> usize {
    let i = data.windows(text.len()).position(|w| w == text.as_bytes()).unwrap();
    data[..i].iter().rposition(|&b| b == b'\n').map_or(0, |j| j + 1)
}

fn stop_payload(path: &str, text: &str) -> Vec<u8> {
    serde_json::json!({"hook_event_name": "Stop", "transcript_path": path, "last_assistant_message": text})
        .to_string()
        .into_bytes()
}

/// The real held delivery: the Stop that hands over a held message empties
/// _result, as the turn goes on, so the first final reply has no provisional
/// copy; the second Stop's reply is the turn's second.
#[test]
fn test_transcript_held_delivery() {
    crate::require_termd!();
    let (env, _home) = setup_sessions();
    let (_, data) = corpus_transcript("held-delivery.jsonl");
    let dir = temp_dir("agent-transcript-");
    let path = temp_path(&dir, "held.jsonl");
    let first_at = line_of(&data, HELD_FIRST);
    let second_at = line_of(&data, HELD_SECOND);
    std::fs::write(&path, &data[..first_at]).unwrap();
    hook_transcript(&env, &path);
    let target = resolve_transcript(&env.ctx, "worker").unwrap();
    let mut f = follow_stream(&env, target, None);
    wait_events(&f.out, whole_stream(&path, &data[..first_at], false).len());

    let mut msg = Message {
        sender_id: "lead".into(),
        receiver_id: "worker".into(),
        content: "[from lead@h] only the port".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        ..Default::default()
    };
    env.store().create_message(&mut msg).unwrap();
    let stop = |text: &str| -> Option<Vec<u8>> {
        let (out, res) = env.service().handle_hook("worker", "", "", &stop_payload(&path, text));
        res.unwrap();
        out
    };
    let out = stop(HELD_FIRST).unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&out).contains("only the port"),
        "the first Stop delivered {:?}, want the held message",
        String::from_utf8_lossy(&out)
    );
    append_file(&path, &data[first_at..second_at]);
    let n = wait_events(&f.out, whole_stream(&path, &data[..second_at], false).len()).len();
    let out = stop(HELD_SECOND);
    assert!(out.is_none(), "the second Stop delivered {out:?}");
    wait_events(&f.out, n + 1);
    append_file(&path, &data[second_at..]);
    let events = wait_events(&f.out, whole_stream(&path, &data, false).len() + 1);
    f.stop().unwrap();
    let first = final_says(&whole_stream(&path, &data, false))[0].id.clone();
    let want = [
        format!("{first} add Two TODOs:"),
        format!("{HELD_TURN}#final2 add Understood"),
        format!("{HELD_TURN}#final2 update Understood"),
    ];
    assert_eq!(final_say_ops(&events), want, "final replies");
}

/// A workgroup Claude agent's Stop sends its reply before the transcript holds
/// it; the transcript's copy updates it.
#[test]
fn test_transcript_provisional() {
    crate::require_termd!();
    let (env, _home) = setup_sessions();
    let (_, data) = corpus_transcript("held-delivery.jsonl");
    let dir = temp_dir("agent-transcript-");
    let path = temp_path(&dir, "held.jsonl");
    let first_at = line_of(&data, HELD_FIRST);
    let second_at = line_of(&data, HELD_SECOND);
    std::fs::write(&path, &data[..first_at]).unwrap();
    hook_transcript(&env, &path);

    let target = resolve_transcript(&env.ctx, "worker").unwrap();
    assert!(
        same_path(&target.path, &path) && target.session.workgroup.as_ref().is_some_and(|w| w.name == "worker"),
        "worker's target: {}, {:?}",
        target.path,
        target.session.workgroup
    );
    let mut f = follow_stream(&env, target, None);
    let n = wait_events(&f.out, whole_stream(&path, &data[..first_at], false).len()).len();

    let stop_hook = |text: &str| {
        env.service()
            .report_hook("worker", "", "", &stop_payload(&path, text))
            .unwrap();
    };
    stop_hook(HELD_FIRST);
    let events = wait_events(&f.out, n + 1);
    let p = &final_says(&events)[0];
    let last: Event = serde_json::from_str(&events[n - 1]).unwrap();
    assert!(
        p.id == format!("{HELD_TURN}#final")
            && p.say.as_ref().unwrap().provisional
            && p.at == first_at as i64
            && p.seq == last.seq + 1
            && p.turn.as_ref().is_some_and(|t| t.index == 1),
        "provisional reply {}",
        events[n]
    );

    append_file(&path, &data[first_at..second_at]);
    let n = wait_events(&f.out, whole_stream(&path, &data[..second_at], false).len() + 1).len();
    stop_hook(HELD_SECOND);
    wait_events(&f.out, n + 1);
    append_file(&path, &data[second_at..]);
    let events = wait_events(&f.out, whole_stream(&path, &data, false).len() + 2);
    // The Stop's report reaches the session line by the report check.
    let deadline = Instant::now() + Duration::from_secs(10);
    while session_lines(&f.out.lines()).last().unwrap().status != "idle" {
        assert!(
            Instant::now() < deadline,
            "no session line saying idle after the Stop:\n{}",
            f.out.lines().join("\n")
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    f.stop().unwrap();

    let want = [
        format!("{HELD_TURN}#final add Two TODOs:"),
        format!("{HELD_TURN}#final update Two TODOs:"),
        format!("{HELD_TURN}#final2 add Understood"),
        format!("{HELD_TURN}#final2 update Understood"),
    ];
    assert_eq!(final_say_ops(&events), want, "final replies");

    // Read again, the transcript holds both replies: no provisional one.
    let target = resolve_transcript(&env.ctx, "worker").unwrap();
    let (_, again) = stream_lines(&env, &target, &opts(-1, -1));
    assert_eq!(
        again.join("\n"),
        whole_stream(&path, &data, false).join("\n"),
        "a whole read after both replies landed"
    );
}

#[test]
fn test_transcript_resolve() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    let projects = format!("{home}/.claude/projects/-home-user-shop");
    for (arg, want) in [
        ("worker".to_string(), "sess-w"),
        ("sess-l".to_string(), "sess-l"),
        ("sess-w".to_string(), "sess-w"),
        (format!("{projects}/sess-l.jsonl"), "sess-l"),
    ] {
        let target = resolve_transcript(&env.ctx, &arg).unwrap_or_else(|e| panic!("{arg}: {e:#}"));
        assert!(
            target.session.session_id == want
                && same_path(&target.path, &format!("{projects}/{want}.jsonl"))
                && target.reader.harness() == "claude",
            "{arg}: session {} at {}",
            target.session.session_id,
            target.path
        );
    }
    let target = resolve_transcript(&env.ctx, "worker").unwrap();
    assert!(
        target.session.workgroup.is_some() && target.session.status == "waiting",
        "worker's session: {:?}",
        target.session
    );
    for (arg, want) in [
        ("sess-".to_string(), r#""sess-" starts the ids of 2 sessions"#),
        ("nope".to_string(), r#"no agent or session matches "nope""#),
        ("worker@hub".to_string(), "transcripts are read per host"),
        (projects.clone(), "is a directory"),
        (format!("{home}/x.jsonl"), "there is no such file"),
    ] {
        let err = resolve_transcript(&env.ctx, &arg).err().map(|e| format!("{e:#}"));
        assert!(
            err.as_deref().is_some_and(|e| e.contains(want)),
            "{arg}: {err:?}, want {want:?}"
        );
    }
    // The command's own refusals of its flags.
    let (res, _, _) = run_words(&mut env, &["transcript", "worker", "--from", "-2"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("--from -2: give the byte offset"));
    let (res, _, _) = run_words(&mut env, &["transcript", "worker", "--follow", "--until", "5"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("--until ends the read at an offset"));
    let (res, _, _) = run_words(&mut env, &["transcript", "worker", "--from", "10", "--until", "10"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("--until 10 is not after --from 10"));
}

/// A pipe's two ends.
fn pipe() -> (std::fs::File, std::fs::File) {
    let mut fds = [0; 2];
    // SAFETY: a plain pipe(2) into a two-int array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: pipe(2) just made both descriptors, owned by nothing else.
    unsafe {
        (
            std::fs::File::from(OwnedFd::from_raw_fd(fds[0])),
            std::fs::File::from(OwnedFd::from_raw_fd(fds[1])),
        )
    }
}

fn transcript_args(arg: &str, follow: bool) -> Args {
    Args {
        follow,
        from: -1,
        requests: false,
        until: -1,
        args: vec![arg.to_string()],
    }
}

/// A running session is listed with its transcript's path before the file is
/// there (Claude Code writes it at the first prompt). --follow waits for it,
/// as tail -F does, and streams it once written; a read without --follow says
/// it isn't written yet. A session that has exited with no transcript is an
/// error at once, or as soon as a wait sees it exit.
#[test]
fn test_transcript_follow_unwritten() {
    crate::require_termd!();
    let (env, home) = setup_sessions();
    let fresh = format!("{home}/.claude/projects/-home-user-shop/sess-n.jsonl");
    hook_transcript(&env, &fresh); // the worker moves on to a session not written yet
    let running = Arc::new(AtomicBool::new(true));
    let r = Arc::clone(&running);
    crate::cli::sessions::set_claude_sessions(
        &env.cfg.database_path,
        Arc::new(move || {
            let mut live = HashMap::from([("sess-l".to_string(), Status::Idle)]);
            if r.load(Ordering::SeqCst) {
                live.insert("sess-n".into(), Status::Idle);
            }
            Ok(live)
        }),
    );
    let iv = Intervals {
        poll: Duration::from_millis(5),
        status: Duration::from_millis(20),
        ..Intervals::default()
    };

    for arg in [fresh.as_str(), "sess-n", "worker"] {
        let target = resolve_transcript(&env.ctx, arg).unwrap_or_else(|e| panic!("{arg}: {e:#}"));
        assert!(
            target.unwritten && target.session.session_id == "sess-n" && target.session.status == "idle",
            "{arg}: {:?}, want sess-n's unwritten transcript, idle",
            target.session
        );
    }
    let err = run_transcript(&env.ctx, &transcript_args(&fresh, false), &mut Vec::new(), None, iv)
        .expect_err("not written, not following");
    assert!(format!("{err:#}").contains("no transcript yet"), "without --follow: {err:#}");

    // follow: run the command on a pipe; it waits, then streams the file once
    // written.
    let follow = || {
        let (r, w) = pipe();
        let out = SyncBuffer::default();
        let mut copy = out.clone();
        // The copier reads the pipe until told to close it (Go: r.Close()).
        let close = Arc::new(AtomicBool::new(false));
        let closing = Arc::clone(&close);
        // SAFETY: fcntl on a descriptor this test owns.
        unsafe {
            let fl = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(r.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let copier = std::thread::spawn(move || {
            let mut reader = r;
            let mut buf = vec![0u8; 64 << 10];
            while !closing.load(Ordering::SeqCst) {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        let _ = copy.write_all(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });
        let copier = (close, copier);
        let share = env.ctx.share().unwrap();
        let (tx, done) = mpsc::channel();
        let fresh = fresh.clone();
        std::thread::spawn(move || {
            let ctx = share.quiet_ctx();
            let fd = w.as_raw_fd();
            let gone: ReaderGone = Arc::new(move || reader_gone(fd));
            let mut w = w;
            let _ = tx.send(run_transcript(&ctx, &transcript_args(&fresh, true), &mut w, Some(gone), iv));
        });
        (out, copier, done)
    };
    let (out, copier, done) = follow();
    // A quiet period, not a wait: nothing marks the follow settling into its
    // wait for the file, so give it a moment to (wrongly) end or write.
    if let Ok(res) = done.recv_timeout(Duration::from_millis(100)) {
        panic!("--follow ended before the transcript was written: {res:?}");
    }
    assert!(out.lines().is_empty(), "wrote before the transcript was there: {:?}", out.lines());
    write_test_file(
        &fresh,
        "{\"type\":\"user\",\"sessionId\":\"sess-n\",\"cwd\":\"/home/user/shop\",\"message\":{\"role\":\"user\",\"content\":\"first\"}}\n",
    );
    let events = wait_events(&out, 1);
    assert!(events[0].contains(r#""first""#), "the first event: {}", events[0]);
    let sessions = session_lines(&out.lines());
    assert!(
        sessions.first().is_some_and(|s| s.session_id == "sess-n"),
        "the stream opens with the session line: {sessions:?}"
    );
    // The reader goes: the stream ends, cleanly.
    copier.0.store(true, Ordering::SeqCst);
    let _ = copier.1.join();
    let res = done
        .recv_timeout(Duration::from_secs(5))
        .expect("the stream ends when its reader goes");
    res.expect("a reader gone");

    // The session exits while the follow waits: the wait ends with an error.
    std::fs::remove_file(&fresh).unwrap();
    let (_out2, copier2, done2) = follow();
    running.store(false, Ordering::SeqCst);
    let res = done2.recv_timeout(Duration::from_secs(5)).expect("the wait ends when its session exits");
    let err = res.expect_err("exited while waiting");
    assert!(format!("{err:#}").contains("has ended"), "exited while waiting: {err:#}");
    copier2.0.store(true, Ordering::SeqCst);
    let _ = copier2.1.join();
    // and an exited session's missing transcript is an error at once
    let err = resolve_transcript(&env.ctx, &fresh).err().map(|e| format!("{e:#}"));
    assert!(err.as_deref().is_some_and(|e| e.contains("has ended")), "exited, no file: {err:?}");
}

/// A reader that has gone ends the stream, cleanly, though nothing is written;
/// reader_gone tells a closed pipe from an open one (Go:
/// TestTranscript_ReaderGone).
#[test]
fn test_transcript_reader_gone() {
    let env = plain_env();
    let (path, data) = corpus_transcript("held-delivery.jsonl");
    let closed = Arc::new(AtomicBool::new(false));
    let c = Arc::clone(&closed);
    let mut f = follow_stream(
        &env,
        file_target(&path, data.len() as i64),
        Some(Arc::new(move || c.load(Ordering::SeqCst))),
    );
    wait_events(&f.out, whole_stream(&path, &data, false).len());
    closed.store(true, Ordering::SeqCst);
    f.wait().unwrap();

    // Each end, before and after its reader closes. A full pipe has no room to
    // write, which macOS reports only alongside the hangup.
    let make_pipe = |full: bool| -> (std::fs::File, std::fs::File) {
        let (r, w) = pipe();
        if full {
            // SAFETY: fcntl on a descriptor this test owns.
            unsafe {
                let fl = libc::fcntl(w.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(w.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
            let buf = vec![0u8; 1 << 16];
            while (&w).write(&buf).is_ok() {}
        }
        (r, w)
    };
    let socket = || -> (std::fs::File, std::fs::File) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        (
            std::fs::File::from(OwnedFd::from(b)),
            std::fs::File::from(OwnedFd::from(a)),
        )
    };
    for (name, (reader, w)) in [("pipe", make_pipe(false)), ("full pipe", make_pipe(true)), ("socket", socket())] {
        assert!(!reader_gone(w.as_raw_fd()), "reader_gone on a {name} whose reader is open");
        drop(reader);
        assert!(reader_gone(w.as_raw_fd()), "reader_gone on a {name} whose reader closed");
    }
    // Neither has a reader to lose.
    let dir = temp_dir("agent-transcript-");
    let file = std::fs::File::create(temp_path(&dir, "out")).unwrap();
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
    for f in [&file, &null] {
        assert!(!reader_gone(f.as_raw_fd()), "reader_gone on {f:?}");
    }
}

/// A reader closing the pipe ends the command cleanly.
#[test]
fn test_transcript_closed_pipe() {
    crate::require_termd!();
    let (env, _home) = setup_sessions();
    let (path, _) = corpus_transcript("held-delivery.jsonl");
    let (r, mut w) = pipe();
    drop(r);
    let fd = w.as_raw_fd();
    run_transcript(
        &env.ctx,
        &transcript_args(&path, false),
        &mut w,
        Some(Arc::new(move || reader_gone(fd))),
        Intervals::default(),
    )
    .expect("a closed pipe ends the stream cleanly");
}

/// A subagent's session found by id is read with the sessions above it, whose
/// status its own follows (Go: TestResolveTranscript_SubagentByID, in
/// sessions_test.go).
#[test]
fn test_resolve_transcript_subagent_by_id() {
    crate::require_termd!();
    let (env, home) = setup_sessions();
    let parent = claude_transcript(&home, "sess-p", "/home/user/shop", "", Duration::from_secs(60));
    claude_subagent(&parent, "/home/user/shop", Duration::from_secs(60));
    for i in 0..5 {
        claude_transcript(
            &home,
            &format!("sess-{i:03}"),
            "/home/user/other",
            "",
            Duration::from_secs(2 * 3600 + i * 60),
        );
    }
    let target = resolve_transcript(&env.ctx, "agent-x").unwrap();
    assert!(
        target.session.session_id == "agent-x1"
            && target.above.len() == 1
            && same_path(&target.above[0], &parent)
            && target.session.status == "ended"
            && target.session.end_reason == "parent",
        "agent-x: {:?}, above {:?}; want agent-x1 under sess-p, ended (parent)",
        target.session,
        target.above
    );
}

// A reply with U+2028 in it reaches a display as Go writes it: the line
// separator escaped (a JavaScript reader would otherwise split the line
// there), and <, > and & as they are. transcript and logs --json both write
// their lines through agent_base::gostd::json.
#[test]
fn test_reply_with_line_separator_is_encoded_as_go_does() {
    crate::require_termd!();
    let mut env = crate::cli::logs::logs_test::setup_logs();
    let dir = temp_dir("agent-transcript-u2028-");
    let path = temp_path(&dir, "sess-u.jsonl");
    write_test_file(
        &path,
        "{\"type\":\"user\",\"sessionId\":\"sess-u\",\"cwd\":\"/home/user/proj\",\"timestamp\":\"2026-08-01T12:04:31Z\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
         {\"type\":\"assistant\",\"timestamp\":\"2026-08-01T12:05:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"one\\u2028two <b> & \\u2029three\"}]}}\n",
    );
    hook_transcript(&env, &path);

    let want = "\"text\":\"one\\u2028two <b> & \\u2029three\"";
    for words in [vec!["transcript", path.as_str()], vec!["logs", "worker", "--json"]] {
        let (res, out, _) = run_words(&mut env, &words);
        res.unwrap();
        let say = out
            .lines()
            .find(|l| l.contains("\"kind\":\"say\""))
            .unwrap_or_else(|| panic!("{words:?}: no say line in {out:?}"));
        assert!(say.contains(want), "{words:?}: {say}");
        assert!(!say.contains('\u{2028}') && !say.contains('\u{2029}'), "{words:?}: {say}");
    }
}
