package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"syscall"
	"time"

	"github.com/OSSystems/vmkit-nix/ch"
	"github.com/OSSystems/vmkit-nix/hostutil"
	"github.com/OSSystems/vmkit-nix/memory"
	"github.com/OSSystems/vmkit-nix/virtiofs"
)

// runWorkspace is the main process of a workspace's unit. It prepares the
// disk and the shares, starts one virtiofsd per share and then becomes
// cloud-hypervisor, so the VM is the unit and not a child of any daemon.
// Restarting a daemon or the CLI never touches it; they use the API socket.
func runWorkspace(dirs []string, name string) error {
	c, err := loadConfig(dirs, name)
	if err != nil {
		return err
	}
	vmUser, err := hostutil.LookupAccount(c.User)
	if err != nil {
		return fmt.Errorf("VM user: %w", err)
	}
	if err := virtiofs.RequirePrivateMounts(); err != nil {
		return err
	}
	if _, err := os.Stat(filepath.Join("/sys/class/net", c.Tap)); err != nil {
		return fmt.Errorf("tap %s: %w", c.Tap, err)
	}
	if err := ensureDir(c.StateDir, 0700, vmUser.UID, vmUser.GID); err != nil {
		return err
	}
	if err := prepareDisk(c, vmUser); err != nil {
		return fmt.Errorf("preparing disk: %w", err)
	}
	for _, dir := range []string{c.vmDir(), c.sharesDir()} {
		if err := os.RemoveAll(dir); err != nil {
			return err
		}
	}
	if err := ensureDir(c.RuntimeDir, 0711, 0, 0); err != nil {
		return err
	}
	if err := ensureDir(c.vmDir(), 0700, vmUser.UID, vmUser.GID); err != nil {
		return err
	}
	if err := ensureDir(c.sharesDir(), 0700, 0, 0); err != nil {
		return err
	}
	shares := c.shares()
	if err := virtiofs.StoreView(c.Closure, shares[0].Dir); err != nil {
		return fmt.Errorf("sharing the closure: %w", err)
	}
	if err := writeConfigShare(c, shares[1].Dir); err != nil {
		return fmt.Errorf("writing the config share: %w", err)
	}
	if err := writeSecretsShare(c, shares[2].Dir); err != nil {
		return fmt.Errorf("writing the secrets share: %w", err)
	}
	for _, s := range shares {
		socket := filepath.Join(c.vmDir(), "fs-"+s.Tag+".sock")
		cmd := exec.Command(c.Virtiofsd, virtiofsdArgs(socket, s.Dir)...)
		cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
		if err := cmd.Start(); err != nil {
			return fmt.Errorf("starting virtiofsd for %s: %w", s.Tag, err)
		}
		if err := hostutil.WaitForSocket(socket, 10*time.Second); err != nil {
			return err
		}
		if err := os.Chown(socket, vmUser.UID, vmUser.GID); err != nil {
			return err
		}
	}
	argv := cloudHypervisorArgv(c, runtime.NumCPU())
	fmt.Fprintf(os.Stderr, "starting workspace %s for %s at %s\n", c.Name, c.Owner, c.IP)
	if err := syscall.Setgroups(groupIDs(vmUser.Groups)); err != nil {
		return err
	}
	if err := syscall.Setgid(vmUser.GID); err != nil {
		return err
	}
	if err := syscall.Setuid(vmUser.UID); err != nil {
		return err
	}
	if err := os.Chdir(c.vmDir()); err != nil {
		return err
	}
	return syscall.Exec(argv[0], argv, []string{"PATH=/var/empty"})
}

func groupIDs(groups []uint32) []int {
	ids := make([]int, len(groups))
	for i, g := range groups {
		ids[i] = int(g)
	}
	return ids
}

// makeDir makes a directory owned by whoever runs this, root in the unit.
func makeDir(dir string, mode os.FileMode) error {
	if err := os.MkdirAll(dir, mode); err != nil {
		return err
	}
	return os.Chmod(dir, mode)
}

func ensureDir(dir string, mode os.FileMode, uid, gid int) error {
	if err := os.MkdirAll(dir, mode); err != nil {
		return err
	}
	if err := os.Chown(dir, uid, gid); err != nil {
		return err
	}
	return os.Chmod(dir, mode)
}

// prepareDisk makes the persistent disk from the OS image on first start
// and grows it to the configured size; it never shrinks it. The guest grows
// its root partition to match at boot.
func prepareDisk(c config, vmUser hostutil.Account) error {
	disk := c.disk()
	if _, err := os.Stat(disk); errors.Is(err, os.ErrNotExist) {
		tmp := disk + ".new"
		os.Remove(tmp)
		out, err := exec.Command(c.QemuImg, "convert", "-f", c.ImageFormat, "-O", "raw", c.Image, tmp).CombinedOutput()
		if err != nil {
			os.Remove(tmp)
			return fmt.Errorf("qemu-img convert: %w: %s", err, out)
		}
		if err := growFile(tmp, c.DiskGiB*memory.GiB); err != nil {
			os.Remove(tmp)
			return err
		}
		if err := os.Rename(tmp, disk); err != nil {
			return err
		}
		fmt.Fprintf(os.Stderr, "created %s from %s\n", disk, c.Image)
	} else if err != nil {
		return err
	} else if err := growFile(disk, c.DiskGiB*memory.GiB); err != nil {
		return err
	}
	if err := os.Chown(disk, vmUser.UID, vmUser.GID); err != nil {
		return err
	}
	return os.Chmod(disk, 0600)
}

func growFile(path string, size int64) error {
	f, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		return err
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil {
		return err
	}
	if fi.Size() >= size {
		return nil
	}
	if err := f.Truncate(size); err != nil {
		return err
	}
	return f.Sync()
}

// guestConfig is workspace.json in the config share: what the guest agent
// and a NixOS guest set themselves up from at each boot.
type guestConfig struct {
	Name        string   `json:"name"`
	Owner       string   `json:"owner"`
	URL         string   `json:"url"`
	IP          string   `json:"ip"`
	Gateway     string   `json:"gateway"`
	Nameservers []string `json:"nameservers"`
	AppPort     int      `json:"appPort"`
	ControlPort int      `json:"controlPort"`
	AgentPort   int      `json:"agentPort"`
}

// writeConfigShare writes what the guest starts its services from: the app
// in the closure, the closure's paths and their registration, the owner's
// SSH keys, the non-secret environment and workspace.json.
func writeConfigShare(c config, dir string) error {
	if err := makeDir(dir, 0755); err != nil {
		return err
	}
	if err := os.Symlink(c.App, filepath.Join(dir, "app")); err != nil {
		return err
	}
	for name, src := range map[string]string{"store-paths": c.Closure, "registration": c.Registration} {
		raw, err := hostutil.ReadBounded(src, 64<<20)
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(dir, name), raw, 0444); err != nil {
			return err
		}
	}
	keys := filepath.Join(dir, "authorized_keys")
	if err := makeDir(keys, 0755); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(keys, c.Owner), []byte(lines(c.SSHAuthorizedKeys)), 0444); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(dir, "environment"), []byte(environmentFile(c.Environment)), 0444); err != nil {
		return err
	}
	raw, err := json.MarshalIndent(guestConfig{
		Name: c.Name, Owner: c.Owner, URL: c.URL,
		IP: c.IP, Gateway: c.Gateway, Nameservers: c.Nameservers,
		AppPort: c.AppPort, ControlPort: c.ControlPort, AgentPort: c.AgentPort,
	}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, "workspace.json"), raw, 0444)
}

func lines(items []string) string {
	var b strings.Builder
	for _, item := range items {
		b.WriteString(item + "\n")
	}
	return b.String()
}

// environmentFile renders variables for systemd's EnvironmentFile=, quoted
// so no value is reinterpreted.
func environmentFile(env map[string]string) string {
	keys := make([]string, 0, len(env))
	for k := range env {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var b strings.Builder
	for _, k := range keys {
		v := strings.NewReplacer(`\`, `\\`, `"`, `\"`).Replace(env[k])
		fmt.Fprintf(&b, "%s=\"%s\"\n", k, v)
	}
	return b.String()
}

// writeSecretsShare copies the workspace's secrets directory into this
// run's share and appends the environment files to its environment file,
// which the guest's app service reads. Nothing of it is in the store.
func writeSecretsShare(c config, dir string) error {
	if err := makeDir(dir, 0755); err != nil {
		return err
	}
	var total int64
	err := filepath.WalkDir(c.SecretsDir, func(path string, d fs.DirEntry, err error) error {
		if errors.Is(err, os.ErrNotExist) && path == c.SecretsDir {
			return fs.SkipAll
		}
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(c.SecretsDir, path)
		target := filepath.Join(dir, rel)
		switch {
		case rel == "." || rel == "environment":
			return nil
		case d.IsDir():
			return os.Mkdir(target, 0755)
		case d.Type().IsRegular():
			raw, err := hostutil.ReadBounded(path, 16<<20)
			if err != nil {
				return err
			}
			if total += int64(len(raw)); total > 64<<20 {
				return errors.New("secrets directory larger than 64 MiB")
			}
			return os.WriteFile(target, raw, 0444)
		}
		return nil
	})
	if err != nil {
		return err
	}
	// The environment is the secrets directory's environment file followed
	// by every environment file, each starting on a line of its own.
	env, err := hostutil.ReadBounded(filepath.Join(c.SecretsDir, "environment"), 1<<20)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	for _, path := range c.EnvironmentFiles {
		raw, err := hostutil.ReadBounded(path, 1<<20)
		if err != nil {
			return err
		}
		env = append(append(env, '\n'), raw...)
	}
	if len(env) == 0 {
		return nil
	}
	return os.WriteFile(filepath.Join(dir, "environment"), env, 0444)
}

func virtiofsdArgs(socket, dir string) []string {
	return append(virtiofs.Args(socket, dir), "--sandbox=namespace")
}

// cloudHypervisorArgv boots the workspace from firmware with its disk, the
// cloud-init seed of an Ubuntu guest, its tap, vsock, the shares and a
// serial console socket. Memory starts at the boot size and grows through
// virtio-mem.
func cloudHypervisorArgv(c config, hostCPUs int) []string {
	cpus := c.CPUs
	if cpus == 0 || cpus > hostCPUs {
		cpus = hostCPUs
	}
	vm := ch.VM{
		Binary:      c.CloudHypervisor,
		APISocket:   c.apiSocket(),
		Firmware:    c.Firmware,
		CPUs:        cpus,
		MemoryMiB:   c.BootMemMiB,
		HotplugMiB:  max(0, c.MaxMemMiB-c.BootMemMiB),
		Disks:       []ch.Disk{{Path: c.disk()}},
		Nets:        []ch.Net{{Tap: c.Tap, MAC: c.MAC}},
		VsockCID:    3,
		VsockSocket: c.vsock(),
		Serial:      "socket=" + c.serial(),
	}
	if c.Seed != "" {
		vm.Disks = append(vm.Disks, ch.Disk{Path: c.Seed, ReadOnly: true})
	}
	for _, s := range c.shares() {
		vm.Shares = append(vm.Shares, ch.Share{Tag: s.Tag, Socket: filepath.Join(c.vmDir(), "fs-"+s.Tag+".sock")})
	}
	return vm.Args()
}

// shutdownWorkspace presses the VM's power button and waits for the guest
// to power off, so a stop is a clean shutdown; systemd kills what is left
// after.
func shutdownWorkspace(dirs []string, name string, timeout time.Duration) error {
	c, err := loadConfig(dirs, name)
	if err != nil {
		return err
	}
	client := ch.NewClient(c.apiSocket())
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	if err := client.PowerButton(ctx); err != nil {
		return nil
	}
	for ctx.Err() == nil {
		if _, err := client.Info(ctx); err != nil {
			return nil
		}
		time.Sleep(500 * time.Millisecond)
	}
	fmt.Fprintf(os.Stderr, "%s did not power off within %s\n", name, timeout)
	return nil
}
