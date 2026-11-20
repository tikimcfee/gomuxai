// Command agent-busd runs the embedded NATS server for one host's agent bus.
//
//	agent-busd --dir <bus dir> --host <name> [--client-port N] [--listen addr:port] [--remote-ops a,b,c] [--no-stdio] [--parent-pid N]
//
// When ready, it atomically writes busd.links and busd.ready in --dir, and
// (unless --no-stdio is given) prints one JSON line on stdout:
//
//	{"pid": N, "client_url": "...", "leaf_addr": "..."}
//
// On stdin/stdout (unless --no-stdio) it serves `links\n` -> `{"linked": N}\n`,
// keeps busd.links current whenever the link count changes, and runs until
// SIGINT, SIGTERM, stdin EOF, or parent exit, removing bus.url, busd.ready and
// busd.links on exit.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
)

const disclaimedEnv = "GOMUX_BUS_DISCLAIMED"

type readyReply struct {
	PID       int    `json:"pid,omitempty"`
	ClientURL string `json:"client_url"`
	LeafAddr  string `json:"leaf_addr"`
}

type linksReply struct {
	Linked int `json:"linked"`
}

type errorReply struct {
	Error string `json:"error"`
}

func main() {
	os.Exit(run(os.Args[1:], os.Stdin, os.Stdout, os.Stderr))
}

func run(args []string, stdin io.Reader, stdout, stderr io.Writer) int {
	cfg, noStdio, parentPID, fs, err := parseFlags(args)
	if errors.Is(err, flag.ErrHelp) {
		fs.SetOutput(stdout)
		fmt.Fprintln(stdout, "Usage: agent-busd --dir <bus dir> --host <name> [--client-port N] [--listen addr:port] [--remote-ops a,b,c] [--no-stdio] [--parent-pid N]")
		fs.PrintDefaults()
		return 0
	}
	reportErr := func(err error) {
		if noStdio && cfg.Dir != "" {
			if werr := writeAtomicFile(cfg.Dir, busErrFile, []byte(err.Error()+"\n")); werr == nil {
				return
			}
		}
		fmt.Fprintln(stderr, err)
	}
	if err != nil {
		reportErr(err)
		return 1
	}

	if parentPID <= 0 {
		parentPID = os.Getppid()
	}

	_ = os.Remove(filepath.Join(cfg.Dir, busErrFile))
	_ = os.Remove(filepath.Join(cfg.Dir, busReadyFile))
	_ = os.Remove(filepath.Join(cfg.Dir, busLinksFile))

	d, err := StartDaemon(cfg)
	if err != nil {
		reportErr(err)
		return 1
	}
	defer d.Shutdown()

	initialLinked := d.LinkedPeers()
	if err := writeLinksFile(cfg.Dir, initialLinked); err != nil {
		reportErr(fmt.Errorf("recording bus links: %w", err))
		return 1
	}

	ready := readyReply{
		PID:       os.Getpid(),
		ClientURL: d.ClientURL(),
		LeafAddr:  d.LeafAddr(),
	}
	readyBytes, err := json.Marshal(ready)
	if err != nil {
		reportErr(err)
		return 1
	}
	if err := writeAtomicFile(cfg.Dir, busReadyFile, append(readyBytes, '\n')); err != nil {
		reportErr(fmt.Errorf("recording bus ready state: %w", err))
		return 1
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	watchBusParent(ctx, cancel, parentPID, stderr)
	go keepLinksFileCurrent(ctx, cfg.Dir, d, initialLinked)

	if !noStdio {
		var outMu sync.Mutex
		writeJSONLine := func(v any) error {
			b, err := json.Marshal(v)
			if err != nil {
				return err
			}
			outMu.Lock()
			defer outMu.Unlock()
			_, err = stdout.Write(append(b, '\n'))
			return err
		}
		if err := writeJSONLine(ready); err != nil {
			fmt.Fprintln(stderr, err)
			return 1
		}
		go func() {
			serveStdin(stdin, d, writeJSONLine)
			cancel()
		}()
	}

	<-ctx.Done()
	return 0
}

func writeLinksFile(dir string, linked int) error {
	b, err := json.Marshal(linksReply{Linked: linked})
	if err != nil {
		return err
	}
	return writeAtomicFile(dir, busLinksFile, append(b, '\n'))
}

func writeAtomicFile(dir, name string, data []byte) error {
	tmp := filepath.Join(dir, fmt.Sprintf(".%s.%d.tmp", name, os.Getpid()))
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	if err := os.Rename(tmp, filepath.Join(dir, name)); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return nil
}

func keepLinksFileCurrent(ctx context.Context, dir string, d *Daemon, initial int) {
	last := initial
	t := time.NewTicker(50 * time.Millisecond)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			if n := d.LinkedPeers(); n != last {
				if writeLinksFile(dir, n) == nil {
					last = n
				}
			}
		}
	}
}

// watchBusParent cancels ctx if the parent process that spawned busd goes away
// (e.g. killed with SIGKILL), so busd never outlives its parent holding ports.
func watchBusParent(ctx context.Context, cancel context.CancelFunc, parent int, out io.Writer) {
	if parent <= 1 {
		return
	}
	if os.Getppid() != parent {
		if os.Getenv(disclaimedEnv) != "" {
			fmt.Fprintln(out, "agent bus: parent process exited")
		}
		cancel()
		return
	}
	go func() {
		t := time.NewTicker(200 * time.Millisecond)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				if os.Getppid() != parent {
					if os.Getenv(disclaimedEnv) != "" {
						fmt.Fprintln(out, "agent bus: parent process exited")
					}
					cancel()
					return
				}
			}
		}
	}()
}

func serveStdin(stdin io.Reader, d *Daemon, writeJSONLine func(any) error) {
	sc := bufio.NewScanner(stdin)
	for sc.Scan() {
		switch strings.TrimSpace(sc.Text()) {
		case "":
		case "links":
			_ = writeJSONLine(linksReply{Linked: d.LinkedPeers()})
		default:
			_ = writeJSONLine(errorReply{Error: "unknown command"})
		}
	}
}

func parseFlags(args []string) (DaemonConfig, bool, int, *flag.FlagSet, error) {
	fs := flag.NewFlagSet("agent-busd", flag.ContinueOnError)
	fs.SetOutput(io.Discard)

	var cfg DaemonConfig
	var noStdio bool
	var parentPID int
	fs.StringVar(&cfg.Dir, "dir", "", "bus directory (identity, peers, runtime files)")
	fs.StringVar(&cfg.Host, "host", "", "this host's name")
	fs.IntVar(&cfg.ClientPort, "client-port", DefaultClientPort, "local client port on 127.0.0.1 (-1 for random)")
	fs.StringVar(&cfg.LeafListen, "listen", "", "accept peer links on addr:port")
	fs.BoolVar(&noStdio, "no-stdio", false, "communicate via files in --dir instead of stdin/stdout")
	fs.IntVar(&parentPID, "parent-pid", 0, "exit if os.Getppid() differs from this PID")
	fs.Func("remote-ops", "comma-separated ops peers may call on this host", func(s string) error {
		if cfg.RemoteOps == nil {
			cfg.RemoteOps = []string{}
		}
		if s == "" {
			return nil
		}
		cfg.RemoteOps = append(cfg.RemoteOps, strings.Split(s, ",")...)
		return nil
	})

	if err := fs.Parse(args); err != nil {
		return cfg, noStdio, parentPID, fs, err
	}
	if fs.NArg() > 0 {
		return cfg, noStdio, parentPID, fs, fmt.Errorf("unexpected argument %q", fs.Arg(0))
	}
	if cfg.Dir == "" {
		return cfg, noStdio, parentPID, fs, fmt.Errorf("--dir is required")
	}
	if cfg.Host == "" {
		return cfg, noStdio, parentPID, fs, fmt.Errorf("--host is required")
	}
	return cfg, noStdio, parentPID, fs, nil
}
