  agent bus start --remote-ops list,status,capture,send,respond,friction,logs,resume

Flags:
      --client-port int      Local client port (127.0.0.1) (default 7420)
  -h, --help                 help for start
      --listen string        Accept peer links on this address (makes this host the hub), e.g. 0.0.0.0:7422
      --remote-ops strings   Ops peers may call on this host (default [list,status,capture,send,respond,friction,logs])
