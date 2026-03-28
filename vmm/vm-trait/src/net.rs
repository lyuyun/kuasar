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

//! Network helpers shared by all VMM backend crates.

use std::os::unix::io::{AsRawFd, OwnedFd};

/// Open `queue` tap file descriptors for the named interface using TUNSETIFF.
///
/// All VMM backends (Cloud Hypervisor, QEMU, StratoVirt) need to open tap fds
/// before passing them to the VMM process. This is a synchronous blocking call
/// that is safe to make from a sync context (`add_network` is `fn`, not `async fn`).
pub fn open_tap_fds(tap_name: &str, queue: u32) -> anyhow::Result<Vec<OwnedFd>> {
    const TUNSETIFF: libc::c_ulong = 0x400454ca;
    const TUNSETPERSIST: libc::c_ulong = 0x400454cb;
    const IFF_TAP: u16 = 0x0002;
    const IFF_NO_PI: u16 = 0x1000;
    const IFF_MULTI_QUEUE: u16 = 0x0100;
    const IFF_VNET_HDR: u16 = 0x4000;

    #[repr(C)]
    struct Ifreq {
        ifr_name: [u8; 16],
        ifr_flags: u16,
        _pad: [u8; 22],
    }

    if tap_name.len() > 15 {
        anyhow::bail!("tap name '{}' exceeds 15 characters", tap_name);
    }
    let mut ifr_name = [0u8; 16];
    ifr_name[..tap_name.len()].copy_from_slice(tap_name.as_bytes());
    let req = Ifreq {
        ifr_name,
        ifr_flags: IFF_TAP | IFF_NO_PI | IFF_MULTI_QUEUE | IFF_VNET_HDR,
        _pad: [0; 22],
    };

    let count = queue.max(1) as usize;
    let mut fds = Vec::with_capacity(count);
    for _ in 0..count {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| anyhow::anyhow!("open /dev/net/tun: {}", e))?;
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF, &req as *const _) };
        if ret < 0 {
            return Err(anyhow::anyhow!(
                "TUNSETIFF on '{}': {}",
                tap_name,
                std::io::Error::last_os_error()
            ));
        }
        fds.push(OwnedFd::from(file));
    }
    if !fds.is_empty() {
        let persist: u64 = 1;
        let ret = unsafe { libc::ioctl(fds[0].as_raw_fd(), TUNSETPERSIST, &persist as *const _) };
        if ret < 0 {
            // Non-fatal: tap interface already exists and persist is best-effort
            let _ = (tap_name, std::io::Error::last_os_error());
        }
    }
    Ok(fds)
}
