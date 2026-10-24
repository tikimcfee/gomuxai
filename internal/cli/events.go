package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"unicode"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// busPublisher publishes this process's status events. It connects on the
// first recorded report, so commands that never report never try the bus,
// and Execute flushes it before the process exits.
var busPublisher = natsbus.NewPublisher(func() (*nats.Conn, error) { return busConnect() })

// warnPublisher reports publish failures on stderr, except for the common
// case of no bus running. Hook mode allows stderr; stdout stays clean.
type warnPublisher struct{ p *natsbus.Publisher }

func (w warnPublisher) PublishStatus(ev natsbus.StatusEvent) error {
	err := w.p.PublishStatus(ev)
	if err != nil && !errors.Is(err, natsbus.ErrNoBus) {
		fmt.Fprintf(os.Stderr, "agent: publishing status event: %v\n", err)
	}
	return err
}

// closeBusPublisher flushes buffered events, bounded by
// natsbus.DefaultFlushTimeout, so they aren't lost when the process exits.
func closeBusPublisher() {
	if err := busPublisher.Close(); err != nil {
		fmt.Fprintf(os.Stderr, "agent: %v\n", err)
	}
}

var (
	eventsHost     string
	eventsAllHosts bool
	eventsJSON     bool
)

var eventsCmd = &cobra.Command{
	Use:   "events [name[@host]]",
	Short: "Stream agent status events from the bus (runs until Ctrl-C)",
	Long: `Events prints each status event published on the agent bus, one line per
event, until interrupted. Every recorded status report (hooks, "agent report",
approve/deny) publishes one. Needs a running bus.

Events are what was reported, not the resolved status: a change only the
backend's native status shows (e.g. an interrupted permission prompt) has no
event. "agent status" stays the authority.

What it is watching is said once on stderr (-q drops it), so stdout holds
only events.`,
	Example: `  agent events                 # every agent on this host
  agent events helper
  agent events helper@air      # an agent on another host
  agent events --all-hosts --json`,
	Args: cobra.MaximumNArgs(1),
	RunE: runEvents,
}

func init() {
	eventsCmd.Flags().StringVar(&eventsHost, "host", "", "Host whose events to show (default: this host)")
	eventsCmd.Flags().BoolVar(&eventsAllHosts, "all-hosts", false, "Show events from every host")
	eventsCmd.Flags().BoolVar(&eventsJSON, "json", false, "Print each event as JSON")
}

func runEvents(cmd *cobra.Command, args []string) error {
	host, agent := cfg.Host, "*"
	if len(args) == 1 {
		ref, err := natsbus.ParseAgentRef(args[0])
		if err != nil {
			return err
		}
		agent = ref.Name
		if ref.Host != "" {
			if eventsHost != "" || eventsAllHosts {
				return fmt.Errorf("give the host once: %s, or --host/--all-hosts", args[0])
			}
			host = ref.Host
		}
	}
	switch {
	case eventsHost != "" && eventsAllHosts:
		return fmt.Errorf("use --host or --all-hosts, not both")
	case eventsAllHosts:
		host = "*"
	case eventsHost != "":
		if err := natsbus.ValidateHost(eventsHost); err != nil {
			return err
		}
		host = eventsHost
	}

	nc, err := busConnect()
	if err != nil {
		return err
	}
	defer nc.Close()

	msgs := make(chan *nats.Msg, 256)
	sub, err := nc.ChanSubscribe(natsbus.SubjectStatus(host, agent), msgs)
	if err != nil {
		return fmt.Errorf("subscribing to status events: %w", err)
	}
	defer sub.Unsubscribe()
	who := "every agent"
	if agent != "*" {
		who = "agent " + agent
	}
	where := "on host " + host
	if host == "*" {
		where = "on every host"
	}
	notice("Watching status events of %s %s. Ctrl-C stops.", who, where)

	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(interrupt)

	out := cmd.OutOrStdout()
	for {
		select {
		case <-interrupt:
			return nil
		case msg := <-msgs:
			// Events can come from other hosts: decode (which rejects
			// forged host/agent fields) and re-encode rather than echo
			// raw bytes into the terminal.
			ev, err := natsbus.DecodeStatusEvent(msg)
			if err != nil {
				fmt.Fprintf(os.Stderr, "agent events: %s\n", strconv.QuoteToGraphic(err.Error()))
				continue
			}
			if eventsJSON {
				data, _ := json.Marshal(ev)
				fmt.Fprintln(out, string(data))
				continue
			}
			fmt.Fprintln(out, formatStatusEvent(ev))
		}
	}
}

// formatStatusEvent renders ev as one line, e.g.
// "15:04:05.000 helper@air waiting event=PermissionRequest tool=Bash detail=...".
// Fields other than the (validated) host and agent are quoted if they hold
// anything but printable text, so an event can't write escape sequences to
// the terminal.
func formatStatusEvent(ev natsbus.StatusEvent) string {
	line := fmt.Sprintf("%s %s@%s %s", ev.At.Local().Format("15:04:05.000"), ev.Agent, ev.Host, printable(ev.Status))
	if ev.Event != "" {
		line += " event=" + printable(ev.Event)
	}
	if ev.Tool != "" {
		line += " tool=" + printable(ev.Tool)
	}
	if ev.Detail != "" {
		line += " detail=" + strconv.Quote(ev.Detail)
	}
	return line
}

// printable returns s, or s quoted if it has non-printable characters or spaces.
func printable(s string) string {
	for _, r := range s {
		if !unicode.IsPrint(r) || unicode.IsSpace(r) {
			return strconv.Quote(s)
		}
	}
	return s
}
