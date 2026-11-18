package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func flagValues(argv []string, flag string) []string {
	var values []string
	for i, a := range argv {
		if a != flag {
			continue
		}
		for _, v := range argv[i+1:] {
			if strings.HasPrefix(v, "--") {
				break
			}
			values = append(values, v)
		}
	}
	return values
}

func TestCloudHypervisorArgv(t *testing.T) {
	c := testConfig()
	argv := cloudHypervisorArgv(c, 16)
	if argv[0] != c.CloudHypervisor {
		t.Fatalf("argv[0] = %s", argv[0])
	}
	want := map[string][]string{
		"--api-socket": {"path=/run/workbench/alice-dev/vm/api.sock"},
		"--firmware":   {c.Firmware},
		"--cpus":       {"boot=16,max=16,nested=off"},
		"--memory":     {"size=2048M,hotplug_method=virtio-mem,hotplug_size=14336M,shared=on"},
		"--balloon":    {"free_page_reporting=on"},
		"--disk":       {"path=/var/lib/workbench/alice-dev/disk.raw,image_type=raw", "path=/nix/store/x-seed.img,readonly=on,image_type=raw"},
		"--net":        {"tap=wb0ae8000a,mac=02:00:0a:e8:00:0a"},
		"--vsock":      {"cid=3,socket=/run/workbench/alice-dev/vm/vsock.sock"},
		"--serial":     {"socket=/run/workbench/alice-dev/vm/serial.sock"},
		"--console":    {"off"},
		"--fs": {
			"tag=store,socket=/run/workbench/alice-dev/vm/fs-store.sock",
			"tag=config,socket=/run/workbench/alice-dev/vm/fs-config.sock",
			"tag=secrets,socket=/run/workbench/alice-dev/vm/fs-secrets.sock",
		},
	}
	for flag, values := range want {
		if got := flagValues(argv, flag); !slices.Equal(got, values) {
			t.Errorf("%s = %q, want %q", flag, got, values)
		}
	}
	if slices.Contains(argv, "--kernel") {
		t.Error("a firmware guest got a kernel")
	}
}

func TestCloudHypervisorArgvSizing(t *testing.T) {
	c := testConfig()
	c.CPUs = 4
	if got := flagValues(cloudHypervisorArgv(c, 16), "--cpus"); got[0] != "boot=4,max=4,nested=off" {
		t.Fatalf("--cpus = %s", got[0])
	}
	c.CPUs = 64
	if got := flagValues(cloudHypervisorArgv(c, 16), "--cpus"); got[0] != "boot=16,max=16,nested=off" {
		t.Fatalf("more vCPUs than host CPUs: %s", got[0])
	}
	c.MaxMemMiB = c.BootMemMiB
	if got := flagValues(cloudHypervisorArgv(c, 16), "--memory"); got[0] != "size=2048M,shared=on" {
		t.Fatalf("fixed memory = %s", got[0])
	}
}

func TestEnvironmentFileQuotes(t *testing.T) {
	got := environmentFile(map[string]string{"B": `say "hi" \o/`, "A": "$HOME stays literal"})
	want := "A=\"$HOME stays literal\"\nB=\"say \\\"hi\\\" \\\\o/\"\n"
	if got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestConfigShare(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "config")
	c := testConfig()
	if err := writeConfigShare(c, dir); err != nil {
		t.Fatal(err)
	}
	if link, err := os.Readlink(filepath.Join(dir, "app")); err != nil || link != c.App {
		t.Fatalf("app -> %s, %v", link, err)
	}
	env, _ := os.ReadFile(filepath.Join(dir, "environment"))
	if string(env) != "LLM_URL=\"https://llm.example\"\n" {
		t.Fatalf("environment = %q", env)
	}
	var gc guestConfig
	raw, _ := os.ReadFile(filepath.Join(dir, "workspace.json"))
	if err := json.Unmarshal(raw, &gc); err != nil || gc.Owner != "alice" || gc.URL != c.URL || gc.AgentPort != 5002 {
		t.Fatalf("workspace.json = %s", raw)
	}
}

func TestSecretsShare(t *testing.T) {
	tmp := t.TempDir()
	c := testConfig()
	c.SecretsDir = filepath.Join(tmp, "secrets")
	os.MkdirAll(filepath.Join(c.SecretsDir, "git"), 0700)
	os.WriteFile(filepath.Join(c.SecretsDir, "environment"), []byte("ANTHROPIC_API_KEY=sk-1"), 0600)
	os.WriteFile(filepath.Join(c.SecretsDir, "git", "credentials"), []byte("https://u:p@git"), 0600)
	extra := filepath.Join(tmp, "extra.env")
	os.WriteFile(extra, []byte("OPENAI_API_KEY=sk-2\n"), 0600)
	c.EnvironmentFiles = []string{extra}
	share := filepath.Join(tmp, "share")
	if err := writeSecretsShare(c, share); err != nil {
		t.Fatal(err)
	}
	env, _ := os.ReadFile(filepath.Join(share, "environment"))
	// A secrets file without a final newline must not swallow the next variable.
	if string(env) != "ANTHROPIC_API_KEY=sk-1\nOPENAI_API_KEY=sk-2\n" {
		t.Fatalf("environment = %q", env)
	}
	if got, _ := os.ReadFile(filepath.Join(share, "git", "credentials")); string(got) != "https://u:p@git" {
		t.Fatalf("credentials = %q", got)
	}
	fi, _ := os.Stat(filepath.Join(share, "git", "credentials"))
	if fi.Mode().Perm()&0222 != 0 {
		t.Fatalf("secret copy is writable: %v", fi.Mode())
	}

	c.SecretsDir, c.EnvironmentFiles = filepath.Join(tmp, "missing"), nil
	if err := writeSecretsShare(c, filepath.Join(tmp, "empty")); err != nil {
		t.Fatalf("a missing secrets directory must leave an empty share: %v", err)
	}
	c.EnvironmentFiles = []string{filepath.Join(tmp, "nope.env")}
	if err := writeSecretsShare(c, filepath.Join(tmp, "broken")); err == nil {
		t.Fatal("a missing environment file was ignored")
	}
}

func TestGrowFileNeverShrinks(t *testing.T) {
	path := filepath.Join(t.TempDir(), "disk.raw")
	os.WriteFile(path, make([]byte, 4096), 0600)
	if err := growFile(path, 1<<20); err != nil {
		t.Fatal(err)
	}
	if fi, _ := os.Stat(path); fi.Size() != 1<<20 {
		t.Fatalf("size = %d", fi.Size())
	}
	if err := growFile(path, 4096); err != nil {
		t.Fatal(err)
	}
	if fi, _ := os.Stat(path); fi.Size() != 1<<20 {
		t.Fatalf("shrunk to %d", fi.Size())
	}
}
