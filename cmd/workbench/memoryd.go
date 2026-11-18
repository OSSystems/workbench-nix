package main

import (
	"bufio"
	"context"
	"fmt"
	"log/slog"
	"math"
	"net"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/OSSystems/vmkit-nix/ch"
	"github.com/OSSystems/vmkit-nix/control"
	"github.com/OSSystems/vmkit-nix/hostutil"
	"github.com/OSSystems/vmkit-nix/memory"
)

type memoryOptions struct {
	dirs           []string
	statusFile     string
	behind         behindPools
	reservePercent float64
	reserveMinGiB  float64
}

// wsVM is one running workspace the memory daemon manages.
type wsVM struct {
	c config
	// run identifies one start of the VM: its directory is made anew by
	// every start.
	run uint64
	ln  net.Listener

	mu     sync.Mutex
	mem    memory.VMMemory
	denied bool
}

func (vm *wsVM) recordStats(s memory.GuestStats) {
	vm.mu.Lock()
	defer vm.mu.Unlock()
	vm.mem.Observe(s)
}

// memoryDaemon grows and shrinks running workspaces with vmkit's memory
// policy and publishes what they hold: workspaces are a first pool, so
// yielding pools boot only into what they leave. It owns no VM: restarting
// it changes nothing but the next decision.
type memoryDaemon struct {
	opts   memoryOptions
	logger *slog.Logger
	vms    map[string]*wsVM
	// deniedAt is when a workspace was last denied a plug.
	deniedAt time.Time
}

func runMemory(opts memoryOptions) error {
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	d := &memoryDaemon{opts: opts, logger: slog.New(slog.NewTextHandler(os.Stderr, nil)), vms: map[string]*wsVM{}}
	tick := time.NewTicker(2 * time.Second)
	defer tick.Stop()
	for {
		d.pass(ctx, time.Now())
		select {
		case <-ctx.Done():
			for _, vm := range d.vms {
				vm.ln.Close()
			}
			return nil
		case <-tick.C:
		}
	}
}

// runID is the identity of a VM directory, which every start recreates.
func runID(dir string) (uint64, bool) {
	fi, err := os.Stat(dir)
	if err != nil {
		return 0, false
	}
	st, ok := fi.Sys().(*syscall.Stat_t)
	return st.Ino, ok
}

// track follows the configured workspaces whose VM is up: a new start gets
// fresh memory state and a fresh control socket.
func (d *memoryDaemon) track(ctx context.Context) {
	seen := map[string]bool{}
	for _, name := range configured(d.opts.dirs) {
		c, err := loadConfig(d.opts.dirs, name)
		if err != nil {
			continue
		}
		run, ok := runID(c.vmDir())
		if !ok {
			continue
		}
		if _, err := ch.NewClient(c.apiSocket()).Info(ctx); err != nil {
			continue
		}
		seen[name] = true
		if vm := d.vms[name]; vm != nil && vm.run == run && socketPresent(c.controlSocket()) {
			continue
		}
		d.forget(name)
		vm, err := d.listen(c, run)
		if err != nil {
			d.logger.Warn("listening for guest stats", "workspace", name, "error", err)
			continue
		}
		d.vms[name] = vm
		d.logger.Info("managing workspace memory", "workspace", name, "bootMiB", c.BootMemMiB, "maxMiB", c.MaxMemMiB)
	}
	for name := range d.vms {
		if !seen[name] {
			d.forget(name)
		}
	}
}

func socketPresent(path string) bool {
	fi, err := os.Stat(path)
	return err == nil && fi.Mode()&os.ModeSocket != 0
}

func (d *memoryDaemon) forget(name string) {
	if vm := d.vms[name]; vm != nil {
		vm.ln.Close()
		delete(d.vms, name)
	}
}

// listen opens the host side of the guest's stats port: cloud-hypervisor
// forwards the guest's connections to it.
func (d *memoryDaemon) listen(c config, run uint64) (*wsVM, error) {
	vmUser, err := hostutil.LookupAccount(c.User)
	if err != nil {
		return nil, err
	}
	socket := c.controlSocket()
	os.Remove(socket)
	ln, err := net.Listen("unix", socket)
	if err != nil {
		return nil, err
	}
	if err := os.Chown(socket, vmUser.UID, vmUser.GID); err != nil {
		ln.Close()
		return nil, err
	}
	if err := os.Chmod(socket, 0600); err != nil {
		ln.Close()
		return nil, err
	}
	vm := &wsVM{c: c, run: run, ln: ln, mem: memory.NewVMMemory(c.BootMemMiB*memory.MiB, c.MaxMemMiB*memory.MiB)}
	go control.Serve(ln, func(conn net.Conn) {
		control.Answer(conn, func(r *bufio.Reader) (any, func(bool)) {
			return answerStats(r, vm, time.Now()), nil
		})
	})
	return vm, nil
}

func answerStats(r *bufio.Reader, vm *wsVM, now time.Time) control.Reply {
	req, bad := control.ReadRequest(r)
	if bad != nil {
		return *bad
	}
	if req.Op != "stats" {
		return control.Reply{Error: "unknown op"}
	}
	s, bad := req.Stats(now)
	if bad != nil {
		return *bad
	}
	vm.recordStats(s)
	return control.Reply{OK: true}
}

func (d *memoryDaemon) pass(ctx context.Context, now time.Time) {
	d.track(ctx)
	for _, vm := range d.vms {
		d.control(ctx, vm, now)
	}
	if err := memory.WriteStatus(d.opts.statusFile, d.status(now)); err != nil {
		d.logger.Warn("publishing workspace memory", "error", err)
	}
}

func (d *memoryDaemon) pending() int64 {
	var pending int64
	for _, vm := range d.vms {
		vm.mu.Lock()
		pending += vm.mem.Pending()
		vm.mu.Unlock()
	}
	return pending
}

// hostMemory is what a workspace may grow into: available memory less the
// host reserve, what other workspaces were promised, and what the pools
// behind workspaces are guaranteed but do not hold yet.
func (d *memoryDaemon) hostMemory(now time.Time) (memory.HostMemory, error) {
	mem, err := hostutil.MeminfoKiB()
	if err != nil {
		return memory.HostMemory{}, err
	}
	total, available := mem["MemTotal"]<<10, mem["MemAvailable"]<<10
	return workspaceHostMemory(total, available, d.opts.reservePercent, d.opts.reserveMinGiB, d.pending(), d.opts.behind.owed(now)), nil
}

func workspaceHostMemory(total, available int64, percent, minGiB float64, pending, owed int64) memory.HostMemory {
	return memory.HostMemory{Available: available, Reserve: memory.Reserve(total, percent, minGiB) + owed, Pending: pending}
}

// behindPool is a pool that yields to workspaces: where it publishes its
// memory, and what it keeps whatever workspaces want.
type behindPool struct {
	status     string
	guaranteed int64
}

type behindPools []behindPool

func (b *behindPools) String() string {
	var parts []string
	for _, p := range *b {
		parts = append(parts, fmt.Sprintf("%s=%g", p.status, float64(p.guaranteed)/float64(memory.GiB)))
	}
	return strings.Join(parts, ",")
}

func (b *behindPools) Set(v string) error {
	path, gib, ok := strings.Cut(v, "=")
	n, err := strconv.ParseFloat(gib, 64)
	if !ok || path == "" || err != nil || n < 0 || math.IsInf(n, 0) {
		return fmt.Errorf("want <status file>=<GiB>, got %q", v)
	}
	*b = append(*b, behindPool{status: path, guaranteed: int64(n * float64(memory.GiB))})
	return nil
}

// owed is what workspaces leave for the pools behind them: each pool's
// guarantee less what it holds.
func (b behindPools) owed(now time.Time) int64 {
	var owed int64
	for _, p := range b {
		s, _ := memory.ReadStatus(p.status, now)
		owed += memory.Owed(p.guaranteed, s)
	}
	return owed
}

func (d *memoryDaemon) control(ctx context.Context, vm *wsVM, now time.Time) {
	client := ch.NewClient(vm.c.apiSocket())
	info, err := client.Info(ctx)
	if err != nil || info.MemoryActualSize <= 0 {
		return
	}
	host, err := d.hostMemory(now)
	if err != nil {
		return
	}
	vm.mu.Lock()
	vm.mem.ActualBytes = info.MemoryActualSize
	m := vm.mem
	action, target := memory.Decide(m, host, now)
	var previous memory.VMMemory
	firstDenial := false
	switch action {
	case memory.MemoryDenied:
		d.deniedAt = now
		firstDenial = !vm.denied
		vm.denied = true
	case memory.MemoryPlug, memory.MemoryUnplug:
		previous = vm.mem.Grant(m.ActualBytes, target, now)
		vm.denied = false
	}
	vm.mu.Unlock()
	if firstDenial {
		d.logger.Warn("host full, workspace memory not grown", "workspace", vm.c.Name, "actualGiB", float64(m.ActualBytes)/float64(memory.GiB))
	}
	if action != memory.MemoryPlug && action != memory.MemoryUnplug {
		return
	}
	if err := client.ResizeMemory(ctx, target); err != nil {
		vm.mu.Lock()
		stats, actual := vm.mem.Stats, vm.mem.ActualBytes
		vm.mem = previous
		vm.mem.Stats, vm.mem.ActualBytes = stats, actual
		vm.mu.Unlock()
		d.logger.Warn("resizing workspace memory", "workspace", vm.c.Name, "error", err)
		return
	}
	vm.mu.Lock()
	vm.mem.ResizedAt = time.Now()
	vm.mu.Unlock()
	d.logger.Info("workspace memory changed", "workspace", vm.c.Name, "action", action.String(), "fromGiB", float64(m.ActualBytes)/float64(memory.GiB), "toGiB", float64(target)/float64(memory.GiB), "guestAvailableGiB", float64(m.Stats.Available())/float64(memory.GiB))
}

func (d *memoryDaemon) status(now time.Time) memory.Status {
	s := memory.Status{UpdatedAt: now, DeniedAt: d.deniedAt}
	for _, vm := range d.vms {
		vm.mu.Lock()
		s.CommittedBytes += vm.mem.Committed()
		s.PendingBytes += vm.mem.Pending()
		vm.mu.Unlock()
	}
	return s
}
