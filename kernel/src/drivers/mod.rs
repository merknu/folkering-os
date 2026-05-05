//! Device drivers module

pub mod serial;
pub mod keyboard;
pub mod mouse;
pub mod pci;
pub mod virtio;
pub mod virtio_blk;
pub mod model_disk;
pub mod virtio_net;
pub mod virtio_gpu;
pub mod virtio_input;
pub mod cmos;
pub mod rng;
pub mod iqe;
pub mod telemetry;
pub mod iommu;
pub mod ac97;
pub mod msix;
pub mod nvme;
pub mod storage_bench;
