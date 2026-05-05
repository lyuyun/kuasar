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
    admin::TemplateAdminServer,
    args,
    cloud_hypervisor::{factory::CloudHypervisorVMFactory, hooks::CloudHypervisorHooks},
    config::Config,
    sandbox::KuasarSandboxer,
    version,
};

#[tokio::main]
async fn main() {
    vmm_common::panic::set_panic_hook();
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    let config: Config<_> = Config::load_config(&args.config).await.unwrap();

    let log_level = args.log_level.unwrap_or(config.sandbox.log_level());
    let service_name = "kuasar-vmm-sandboxer-clh-service";
    trace::set_enabled(config.sandbox.enable_tracing);
    trace::setup_tracing(&log_level, service_name).unwrap();
    vmm_sandboxer::utils::start_watchdog();
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        log::error!("failed to send ready notify: {}", e);
    }

    let template_pool_cfg = config.template_pool;

    let mut sandboxer: KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> =
        KuasarSandboxer::new(
            config.sandbox,
            config.hypervisor,
            CloudHypervisorHooks::default(),
        );

    if let Some(pool_cfg) = template_pool_cfg {
        sandboxer
            .init_template_pool(pool_cfg.store_dir, pool_cfg.max_per_key.unwrap_or(10))
            .await
            .unwrap_or_else(|e| log::error!("failed to init template pool: {}", e));
    }

    // Spawn the admin API server if the template pool is configured.
    if let Some(handle) = sandboxer.admin_handle() {
        let admin_sock = args.admin.clone();
        tokio::spawn(async move {
            TemplateAdminServer::new(handle, admin_sock).serve().await;
        });
    }

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
