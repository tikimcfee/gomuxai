// Package await is how tests wait for state: poll a condition until it
// holds, and on timeout fail saying what was awaited and what was last seen.
//
// A test that sleeps a fixed time and then asserts guesses at timing, and
// under a loaded run the guess is wrong. Waiting on the condition itself
// costs nothing when the state arrives early and only spends the timeout
// when something is really broken.
package await

import (
	"testing"
	"time"
)

// Timeout is how long Until waits. It is generous on purpose: a met
// condition returns at once, so the timeout is only spent on a failure, and
// a loaded machine can take seconds to start a shell.
const Timeout = 15 * time.Second

// Interval is how often a condition is checked.
const Interval = 20 * time.Millisecond

// Until checks cond until it reports done, for up to Timeout. cond returns
// the state it saw (a screen, a status) and whether that is what the test
// waits for. On timeout the test fails with what and the last state seen.
// It returns the state that satisfied cond.
func Until(t testing.TB, what string, cond func() (state string, done bool)) string {
	t.Helper()
	return Within(t, Timeout, what, cond)
}

// Within is Until with its own timeout, for a wait that is part of what the
// test asserts (the state must arrive within d).
func Within(t testing.TB, d time.Duration, what string, cond func() (state string, done bool)) string {
	t.Helper()
	state, ok := Poll(d, cond)
	if !ok {
		t.Fatalf("timed out after %v waiting for %s; last saw:\n%s", d, what, state)
	}
	return state
}

// Poll checks cond until it reports done or d passes, and returns the last
// state and whether cond was met. It never fails the test, for a caller
// that has its own way to report, or that may run off the test goroutine.
func Poll(d time.Duration, cond func() (state string, done bool)) (string, bool) {
	deadline := time.Now().Add(d)
	for {
		state, done := cond()
		if done {
			return state, true
		}
		if time.Now().After(deadline) {
			return state, false
		}
		time.Sleep(Interval) // the polling interval, not a guess at timing
	}
}
