package cli

import (
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

var taskCmd = &cobra.Command{
	Use:   "task",
	Short: "Save reusable tasks, to spawn agents with",
	Long: `Task manages reusable task definitions, stored in this host's database.

Tasks exist independently of agents: several agents can be spawned with the
same task ("agent spawn <name> --task <task>" sends its content as the
agent's first message).`,
	Example: `  agent task set review-auth -f tasks/review-auth.md
  agent spawn reviewer --backend claude --task review-auth`,
	RunE: runGroup,
}

var taskSetCmd = &cobra.Command{
	Use:   "set <name> [content]",
	Short: "Create or update a task",
	Long: `Set creates or updates a task in the repository.

Content can be provided directly as an argument or read from a file using -f.
When using -f, the content is stored in the database (not just the path).`,
	Example: `  agent task set research-auth "Analyze the authentication patterns in internal/auth"
  agent task set research-auth -f tasks/research-auth.md`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runTaskSet,
}

var taskGetCmd = &cobra.Command{
	Use:   "get <name>",
	Short: "Print a task's content",
	Long:  `Get prints a task's content. It exits 1 if there is no such task.`,
	Example: `  agent task get research-auth
  agent task get research-auth --json`,
	Args: cobra.ExactArgs(1),
	RunE: runTaskGet,
}

var taskListCmd = &cobra.Command{
	Use:     "list",
	Short:   "List all tasks",
	Example: `  agent task list`,
	Args:    cobra.NoArgs,
	RunE:    runTaskList,
}

var taskClearCmd = &cobra.Command{
	Use:   "clear <name>",
	Short: "Remove a task",
	Long: `Clear removes a task from the repository. It exits 1 if there is no such
task.`,
	Example: `  agent task clear research-auth`,
	Args:    cobra.ExactArgs(1),
	RunE:    runTaskClear,
}

var (
	taskSetFile string
	taskJSON    bool
)

func init() {
	taskSetCmd.Flags().StringVarP(&taskSetFile, "file", "f", "", "Read content from file")
	for _, c := range []*cobra.Command{taskGetCmd, taskListCmd} {
		c.Flags().BoolVar(&taskJSON, "json", false, "Print as JSON")
	}

	taskCmd.AddCommand(taskSetCmd)
	taskCmd.AddCommand(taskGetCmd)
	taskCmd.AddCommand(taskListCmd)
	taskCmd.AddCommand(taskClearCmd)
}

// taskJSONEntry is a task as --json prints it.
type taskJSONEntry struct {
	Name       string    `json:"name"`
	Content    string    `json:"content,omitempty"`
	SourceFile string    `json:"source_file,omitempty"`
	CreatedAt  time.Time `json:"created_at"`
}

func runTaskSet(cmd *cobra.Command, args []string) error {
	name := args[0]
	var content string
	var sourceFile string

	switch {
	case taskSetFile != "" && len(args) > 1:
		return usageError(cmd, fmt.Errorf("give the content or -f <file>, not both"))
	case taskSetFile != "":
		// Read from file
		data, err := os.ReadFile(taskSetFile)
		if err != nil {
			return fmt.Errorf("reading file: %w", err)
		}
		content = string(data)
		sourceFile = taskSetFile
	case len(args) > 1:
		// Content provided directly
		content = args[1]
	default:
		return usageError(cmd, fmt.Errorf("task %s needs content: as an argument, or from a file with -f", name))
	}

	ctx := getContext()
	existing, err := store.GetTask(ctx, name)
	if err != nil {
		return err
	}
	task := &storage.Task{
		Name:       name,
		Content:    content,
		SourceFile: sourceFile,
		CreatedAt:  time.Now(),
	}

	if err := store.SetTask(ctx, task); err != nil {
		return err
	}

	verb := "Saved"
	if existing != nil {
		verb = "Updated"
	}
	from := "inline"
	if sourceFile != "" {
		from = "from " + sourceFile
	}
	fmt.Printf("%s task %s (%s, %s).\n", verb, name, plural(strings.Count(content, "\n")+1, "line"), from)
	return nil
}

// getTask returns the named task, or an error that suggests close names.
func getTask(name string) (*storage.Task, error) {
	ctx := getContext()
	task, err := store.GetTask(ctx, name)
	if err != nil || task != nil {
		return task, err
	}
	tasks, err := store.ListTasks(ctx)
	if err != nil {
		return nil, fmt.Errorf("no task %q", name)
	}
	names := make([]string, len(tasks))
	for i, t := range tasks {
		names[i] = t.Name
	}
	if similar := util.Similar(name, names); len(similar) > 0 {
		return nil, fmt.Errorf("no task %q; did you mean %s?", name, util.OrList(similar, nil))
	}
	if len(tasks) == 0 {
		return nil, fmt.Errorf("no task %q: there are no tasks yet (agent task set <name> -f <file> adds one)", name)
	}
	return nil, fmt.Errorf("no task %q (%s: agent task list)", name, plural(len(tasks), "task"))
}

func runTaskGet(cmd *cobra.Command, args []string) error {
	task, err := getTask(args[0])
	if err != nil {
		return err
	}

	if taskJSON {
		return printJSON(taskJSONEntry{Name: task.Name, Content: task.Content, SourceFile: task.SourceFile, CreatedAt: task.CreatedAt})
	}
	fmt.Print(task.Content)
	if task.Content != "" && task.Content[len(task.Content)-1] != '\n' {
		fmt.Println()
	}
	return nil
}

func runTaskList(cmd *cobra.Command, args []string) error {
	tasks, err := store.ListTasks(getContext())
	if err != nil {
		return err
	}

	if taskJSON {
		out := make([]taskJSONEntry, 0, len(tasks))
		for _, t := range tasks {
			out = append(out, taskJSONEntry{Name: t.Name, SourceFile: t.SourceFile, CreatedAt: t.CreatedAt})
		}
		return printJSON(out)
	}

	if len(tasks) == 0 {
		fmt.Println("No tasks yet (agent task set <name> -f <file> adds one).")
		return nil
	}

	fmt.Printf("%-25s %-30s %s\n", "NAME", "SOURCE", "CREATED")
	fmt.Println("--------------------------------------------------------------------------------")

	for _, task := range tasks {
		source := task.SourceFile
		if source == "" {
			source = "(inline)"
		} else if len(source) > 30 {
			source = "..." + source[len(source)-27:]
		}
		fmt.Printf("%-25s %-30s %s\n", task.Name, source, task.CreatedAt.Format(time.RFC3339))
	}

	return nil
}

func runTaskClear(cmd *cobra.Command, args []string) error {
	name := args[0]
	if _, err := getTask(name); err != nil {
		return err
	}

	if err := store.DeleteTask(getContext(), name); err != nil {
		return err
	}

	fmt.Printf("Removed task %s.\n", name)
	return nil
}
