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

Internal:
  run <workspace>              become the workspace's VM (the unit's ExecStart)
  shutdown <workspace>         power the VM off cleanly (the unit's ExecStop)
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
	timeout := f.Duration("timeout", 90*time.Second, "shutdown: how long the guest may take to power off")
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
	}
	return fmt.Errorf("unknown command %q\n%s", cmd, usage)
}
