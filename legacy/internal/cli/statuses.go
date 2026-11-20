package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend/types"
)

var statusesCmd = &cobra.Command{
	Use:   "statuses",
	Short: "Say what each status and end reason means",
	Long: `Statuses prints every status an agent or session can have, and every end
reason a status of ended carries, with what each means. agent status,
agent list, agent wait, agent sessions and agent export all use these words;
--json prints the end reason as "end_reason".`,
	Example: `  agent statuses`,
	Args:    cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		out := cmd.OutOrStdout()
		fmt.Fprintln(out, "Statuses:")
		for _, m := range types.Statuses {
			fmt.Fprintf(out, "  %-11s %s\n", m.Name, m.Means)
		}
		fmt.Fprintln(out, "\nEnd reasons (beside ended):")
		for _, m := range types.EndReasons {
			fmt.Fprintf(out, "  %-11s %s\n", m.Name, m.Means)
		}
		return nil
	},
}
