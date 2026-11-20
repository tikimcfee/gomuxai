package natsbus

import (
	"maps"
	"slices"
	"testing"
)

// AllOps and rpcOps both list the RPC ops. The bus serves AllOps, so an op
// missing from it is never served, and one missing from rpcOps keeps the bus
// from starting (ServeRPC refuses it as unknown).
func TestAllOpsMatchesRPCOps(t *testing.T) {
	all := slices.Sorted(slices.Values(AllOps))
	served := slices.Sorted(maps.Keys(rpcOps))
	if !slices.Equal(all, served) {
		t.Errorf("AllOps = %q, rpcOps has %q: list each op in both", all, served)
	}
}
