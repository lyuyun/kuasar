# Copyright 2026 The Kuasar Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Build: docker build -t example/llm-server:latest python/
#
# WarmFork requirements for this image:
#   - The workload process must be PID 1 so SIGTERM is delivered correctly.
#   - The inject socket parent directory must be writable by the process user.
#   - No background daemon should own the inject socket path before the
#     workload reaches Phase 3.
#
# Socket directory: /run is owned by root (755) on standard images and is not
# writable by non-root users. This example runs as root so it can create the
# socket at the protocol-default path /run/warmfork-readiness.sock. For production
# deployments, either run as root or set WARMFORK_READINESS_SOCKET to a directory
# the workload user owns (e.g. /tmp/warmfork-readiness.sock), and set the
# kuasar.io/warm-fork-readiness-socket annotation to match.
FROM python:3.12-slim

WORKDIR /app

# Copy protocol client and application in two layers so that changes to the
# application code do not invalidate the (heavier) base-layer cache.
COPY warmfork.py .
COPY llm_server.py .

# Default environment — all can be overridden via Pod env: or ConfigMap.
ENV MODEL_NAME=gpt2-simulated \
    LISTEN_ADDR=:8080 \
    WARMFORK_READINESS_SOCKET=/run/warmfork-readiness.sock

EXPOSE 8080

# Use exec form so the Python process becomes PID 1 and receives SIGTERM.
CMD ["python3", "llm_server.py"]
