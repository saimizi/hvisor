// Copyright (c) 2025 Syswonder
// hvisor is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//     http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY KIND, EITHER
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO NON-INFRINGEMENT, MERCHANTABILITY OR
// FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.
//
// Syswonder Website:
//      https://www.syswonder.org
//
// Authors:
//

use core::{array::from_fn, fmt::Debug, sync::atomic::fence};

// use aarch64_cpu::registers::VTCR_EL2::SH0::Non;
use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use spin::{rwlock::RwLock, Lazy};

use crate::{
    arch::cpu::this_cpu_id,
    device::{
        irqchip::inject_irq,
        virtio_trampoline::{
            VirtioPCIConfigInfo, VirtioPCIDataInfo, VirtqueueAreaInfo, MAX_DEVS, VIRTIO_PCI_BRIDGE,
        },
    },
    error::HvResult,
    event::{send_event, IPI_EVENT_VIRTIO_PCI_CONFIG, IPI_EVENT_VIRTIO_PCI_DATA},
    hypercall::SGI_IPI_ID,
    memory::{GuestPhysAddr, MMIOAccess},
    pci::{
        pci_struct::PciCapabilityRegion,
        vpci_dev::virtio_queue::{AvailRing, DescriptorTable, VirtqUsed},
    },
};

pub type PciCapabilityHandler = fn(&mut MMIOAccess, usize) -> HvResult;

struct VirtioPCIInterface{
    
}

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTIO_F_VERSION_1: usize = 32;
pub static mut MAPTI_INTERCEPTOR: Option<Arc<RwLock<MsixTable>>> = None;
pub static mut VIRTIO_MSIX_MANAGER: Lazy<Arc<RwLock<VirtioPCIMsixManager>>> =
    Lazy::new(|| Arc::new(RwLock::new(VirtioPCIMsixManager::new())));

#[allow(unused_variables)]
pub unsafe fn virtio_pci_intercept_its(deviceid: usize, event_id: usize, intid: usize) {
    #[cfg(feature = "virtio_pci")]
    unsafe {
        if let Some(x) = MAPTI_INTERCEPTOR.clone() {
            x.write().intercept_its(deviceid, event_id, intid);
        }
    }
}

#[allow(unused_variables)]
pub unsafe fn virtio_pci_add_pending_data_req_id(data_req_id: u64) {
    #[cfg(feature = "virtio_pci")]
    unsafe {
        VIRTIO_MSIX_MANAGER
            .write()
            .add_pending_data_req_id(data_req_id);
    }
}

#[allow(unused_variables)]
pub unsafe fn virtio_pci_activate_all_pending_irq() {
    #[cfg(feature = "virtio_pci")]
    unsafe {
        VIRTIO_MSIX_MANAGER.write().activate_all_pending_irq();
    }
}

fn put_together(src: (u8, u8, u8, u8)) -> u32 {
    let a = (src.0 as u32) << 24 | (src.1 as u32) << 16 | (src.2 as u32) << 8 | (src.3 as u32);
    a
}

#[derive(Clone, Copy, Debug)]
pub enum VirtioCfgType {
    CommonCfg,
    NotifyCfg(u32),
    IsrCfg,
    DeviceCfg,
    PciCfg,
    SharedMemoryCfg,
    VendorCfg,
}

impl From<VirtioCfgType> for u8 {
    fn from(value: VirtioCfgType) -> Self {
        match value {
            VirtioCfgType::CommonCfg => 1,
            VirtioCfgType::NotifyCfg(_) => 2,
            VirtioCfgType::IsrCfg => 3,
            VirtioCfgType::DeviceCfg => 4,
            VirtioCfgType::PciCfg => 5,
            VirtioCfgType::SharedMemoryCfg => 8,
            VirtioCfgType::VendorCfg => 9,
        }
    }
}

/// Corresponding to the struct 'virtio_pci_cap' defined in virtio manual(virtio-v1.2-csd01)
#[derive(Debug)]
pub struct VirtioPciCap {
    cap_vndr: u8,
    cap_next: u8,
    cap_len: u8,
    cfg_type: VirtioCfgType,
    bar: u8,
    id: u8,
    padding: [u8; 2],
    offset: u32,
    length: u32,
}

impl PciCapabilityRegion for VirtioPciCap {
    fn read(
        &self,
        offset: crate::pci::PciConfigAddress,
        size: usize,
    ) -> crate::error::HvResult<u32> {
        // info!("read cap:{:x},size:{}", offset, size);
        if offset as usize % size != 0 {
            warn!("cap read is misalign!");
            return Ok(0);
        }

        // if the capability read is the notifycfg, then we have to provide a multiplier field in the capability space
        // Refer to virtio-v1.2-csd01(Session 4.1.4.4:Notification structure layout)
        if let VirtioCfgType::NotifyCfg(multiplier) = self.cfg_type {
            if (offset, size) == (16, 4) {
                return Ok(multiplier);
            }
        }
        // match self.cfg_type {
        //     VirtioCfgType::NotifyCfg(multiplier) => {
        //         if (offset, size) == (16, 4) {
        //             // warn!("read multiplier!!!");
        //             return Ok(multiplier);
        //         }
        //     }
        //     _ => (),
        // };
        if size == 1 {
            match offset {
                0 => return Ok(self.cap_vndr as u32),
                1 => return Ok(self.cap_next as u32),
                2 => return Ok(self.cap_len as u32),
                3 => return Ok(u8::from(self.cfg_type) as u32),
                4 => return Ok(self.bar as u32),
                5 => return Ok(self.id as u32),
                _ => {
                    warn!("read u8 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        if size == 2 {
            match offset {
                0 => return Ok(put_together((0, 0, self.cap_next, self.cap_vndr))),
                2 => return Ok(put_together((0, 0, self.cfg_type.into(), self.cap_len))),
                4 => return Ok(put_together((0, 0, self.id, self.bar))),
                _ => {
                    warn!("read u16 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        if size == 4 {
            match offset {
                0 => {
                    return Ok(put_together((
                        self.cfg_type.into(),
                        self.cap_len,
                        self.cap_next,
                        self.cap_vndr,
                    )))
                }
                4 => return Ok(put_together((0, 0, self.id, self.bar))),
                8 => return Ok(self.offset),
                12 => return Ok(self.length),
                _ => {
                    warn!("read u32 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        warn!("size is not any of 1,2,4!");
        Ok(0)
    }

    // All fields in virtio pci cap is read-only for guest, thus we need not to implement any write function
    fn write(
        &mut self,
        _offset: crate::pci::PciConfigAddress,
        _size: usize,
        _value: u32,
    ) -> crate::error::HvResult {
        Ok(())
    }

    // This is a dummy implement.It's not that the offset of this capability is 0
    fn get_offset(&self) -> crate::pci::PciConfigAddress {
        0
    }

    fn get_size(&self) -> usize {
        self.cap_len as usize
    }

    fn next_cap(&self) -> crate::error::HvResult<crate::pci::PciConfigAddress> {
        Ok(self.cap_next as u64)
    }
}

impl VirtioPciCap {
    pub fn new(
        config_type: VirtioCfgType,
        cap_next: u8,
        cap_len: u8,
        bar: u8,
        offset: u32,
        length: u32,
    ) -> Self {
        // According to virtio-v1.2-csd01, every capability specially defined by virtio specification is a vender-specific capability,thus the cap_vndr is 0x09
        // Currently, most usage of bar from virtio capabilities is 0x04, but it seems doesn't matter which bar you use
        // Some device types would use multiple capabilities of a certain type, this field is intended to distinguish these capabilities.
        // If you don't have multiple capabilities of a certain type in single device, this field is useless
        Self {
            cap_vndr: 0x09,
            cap_next,
            cap_len,
            cfg_type: config_type,
            bar,
            id: 0x00,
            padding: [0, 0],
            offset,
            length,
        }
    }
}

#[derive(Debug)]
pub struct Virtqueue {
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
    queue_notify_data: u16,
    queue_reset: u16,

    this_dev_id: u16,
    queue_id: u16,
    msix_table: Arc<RwLock<MsixTable>>,

    desc_table: Option<DescriptorTable>,
    used_area: Option<VirtqUsed>,
    avail_area: Option<AvailRing>,
    last_avail: usize,
}

impl Virtqueue {
    pub fn new(msix: Arc<RwLock<MsixTable>>, queue_id: u16) -> Self {
        Self {
            queue_size: 256,
            queue_msix_vector: 1,
            queue_enable: 1,
            queue_notify_off: 0,
            queue_desc: 0,
            queue_driver: 0,
            queue_device: 0,
            queue_notify_data: 0,
            queue_reset: 0,

            this_dev_id: MAX_DEVS as u16,
            queue_id,
            msix_table: msix,
            desc_table: None,
            used_area: None,
            avail_area: None,
            last_avail: 0,
        }
    }

    pub fn set_dev_id(&mut self, dev_id: u16) {
        self.this_dev_id = dev_id;
    }

    pub fn set_queue_id(&mut self, queue_id: u16) {
        self.queue_id = queue_id;
    }

    pub fn notify_driver(&self) {
        self.msix_table
            .read()
            .inject_irq(self.queue_msix_vector as usize);
    }

    pub fn get_msix_entry(&self) -> MsixTableEntry {
        self.msix_table
            .read()
            .get_entry(self.queue_msix_vector as usize)
    }

    pub fn register_interrupt(&self, data_info: VirtioPCIDataInfo) {
        unsafe {
            VIRTIO_MSIX_MANAGER
                .write()
                .insert(data_info, self.get_msix_entry());
        }
    }

    pub fn set_desc_area(&mut self) {
        let base = self.queue_desc as usize;
        match self.desc_table {
            Some(ref mut x) => {
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None => {
                let desc = DescriptorTable::new(base, self.queue_size as usize);
                self.desc_table = Some(desc);
            }
        }
    }

    pub fn set_avail_area(&mut self) {
        let base = self.queue_driver as usize;
        match self.avail_area {
            Some(ref mut x) => {
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None => {
                let avail = AvailRing::new(base, self.queue_size);
                self.avail_area = Some(avail);
            }
        }
    }

    pub fn set_used_area(&mut self) {
        let base = self.queue_device as usize;
        match self.used_area {
            Some(ref mut x) => {
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None => {
                let desc = VirtqUsed::new(base, self.queue_size);
                self.used_area = Some(desc);
            }
        }
    }

    // pub fn consume_avail_with_zero(&self) -> Option<()> {
    //     let avail_area = self.avail_area?;
    //     let used_area = self.used_area?;
    //     let desc_area = self.desc_table?;

    //     let avail_idx = avail_area.get_idx();
    //     let used_idx = used_area.get_idx();
    //     let queue_size = self.queue_size;
    //     for i in used_idx..avail_idx {
    //         let idx = i % queue_size;
    //         let avail_ring_content = avail_area.get_ring_content(idx as usize);
    //         let desc = desc_area.get(avail_ring_content as usize);
    //         unsafe {
    //             write_bytes(desc.addr as *mut u8, 'Z' as u8, desc.len as usize);
    //         }
    //         let used_item = VirtqUsedElem::new(avail_ring_content as u32, desc.len);
    //         used_area.write_ring(idx as usize, used_item);
    //     }

    //     used_area.set_idx(avail_idx);
    //     return Some(());
    // }

    pub fn get_area_info(&self) -> VirtqueueAreaInfo {
        VirtqueueAreaInfo::new(self.queue_desc, self.queue_driver, self.queue_device)
    }

    pub fn get_data_info(&self) -> VirtioPCIDataInfo {
        VirtioPCIDataInfo::new(self.this_dev_id, self.queue_id)
    }
}

#[derive(Debug)]
pub struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: (u32, u32),
    driver_feature_select: u32,
    driver_feature: (u32, u32),
    config_msix_vector: u16,
    num_queue: u16,
    device_status: u8,
    config_generation: u8,

    queue_select: usize,
    queue_list: Vec<Arc<RwLock<Virtqueue>>>,
    config_changed: bool,
}

impl VirtioPciCommonCfg {
    pub fn new() -> Self {
        let cfg = VirtioPciCommonCfg {
            device_feature_select: 0,
            device_feature: (0, 65),
            // device_feature:(0,1),
            driver_feature_select: 0,
            driver_feature: (0, 0),
            config_msix_vector: 0x0,
            num_queue: 0,
            device_status: 0,
            config_generation: 0,
            queue_select: 0,
            queue_list: Vec::new(),
            config_changed: false,
        };
        cfg
    }

    pub fn insert_queue(&mut self, qu: Arc<RwLock<Virtqueue>>) {
        self.queue_list.push(qu);
        self.num_queue += 1;
    }
}

impl VirtioPciCommonCfg {
    // This function initialize the virtqueue in root linux
    pub fn init_virtqueue_shared_space(&self) {
        // info!("queue_info:{:x?}", queue_info);
        let mut config_info = VirtioPCIConfigInfo::dummy();
        config_info.set_features(self.get_features());
        config_info.set_dev_id(MAX_DEVS as u16);
        config_info.set_dtype(4);
        config_info.set_num_of_queues(self.num_queue);
        for i in 0..self.num_queue as usize {
            config_info.set_vqs(i, self.queue_list[i].read().get_area_info());
        }
        VIRTIO_PCI_BRIDGE.lock().write_dev_info(config_info);
        send_event(0, SGI_IPI_ID as usize, IPI_EVENT_VIRTIO_PCI_CONFIG);
        let dev_id = VIRTIO_PCI_BRIDGE.lock().til_config_finish();
        for i in self.queue_list.iter() {
            i.write().set_dev_id(dev_id);
        }
    }

    fn get_features(&self) -> u64 {
        (self.driver_feature.1 as u64) << 32 | (self.driver_feature.0 as u64)
    }

    pub fn write_into(&mut self, mmio_ac: &MMIOAccess) -> bool {
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        let value = mmio_ac.value;
        if size == 1 {
            match addr {
                0x14 => {
                    // we use FEATURES_OK to confirm that initialization is completed
                    if value & 0x04 != 0 {
                        self.init_virtqueue_shared_space();
                    }
                    self.device_status = value as u8;
                    return true;
                }
                _ => {
                    warn!("write:size is misalign!");
                    return false;
                }
            }
        }

        if size == 2 {
            match addr {
                0x10 => {
                    self.config_msix_vector = value as u16;
                    return true;
                }
                0x16 => {
                    self.queue_select = value;
                    return true;
                }
                0x18 => {
                    self.queue_list[self.queue_select].write().queue_size = value as u16;
                    return true;
                }
                0x1a => {
                    // info!(
                    //     "queue No.{} has msix vector: 0x{:x}",
                    //     self.queue_select, value
                    // );
                    self.queue_list[self.queue_select].write().queue_msix_vector = value as u16;
                    return true;
                }
                0x1c => {
                    self.queue_list[self.queue_select].write().queue_enable = value as u16;
                }
                // 0x1e is read-only for driver
                _ => {
                    return false;
                }
            }
        }

        if size == 4 {
            match addr {
                0x00 => {
                    self.device_feature_select = value as u32;
                    return true;
                }
                0x08 => {
                    self.driver_feature_select = value as u32;
                    return true;
                }
                0x0c => {
                    if self.driver_feature_select == 0 {
                        self.driver_feature.0 = value as u32;
                    } else {
                        self.driver_feature.1 = value as u32;
                    }
                    return true;
                }
                0x20 => {
                    let mut queue = self.queue_list[self.queue_select].write();
                    queue.queue_desc = value as u64;
                    return true;
                }
                0x24 => {
                    let mut queue = self.queue_list[self.queue_select].write();
                    queue.queue_desc |= (value as u64) << 32;
                    queue.set_desc_area();
                    return true;
                }
                0x28 => {
                    self.queue_list[self.queue_select].write().queue_driver = value as u64;
                    return true;
                }
                0x2c => {
                    let mut queue = self.queue_list[self.queue_select].write();
                    queue.queue_driver |= (value as u64) << 32;
                    queue.set_avail_area();
                    return true;
                }
                0x30 => {
                    self.queue_list[self.queue_select].write().queue_device = value as u64;
                    return true;
                }
                0x34 => {
                    let mut queue = self.queue_list[self.queue_select].write();
                    queue.queue_device |= (value as u64) << 32;
                    queue.set_used_area();
                    return true;
                }
                _ => {
                    warn!("write:not implement yet!addr:{:x}", addr);
                    return false;
                }
            }
        }
        false
    }
}

impl AreaInBar for VirtioPciCommonCfg {
    fn read(&mut self, mmio_ac: &mut MMIOAccess) -> HvResult {
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        // info!(
        //     "----read in common cfg !!! addr:{:x},size:{:x}----",
        //     addr, size
        // );
        if size == 1 {
            match addr {
                0x14 => {
                    mmio_ac.value = self.device_status as usize;
                }
                0x15 => {
                    if self.config_changed {
                        self.config_generation += 1;
                    }
                    mmio_ac.value = self.config_generation as usize;
                }
                _ => {
                    warn!("read:size is misalign!");
                    return Ok(());
                }
            }
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
        }

        if size == 2 {
            match addr {
                0x10 => {
                    mmio_ac.value = self.config_msix_vector as usize;
                }
                0x12 => {
                    mmio_ac.value = self.num_queue as usize;
                }
                0x18 => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_size as usize;
                }
                0x1a => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_enable as usize;
                }
                0x1c => {
                    mmio_ac.value =
                        self.queue_list[self.queue_select].read().queue_notify_off as usize;
                }
                0x1e => {
                    mmio_ac.value =
                        self.queue_list[self.queue_select].read().queue_notify_off as usize;
                }
                _ => {
                    warn!("read:size is misalign!");
                    return Ok(());
                }
            }
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
        }

        if size == 4 {
            match addr {
                0x04 => {
                    if self.device_feature_select == 0 {
                        mmio_ac.value = self.device_feature.0 as usize;
                    } else {
                        mmio_ac.value = self.device_feature.1 as usize;
                    }
                }
                _ => {
                    warn!("read:not implement yet!addr:{:x}", mmio_ac.value);
                    return Ok(());
                }
            }
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
        }
        Ok(())
    }

    fn write(&mut self, mmio_ac: &MMIOAccess) -> HvResult {
        if self.write_into(mmio_ac) {
            self.config_changed = true;
            return Ok(());
        }
        warn!("the write has not reached the common config!");
        Ok(())
    }
}

/// Bar area is just a MMIO memory area
/// There are many virtio capability structures such as commoncfg being put in bar
/// Any structure put in bar has to implement this trait and be registered by function 'register_bar_area'
pub trait AreaInBar: Send + Sync + Debug{
    fn read(&mut self, mmio_ac: &mut MMIOAccess) -> HvResult;

    fn write(&mut self, mmio_ac: &MMIOAccess) -> HvResult;
}

#[derive(Debug)]
pub struct BarArea{
    size:usize,
    area:Vec<(GuestPhysAddr, usize, Arc<RwLock<dyn AreaInBar>>)>,
}

impl BarArea{
    pub fn new(size:usize)->Self{
        Self { size, area: Vec::new() }
    }

    pub fn set_size(&mut self,size:usize){
        self.size = size;
    }
}

/// This structure is responsible for mmio route
/// Capability A may share the same bar with Capability B.When a mmio is triggered, we need a router to decide which capability will handle this mmio.
#[derive(Debug)]
pub struct BarAreaManager {
    area: [Vec<(GuestPhysAddr, usize, Arc<RwLock<dyn AreaInBar>>)>; 6],
    // area: [BarArea; 6],
}

impl BarAreaManager {
    pub fn new() -> Self {
        BarAreaManager {
            area: from_fn(|_| Vec::new()),
            // area: from_fn(|_| BarArea::new(0)),
        }
    }

    pub fn set_bar_size(&mut self,bar:usize,size: usize){
        // self.area[bar].set_size(size);
    }

    pub fn insert(
        &mut self,
        bar: usize,
        addr: GuestPhysAddr,
        size: usize,
        area: Arc<RwLock<dyn AreaInBar>>,
    ) {
        self.area[bar].push((addr, size, area));
    }

    fn find_cap(
        &self,
        bar: usize,
        addr: GuestPhysAddr,
        size: usize,
    ) -> Option<&(GuestPhysAddr, usize, Arc<RwLock<dyn AreaInBar>>)> {
        let res = self.area[bar]
            .iter()
            .filter(|&e| e.0 <= addr && e.0 + e.1 >= addr + size)
            .max_by_key(|(k, _, _)| k);
        res
    }

    pub fn handle_bar_access(&self, bar: usize, mmio_ac: &mut MMIOAccess) -> HvResult {
        let target_cap = self.find_cap(bar, mmio_ac.address, mmio_ac.size);
        if let Some((_, _, area)) = target_cap {
            if mmio_ac.is_write {
                return area.write().write(mmio_ac);
            } else {
                return area.write().read(mmio_ac);
            }
        }
        warn!("we didn't find the access result!");
        Ok(())
    }
}

pub struct MsixCap {
    cap_id: u8,
    cap_next: u8,
    message_control: u16,
    table_bar: u8,
    table_offset: u32,
    pending_bar: u8,
    pending_offset: u32,
}

impl MsixCap {
    pub fn new(next: u8, table_size: u16) -> Self {
        let mut res = Self {
            cap_id: 0x11,
            cap_next: next,
            message_control: 0x0,
            table_bar: 0x01,
            table_offset: 0x0000_0000,
            pending_bar: 0x01,
            pending_offset: 0x0000_08000,
        };
        res.set_table_size(table_size);
        res
    }

    pub fn get_table_mesg(&self) -> u32 {
        (self.table_offset << 3) | (self.table_bar as u32)
    }

    pub fn get_pending_mesg(&self) -> u32 {
        (self.pending_offset << 3) | (self.pending_bar as u32)
    }

    pub fn set_table_mesg(&mut self, mesg: u32) {
        self.table_offset = mesg >> 3;
        self.table_bar = (mesg & 0x0000_0003) as u8;
    }
    pub fn set_pending_mesg(&mut self, mesg: u32) {
        self.pending_offset = mesg >> 3;
        self.pending_bar = (mesg & 0x0000_0003) as u8;
    }

    pub fn set_table_size(&mut self, size: u16) {
        if size > 2048 {
            warn!("msix table size cannot larger than 2048");
            return;
        }
        let mask = 0xf800;
        self.message_control &= mask;
        self.message_control |= size;
    }
}

impl PciCapabilityRegion for MsixCap {
    fn read(
        &self,
        offset: crate::pci::PciConfigAddress,
        size: usize,
    ) -> crate::error::HvResult<u32> {
        if offset as usize % size != 0 {
            warn!("cap read is misalign!");
            return Ok(0);
        }
        if size == 1 {
            match offset {
                0x00 => return Ok(self.cap_id as u32),
                0x01 => return Ok(self.cap_next as u32),
                _ => {
                    warn!("read u8 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        if size == 2 {
            match offset {
                0x00 => return Ok(self.cap_id as u32 | (self.cap_next as u32) << 8),
                0x02 => return Ok(self.message_control as u32),
                _ => {
                    warn!("read u16 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        if size == 4 {
            match offset {
                0x00 => {
                    return Ok((self.cap_id as u32)
                        | (self.cap_next as u32) << 8
                        | (self.message_control as u32) << 16)
                }
                0x04 => return Ok(self.get_table_mesg()),
                0x08 => return Ok(self.get_pending_mesg()),
                _ => {
                    warn!("read u32 from unexpected area! offset:{}", offset);
                    return Ok(0);
                }
            }
        };
        warn!("size is not any of 1,2,4!");
        Ok(0)
    }

    fn write(
        &mut self,
        offset: crate::pci::PciConfigAddress,
        size: usize,
        value: u32,
    ) -> crate::error::HvResult {
        if size == 1 {
            warn!("there is no writeable field with size 1!")
        }
        if size == 2 {
            match offset {
                0x02 => self.message_control = value as u16,
                _ => {
                    warn!("write into unexpected area! offset:{}", offset)
                }
            }
            return Ok(());
        }

        if size == 4 {
            match offset {
                0x04 => self.set_table_mesg(value),
                0x08 => self.set_pending_mesg(value),
                _ => {
                    warn!("write into unexpected area! offset:{}", offset)
                }
            }
            return Ok(());
        }
        Ok(())
    }

    fn get_offset(&self) -> crate::pci::PciConfigAddress {
        0
    }

    fn get_size(&self) -> usize {
        0x0c
    }

    fn next_cap(&self) -> crate::error::HvResult<crate::pci::PciConfigAddress> {
        Ok(self.cap_next as u64)
    }
}

#[derive(Clone, Debug)]
pub struct MsixTableEntry {
    pub message_address: u32,
    pub message_upper_address: u32,
    pub msg_data: u32,
    pub vector_control: u32,
    pub intid: Option<usize>,
}

impl MsixTableEntry {
    pub fn activate_irq(&self) {
        // info!("entry:{:x?}", self);
        match self.intid {
            Some(x) => {
                inject_irq(x, false);
            }
            None => {
                warn!("this msix vector has not gotten a intid:{:x?}", self);
            }
        }
    }
}

impl MsixTableEntry {
    pub fn dummy() -> Self {
        Self {
            message_address: 0,
            message_upper_address: 0,
            msg_data: 0,
            vector_control: 0,
            intid: None,
        }
    }
}

#[derive(Debug)]
pub struct MsixTable {
    table: Vec<MsixTableEntry>,
    device_id: usize,
    event_id: Vec<(usize, usize)>,
}

impl MsixTable {
    pub fn new(size: usize, deviceid: usize) -> Self {
        let mut vec = Vec::new();
        vec.resize(size, MsixTableEntry::dummy());
        Self {
            table: vec,
            device_id: deviceid,
            event_id: Vec::new(),
        }
    }

    pub fn inject_irq(&self, vector_index: usize) {
        self.table[vector_index].activate_irq();
    }

    pub fn get_entry(&self, vector_index: usize) -> MsixTableEntry {
        self.table[vector_index].clone()
    }

    pub fn intercept_its(&mut self, deviceid: usize, event_id: usize, intid: usize) {
        if deviceid == self.device_id {
            warn!("MAPTI's deviceid != current deviceid!");
        }
        self.event_id.push((event_id, intid));
    }

    pub fn init_msix_intid(&mut self, index: usize) {
        let selected_vector = &mut self.table[index];
        for i in self.event_id.iter() {
            if selected_vector.msg_data as usize == i.0 {
                selected_vector.intid = Some(i.1);
                break;
            }
        }
    }
}

impl AreaInBar for MsixTable {
    fn read(&mut self, mmio_ac: &mut MMIOAccess) -> HvResult {
        let offset = mmio_ac.address;
        // 16 is the size of entry
        let index = offset / 16;
        let offset_in_entry = offset % 16;
        match offset_in_entry {
            0x00 => mmio_ac.value = self.table[index].message_address as usize,
            0x04 => mmio_ac.value = self.table[index].message_upper_address as usize,
            0x08 => mmio_ac.value = self.table[index].msg_data as usize,
            0x0c => mmio_ac.value = self.table[index].vector_control as usize,
            _ => {
                warn!("access address is misalign!");
            }
        }
        Ok(())
    }

    fn write(&mut self, mmio_ac: &MMIOAccess) -> HvResult {
        if mmio_ac.size != 4 {
            warn!("only write with size of 4 would work correctly");
        }
        let offset = mmio_ac.address;
        let index = offset / 16;
        let offset_in_entry = offset % 16;
        let value = mmio_ac.value;
        match offset_in_entry {
            0x00 => self.table[index].message_address = value as u32,
            0x04 => self.table[index].message_upper_address = value as u32,
            0x08 => self.table[index].msg_data = value as u32,
            0x0c => self.table[index].vector_control = value as u32,
            _ => {
                warn!("access address is misalign!");
            }
        }
        self.init_msix_intid(index);
        Ok(())
    }
}

#[derive(Debug)]
pub struct VirtioISRCap {
    isr: u32,
}

impl VirtioISRCap {
    pub fn new() -> Self {
        Self { isr: 0 }
    }

    pub fn set_isr(&mut self, value: u32) {
        self.isr = value
    }
}

impl AreaInBar for VirtioISRCap {
    fn read(&mut self, mmio_ac: &mut MMIOAccess) -> HvResult {
        let offset = mmio_ac.address;
        match offset {
            0x00 => {
                mmio_ac.value = self.isr as usize;
            }
            _ => {
                warn!("illegal isr address");
            }
        };
        return Ok(());
    }

    fn write(&mut self, _mmio_ac: &MMIOAccess) -> HvResult {
        warn!("isr should not be write");
        return Ok(());
    }
}

#[derive(Debug)]
pub struct VirtioNotifyCap {
    cap: VirtioPciCap,
    queue_list: Vec<(usize, Arc<RwLock<Virtqueue>>)>,
}

impl VirtioNotifyCap {
    pub fn new(next: u8, offset: u32, length: u32) -> Self {
        let cap = VirtioPciCap::new(
            VirtioCfgType::NotifyCfg(0x04),
            next,
            0x14,
            0x04,
            offset,
            length,
        );
        Self {
            cap,
            queue_list: Vec::new(),
        }
    }

    pub fn insert_queue(&mut self, qu: Arc<RwLock<Virtqueue>>) {
        let queue_notify_off = qu.read().queue_notify_off as u32;
        if let VirtioCfgType::NotifyCfg(multiplier) = self.cap.cfg_type {
            let offset = self.cap.offset + queue_notify_off * multiplier;
            self.queue_list.push((offset as usize, qu));
        } else {
            error!("Notify cap has to have NotifyCfg type!");
        }
    }

    fn get_queues(&self, offset: usize) -> impl Iterator<Item = &Arc<RwLock<Virtqueue>>> {
        self.queue_list
            .iter()
            .filter(move |(x, _)| *x == offset)
            .map(|(_, res)| res)
    }
}

impl PciCapabilityRegion for VirtioNotifyCap {
    fn get_offset(&self) -> crate::pci::PciConfigAddress {
        self.cap.get_offset()
    }

    fn read(&self, offset: crate::pci::PciConfigAddress, size: usize) -> HvResult<u32> {
        self.cap.read(offset, size)
    }

    fn write(&mut self, offset: crate::pci::PciConfigAddress, size: usize, value: u32) -> HvResult {
        self.cap.write(offset, size, value)
    }

    fn get_size(&self) -> usize {
        self.cap.get_size()
    }

    fn next_cap(&self) -> HvResult<crate::pci::PciConfigAddress> {
        self.cap.next_cap()
    }
}

impl AreaInBar for VirtioNotifyCap {
    fn read(&mut self, _mmio_ac: &mut MMIOAccess) -> HvResult {
        warn!("Notify read has not been implemented yet");
        Ok(())
    }

    fn write(&mut self, mmio_ac: &MMIOAccess) -> HvResult {
        let offset = mmio_ac.address;
        for i in self.get_queues(offset) {
            // let info = VirtioPCIDataInfo::new(0, 0);
            let info = i.read().get_data_info();
            VIRTIO_PCI_BRIDGE.lock().write_data_info(info);
            i.read().register_interrupt(info);
            send_event(0, SGI_IPI_ID as usize, IPI_EVENT_VIRTIO_PCI_DATA);
            fence(core::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
}

pub struct VirtioPCIMsixManager {
    table: BTreeMap<u64, MsixTableEntry>,
    pending: Vec<u64>,
}

impl VirtioPCIMsixManager {
    pub const fn new() -> Self {
        Self {
            table: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    pub fn insert(&mut self, data_info: VirtioPCIDataInfo, entry: MsixTableEntry) {
        // info!("Msix Manager insert:{:x?}", data_info);
        let data_req_id = data_info.get_identifier();
        self.table.insert(data_req_id, entry);
    }

    pub fn add_pending_data_req_id(&mut self, data_req_id: u64) {
        // info!("pending data req id add:0x{:x}", data_req_id);
        self.pending.push(data_req_id);
    }

    pub fn activate_all_pending_irq(&mut self) {
        let cpu_id = (this_cpu_id() as u64) << 32;
        let mut target = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if (self.pending[i] & 0x0000_ffff_0000_0000) == cpu_id {
                target.push(self.pending.swap_remove(i));
            } else {
                i += 1;
            }
        }

        for j in target {
            self.activate_irq(j);
        }
    }

    pub fn activate_irq(&mut self, data_req_id: u64) {
        let entry = self.table.remove(&data_req_id);
        // info!(
        //     "irq activate!!! entry:{:x?},data_req_id:0x{:x}",
        //     entry, data_req_id
        // );
        match entry {
            Some(x) => {
                x.activate_irq();
            }
            None => {
                return;
            }
        }
    }
}
