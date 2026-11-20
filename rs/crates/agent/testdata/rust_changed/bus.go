Peers may only call the ops in --remote-ops on this host (default: list,
status, capture, send, respond, friction, logs). keys, spawn and resume
stay local-only unless listed: spawn lets every linked peer start agents
here (agent spawn name@<this host>), and resume lets them start an ended
agent again (agent resume name@<this host>), which amounts to letting them
run commands
as this user (start an agent, then approve its prompts); list them only
for peers you'd give a shell. E.g. --remote-ops
list,status,capture,send,respond,friction,logs,spawn,resume.
