package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/OSSystems/vmkit-nix/hostutil"
)

// defaultConfigDirs are searched in order for <workspace>.json: the
// declarative configs Nix renders, then configs made at run time.
var defaultConfigDirs = []string{"/etc/workbench", "/var/lib/workbench/configs"}

// config is one workspace, as the host module renders it.
type config struct {
	Name  string `json:"name"`
	Owner string `json:"owner"`
	// User is the host user cloud-hypervisor runs as.
	User string `json:"user"`
	URL  string `json:"url"`
	// StateDir keeps the disk across restarts; RuntimeDir holds the sockets
	// and shares of one run.
	StateDir   string `json:"stateDir"`
	RuntimeDir string `json:"runtimeDir"`

	CloudHypervisor string `json:"cloudHypervisor"`
	Virtiofsd       string `json:"virtiofsd"`
	QemuImg         string `json:"qemuImg"`
	Firmware        string `json:"firmware"`
	// OS is the guest's: "ubuntu", set up by cloud-init from Seed, its
	// NoCloud disk, or "nixos", set up by workbench.nixosModules.guest
	// from the config share. Image is the OS image a new disk is made from.
	OS          string `json:"os"`
	Image       string `json:"image"`
	ImageFormat string `json:"imageFormat"`
	Seed        string `json:"seed,omitempty"`
	DiskGiB     int64  `json:"diskGiB"`

	CPUs       int   `json:"cpus"`
	BootMemMiB int64 `json:"bootMemMiB"`
	MaxMemMiB  int64 `json:"maxMemMiB"`

	Tap         string   `json:"tap"`
	MAC         string   `json:"mac"`
	IP          string   `json:"ip"`
	Gateway     string   `json:"gateway"`
	Nameservers []string `json:"nameservers"`
	AppPort     int      `json:"appPort"`

	SSHAuthorizedKeys []string `json:"sshAuthorizedKeys"`

	// Closure lists the store paths the guest is shared, Registration their
	// validity for a guest that copies them into a store of its own; App is
	// the one it starts from.
	Closure      string `json:"closure"`
	Registration string `json:"registration"`
	App          string `json:"app"`

	Environment      map[string]string `json:"environment"`
	EnvironmentFiles []string          `json:"environmentFiles"`
	SecretsDir       string            `json:"secretsDir"`

	ControlPort int `json:"controlPort"`
	AgentPort   int `json:"agentPort"`
}

var (
	workspaceName = regexp.MustCompile(`^[a-z][a-z0-9-]{0,26}[a-z0-9]$|^[a-z]$`)
	userName      = regexp.MustCompile(`^[a-z_][a-z0-9_.-]{0,31}$`)
	macAddress    = regexp.MustCompile(`^[0-9a-f]{2}(:[0-9a-f]{2}){5}$`)
	envName       = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*$`)
)

func (c config) validate() error {
	if !workspaceName.MatchString(c.Name) {
		return fmt.Errorf("invalid workspace name %q", c.Name)
	}
	if !userName.MatchString(c.Owner) || c.Owner == "root" {
		return fmt.Errorf("invalid owner %q", c.Owner)
	}
	if !userName.MatchString(c.User) || c.User == "root" {
		return fmt.Errorf("invalid VM user %q", c.User)
	}
	for name, path := range map[string]string{
		"stateDir": c.StateDir, "runtimeDir": c.RuntimeDir, "cloudHypervisor": c.CloudHypervisor,
		"virtiofsd": c.Virtiofsd, "qemuImg": c.QemuImg, "firmware": c.Firmware, "image": c.Image,
		"closure": c.Closure, "registration": c.Registration, "app": c.App, "secretsDir": c.SecretsDir,
	} {
		if !filepath.IsAbs(path) || filepath.Clean(path) != path || strings.ContainsAny(path, ",\n") {
			return fmt.Errorf("%s must be a clean absolute path without commas", name)
		}
	}
	switch c.OS {
	case "ubuntu":
		if !filepath.IsAbs(c.Seed) || filepath.Clean(c.Seed) != c.Seed || strings.ContainsAny(c.Seed, ",\n") {
			return errors.New("an Ubuntu guest needs a seed: a clean absolute path without commas")
		}
	case "nixos":
		if c.Seed != "" {
			return errors.New("a NixOS guest takes no cloud-init seed")
		}
	default:
		return fmt.Errorf("unknown guest OS %q", c.OS)
	}
	for _, f := range c.EnvironmentFiles {
		if !filepath.IsAbs(f) {
			return fmt.Errorf("environment file %q must be absolute", f)
		}
	}
	if c.ImageFormat != "qcow2" && c.ImageFormat != "raw" {
		return fmt.Errorf("unknown image format %q", c.ImageFormat)
	}
	if c.DiskGiB <= 0 || c.CPUs < 0 {
		return errors.New("diskGiB must be positive and cpus nonnegative")
	}
	if c.BootMemMiB <= 0 || c.BootMemMiB%128 != 0 || c.MaxMemMiB < c.BootMemMiB || c.MaxMemMiB%128 != 0 {
		return errors.New("bootMemMiB and maxMemMiB must be multiples of 128 with bootMemMiB <= maxMemMiB")
	}
	if len(c.Tap) == 0 || len(c.Tap) > 15 || strings.ContainsAny(c.Tap, "/ \t\n") {
		return fmt.Errorf("invalid tap %q", c.Tap)
	}
	if !macAddress.MatchString(c.MAC) {
		return fmt.Errorf("invalid MAC %q", c.MAC)
	}
	for _, ip := range append([]string{c.IP, c.Gateway}, c.Nameservers...) {
		if parsed := net.ParseIP(ip); parsed == nil || parsed.To4() == nil {
			return fmt.Errorf("invalid IPv4 address %q", ip)
		}
	}
	for _, key := range c.SSHAuthorizedKeys {
		if key == "" || strings.ContainsAny(key, "\n\r\x00") {
			return errors.New("an SSH key must be one nonempty line")
		}
	}
	for _, port := range []int{c.AppPort, c.ControlPort, c.AgentPort} {
		if port <= 0 || port > 65535 {
			return errors.New("invalid port")
		}
	}
	if c.ControlPort == c.AgentPort {
		return errors.New("controlPort and agentPort must differ")
	}
	for k, v := range c.Environment {
		if !envName.MatchString(k) || strings.ContainsAny(v, "\n\x00") {
			return fmt.Errorf("invalid environment variable %q", k)
		}
	}
	// The serial console socket is the longest path in the VM directory.
	if len(filepath.Join(c.vmDir(), "vsock.sock_65535")) > 107 {
		return errors.New("runtimeDir too long for its sockets")
	}
	return nil
}

func (c config) disk() string      { return filepath.Join(c.StateDir, "disk.raw") }
func (c config) vmDir() string     { return filepath.Join(c.RuntimeDir, "vm") }
func (c config) sharesDir() string { return filepath.Join(c.RuntimeDir, "shares") }
func (c config) apiSocket() string { return filepath.Join(c.vmDir(), "api.sock") }
func (c config) vsock() string     { return filepath.Join(c.vmDir(), "vsock.sock") }
func (c config) serial() string    { return filepath.Join(c.vmDir(), "serial.sock") }
func (c config) controlSocket() string {
	return fmt.Sprintf("%s_%d", c.vsock(), c.ControlPort)
}

// share is one host directory the guest mounts read-only over virtiofs.
type share struct {
	Tag string
	Dir string
}

// shares are what the guest mounts: its closure at /nix/store, the
// workspace config and its secrets.
func (c config) shares() []share {
	return []share{
		{"store", filepath.Join(c.sharesDir(), "store")},
		{"config", filepath.Join(c.sharesDir(), "config")},
		{"secrets", filepath.Join(c.sharesDir(), "secrets")},
	}
}

func parseConfig(raw []byte) (config, error) {
	var c config
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&c); err != nil {
		return c, fmt.Errorf("workspace config: %w", err)
	}
	return c, c.validate()
}

// loadConfig finds a workspace's config in the first directory that has it.
func loadConfig(dirs []string, name string) (config, error) {
	if !workspaceName.MatchString(name) {
		return config{}, fmt.Errorf("invalid workspace name %q", name)
	}
	for _, dir := range dirs {
		raw, err := hostutil.ReadBounded(filepath.Join(dir, name+".json"), 1<<20)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return config{}, err
		}
		c, err := parseConfig(raw)
		if err != nil {
			return c, err
		}
		if c.Name != name {
			return c, fmt.Errorf("%s/%s.json names workspace %q", dir, name, c.Name)
		}
		return c, nil
	}
	return config{}, fmt.Errorf("no workspace %q in %s", name, strings.Join(dirs, ", "))
}

// configured lists the workspaces the config directories declare, the
// first directory winning.
func configured(dirs []string) []string {
	seen := map[string]bool{}
	var names []string
	for _, dir := range dirs {
		entries, _ := os.ReadDir(dir)
		for _, e := range entries {
			name, ok := strings.CutSuffix(e.Name(), ".json")
			if ok && workspaceName.MatchString(name) && !seen[name] {
				seen[name] = true
				names = append(names, name)
			}
		}
	}
	sort.Strings(names)
	return names
}
