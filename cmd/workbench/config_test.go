package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func testConfig() config {
	return config{
		Name:              "alice-dev",
		Owner:             "alice",
		User:              "wb-alice-dev",
		URL:               "https://alice-dev.ws.test",
		StateDir:          "/var/lib/workbench/alice-dev",
		RuntimeDir:        "/run/workbench/alice-dev",
		CloudHypervisor:   "/nix/store/x-ch/bin/cloud-hypervisor",
		Virtiofsd:         "/nix/store/x-virtiofsd/bin/virtiofsd",
		QemuImg:           "/nix/store/x-qemu/bin/qemu-img",
		Firmware:          "/nix/store/x-ovmf/FV/CLOUDHV.fd",
		OS:                "ubuntu",
		Image:             "/nix/store/x-ubuntu.img",
		ImageFormat:       "qcow2",
		Seed:              "/nix/store/x-seed.img",
		DiskGiB:           64,
		BootMemMiB:        2048,
		MaxMemMiB:         16384,
		Tap:               "wb0ae8000a",
		MAC:               "02:00:0a:e8:00:0a",
		IP:                "10.232.0.10",
		Gateway:           "10.232.0.1",
		Nameservers:       []string{"1.1.1.1"},
		AppPort:           3773,
		SSHAuthorizedKeys: []string{"ssh-ed25519 AAAA alice@laptop"},
		Closure:           "/nix/store/x-closure/store-paths",
		Registration:      "/nix/store/x-closure/registration",
		App:               "/nix/store/x-app",
		Environment:       map[string]string{"LLM_URL": "https://llm.example"},
		SecretsDir:        "/var/lib/workbench-secrets/alice-dev",
		ControlPort:       5001,
		AgentPort:         5002,
	}
}

func TestConfigValidation(t *testing.T) {
	if err := testConfig().validate(); err != nil {
		t.Fatal(err)
	}
	nixos := testConfig()
	nixos.OS, nixos.Seed = "nixos", ""
	if err := nixos.validate(); err != nil {
		t.Fatal(err)
	}
	for name, change := range map[string]func(*config){
		"uppercase name":      func(c *config) { c.Name = "Alice" },
		"name too long":       func(c *config) { c.Name = strings.Repeat("a", 29) },
		"trailing dash":       func(c *config) { c.Name = "alice-" },
		"root owner":          func(c *config) { c.Owner = "root" },
		"owner with slash":    func(c *config) { c.Owner = "a/b" },
		"root VM user":        func(c *config) { c.User = "root" },
		"relative state dir":  func(c *config) { c.StateDir = "var/lib/x" },
		"comma in disk path":  func(c *config) { c.StateDir = "/var/lib/a,b" },
		"unclean path":        func(c *config) { c.Seed = "/nix/store/../etc/shadow" },
		"unknown format":      func(c *config) { c.ImageFormat = "vmdk" },
		"boot over max":       func(c *config) { c.BootMemMiB = 32768 },
		"boot not aligned":    func(c *config) { c.BootMemMiB = 1000 },
		"long tap":            func(c *config) { c.Tap = "wb0123456789abcd" },
		"bad mac":             func(c *config) { c.MAC = "02:00:0a" },
		"ipv6":                func(c *config) { c.IP = "fd00::1" },
		"bad gateway":         func(c *config) { c.Gateway = "host" },
		"bad nameserver":      func(c *config) { c.Nameservers = []string{"dns.example"} },
		"two keys in one":     func(c *config) { c.SSHAuthorizedKeys = []string{"ssh-ed25519 A\nssh-ed25519 B"} },
		"unknown os":          func(c *config) { c.OS = "arch" },
		"ubuntu without seed": func(c *config) { c.Seed = "" },
		"nixos with seed":     func(c *config) { c.OS = "nixos" },
		"same vsock ports":    func(c *config) { c.AgentPort = c.ControlPort },
		"env newline":         func(c *config) { c.Environment = map[string]string{"A": "x\nB=y"} },
		"env name":            func(c *config) { c.Environment = map[string]string{"A-B": "x"} },
		"relative env file":   func(c *config) { c.EnvironmentFiles = []string{"keys.env"} },
		"long runtime dir":    func(c *config) { c.RuntimeDir = "/run/" + strings.Repeat("x", 100) },
	} {
		t.Run(name, func(t *testing.T) {
			c := testConfig()
			change(&c)
			if err := c.validate(); err == nil {
				t.Fatal("accepted")
			}
		})
	}
}

func TestParseConfigRejectsUnknownFields(t *testing.T) {
	raw, _ := json.Marshal(testConfig())
	if _, err := parseConfig(raw); err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	json.Unmarshal(raw, &m)
	m["sudoers"] = "ALL"
	raw, _ = json.Marshal(m)
	if _, err := parseConfig(raw); err == nil {
		t.Fatal("an unknown field was accepted")
	}
}

// The declarative directory comes first; a later directory only adds
// workspaces, which is where self-service configs are meant to go.
func TestConfigDirectoriesPrecedence(t *testing.T) {
	etc, lib := t.TempDir(), t.TempDir()
	write := func(dir string, c config) {
		raw, _ := json.Marshal(c)
		if err := os.WriteFile(filepath.Join(dir, c.Name+".json"), raw, 0644); err != nil {
			t.Fatal(err)
		}
	}
	a := testConfig()
	write(etc, a)
	shadow := a
	shadow.Owner = "mallory"
	write(lib, shadow)
	b := testConfig()
	b.Name, b.Owner = "bob", "bob"
	write(lib, b)
	os.WriteFile(filepath.Join(lib, "notes.txt"), nil, 0644)
	os.WriteFile(filepath.Join(lib, "Bad.json"), nil, 0644)
	dirs := []string{etc, lib}
	if got := configured(dirs); !slices.Equal(got, []string{"alice-dev", "bob"}) {
		t.Fatalf("configured = %v", got)
	}
	c, err := loadConfig(dirs, "alice-dev")
	if err != nil || c.Owner != "alice" {
		t.Fatalf("loaded %+v, %v; want the /etc config", c, err)
	}
	if _, err := loadConfig(dirs, "bob"); err != nil {
		t.Fatal(err)
	}
	if _, err := loadConfig(dirs, "../etc/passwd"); err == nil {
		t.Fatal("loaded a path outside the config directories")
	}
	renamed := testConfig()
	renamed.Name = "carol"
	raw, _ := json.Marshal(renamed)
	os.WriteFile(filepath.Join(lib, "dave.json"), raw, 0644)
	if _, err := loadConfig(dirs, "dave"); err == nil {
		t.Fatal("a config was loaded under another workspace's name")
	}
}
