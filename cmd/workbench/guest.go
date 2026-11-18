package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"github.com/OSSystems/vmkit-nix/control"
	"github.com/OSSystems/vmkit-nix/hostutil"
	"github.com/OSSystems/vmkit-nix/vsock"
)

// agentRequest is what the host asks the guest agent over vsock. Only the
// host reaches a guest's vsock listener.
type agentRequest struct {
	Op   string   `json:"op"`
	Args []string `json:"args,omitempty"`
}

type agentReply struct {
	Stdout   string `json:"stdout,omitempty"`
	Stderr   string `json:"stderr,omitempty"`
	ExitCode int    `json:"exitCode"`
	Error    string `json:"error,omitempty"`
}

const guestApp = "/run/workspace/config/app"

// runGuestAgent runs inside the guest as root: it sets memory hotplug up,
// reports memory to the host and answers the host's requests.
func runGuestAgent(configPath string) error {
	raw, err := hostutil.ReadBounded(configPath, 1<<20)
	if err != nil {
		return err
	}
	var gc guestConfig
	if err := json.Unmarshal(raw, &gc); err != nil {
		return err
	}
	configureHotplug()
	go reportStats(uint32(gc.ControlPort))
	ln, err := vsock.Listen(uint32(gc.AgentPort))
	if err != nil {
		return err
	}
	defer ln.Close()
	for {
		conn, err := ln.Accept()
		if err != nil {
			return err
		}
		go serveAgent(conn, gc)
	}
}

// configureHotplug onlines memory the host plugs as movable, so it can be
// unplugged again.
func configureHotplug() {
	for path, value := range map[string]string{
		"/sys/devices/system/memory/auto_online_blocks": "online_movable",
	} {
		if err := os.WriteFile(path, []byte(value), 0); err != nil {
			fmt.Fprintf(os.Stderr, "%s: %v\n", path, err)
		}
	}
}

func reportStats(port uint32) {
	for ; ; time.Sleep(2 * time.Second) {
		meminfo, err := os.ReadFile("/proc/meminfo")
		if err != nil {
			continue
		}
		pressure, _ := os.ReadFile("/proc/pressure/memory")
		req, err := control.StatsRequest(string(meminfo), string(pressure))
		if err != nil {
			continue
		}
		raw, _ := json.Marshal(req)
		conn, err := vsock.Dial(vsock.CIDHost, port)
		if err != nil {
			continue
		}
		conn.SetDeadline(time.Now().Add(2 * time.Second))
		if _, err := conn.Write(append(raw, '\n')); err == nil {
			bufio.NewReader(conn).ReadString('\n')
		}
		conn.Close()
	}
}

func serveAgent(conn io.ReadWriteCloser, gc guestConfig) {
	defer conn.Close()
	line, err := control.ReadLine(bufio.NewReader(conn), 64<<10)
	var reply agentReply
	var req agentRequest
	switch {
	case err != nil:
		reply.Error = err.Error()
	case json.Unmarshal(line, &req) != nil:
		reply.Error = "invalid JSON"
	default:
		reply = answerAgent(req, gc)
	}
	raw, _ := json.Marshal(reply)
	conn.Write(append(raw, '\n'))
}

func answerAgent(req agentRequest, gc guestConfig) agentReply {
	switch req.Op {
	case "ping":
		return agentReply{}
	case "pair":
		return pairAsOwner(gc, req.Args)
	}
	return agentReply{Error: "unknown op"}
}

// pairArgv is the pairing command for a workspace: the owner's T3 Code
// server is reached at the workspace URL.
func pairArgv(gc guestConfig, args []string) []string {
	return append([]string{filepath.Join(guestApp, "bin", "t3"), "auth", "pairing", "create", "--base-url", gc.URL, "--label", "workbench"}, args...)
}

// pairAsOwner creates a pairing credential as the owner, against the T3
// Code state the owner's server uses.
func pairAsOwner(gc guestConfig, args []string) agentReply {
	u, err := user.Lookup(gc.Owner)
	if err != nil {
		return agentReply{Error: err.Error()}
	}
	uid, _ := strconv.Atoi(u.Uid)
	gid, _ := strconv.Atoi(u.Gid)
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	argv := pairArgv(gc, args)
	cmd := exec.CommandContext(ctx, argv[0], argv[1:]...)
	cmd.Dir = u.HomeDir
	cmd.Env = []string{
		"HOME=" + u.HomeDir,
		"USER=" + gc.Owner,
		"LOGNAME=" + gc.Owner,
		"T3CODE_HOME=" + filepath.Join(u.HomeDir, ".t3"),
		"PATH=" + filepath.Join(guestApp, "sw", "bin") + ":/run/wrappers/bin:/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin",
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{Credential: &syscall.Credential{Uid: uint32(uid), Gid: uint32(gid)}}
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err = cmd.Run()
	reply := agentReply{Stdout: stdout.String(), Stderr: stderr.String()}
	var exit *exec.ExitError
	switch {
	case errors.As(err, &exit):
		reply.ExitCode = exit.ExitCode()
	case err != nil:
		reply.Error = err.Error()
	}
	return reply
}
