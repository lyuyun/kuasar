/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Cgroup-v1 management for VMM sandboxes.
//!
//! Ported from `vmm/sandbox/src/cgroup.rs`.  On cgroup-v2 unified-mode systems
//! all operations gracefully no-op (guarded at call sites with
//! `cgroups_rs::hierarchies::is_cgroup2_unified_mode()`).

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use cgroups_rs::{
    cgroup_builder::CgroupBuilder, cpu::CpuController, cpuset::CpuSetController,
    hugetlb::HugeTlbController, memory::MemController, Cgroup,
};
use containerd_sandbox::{cri::api::v1::LinuxContainerResources, data::SandboxData};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CGROUP_PARENT_PATH: &str = "kuasar-vmm";
pub const VCPU_CGROUP_NAME: &str = "vcpu";
pub const POD_OVERHEAD_CGROUP_NAME: &str = "pod_overhead";

/// vCPU thread IDs, used for placing vcpu threads in the correct cgroup.
/// Maps vCPU index → thread ID (tid).
#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>,
}

/// Host-side cgroup set for a sandbox (sandbox cgroup, vcpu cgroup, pod-overhead cgroup).
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct SandboxCgroup {
    pub cgroup_parent_path: String,
    #[serde(skip)]
    pub sandbox_cgroup: Cgroup,
    #[serde(skip)]
    pub vcpu_cgroup: Cgroup,
    #[serde(skip)]
    pub pod_overhead_cgroup: Cgroup,
}

impl SandboxCgroup {
    /// Create the sandbox, vcpu, and pod-overhead cgroups under `cgroup_parent_path`.
    pub fn create_sandbox_cgroups(cgroup_parent_path: &str, sandbox_id: &str) -> Result<Self> {
        let sandbox_cgroup_path = format!("{}/{}", cgroup_parent_path, sandbox_id);
        let sandbox_cgroup_rela_path = sandbox_cgroup_path.trim_start_matches('/');

        let sandbox_cgroup =
            CgroupBuilder::new(sandbox_cgroup_rela_path).build(cgroups_rs::hierarchies::auto())?;

        let vcpu_cgroup_path = format!("{}/{}", sandbox_cgroup_rela_path, VCPU_CGROUP_NAME);
        let vcpu_cgroup = CgroupBuilder::new(&vcpu_cgroup_path)
            .set_specified_controllers(vec!["cpu".to_string()])
            .build(cgroups_rs::hierarchies::auto())?;

        let pod_overhead_cgroup_path =
            format!("{}/{}", sandbox_cgroup_rela_path, POD_OVERHEAD_CGROUP_NAME);
        let pod_overhead_cgroup = CgroupBuilder::new(&pod_overhead_cgroup_path)
            .set_specified_controllers(vec!["cpu".to_string()])
            .build(cgroups_rs::hierarchies::auto())?;

        Ok(SandboxCgroup {
            cgroup_parent_path: cgroup_parent_path.to_string(),
            sandbox_cgroup,
            vcpu_cgroup,
            pod_overhead_cgroup,
        })
    }

    /// Apply pod resource limits to the sandbox cgroup set.
    pub fn update_res_for_sandbox_cgroups(&self, sandbox_data: &SandboxData) -> Result<()> {
        if let Some(total_resources) = get_total_resources(sandbox_data) {
            apply_cpu_resource(&self.sandbox_cgroup, &total_resources)?;
            apply_memory_resource(&self.sandbox_cgroup, &total_resources)?;
            apply_cpuset_resources(&self.sandbox_cgroup, &total_resources)?;
            apply_hugetlb_resources(&self.sandbox_cgroup, &total_resources)?;
        }
        if let Some(containers_resources) = get_resources(sandbox_data) {
            apply_cpu_resource(&self.vcpu_cgroup, containers_resources)?;
        }
        if let Some(overhead_resources) = get_overhead_resources(sandbox_data) {
            apply_cpu_resource(&self.pod_overhead_cgroup, overhead_resources)?;
        }
        Ok(())
    }

    /// Add `pid` (and optionally its vCPU threads) to the sandbox cgroup set.
    pub fn add_process_into_sandbox_cgroups(
        &self,
        pid: u32,
        vcpu_threads: Option<VcpuThreads>,
    ) -> Result<()> {
        self.sandbox_cgroup.add_task_by_tgid((pid as u64).into())?;
        self.pod_overhead_cgroup
            .add_task_by_tgid((pid as u64).into())?;

        if let Some(all_vcpu_threads) = vcpu_threads {
            for (_, vcpu_thread_tid) in all_vcpu_threads.vcpus {
                self.vcpu_cgroup.add_task((vcpu_thread_tid as u64).into())?;
            }
        }
        Ok(())
    }

    /// Destroy the sandbox cgroup hierarchy.
    pub fn remove_sandbox_cgroups(&self) -> Result<()> {
        remove_sandbox_cgroup(&self.vcpu_cgroup)?;
        remove_sandbox_cgroup(&self.pod_overhead_cgroup)?;
        remove_sandbox_cgroup(&self.sandbox_cgroup)?;
        Ok(())
    }
}

// ── Resource helpers ──────────────────────────────────────────────────────────

fn get_resources(data: &SandboxData) -> Option<&LinuxContainerResources> {
    data.config
        .as_ref()
        .and_then(|c| c.linux.as_ref())
        .and_then(|l| l.resources.as_ref())
}

fn get_overhead_resources(data: &SandboxData) -> Option<&LinuxContainerResources> {
    data.config
        .as_ref()
        .and_then(|c| c.linux.as_ref())
        .and_then(|l| l.overhead.as_ref())
}

fn get_total_resources(data: &SandboxData) -> Option<LinuxContainerResources> {
    data.config
        .as_ref()
        .and_then(|c| c.linux.as_ref())
        .and_then(|l| {
            l.resources.as_ref()?;
            if l.overhead.is_none() {
                return l.resources.clone();
            }
            Some(merge_resources(
                l.resources.as_ref().unwrap(),
                l.overhead.as_ref().unwrap(),
            ))
        })
}

fn merge_resources(
    r1: &LinuxContainerResources,
    r2: &LinuxContainerResources,
) -> LinuxContainerResources {
    let oom_score_adj = r1.oom_score_adj.max(r2.oom_score_adj);

    let mut hugepage_limits = r1.hugepage_limits.clone();
    for h2 in &r2.hugepage_limits {
        let mut found = false;
        for l in &mut hugepage_limits {
            if l.page_size == h2.page_size {
                l.limit += h2.limit;
                found = true;
            }
        }
        if !found {
            hugepage_limits.push(h2.clone());
        }
    }

    let mut unified = r1.unified.clone();
    for (k, v) in &r2.unified {
        unified.entry(k.clone()).or_insert_with(|| v.clone());
    }

    let cpuset_cpus =
        merge_cpusets(&r1.cpuset_cpus, &r2.cpuset_cpus).unwrap_or_else(|_| r1.cpuset_cpus.clone());
    let cpuset_mems =
        merge_cpusets(&r1.cpuset_mems, &r2.cpuset_mems).unwrap_or_else(|_| r1.cpuset_mems.clone());

    LinuxContainerResources {
        cpu_period: r1.cpu_period,
        cpu_quota: if r2.cpu_period != 0 {
            r1.cpu_quota + r2.cpu_quota * r1.cpu_period / r2.cpu_period
        } else {
            r1.cpu_quota
        },
        cpu_shares: r1.cpu_shares + r2.cpu_shares,
        memory_limit_in_bytes: r1.memory_limit_in_bytes + r2.memory_limit_in_bytes,
        oom_score_adj,
        cpuset_cpus,
        cpuset_mems,
        hugepage_limits,
        unified,
        memory_swap_limit_in_bytes: r1.memory_swap_limit_in_bytes + r2.memory_swap_limit_in_bytes,
    }
}

fn merge_cpusets(cpus1: &str, cpus2: &str) -> Result<String> {
    let parts1 = cpuset_parts(cpus1)?;
    let parts2 = cpuset_parts(cpus2)?;
    let mut merged = vec![];
    for p1 in &parts1 {
        let mut base = *p1;
        for p2 in &parts2 {
            base = merge_cpuset_range(base, *p2);
        }
        merged.push(base);
    }
    for p2 in &parts2 {
        if !merged.iter().any(|m| cpuset_intersect(*m, *p2)) {
            merged.push(*p2);
        }
    }
    Ok(merged
        .into_iter()
        .map(cpuset_range_to_string)
        .collect::<Vec<_>>()
        .join(","))
}

fn cpuset_parts(cpuset: &str) -> Result<Vec<(u32, u32)>> {
    if cpuset.is_empty() {
        return Ok(vec![]);
    }
    cpuset.split(',').map(cpuset_one_part).collect()
}

fn cpuset_one_part(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.split('-').collect();
    let low = parts[0]
        .trim()
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid cpuset: {}", s))?;
    let high = if parts.len() == 2 {
        parts[1]
            .trim()
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid cpuset: {}", s))?
    } else {
        low
    };
    Ok((low, high))
}

fn merge_cpuset_range(base: (u32, u32), delta: (u32, u32)) -> (u32, u32) {
    if delta.1 < base.0 || delta.0 > base.1 {
        return base;
    }
    (base.0.min(delta.0), base.1.max(delta.1))
}

fn cpuset_intersect(a: (u32, u32), b: (u32, u32)) -> bool {
    !(b.1 < a.0 || b.0 > a.1)
}

fn cpuset_range_to_string(cpuset: (u32, u32)) -> String {
    if cpuset.0 == cpuset.1 {
        cpuset.0.to_string()
    } else {
        format!("{}-{}", cpuset.0, cpuset.1)
    }
}

// ── Controller helpers ────────────────────────────────────────────────────────

fn apply_cpu_resource(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let ctrl: &CpuController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No cpu controller attached!"))?;
    if res.cpu_period != 0 {
        ctrl.set_cfs_period(res.cpu_period.try_into()?)?;
    }
    if res.cpu_quota != 0 {
        ctrl.set_cfs_quota(res.cpu_quota)?;
    }
    if res.cpu_shares != 0 {
        ctrl.set_shares(res.cpu_shares.try_into()?)?;
    }
    Ok(())
}

fn apply_memory_resource(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let ctrl: &MemController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No memory controller attached!"))?;
    if res.memory_limit_in_bytes != 0 {
        ctrl.set_limit(res.memory_limit_in_bytes)?;
    }
    if res.memory_swap_limit_in_bytes != 0 {
        ctrl.set_memswap_limit(res.memory_swap_limit_in_bytes)?;
    }
    Ok(())
}

fn apply_cpuset_resources(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let ctrl: &CpuSetController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No cpuset controller attached!"))?;
    if !res.cpuset_cpus.is_empty() {
        ctrl.set_cpus(&res.cpuset_cpus)?;
    }
    if !res.cpuset_mems.is_empty() {
        ctrl.set_mems(&res.cpuset_mems)?;
    }
    Ok(())
}

fn apply_hugetlb_resources(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let ctrl: &HugeTlbController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No hugetlb controller attached!"))?;
    for h in &res.hugepage_limits {
        ctrl.set_limit_in_bytes(&h.page_size, h.limit)?;
    }
    Ok(())
}

fn remove_sandbox_cgroup(cgroup: &Cgroup) -> Result<()> {
    use std::error::Error;
    for tid in cgroup.tasks() {
        cgroup.move_task_to_parent(tid).unwrap_or_default();
    }
    if let Err(e) = cgroup.delete() {
        if e.kind() == &cgroups_rs::error::ErrorKind::RemoveFailed {
            if let Some(cause) = e.source() {
                if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
                    if ioe.kind() == std::io::ErrorKind::NotFound {
                        return Ok(());
                    }
                }
            }
        }
        return Err(e.into());
    }
    Ok(())
}
