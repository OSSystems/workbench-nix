package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"text/tabwriter"
	"time"
	"unsafe"

	"github.com/OSSystems/vmkit-nix/ch"
	"github.com/OSSystems/vmkit-nix/control"
	"github.com/OSSystems/vmkit-nix/memory"
)

func unitName(name string) string { return "workbench@" + name + ".service" }

// askAgent sends one request to a workspace's guest agent over vsock.
func askAgent(c config, req agentRequest, timeout time.Duration) (agentReply, error) {
	var reply agentReply
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	conn, err := ch.DialGuest(ctx, c.vsock(), c.AgentPort)
	if err != nil {
		return reply, fmt.Errorf("reaching the guest agent of %s: %w", c.Name, err)
	}
	defer conn.Close()
	if deadline, ok := ctx.Deadline(); ok {
		conn.SetDeadline(deadline)
	}
	raw, _ := json.Marshal(req)
	if _, err := conn.Write(append(raw, '\n')); err != nil {
		return reply, err
	}
	line, err := control.ReadLine(bufio.NewReader(conn), 1<<20)
	if err != nil {
		return reply, err
	}
	if err := json.Unmarshal(line, &reply); err != nil {
		return reply, err
	}
	if reply.Error != "" {
		return reply, errors.New(reply.Error)
	}
	return reply, nil
}

// pair runs `t3 auth pairing create --base-url <url>` as the owner inside
// the guest and prints what it prints.
func pair(dirs []string, name string, args []string) error {
	c, err := loadConfig(dirs, name)
	if err != nil {
		return err
	}
	reply, err := askAgent(c, agentRequest{Op: "pair", Args: args}, 90*time.Second)
	if err != nil {
		return err
	}
	fmt.Print(reply.Stdout)
	fmt.Fprint(os.Stderr, reply.Stderr)
	if reply.ExitCode != 0 {
		return exitCode(reply.ExitCode)
	}
	return nil
}

type workspaceStatus struct {
	Name        string `json:"name"`
	Configured  bool   `json:"configured"`
	Owner       string `json:"owner,omitempty"`
	URL         string `json:"url,omitempty"`
	IP          string `json:"ip,omitempty"`
	Unit        string `json:"unit"`
	VM          string `json:"vm"`
	MemoryBytes int64  `json:"memoryBytes,omitempty"`
	MaxMemBytes int64  `json:"maxMemoryBytes,omitempty"`
	DiskBytes   int64  `json:"diskBytes"`
	DiskUsed    int64  `json:"diskUsedBytes"`
	Agent       string `json:"agent,omitempty"`
}

func unitState(unit string) string {
	out, err := exec.Command("systemctl", "show", "--property=ActiveState", "--value", unit).Output()
	if err != nil {
		return "unknown"
	}
	return strings.TrimSpace(string(out))
}

// status lists configured workspaces and disks left by removed ones.
func status(w io.Writer, dirs []string, stateRoot string, asJSON bool) error {
	names := configured(dirs)
	known := map[string]bool{}
	for _, n := range names {
		known[n] = true
	}
	entries, _ := os.ReadDir(stateRoot)
	for _, e := range entries {
		if e.IsDir() && workspaceName.MatchString(e.Name()) && !known[e.Name()] {
			if _, err := os.Stat(filepath.Join(stateRoot, e.Name(), "disk.raw")); err == nil {
				names = append(names, e.Name())
			}
		}
	}
	sort.Strings(names)
	var all []workspaceStatus
	for _, name := range names {
		s := workspaceStatus{Name: name, Unit: unitState(unitName(name)), VM: "-"}
		disk := filepath.Join(stateRoot, name, "disk.raw")
		if c, err := loadConfig(dirs, name); err == nil {
			s.Configured, s.Owner, s.URL, s.IP, disk = true, c.Owner, c.URL, c.IP, c.disk()
			s.MaxMemBytes = c.MaxMemMiB * memory.MiB
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			if info, err := ch.NewClient(c.apiSocket()).Info(ctx); err == nil {
				s.VM, s.MemoryBytes = info.State, info.MemoryActualSize
				if _, err := askAgent(c, agentRequest{Op: "ping"}, 2*time.Second); err == nil {
					s.Agent = "up"
				} else {
					s.Agent = "down"
				}
			}
			cancel()
		}
		var st syscall.Stat_t
		if syscall.Stat(disk, &st) == nil {
			s.DiskBytes, s.DiskUsed = st.Size, st.Blocks*512
		}
		all = append(all, s)
	}
	if asJSON {
		enc := json.NewEncoder(w)
		enc.SetIndent("", "  ")
		return enc.Encode(all)
	}
	tw := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	fmt.Fprintln(tw, "WORKSPACE\tOWNER\tIP\tUNIT\tVM\tMEMORY\tDISK USED/SIZE\tAGENT\tURL")
	for _, s := range all {
		owner, url := s.Owner, s.URL
		if !s.Configured {
			owner, url = "-", "(removed from the configuration; disk kept)"
		}
		mem := "-"
		if s.MemoryBytes > 0 {
			mem = fmt.Sprintf("%.1f/%.0f GiB", gibs(s.MemoryBytes), gibs(s.MaxMemBytes))
		}
		fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%s\t%s\t%.1f/%.0f GiB\t%s\t%s\n", s.Name, owner, orDash(s.IP), s.Unit, s.VM, mem, gibs(s.DiskUsed), gibs(s.DiskBytes), orDash(s.Agent), url)
	}
	return tw.Flush()
}

func gibs(b int64) float64 { return float64(b) / float64(memory.GiB) }

func orDash(s string) string {
	if s == "" {
		return "-"
	}
	return s
}

// console attaches the terminal to the VM's serial port until Ctrl-] is
// typed.
func console(dirs []string, name string) error {
	c, err := loadConfig(dirs, name)
	if err != nil {
		return err
	}
	conn, err := net.Dial("unix", c.serial())
	if err != nil {
		return fmt.Errorf("serial console of %s: %w", name, err)
	}
	defer conn.Close()
	restore, err := rawTerminal(int(os.Stdin.Fd()))
	if err == nil {
		defer restore()
	}
	fmt.Fprintf(os.Stderr, "Connected to %s; Ctrl-] quits.\r\n", name)
	done := make(chan struct{}, 2)
	go func() {
		io.Copy(os.Stdout, conn)
		done <- struct{}{}
	}()
	go func() {
		buf := make([]byte, 1024)
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				if i := strings.IndexByte(string(buf[:n]), 0x1d); i >= 0 {
					conn.Write(buf[:i])
					break
				}
				if _, err := conn.Write(buf[:n]); err != nil {
					break
				}
			}
			if err != nil {
				break
			}
		}
		done <- struct{}{}
	}()
	<-done
	return nil
}

// rawTerminal puts a terminal in raw mode and returns how to restore it.
func rawTerminal(fd int) (func(), error) {
	var old syscall.Termios
	if _, _, errno := syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), syscall.TCGETS, uintptr(unsafe.Pointer(&old))); errno != 0 {
		return nil, errno
	}
	raw := old
	raw.Iflag &^= syscall.IGNBRK | syscall.BRKINT | syscall.PARMRK | syscall.ISTRIP | syscall.INLCR | syscall.IGNCR | syscall.ICRNL | syscall.IXON
	raw.Oflag &^= syscall.OPOST
	raw.Lflag &^= syscall.ECHO | syscall.ECHONL | syscall.ICANON | syscall.ISIG | syscall.IEXTEN
	raw.Cflag &^= syscall.CSIZE | syscall.PARENB
	raw.Cflag |= syscall.CS8
	raw.Cc[syscall.VMIN], raw.Cc[syscall.VTIME] = 1, 0
	if _, _, errno := syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), syscall.TCSETS, uintptr(unsafe.Pointer(&raw))); errno != 0 {
		return nil, errno
	}
	return func() {
		syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), syscall.TCSETS, uintptr(unsafe.Pointer(&old)))
	}, nil
}

// destroy stops a workspace and deletes its disk, after the name is typed
// again unless yes is set. It is the only way workspace data is deleted.
func destroy(dirs []string, stateRoot, name string, yes bool, in io.Reader, out io.Writer) error {
	if !workspaceName.MatchString(name) {
		return fmt.Errorf("invalid workspace name %q", name)
	}
	stateDir := filepath.Join(stateRoot, name)
	c, err := loadConfig(dirs, name)
	declared := err == nil
	if declared {
		stateDir = c.StateDir
	}
	if _, err := os.Stat(stateDir); errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("workspace %s has no data in %s", name, stateDir)
	}
	if !yes {
		fmt.Fprintf(out, "This stops workspace %s and deletes %s, its disk and everything in it.\nType the workspace name to confirm: ", name, stateDir)
		answer, _ := bufio.NewReader(in).ReadString('\n')
		if strings.TrimSpace(answer) != name {
			return errors.New("not confirmed; nothing deleted")
		}
	}
	if out, err := exec.Command("systemctl", "stop", unitName(name)).CombinedOutput(); err != nil {
		return fmt.Errorf("stopping %s: %w: %s", unitName(name), err, out)
	}
	if state := unitState(unitName(name)); state == "active" || state == "activating" || state == "deactivating" {
		return fmt.Errorf("%s is still %s; nothing deleted", unitName(name), state)
	}
	if err := os.RemoveAll(stateDir); err != nil {
		return err
	}
	fmt.Fprintf(out, "Deleted %s.\n", stateDir)
	if declared {
		fmt.Fprintf(out, "%s is still declared: its next start makes a fresh disk. Remove it from the configuration to keep it gone.\n", name)
	}
	return nil
}
