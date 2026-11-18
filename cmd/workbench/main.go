// workbench runs persistent per-person AI workspaces: one long-lived
// cloud-hypervisor VM each, an Ubuntu guest with an AI development app (T3
// Code today), declared in Nix.
// Every workspace is its own systemd unit whose main process this binary
// becomes ("run"); the memory daemon and the ops commands only talk to
// running VMs through their API and vsock sockets, so restarting them never
// stops a VM.
package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"strings"
	"time"
)

const usage = `usage: workbench <command> [flags] [args]

Ops:
  status [--json]              list workspaces, their VMs and disks
  pair <workspace> [flags...]  create a T3 Code pairing link for the owner
  console <workspace>          attach to the serial console (Ctrl-] quits)
  destroy <workspace> [--yes]  stop a workspace and delete its disk

Internal:
  run <workspace>              become the workspace's VM (the unit's ExecStart)
  shutdown <workspace>         power the VM off cleanly (the unit's ExecStop)
  memory                       grow and shrink workspace memory
  guest-agent                  the agent inside the guest
`

type configDirs []string

func (d *configDirs) String() string { return strings.Join(*d, ":") }
func (d *configDirs) Set(v string) error {
	*d = append(*d, v)
	return nil
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		var exit exitCode
		if errors.As(err, &exit) {
			os.Exit(int(exit))
		}
		fmt.Fprintln(os.Stderr, "workbench:", err)
		os.Exit(1)
	}
}

// exitCode ends the process with a code and no message.
type exitCode int

func (e exitCode) Error() string { return fmt.Sprintf("exit status %d", int(e)) }

func run(args []string) error {
	if len(args) == 0 || args[0] == "-h" || args[0] == "--help" || args[0] == "help" {
		fmt.Print(usage)
		return nil
	}
	cmd, args := args[0], args[1:]
	f := flag.NewFlagSet(cmd, flag.ContinueOnError)
	var dirs configDirs
	f.Var(&dirs, "config-dir", "directory of <workspace>.json configs; repeat to search several (default "+strings.Join(defaultConfigDirs, ", ")+")")
	stateRoot := f.String("state-root", "/var/lib/workbench", "destroy: where workspace disks live")
	yes := f.Bool("yes", false, "destroy: do not ask for confirmation")
	asJSON := f.Bool("json", false, "status: print JSON")
	timeout := f.Duration("timeout", 90*time.Second, "shutdown: how long the guest may take to power off")
	guestConfigPath := f.String("config", "/run/workspace/config/workspace.json", "guest-agent: workspace settings from the config share")
	statusFile := f.String("memory-status", "", "memory: where to publish the memory workspaces hold")
	var behind behindPools
	f.Var(&behind, "behind", "memory: <status file>=<GiB> of a pool that yields to workspaces and the memory it keeps whatever they want; repeat for each")
	reservePercent := f.Float64("mem-reserve-percent", 10, "memory: host memory reserve, percent of MemTotal")
	reserveMinGiB := f.Float64("mem-reserve-min-gib", 2, "memory: minimum host memory reserve, GiB")
	// pair passes everything after the workspace to t3 untouched.
	var rest []string
	if cmd == "pair" {
		for i := 0; i < len(args); i++ {
			a := args[i]
			if a == "--config-dir" || a == "-config-dir" {
				i++
				continue
			}
			if !strings.HasPrefix(a, "-") {
				rest = args[i+1:]
				args = args[:i+1]
				break
			}
		}
	}
	if err := f.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return nil
		}
		return err
	}
	if len(dirs) == 0 {
		dirs = defaultConfigDirs
	}
	pos := f.Args()
	one := func() (string, error) {
		if len(pos) != 1 {
			return "", fmt.Errorf("%s takes one workspace name", cmd)
		}
		return pos[0], nil
	}
	switch cmd {
	case "run":
		name, err := one()
		if err != nil {
			return err
		}
		return runWorkspace(dirs, name)
	case "shutdown":
		name, err := one()
		if err != nil {
			return err
		}
		return shutdownWorkspace(dirs, name, *timeout)
	case "memory":
		return runMemory(memoryOptions{
			dirs:           dirs,
			statusFile:     *statusFile,
			behind:         behind,
			reservePercent: *reservePercent,
			reserveMinGiB:  *reserveMinGiB,
		})
	case "guest-agent":
		return runGuestAgent(*guestConfigPath)
	case "pair":
		name, err := one()
		if err != nil {
			return err
		}
		return pair(dirs, name, rest)
	case "status":
		return status(os.Stdout, dirs, *stateRoot, *asJSON)
	case "console":
		name, err := one()
		if err != nil {
			return err
		}
		return console(dirs, name)
	case "destroy":
		name, err := one()
		if err != nil {
			return err
		}
		return destroy(dirs, *stateRoot, name, *yes, os.Stdin, os.Stdout)
	}
	return fmt.Errorf("unknown command %q\n%s", cmd, usage)
}
