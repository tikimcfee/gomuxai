package cli

import (
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var taskCmd = &cobra.Command{
	Use:   "task",
	Short: "Manage reusable task definitions",
	Long: `Task provides operations for managing reusable task definitions.

Tasks are standalone entities stored in the database. Multiple agents can
reference the same task. Tasks exist independently of agent lifecycle.`,
}

var taskSetCmd = &cobra.Command{
	Use:   "set <name> [content]",
	Short: "Create or update a task",
	Long: `Set creates or updates a task in the repository.

Content can be provided directly as an argument or read from a file using -f.
When using -f, the content is stored in the database (not just the path).

Examples:
  agent task set research-auth "# Research Auth\n\nAnalyze authentication patterns"
  agent task set research-auth -f tasks/research-auth.md`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runTaskSet,
}

var taskGetCmd = &cobra.Command{
	Use:   "get <name>",
	Short: "Get a task by name",
	Long: `Get retrieves a task's content by name.

Examples:
  agent task get research-auth`,
	Args: cobra.ExactArgs(1),
	RunE: runTaskGet,
}

var taskListCmd = &cobra.Command{
	Use:   "list",
	Short: "List all tasks",
	Long: `List shows all tasks in the repository.

Examples:
  agent task list`,
	Args: cobra.NoArgs,
	RunE: runTaskList,
}

var taskClearCmd = &cobra.Command{
	Use:   "clear <name>",
	Short: "Remove a task",
	Long: `Clear removes a task from the repository.

Examples:
  agent task clear research-auth`,
	Args: cobra.ExactArgs(1),
	RunE: runTaskClear,
}

var taskFile string

func init() {
	taskSetCmd.Flags().StringVarP(&taskFile, "file", "f", "", "Read content from file")

	taskCmd.AddCommand(taskSetCmd)
	taskCmd.AddCommand(taskGetCmd)
	taskCmd.AddCommand(taskListCmd)
	taskCmd.AddCommand(taskClearCmd)
}

func runTaskSet(cmd *cobra.Command, args []string) error {
	name := args[0]
	var content string
	var sourceFile string

	if taskFile != "" {
		// Read from file
		data, err := os.ReadFile(taskFile)
		if err != nil {
			return fmt.Errorf("reading file: %w", err)
		}
		content = string(data)
		sourceFile = taskFile
	} else if len(args) > 1 {
		// Content provided directly
		content = args[1]
	} else {
		return fmt.Errorf("either content or -f flag is required")
	}

	task := &storage.Task{
		Name:       name,
		Content:    content,
		SourceFile: sourceFile,
		CreatedAt:  time.Now(),
	}

	if err := store.SetTask(getContext(), task); err != nil {
		return err
	}

	fmt.Printf("Task %q saved\n", name)
	return nil
}

func runTaskGet(cmd *cobra.Command, args []string) error {
	name := args[0]

	task, err := store.GetTask(getContext(), name)
	if err != nil {
		return err
	}

	if task == nil {
		fmt.Printf("Task %q not found\n", name)
		return nil
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

	if len(tasks) == 0 {
		fmt.Println("No tasks found")
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

	if err := store.DeleteTask(getContext(), name); err != nil {
		return err
	}

	fmt.Printf("Task %q removed\n", name)
	return nil
}
