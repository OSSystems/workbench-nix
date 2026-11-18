package main

import (
	"bufio"
	"encoding/json"
	"net"
	"path/filepath"
	"slices"
	"testing"
)

func TestPairArgv(t *testing.T) {
	gc := guestConfig{URL: "https://alice.ws.test"}
	got := pairArgv(gc, []string{"--json", "--base-url", "https://evil.test"})
	want := []string{"/run/workspace/config/app/bin/t3", "auth", "pairing", "create", "--base-url", "https://alice.ws.test", "--label", "workbench", "--json", "--base-url", "https://evil.test"}
	if !slices.Equal(got, want) {
		t.Fatalf("argv = %q", got)
	}
}

// fakeGuest stands in for cloud-hypervisor's host-side vsock socket with
// one guest agent behind it.
func fakeGuest(t *testing.T, port int, gc guestConfig) string {
	t.Helper()
	socket := filepath.Join(t.TempDir(), "vsock.sock")
	ln, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			r := bufio.NewReader(conn)
			line, _ := r.ReadString('\n')
			if line != "CONNECT 5002\n" {
				conn.Write([]byte("refused\n"))
				conn.Close()
				continue
			}
			conn.Write([]byte("OK 1073741824\n"))
			serveAgent(bufferedConn{r, conn}, gc)
		}
	}()
	return socket
}

func TestServeAgentRejectsGarbage(t *testing.T) {
	server, client := net.Pipe()
	go serveAgent(server, guestConfig{})
	client.Write([]byte("not json\n"))
	var reply agentReply
	if err := json.NewDecoder(client).Decode(&reply); err != nil || reply.Error == "" {
		t.Fatalf("reply %+v, %v", reply, err)
	}
}

// bufferedConn reads what a bufio.Reader already took from conn first.
type bufferedConn struct {
	r *bufio.Reader
	net.Conn
}

func (c bufferedConn) Read(p []byte) (int, error) { return c.r.Read(p) }
