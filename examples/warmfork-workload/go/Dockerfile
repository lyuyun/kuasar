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

# Build: docker build -t example/warmfork-go-service:latest go/
#
# Two-stage build: compile on golang:1.22, run on distroless/static for a
# minimal attack surface and a ~5 MB final image.
FROM golang:1.22 AS builder

WORKDIR /build

COPY go.mod .
COPY warmfork/ warmfork/
COPY main.go .

# CGO_ENABLED=0 produces a fully static binary compatible with distroless.
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -ldflags="-s -w" -o inference-service .

# ── Runtime image ─────────────────────────────────────────────────────────────
# Socket directory: /run is owned by root (755) and is not writable by
# non-root users. This example uses the default distroless image (root) so it
# can create the socket at /run/warmfork-readiness.sock. For production deployments,
# either keep root or set WARMFORK_READINESS_SOCKET to a directory the workload
# user owns (e.g. /tmp/warmfork-readiness.sock) and set
# kuasar.io/warm-fork-readiness-socket to match, then switch to :nonroot.
FROM gcr.io/distroless/static-debian12

COPY --from=builder /build/inference-service /inference-service

ENV MODEL_NAME=default-model \
    LISTEN_ADDR=:8080 \
    WARMFORK_READINESS_SOCKET=/run/warmfork-readiness.sock

EXPOSE 8080

ENTRYPOINT ["/inference-service"]
