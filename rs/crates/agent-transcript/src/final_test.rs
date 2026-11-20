use agent_base::gostd::filepath;

use crate::transcript_test::summaries;
use crate::{Claude, Event, OP_ADD, OP_UPDATE, Options, Reader, latest, new_finals};

fn held_delivery() -> Vec<u8> {
    let path = filepath::join(&[
        env!("CARGO_MANIFEST_DIR"),
        "testdata",
        "corpus",
        "claude",
        "held-delivery.jsonl",
    ]);
    std::fs::read(path).expect("read held-delivery.jsonl")
}

fn line_start(data: &[u8], s: &str) -> usize {
    let needle = s.as_bytes();
    let i = data
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("no {s:?}"));
    data[..i]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|nl| nl + 1)
        .unwrap_or(0)
}

const HELD_TURN: &str = "389c7d4d-b2a5-5fba-8c0a-ad513ef44a69";
const HELD_FIRST: &str = "Two TODOs: the port should come from the config (cmd/widget/main.go:22), and cmd/legacy/run.go is marked for deletion after v2.";
const HELD_SECOND: &str =
    "Understood: the only TODO left to act on is reading the port from the config.";

#[test]
fn test_finals() {
    let data = held_delivery();
    let mut p = Claude.new_parser(Options::default());
    let mut f = new_finals();
    let mut stream: Vec<Event> = Vec::new();

    let feed =
        |p: &mut crate::Parser, f: &mut crate::Finals, stream: &mut Vec<Event>, up_to: usize| {
            let start = p.pos().at as usize;
            for e in p.feed(&data[start..up_to]) {
                stream.push(f.merge(e));
            }
        };
    let stop = |p: &crate::Parser,
                f: &mut crate::Finals,
                stream: &mut Vec<Event>,
                text: &str,
                ts: &str| {
        let e = f
            .provisional(HELD_TURN, text, ts, p.pos())
            .unwrap_or_else(|| panic!("no provisional reply for {text:?}"));
        stream.push(e);
    };

    feed(&mut p, &mut f, &mut stream, line_start(&data, HELD_FIRST));
    stop(
        &p,
        &mut f,
        &mut stream,
        HELD_FIRST,
        "2026-10-20T11:00:06.050Z",
    );
    feed(&mut p, &mut f, &mut stream, line_start(&data, HELD_SECOND));
    stop(
        &p,
        &mut f,
        &mut stream,
        HELD_SECOND,
        "2026-10-20T11:00:07.950Z",
    );
    feed(&mut p, &mut f, &mut stream, data.len());

    let says: Vec<&Event> = stream.iter().filter(|e| e.say.is_some()).collect();
    let want = [
        (format!("{HELD_TURN}#final"), OP_ADD, HELD_FIRST, true),
        (format!("{HELD_TURN}#final"), OP_UPDATE, HELD_FIRST, false),
        (format!("{HELD_TURN}#final2"), OP_ADD, HELD_SECOND, true),
        (format!("{HELD_TURN}#final2"), OP_UPDATE, HELD_SECOND, false),
    ];
    assert_eq!(says.len(), want.len(), "says: {:?}", summaries(&stream));
    for (i, (id, op, text, provisional)) in want.iter().enumerate() {
        let e = says[i];
        let say = e.say.as_ref().unwrap();
        assert!(
            &e.id == id
                && e.op == *op
                && say.text == *text
                && say.provisional == *provisional
                && say.r#final,
            "say {i} = {} {} {say:?}, want {id} {op} {text:?} provisional {provisional}",
            e.id,
            e.op
        );
        let turn = e.turn.as_ref().expect("turn");
        assert!(
            turn.id == HELD_TURN && turn.index == 1,
            "say {i} turn = {turn:?}"
        );
    }
    let u = says[1];
    assert_eq!(u.line, 5);
    assert_eq!(u.at, line_start(&data, HELD_FIRST) as i64);
    assert_eq!(u.ts, "2026-10-20T11:00:06.000Z");

    let folded = latest(&stream);
    assert_eq!(
        folded.len(),
        8,
        "latest has {} events, want 8: {:?}",
        folded.len(),
        summaries(&folded)
    );
}

#[test]
fn test_finals_transcript_first() {
    let data = held_delivery();
    let mut p = Claude.new_parser(Options::default());
    let mut f = new_finals();
    let end = line_start(&data, HELD_SECOND);
    let mut stream = Vec::new();
    for e in p.feed(&data[..end]) {
        stream.push(f.merge(e));
    }
    assert!(
        f.provisional(HELD_TURN, HELD_FIRST, "2026-10-20T11:00:06.050Z", p.pos())
            .is_none(),
        "a provisional reply for a reply the transcript holds"
    );
    for e in &stream {
        assert!(
            !(e.op == OP_UPDATE && e.say.is_some()),
            "an update with no provisional reply: {e:?}"
        );
    }
    assert!(
        f.provisional(HELD_TURN, "", "", p.pos()).is_none(),
        "a provisional reply with no text"
    );
}

#[test]
fn test_finals_copy_not_final() {
    for (stop, agent) in [
        ("", ""),
        ("", r#""agentId":"s1","#),
        (r#","stop_reason":"max_tokens""#, ""),
    ] {
        let lines = [
            format!(
                r#"{{"type":"user","uuid":"u1",{agent}"promptId":"p1","timestamp":"2026-10-20T11:00:00Z","message":{{"role":"user","content":"go"}}}}"#
            ),
            format!(
                r#"{{"type":"assistant","uuid":"a1",{agent}"timestamp":"2026-10-20T11:00:01Z","message":{{"id":"m1","role":"assistant"{stop},"content":[{{"type":"text","text":"Done."}}]}}}}"#
            ),
            format!(
                r#"{{"type":"system","subtype":"turn_duration","uuid":"s1",{agent}"durationMs":1000,"timestamp":"2026-10-20T11:00:02Z"}}"#
            ),
        ];
        let mut p = Claude.new_parser(Options::default());
        let mut f = new_finals();
        let prompt = f.merge(p.feed(format!("{}\n", lines[0]).as_bytes()).remove(0));
        let turn = prompt.turn.unwrap().id;
        let e = f
            .provisional(&turn, "Done.\n", "2026-10-20T11:00:01.500Z", p.pos())
            .expect("provisional");
        assert_eq!(e.id, format!("{turn}#final"));
        let mut got = Vec::new();
        for ev in p.feed(format!("{}\n{}\n", lines[1], lines[2]).as_bytes()) {
            let ev = f.merge(ev);
            if let Some(ref say) = ev.say {
                got.push(format!("{} {} final={}", ev.id, ev.op, say.r#final));
            }
        }
        let mut want = vec![format!("{turn}#final update final=false")];
        if !agent.is_empty() {
            want.push(format!("{turn}#final update final=true"));
        }
        assert_eq!(got, want, "stop {stop:?}, {agent}");
    }
}

#[test]
fn test_finals_late_stop() {
    let data = held_delivery();
    let mut p = Claude.new_parser(Options::default());
    let mut f = new_finals();
    let mut stream = Vec::new();

    let end1 = line_start(&data, HELD_SECOND);
    for e in p.feed(&data[..end1]) {
        stream.push(f.merge(e));
    }
    assert!(
        f.provisional(
            HELD_TURN,
            &format!("{HELD_FIRST}\n"),
            "2026-10-20T11:00:06.050Z",
            p.pos()
        )
        .is_none(),
        "a provisional reply for the reply the transcript holds"
    );
    let e = f
        .provisional(HELD_TURN, HELD_SECOND, "2026-10-20T11:00:07.950Z", p.pos())
        .expect("second Stop");
    assert_eq!(e.id, format!("{HELD_TURN}#final2"));
    stream.push(e);

    let start2 = p.pos().at as usize;
    for ev in p.feed(&data[start2..]) {
        stream.push(f.merge(ev));
    }
    let got: Vec<String> = stream
        .iter()
        .filter(|ev| ev.say.as_ref().is_some_and(|s| s.r#final))
        .map(|ev| format!("{} {}", ev.id, ev.op))
        .collect();
    let want = vec![
        "583b7d2b-a4aa-52a2-9829-c8012f0f9942#0 add".to_string(),
        format!("{HELD_TURN}#final2 add"),
        format!("{HELD_TURN}#final2 update"),
    ];
    assert_eq!(got, want);
}
