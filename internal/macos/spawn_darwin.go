//go:build darwin && cgo

package macos

/*
#include <signal.h>
#include <spawn.h>
#include <stdlib.h>

// Private libSystem call, available since macOS 10.14 and used by Chromium
// and iTerm2 for the same purpose: the spawned process becomes its own
// responsible process.
int responsibility_spawnattrs_setdisclaim(posix_spawnattr_t *attrs, int disclaim);

static int spawn_disclaimed(pid_t *pid, const char *path, char *const argv[], char *const envp[]) {
	posix_spawnattr_t attr;
	int r = posix_spawnattr_init(&attr);
	if (r != 0) {
		return r;
	}
	// The child inherits the calling thread's signal mask and any ignored
	// signals; start it with neither, as exec from a shell would.
	sigset_t none, defaults;
	sigemptyset(&none);
	sigemptyset(&defaults);
	sigaddset(&defaults, SIGHUP);
	sigaddset(&defaults, SIGINT);
	sigaddset(&defaults, SIGQUIT);
	sigaddset(&defaults, SIGTERM);
	sigaddset(&defaults, SIGPIPE);
	posix_spawnattr_setsigmask(&attr, &none);
	posix_spawnattr_setsigdefault(&attr, &defaults);
	r = posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);
	if (r == 0) {
		r = responsibility_spawnattrs_setdisclaim(&attr, 1);
	}
	if (r == 0) {
		r = posix_spawn(pid, path, NULL, &attr, argv, envp);
	}
	posix_spawnattr_destroy(&attr);
	return r;
}
*/
import "C"

import (
	"fmt"
	"syscall"
	"unsafe"
)

// Supported reports whether SpawnDisclaimed works in this build.
const Supported = true

// SpawnDisclaimed starts path with argv and env as its own responsible
// process and returns its pid. The child inherits stdin, stdout, stderr,
// and the caller's process group (so a terminal's Ctrl-C reaches it), but
// none of Go's other file descriptors, which are close-on-exec.
func SpawnDisclaimed(path string, argv, env []string) (int, error) {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))
	cArgv := cStrings(argv)
	defer freeCStrings(cArgv)
	cEnv := cStrings(env)
	defer freeCStrings(cEnv)

	var pid C.pid_t
	if r := C.spawn_disclaimed(&pid, cPath, &cArgv[0], &cEnv[0]); r != 0 {
		return 0, fmt.Errorf("spawning %s: %w", path, syscall.Errno(r))
	}
	return int(pid), nil
}

// cStrings converts ss to a NULL-terminated C string array.
func cStrings(ss []string) []*C.char {
	out := make([]*C.char, len(ss)+1)
	for i, s := range ss {
		out[i] = C.CString(s)
	}
	return out
}

func freeCStrings(cs []*C.char) {
	for _, c := range cs {
		if c != nil {
			C.free(unsafe.Pointer(c))
		}
	}
}
