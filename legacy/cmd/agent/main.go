// Command agent orchestrates AI agents in g3d-termd sessions.
package main

import (
	"os"

	"github.com/tikimcfee/gomuxai/internal/cli"
)

func main() {
	if err := cli.Execute(); err != nil {
		os.Exit(cli.ExitCode(err))
	}
}
