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

use std::path::Path;

use clap::Parser;
use vmm_common::{signal, trace};
use vmm_sandboxer::{
    args,
    cloud_hypervisor::{
        appliance_factory::CloudHypervisorApplianceVMFactory,
        appliance_hooks::CloudHypervisorApplianceHooks, factory::CloudHypervisorVMFactory,
        hooks::CloudHypervisorHooks,
    },
    config::Config,
    sandbox::{KuasarSandboxer, SandboxMode},
    version,
};

#[tokio::main]
async fn main() {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    let config = Config::load_config(&args.config).await.unwrap();

    let log_level = args.log_level.unwrap_or(config.sandbox.log_level());
    trace::set_enabled(config.sandbox.enable_tracing);

    match config.sandbox.mode {
        SandboxMode::Appliance => {
            let service_name = "kuasar-vmm-sandboxer-clh-appliance-service";
            trace::setup_tracing(&log_level, service_name).unwrap();

            let mut sandboxer: KuasarSandboxer<
                CloudHypervisorApplianceVMFactory,
                CloudHypervisorApplianceHooks,
            > = KuasarSandboxer::new(
                config.sandbox,
                config.hypervisor,
                CloudHypervisorApplianceHooks::default(),
            );

            tokio::spawn(async move {
                signal::handle_signals(&log_level, service_name).await;
            });

            if Path::new(&args.dir).exists() {
                sandboxer.recover(&args.dir).await;
            }

            containerd_sandbox::run(
                "kuasar-vmm-sandboxer-clh-appliance",
                &args.listen,
                &args.dir,
                sandboxer,
            )
            .await
            .unwrap();
        }
        SandboxMode::Standard => {
            let service_name = "kuasar-vmm-sandboxer-clh-service";
            trace::setup_tracing(&log_level, service_name).unwrap();

            let mut sandboxer: KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> =
                KuasarSandboxer::new(
                    config.sandbox,
                    config.hypervisor,
                    CloudHypervisorHooks::default(),
                );

            tokio::spawn(async move {
                signal::handle_signals(&log_level, service_name).await;
            });

            if Path::new(&args.dir).exists() {
                sandboxer.recover(&args.dir).await;
            }

            containerd_sandbox::run(
                "kuasar-vmm-sandboxer-clh",
                &args.listen,
                &args.dir,
                sandboxer,
            )
            .await
            .unwrap();
        }
    }
}
