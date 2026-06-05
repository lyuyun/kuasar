// Copyright 2026 The Kuasar Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Package warmfork implements the WarmFork readiness and injection protocol client (v1).
//
// Protocol flow per connection:
//
//	workload → CAPABILITIES
//	sandboxer → PREPARE  (injection mode)
//	workload → READY     (injection mode; no external side effects before this point)
//	sandboxer → COMMIT   (injection mode after READY, or directly in autonomous mode)
//	workload → STARTED   (formal commit point; execution begins after this)
//
// Copy this package into your workload and call [WaitForInjection] at the
// end of your quiescent phase (Phase 3). See the protocol specification at
// docs/proposals/warmfork_readiness_and_injection_protocol.md for the full description.
package warmfork

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"os"
)

const (
	// DefaultSocketPath is the Unix socket path used when no override is set.
	DefaultSocketPath = "/run/warmfork-readiness.sock"

	// ProtocolVersion is the protocol version declared in CAPABILITIES.
	ProtocolVersion = "1"

	maxFrameLen = 4 * 1024 * 1024 // 4 MiB
)

// TaskParams contains the per-task parameters delivered by the sandboxer after
// the VM is restored from a WarmFork template.
type TaskParams struct {
// TaskID is the unique identifier for this task instance; it is empty in
// autonomous mode.
	TaskID string
	// EnvOverrides is a map of environment variable overrides to apply before
	// starting task execution.
	EnvOverrides map[string]string
	// Context is an opaque string for the workload (e.g. serialised prompt).
	Context string
}

// ApplyEnvOverrides sets each key/value pair from EnvOverrides into the current
// process environment.
func (p *TaskParams) ApplyEnvOverrides() {
	for k, v := range p.EnvOverrides {
		os.Setenv(k, v)
	}
}

// WaitForInjection opens the inject socket, runs the WarmFork protocol, and
// returns the task parameters once STARTED has been sent to the sandboxer.
// In autonomous mode the returned TaskID is empty.
//
// This function blocks until restore commits. The caller MUST NOT open network
// connections, start worker goroutines, or execute non-idempotent operations
// before this function returns — those side effects must be deferred until
// after the function returns (i.e. after STARTED has been sent).
//
// socketPath defaults to [DefaultSocketPath] if empty.
func WaitForInjection(socketPath string) (*TaskParams, error) {
	if socketPath == "" {
		socketPath = DefaultSocketPath
	}

	// Remove any stale socket from a previous run (e.g. a crashed instance).
	os.Remove(socketPath)

	ln, err := net.Listen("unix", socketPath)
	if err != nil {
		return nil, fmt.Errorf("warmfork: listen %s: %w", socketPath, err)
	}
	defer ln.Close()

	log.Printf("[warmfork] listening on %s", socketPath)

	for {
		conn, err := ln.Accept()
		if err != nil {
			return nil, fmt.Errorf("warmfork: accept: %w", err)
		}

		params, injected, err := handleConn(conn)
		conn.Close()

		if err != nil {
			log.Printf("[warmfork] connection error: %v; continuing", err)
			continue
		}
		if injected {
			return params, nil
		}
		// Probe connection: sandboxer closed after reading CAPABILITIES.
		// Loop back to accept() — state must be equivalent to pre-probe READY.
		log.Printf("[warmfork] probe complete; back to READY")
	}
}

// ── Internal ─────────────────────────────────────────────────────────────────

type capabilitiesMsg struct {
	Type              string   `json:"type"`
	ProtocolVersion   string   `json:"protocol_version"`
	SupportedFeatures []string `json:"supported_features"`
}

// inMsg covers all sandboxer → workload messages: PREPARE, COMMIT, CANCEL.
type inMsg struct {
	Type         string            `json:"type"`
	TaskID       string            `json:"task_id"`       // PREPARE
	EnvOverrides map[string]string `json:"env_overrides"` // PREPARE
	Context      string            `json:"context"`       // PREPARE
	Reason       string            `json:"reason"`        // CANCEL
}

type outMsg struct {
	Type    string `json:"type"`
	Reason  string `json:"reason,omitempty"`
	Message string `json:"message,omitempty"`
}

func handleConn(conn net.Conn) (*TaskParams, bool, error) {
	// Step 1: send CAPABILITIES — first message on every connection.
	err := writeMsg(conn, capabilitiesMsg{
		Type:              "CAPABILITIES",
		ProtocolVersion:   ProtocolVersion,
		SupportedFeatures: []string{"prepare", "commit", "cancel"},
	})
	if err != nil {
		return nil, false, fmt.Errorf("write CAPABILITIES: %w", err)
	}

	// Step 2: read PREPARE, COMMIT, CANCEL, or EOF (probe).
	msg, err := readMsg[inMsg](conn)
	if err != nil {
		if err == io.EOF || err == io.ErrUnexpectedEOF {
			return nil, false, nil // probe: sandboxer closed after reading CAPABILITIES
		}
		return nil, false, fmt.Errorf("read sandboxer message: %w", err)
	}

	switch msg.Type {
	case "PREPARE":
		return handlePrepare(conn, &msg)
	case "COMMIT":
		return handleAutonomous(conn)
	case "CANCEL":
		log.Printf("[warmfork] CANCEL received (reason=%q); cleaning up and exiting", msg.Reason)
		os.Exit(0)
		return nil, false, nil // unreachable
	default:
		return nil, false, fmt.Errorf("unexpected message type %q", msg.Type)
	}
}

func handlePrepare(conn net.Conn, msg *inMsg) (*TaskParams, bool, error) {
	// Validate PREPARE. All validation must complete here.
	// No external side effects may have occurred before READY is sent.
	// This is a protocol-violation defense: PREPARE in injection mode must
	// carry a non-empty task_id. Autonomous mode never reaches this branch
	// because it skips PREPARE entirely.
	if msg.TaskID == "" {
		_ = writeMsg(conn, outMsg{
			Type:    "REJECT",
			Reason:  "invalid_task_id",
			Message: "task_id is absent or empty",
		})
		return nil, false, fmt.Errorf("PREPARE has empty task_id; sent REJECT")
	}

	// Send READY — commit to no side effects up to this point.
	// Sandboxer will wait until all targets reply READY before sending COMMIT.
	if err := writeMsg(conn, outMsg{Type: "READY"}); err != nil {
		return nil, false, fmt.Errorf("write READY: %w", err)
	}

	// Wait for COMMIT or CANCEL.
	commit, err := readMsg[inMsg](conn)
	if err != nil {
		return nil, false, fmt.Errorf("read COMMIT/CANCEL: %w", err)
	}
	switch commit.Type {
	case "COMMIT":
		// Send STARTED before beginning execution. This is the formal commit point:
		// the sandboxer considers the restore committed on receipt of STARTED.
		if err := writeMsg(conn, outMsg{Type: "STARTED"}); err != nil {
			return nil, false, fmt.Errorf("write STARTED: %w", err)
		}
		return &TaskParams{
			TaskID:       msg.TaskID,
			EnvOverrides: msg.EnvOverrides,
			Context:      msg.Context,
		}, true, nil
	case "CANCEL":
		log.Printf("[warmfork] CANCEL after READY (reason=%q); cleaning up and exiting", commit.Reason)
		os.Exit(0)
		return nil, false, nil // unreachable
	default:
		return nil, false, fmt.Errorf("expected COMMIT or CANCEL, got %q", commit.Type)
	}
}

func handleAutonomous(conn net.Conn) (*TaskParams, bool, error) {
	// Autonomous mode: the sandboxer sends COMMIT directly, so the workload
	// self-starts without receiving a task identity.
	if err := writeMsg(conn, outMsg{Type: "STARTED"}); err != nil {
		return nil, false, fmt.Errorf("write STARTED: %w", err)
	}
	return &TaskParams{}, true, nil
}

// readMsg reads one length-prefixed JSON message from r.
func readMsg[T any](r io.Reader) (T, error) {
	var zero T
	var lenBuf [4]byte
	if _, err := io.ReadFull(r, lenBuf[:]); err != nil {
		return zero, err
	}
	n := binary.BigEndian.Uint32(lenBuf[:])
	if n > maxFrameLen {
		return zero, fmt.Errorf("frame too large: %d bytes", n)
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(r, body); err != nil {
		return zero, err
	}
	var v T
	return v, json.Unmarshal(body, &v)
}

// writeMsg writes one length-prefixed JSON message to w.
func writeMsg(w io.Writer, v any) error {
	body, err := json.Marshal(v)
	if err != nil {
		return err
	}
	var lenBuf [4]byte
	binary.BigEndian.PutUint32(lenBuf[:], uint32(len(body)))
	if _, err := w.Write(lenBuf[:]); err != nil {
		return err
	}
	_, err = w.Write(body)
	return err
}
