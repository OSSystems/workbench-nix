package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// destroy deletes nothing unless the workspace name is typed back.
func TestDestroyNeedsConfirmation(t *testing.T) {
	root := t.TempDir()
	disk := filepath.Join(root, "bob", "disk.raw")
	os.MkdirAll(filepath.Dir(disk), 0700)
	os.WriteFile(disk, []byte("data"), 0600)
	var out bytes.Buffer
	for _, answer := range []string{"", "alice\n", "bo\n", "yes\n"} {
		err := destroy([]string{t.TempDir()}, root, "bob", false, strings.NewReader(answer), &out)
		if err == nil || !strings.Contains(err.Error(), "not confirmed") {
			t.Fatalf("answer %q: %v", answer, err)
		}
	}
	if _, err := os.Stat(disk); err != nil {
		t.Fatal("disk deleted without confirmation")
	}
	if !strings.Contains(out.String(), "Type the workspace name") {
		t.Fatalf("prompt = %q", out.String())
	}
	if err := destroy(nil, root, "../etc", true, nil, &out); err == nil {
		t.Fatal("destroyed a path outside the state root")
	}
	if err := destroy(nil, root, "carol", true, nil, &out); err == nil || !strings.Contains(err.Error(), "no data") {
		t.Fatalf("destroying an unknown workspace: %v", err)
	}
}
