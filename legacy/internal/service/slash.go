package service

import "regexp"

// slashCommand matches a CLI slash command: "/compact", "/model
// claude-fable-5-1", "/plugin:skill args". A path ("/tmp/out.txt is ready")
// has a second slash before any space, and isn't one.
var slashCommand = regexp.MustCompile(`^/[A-Za-z][A-Za-z0-9_:.-]*(?:\s|$)`)

// IsSlashCommand reports whether message is a slash command for the CLI it
// is typed into. Such a message is typed as is: a "[from ...]" prefix
// would turn it into chat, and a hook that hands over text can't run it.
func IsSlashCommand(message string) bool {
	return slashCommand.MatchString(message)
}
