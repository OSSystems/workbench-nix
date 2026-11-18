package main

import (
	"bufio"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/OSSystems/vmkit-nix/control"
	"github.com/OSSystems/vmkit-nix/memory"
)

const gib = memory.GiB

// Workspaces may not grow into the part of a yielding pool's guarantee
// that pool does not hold yet.
func TestWorkspacesLeavePoolsBehindTheirGuarantee(t *testing.T) {
	now := time.Date(2026, 11, 18, 12, 0, 0, 0, time.UTC)
	dir := t.TempDir()
	first, second := filepath.Join(dir, "first.json"), filepath.Join(dir, "second.json")
	var behind behindPools
	for _, v := range []string{first + "=8", second + "=1.5"} {
		if err := behind.Set(v); err != nil {
			t.Fatal(err)
		}
	}
	for _, bad := range []string{"no-guarantee", "=1", first + "=-1", first + "=x"} {
		if err := (&behindPools{}).Set(bad); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
	// Neither pool has published: both guarantees are owed in full.
	if got := behind.owed(now); got != 8*gib+3*gib/2 {
		t.Fatalf("idle pools: owed %d", got)
	}
	memory.WriteStatus(first, memory.Status{UpdatedAt: now, CommittedBytes: 5 * gib})
	memory.WriteStatus(second, memory.Status{UpdatedAt: now, CommittedBytes: 30 * gib})
	if got := behind.owed(now); got != 3*gib {
		t.Fatalf("one pool holding 5 of its 8 GiB, the other past its 1.5: owed %d", got)
	}
	// Reserve max(10% of 64 GiB, 2 GiB) = 6.4 GiB, plus what is owed.
	h := workspaceHostMemory(64*gib, 20*gib, 10, 2, gib, behind.owed(now))
	if h.Reserve != 64*gib/10+3*gib || h.Pending != gib || h.Available != 20*gib {
		t.Fatalf("host memory %+v", h)
	}
}

func TestStatsChannelRecordsOnlyStats(t *testing.T) {
	vm := &wsVM{mem: memory.NewVMMemory(2*gib, 16*gib)}
	at := time.Date(2026, 11, 17, 12, 0, 0, 0, time.UTC)
	ask := func(line string) control.Reply {
		return answerStats(bufio.NewReader(strings.NewReader(line+"\n")), vm, at)
	}
	if r := ask(`{"op":"reboot"}`); r.Error == "" || r.OK {
		t.Fatalf("a workspace was served an unknown op: %+v", r)
	}
	if r := ask(`{"op":"stats","memAvailableKiB":5,"memTotalKiB":4,"psiSomeAvg10":0,"psiFullAvg10":0}`); r.Error == "" {
		t.Fatal("invalid stats accepted")
	}
	if r := ask(`{"op":"stats","memAvailableKiB":524288,"memTotalKiB":2097152,"psiSomeAvg10":12,"psiFullAvg10":1}`); !r.OK {
		t.Fatalf("stats rejected: %+v", r)
	}
	vm.mu.Lock()
	defer vm.mu.Unlock()
	if !vm.mem.Stats.ReceivedAt.Equal(at) || !vm.mem.Stats.Pressured() {
		t.Fatalf("recorded %+v", vm.mem.Stats)
	}
	if action, target := memory.Decide(vm.mem, memory.HostMemory{Available: 40 * gib, Reserve: 6 * gib}, at.Add(time.Second)); action != memory.MemoryPlug || target != 3*gib {
		t.Fatalf("decision = %s %d", action, target)
	}
}
