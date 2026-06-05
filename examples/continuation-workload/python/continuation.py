#!/usr/bin/env python3
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

"""Continuation workload identity helper.

Reads the annotations used by the continuation example:

- ``kuasar.io/pod-uid``
- ``kuasar.io/workload-generation``

The helper is intentionally tiny so it can be copied into a real service.
"""

from __future__ import annotations

import os

from dataclasses import dataclass

POD_UID_ANNOTATION = "kuasar.io/pod-uid"
WORKLOAD_GENERATION_ANNOTATION = "kuasar.io/workload-generation"


@dataclass(frozen=True)
class WorkloadIdentity:
    pod_uid: str
    generation: int

    @property
    def key(self) -> str:
        return f"cont:{self.pod_uid}:{self.generation}"


def load_identity_from_env() -> WorkloadIdentity | None:
    """Load workload identity from environment variables.

    The example uses env vars so it can run without Kubernetes. In a real pod,
    read these values from the downward API or injected config.
    """
    pod_uid = os.environ.get("KUASAR_POD_UID", "").strip()
    generation = os.environ.get("KUASAR_WORKLOAD_GENERATION", "").strip()
    if not pod_uid:
        return None
    try:
        gen = int(generation or "0")
    except ValueError:
        raise ValueError("KUASAR_WORKLOAD_GENERATION must be an integer")
    return WorkloadIdentity(pod_uid=pod_uid, generation=gen)
