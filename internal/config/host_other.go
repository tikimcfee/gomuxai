//go:build !darwin

package config

// localHostName is macOS's: elsewhere the hostname is the machine's name.
func localHostName() string { return "" }
