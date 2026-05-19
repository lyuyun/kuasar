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

// WarmFork example: Go HTTP inference service.
//
// This program simulates a model-serving HTTP service adapted for WarmFork.
// It loads a model and warms up an in-memory cache during Phase 1, reaches
// a quiescent state in Phase 2, waits for WarmFork restore commit in Phase 3,
// and then starts serving the restored workload instance in Phase 4.
//
// Typical deployment:
//
//	$ WARMFORK_READINESS_SOCKET=/run/warmfork-readiness.sock ./inference-service
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"example/warmfork-workload/warmfork"
)

// ── Simulated model ──────────────────────────────────────────────────────────

type model struct {
	name    string
	weights []float32 // placeholder for actual model weights
}

func loadModel(name string) *model {
	log.Printf("[phase1] loading model %q ...", name)
	// In a real workload: load from disk, initialise GPU context, etc.
	time.Sleep(3 * time.Second) // simulate slow model load
	weights := make([]float32, 1024*1024)
	for i := range weights {
		weights[i] = float32(i) * 0.001
	}
	log.Printf("[phase1] model %q loaded (%d params)", name, len(weights))
	return &model{name: name, weights: weights}
}

func (m *model) warmup() {
	log.Printf("[phase1] running %d warm-up inference passes...", 5)
	for i := range 5 {
		time.Sleep(100 * time.Millisecond) // simulate inference
		log.Printf("[phase1]   warm-up pass %d/5 complete", i+1)
	}
}

func (m *model) infer(input string) string {
	// Simulate inference — replace with real model call.
	time.Sleep(50 * time.Millisecond)
	return fmt.Sprintf("result from model %q for input %q", m.name, input)
}

// ── Background worker (Phase 1 only) ────────────────────────────────────────

type metricsCollector struct {
	wg     sync.WaitGroup
	cancel context.CancelFunc
}

func startMetricsCollector(ctx context.Context) *metricsCollector {
	ctx, cancel := context.WithCancel(ctx)
	mc := &metricsCollector{cancel: cancel}
	mc.wg.Add(1)
	go func() {
		defer mc.wg.Done()
		ticker := time.NewTicker(10 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				log.Println("[metrics] collecting metrics...")
			case <-ctx.Done():
				log.Println("[metrics] collector stopped")
				return
			}
		}
	}()
	return mc
}

func (mc *metricsCollector) stop() {
	mc.cancel()
	mc.wg.Wait()
}

// ── HTTP handler ─────────────────────────────────────────────────────────────

type server struct {
	taskID string
	mdl    *model
}

func (s *server) handleInfer(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Input string `json:"input"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	result := s.mdl.infer(req.Input)
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{
		"task_id": s.taskID,
		"result":  result,
	})
}

func (s *server) handleHealth(w http.ResponseWriter, _ *http.Request) {
	w.WriteHeader(http.StatusOK)
}

// ── Main ─────────────────────────────────────────────────────────────────────

func main() {
    socketPath := os.Getenv("WARMFORK_READINESS_SOCKET")

	// ── Phase 1: Heavy initialisation ───────────────────────────────────────
	// Threads, I/O, and network connections are all permitted here.
	modelName := os.Getenv("MODEL_NAME")
	if modelName == "" {
		modelName = "default-model"
	}
	mdl := loadModel(modelName)
	mdl.warmup()

	ctx := context.Background()
	metrics := startMetricsCollector(ctx)

	// ── Phase 2: Reach quiescent state ──────────────────────────────────────
	// Stop all background goroutines. Close all outbound connections.
	// Flush any in-flight writes. Install deferred signal handlers.
	log.Println("[phase2] stopping background workers...")
	metrics.stop()
	// In a real workload: db.Close(), grpcConn.Close(), s3Client.Close(), ...

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)

	log.Println("[phase2] quiescent state reached")

	// ── Phase 3: Open inject socket and block ────────────────────────────────
	// The VM snapshot is taken while this process is suspended in accept().
	// No external side effects until WaitForInjection returns.
	log.Println("[phase3] waiting for WarmFork restore...")
	params, err := warmfork.WaitForInjection(socketPath)
	if err != nil {
		log.Fatalf("[phase3] injection failed: %v", err)
	}

	// ── Phase 4: Post-restore execution ─────────────────────────────────────
	// STARTED has been sent; restore is committed. Apply task identity if the
	// restore ran in injection mode and serve.
	params.ApplyEnvOverrides()
	if params.TaskID != "" {
		log.Printf("[phase4] injected: task_id=%q context=%q", params.TaskID, params.Context)
	} else {
		log.Printf("[phase4] autonomous restore (no task identity)")
	}
	displayTaskID := params.TaskID
	if displayTaskID == "" {
		displayTaskID = "autonomous"
	}

	srv := &server{taskID: params.TaskID, mdl: mdl}
	mux := http.NewServeMux()
	mux.HandleFunc("/infer", srv.handleInfer)
	mux.HandleFunc("/healthz", srv.handleHealth)

	listenAddr := os.Getenv("LISTEN_ADDR")
	if listenAddr == "" {
		listenAddr = ":8080"
	}

	httpSrv := &http.Server{Addr: listenAddr, Handler: mux}
	go func() {
		log.Printf("[phase4] serving task %s on %s", displayTaskID, listenAddr)
		if err := httpSrv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Printf("[phase4] server error: %v", err)
		}
	}()

	// Wait for shutdown signal
	sig := <-sigCh
	log.Printf("[phase4] received %v; shutting down task %s", sig, displayTaskID)
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	httpSrv.Shutdown(shutdownCtx)
}
