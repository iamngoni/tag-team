//! Best-effort host capacity measurements for node heartbeats.
//!
//! Only metrics available from the local operating system are reported; unsupported battery,
//! thermal, toolchain, repository, and running-command details use explicit empty defaults.

use sysinfo::{Disks, System};

use crate::HeartbeatSource;

#[derive(Default)]
pub struct HostHeartbeat;

impl HeartbeatSource for HostHeartbeat {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        let cores = system.cpus().len();
        let free_memory = system.available_memory();
        let disks = Disks::new_with_refreshed_list();
        let free_disk = disks
            .list()
            .iter()
            .map(|disk| disk.available_space())
            .max()
            .unwrap_or(0);
        let load = System::load_average().one;
        let cores_free = if cores == 0 {
            0.0
        } else {
            (cores as f32 - load as f32).clamp(0.0, cores as f32)
        };
        tagteam_proto::v1::Heartbeat {
            capacity: Some(tagteam_proto::v1::Capacity {
                cores_total: u32::try_from(cores).unwrap_or(u32::MAX),
                cores_free,
                memory_free: free_memory,
                disk_free: free_disk,
                running: 0,
                on_battery: false,
                thermal: String::new(),
            }),
            presence: tagteam_proto::v1::OwnerPresence::Away as i32,
            toolchains: Vec::new(),
            repos: Vec::new(),
            running: Vec::new(),
        }
    }
}
