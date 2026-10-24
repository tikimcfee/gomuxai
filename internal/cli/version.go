package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/buildinfo"
)

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print which build of agent this is",
	Long: `Version prints one line: the module version ("devel" unless built from a
tag), the VCS revision ("-dirty" if built with uncommitted changes), the
commit time, the Go version, and the platform. Builds without VCS info (e.g.
go test, or outside the checkout) say "(no VCS info)". "agent --version"
prints the same line.`,
	Example: `  agent version
  agent --version`,
	Args: cobra.NoArgs,
	// Needs no database: it must work even when nothing else does.
	PersistentPreRunE: func(*cobra.Command, []string) error { return nil },
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Fprintln(cmd.OutOrStdout(), versionLine())
	},
}

func versionLine() string {
	return "agent " + buildinfo.Current().String()
}

func init() {
	// cobra's own --version flag would take -v, which status uses for verbose.
	rootCmd.Flags().Bool("version", false, "Print which build of agent this is")
	rootCmd.Version = versionLine()
	rootCmd.SetVersionTemplate("{{.Version}}\n")
}
