use alloc::{sync::Arc, vec::Vec};
use spin::RwLock;

use crate::{error::HvResult, memory::MMIOAccess, pci::vpci_dev::virtio_cap::VirtioPciCommonCfg};

pub enum CapBarArea {
    VirtioCommonCfg,
}

type OffsetInBar = usize;
type AreaSize = usize;

static mut VIRTIO_CAP_COMMON_CFG: Vec<(OffsetInBar, AreaSize, Arc<RwLock<VirtioPciCommonCfg>>)> =
    Vec::new();

pub fn virtio_common_cfg_handler(mmio_ac: &mut MMIOAccess, base: usize) -> HvResult {
    warn!("hi common_cfg_handler!{:?},base:0x{:x}", mmio_ac, base);
    Ok(())
}

// pub fn handler_data_init(cap_type:CapBarArea,offset_in_bar:usize,area_size:usize){
//     match cap_type {
//         CapBarArea::VirtioCommonCfg => {
//             let cfg_struct = VirtioPciCommonCfg::new();
//             let locked = Arc::new(RwLock::new(cfg_struct));
//             VIRTIO_CAP_COMMON_CFG.push((offset_in_bar,area_size,locked));
//         }
//     }
// }
