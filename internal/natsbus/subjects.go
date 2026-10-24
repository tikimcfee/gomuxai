package natsbus

import "fmt"

// Subject hierarchy:
//
// agent.{name}.event.{type}     -- agent lifecycle events (JetStream, persisted)
//   agent.researcher.event.spawned
//   agent.researcher.event.status     -- status changed (idle/processing/error)
//   agent.researcher.event.done       -- agent signaled completion
//   agent.researcher.event.request    -- agent is requesting help
//   agent.researcher.event.message    -- message delivered to agent
//
// agent.{name}.inbox              -- direct messages to an agent (JetStream, persisted)
//
// agent.{name}.state.{key}       -- NATS KV bucket per agent (JetStream KV)
//
// system.agents                   -- global agent registry events (spawn/cleanup)

func SubjectEvent(agentName, eventType string) string {
	return fmt.Sprintf("agent.%s.event.%s", agentName, eventType)
}

func SubjectInbox(agentName string) string {
	return fmt.Sprintf("agent.%s.inbox", agentName)
}

func SubjectAllEvents() string {
	return "agent.*.event.*"
}

func SubjectAgentEvents(agentName string) string {
	return fmt.Sprintf("agent.%s.event.*", agentName)
}

const SubjectSystemAgents = "system.agents"
