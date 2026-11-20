package natsbus_test

import (
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
)

func TestParseAgentRef(t *testing.T) {
	tests := []struct {
		in      string
		want    natsbus.AgentRef
		wantErr bool
	}{
		{in: "helper", want: natsbus.AgentRef{Name: "helper"}},
		{in: "helper@air-1", want: natsbus.AgentRef{Name: "helper", Host: "air-1"}},
		{in: "helper@", wantErr: true},
		{in: "@air", wantErr: true},
		{in: "a.b@air", wantErr: true},
		{in: "helper@air.local", wantErr: true}, // dots would split the subject
		{in: "helper@*", wantErr: true},
	}
	for _, tt := range tests {
		got, err := natsbus.ParseAgentRef(tt.in)
		if (err != nil) != tt.wantErr {
			t.Errorf("ParseAgentRef(%q) err = %v, wantErr %v", tt.in, err, tt.wantErr)
			continue
		}
		if !tt.wantErr && got != tt.want {
			t.Errorf("ParseAgentRef(%q) = %+v, want %+v", tt.in, got, tt.want)
		}
		if !tt.wantErr && got.String() != tt.in {
			t.Errorf("String() = %q, want %q", got.String(), tt.in)
		}
	}
}

func TestSubjects(t *testing.T) {
	if got := natsbus.SubjectRPC("air", natsbus.OpStatus); got != "gomux.air.rpc.status" {
		t.Errorf("SubjectRPC = %q", got)
	}
	if got := natsbus.SubjectStatus("air", "helper"); got != "gomux.air.agent.helper.status" {
		t.Errorf("SubjectStatus = %q", got)
	}
	if got := natsbus.SubjectAllStatus("*"); got != "gomux.*.agent.*.status" {
		t.Errorf("SubjectAllStatus = %q", got)
	}
}

func TestNatstestRoundTrip(t *testing.T) {
	nc := natstest.Start(t)
	other := natstest.Connect(t, nc.ConnectedUrl())

	sub, err := other.SubscribeSync(natsbus.SubjectAllStatus("*"))
	if err != nil {
		t.Fatal(err)
	}
	other.Flush()
	if err := nc.Publish(natsbus.SubjectStatus("air", "helper"), []byte(`{}`)); err != nil {
		t.Fatal(err)
	}
	if _, err := sub.NextMsg(2 * time.Second); err != nil {
		t.Fatalf("event not received: %v", err)
	}
}
