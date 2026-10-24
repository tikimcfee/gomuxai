// Command agent orchestrates AI agents in tmux sessions.
package main

import (
	"os"

	"github.com/tikimcfee/gomuxai/internal/cli"
)

func main() {
	if err := cli.Execute(); err != nil {
		os.Exit(1)
	}
}
