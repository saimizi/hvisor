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

use alloc::sync::Arc;
use spin::rwlock::RwLock;

use super::{PciConfigAccessStatus, VpciDeviceHandler};
use crate::cpu_data::this_zone;
use crate::memory::MMIOAccess;
use crate::pci::config_accessors::PciRegion;
use crate::pci::pci_access::{
    BaseClass, DeviceId, DeviceRevision, EndpointField, Interface, PciMemType, SubClass, VendorId,
};
use crate::pci::pci_struct::{ArcRwLockVirtualPciConfigSpace, CapabilityType, PciCapability};
use crate::pci::vpci_dev::virtio_cap::{
    BarAreaManager, MAPTI_INTERCEPTOR, MsixCap, MsixTable, VirtioISRCap, VirtioNotifyCap, VirtioPciCap, VirtioPciCommonCfg, Virtqueue
};
use crate::pci::PciConfigAddress;
use crate::{error::HvResult, pci::pci_struct::VirtualPciConfigSpace};

macro_rules! arc_rwlock {
    ($val:expr) => {
        Arc::new(RwLock::new($val))
    };
}

pub struct VirtioRngHandler;

impl VpciDeviceHandler for VirtioRngHandler {
    fn read_cfg(
        &self,
        space: ArcRwLockVirtualPciConfigSpace,
        offset: PciConfigAddress,
        size: usize,
    ) -> HvResult<PciConfigAccessStatus> {
        match EndpointField::from(offset as usize, size) {
            EndpointField::ID => Ok(PciConfigAccessStatus::Done(0x0000_0000_1044_1af4)),
            EndpointField::Bar(n) => {
                // info!("Bar read:{}", n);
                let bar = &space.get_bararr()[n];
                if n == 0 {
                    // info!("Bar 0 has been read!");
                    return Ok(PciConfigAccessStatus::Done(0x0000_0001));
                }
                if bar.get_size_read() {
                    return Ok(PciConfigAccessStatus::Done(bar.get_size() as usize));
                } else {
                    // info!("Bar read virtual:{:x}", bar.get_virtual_value());
                    return Ok(PciConfigAccessStatus::Done(bar.get_virtual_value() as usize));
                }
            }
            EndpointField::CapabilityPointer => {
                // info!("Cap ptr read!");
                return Ok(PciConfigAccessStatus::Done(0x74));
            }
            EndpointField::Command => {
                // info!("cmd read!");
                return Ok(PciConfigAccessStatus::Done(0x0010_0406));
            }
            EndpointField::RevisionIDAndClassCode => {
                return Ok(PciConfigAccessStatus::Done(0xff00_0000));
            }
            EndpointField::Unknown(x) => {
                warn!("----unknown read!!!:0x{:x}----", x);
                return Ok(PciConfigAccessStatus::Default);
            }
            _ => Ok(PciConfigAccessStatus::Default),
        }
    }

    fn write_cfg(
        &self,
        space: ArcRwLockVirtualPciConfigSpace,
        offset: PciConfigAddress,
        size: usize,
        value: usize,
    ) -> HvResult<PciConfigAccessStatus> {
        // info!("virt pci standard write_cfg, offset {:#x}, size {:#x}, value {:#x}", offset, size, value);
        match EndpointField::from(offset as usize, size) {
            EndpointField::ID => Ok(PciConfigAccessStatus::Reject),
            EndpointField::Bar(n) => {
                if value == 0xffff_ffff {
                    space.write().set_bar_size_read(n);
                    Ok(PciConfigAccessStatus::Done(0x0))
                } else if value == 0x0 {
                    Ok(PciConfigAccessStatus::Done(0x0))
                } else {
                    let b = &space.get_bararr()[n];
                    let zone = this_zone();
                    let mut guard = zone.write();
                    guard.mmio_region_register(
                        value,
                        b.get_size() as usize,
                        rng_mmio_handler,
                        value,
                    );
                    drop(guard);
                    space.write().clear_bar_size_read(n);
                    space.with_bar_ref_mut(n, |bar| {
                        bar.set_virtual_value(value as u64);
                    });
                    Ok(PciConfigAccessStatus::Done(0x0))
                }
            }
            _ => Ok(PciConfigAccessStatus::Default),
        }
    }

    fn vdev_init(&self, mut dev: VirtualPciConfigSpace) -> VirtualPciConfigSpace {
        let id: (DeviceId, VendorId) = (0x1044, 0x1af4);
        let revision: DeviceRevision = 0xFFu8;
        let base_class: BaseClass = 0x0;
        let sub_class: SubClass = 0x0;
        let interface: Interface = 0x0;
        dev.with_config_value_mut(|config_value| {
            config_value.set_id(id);
            config_value.set_class_and_revision_id((base_class, sub_class, interface, revision));
        });
        dev.with_bararr_mut(|bararr| {
            bararr[1].config_init(PciMemType::Mem32, false, 0x4000, 0x0);
            bararr[4].config_init(PciMemType::Mem64Low, true, 0x10000, 0x0);
            bararr[5].config_init(PciMemType::Mem64High, true, 0x0, 0x0);
        });

        let commoncfg_cap = arc_rwlock!(VirtioPciCap::new(
            super::virtio_cap::VirtioCfgType::CommonCfg,
            0,
            0x10,
            0x04,
            0x0,
            0x1000
        ));
        let commoncfg = PciCapability::new_virt(commoncfg_cap);
        let commoncfg_bar = arc_rwlock!(VirtioPciCommonCfg::new());

        let isr_cap = arc_rwlock!(VirtioPciCap::new(
            super::virtio_cap::VirtioCfgType::IsrCfg,
            0x40,
            0x10,
            0x04,
            0x1000,
            0x1000,
        ));
        let isr = PciCapability::new_virt(isr_cap);
        let isr_bar: Arc<RwLock<VirtioISRCap>> = Arc::new(RwLock::new(VirtioISRCap::new()));
        isr_bar.write().set_isr(1);

        let notify_cap = arc_rwlock!(VirtioNotifyCap::new(0x50, 0x2000, 0x1000));
        let notify = PciCapability::new_virt(notify_cap.clone());

        let msix_cap = arc_rwlock!(MsixCap::new(0x60, 0x10));
        let msix = PciCapability::new_cap(CapabilityType::MsiX, msix_cap);
        let msix_table: Arc<RwLock<MsixTable>> = Arc::new(RwLock::new(MsixTable::new(
            0x10,
            dev.get_bdf().requester_id() as usize,
        )));

        dev.with_cap_mut(|capabilities| {
            capabilities.insert_cap(0x40, commoncfg);
            capabilities.insert_cap(0x50, isr);
            capabilities.insert_cap(0x60, notify);
            capabilities.insert_cap(0x74, msix);
        });

        dev.with_cap_mut(|capabilities| {
            capabilities.register_bar_area(0x04, 0x0000, 0x1000, commoncfg_bar.clone());
            capabilities.register_bar_area(0x04, 0x1000, 0x1000, isr_bar.clone());
            capabilities.register_bar_area(0x04, 0x2000, 0x1000, notify_cap.clone());
            capabilities.register_bar_area(0x01, 0x0000, 0x800, msix_table.clone());
        });

        dev.with_access_mut(|access| {
            access.set_bits(0x34..0x38);
            access.set_bits(0x40..0x50);
            access.set_bits(0x50..0x60);
            access.set_bits(0x60..0x70);
            access.set_bits(0x70..0x90);
        });

        let vq: Arc<RwLock<Virtqueue>> = arc_rwlock!(Virtqueue::new(msix_table.clone(), 0));
        commoncfg_bar.write().insert_queue(vq.clone());
        notify_cap.write().insert_queue(vq);

        unsafe {
            MAPTI_INTERCEPTOR = Some(msix_table.clone());
        }
        dev
    }
}

pub const HANDLER: VirtioRngHandler = VirtioRngHandler;

pub fn rng_mmio_handler(mmio: &mut MMIOAccess, base: usize) -> HvResult {
    let zone = this_zone();
    let zone_lock = zone.read();
    let bus = zone_lock.vpci_bus();
    let (mut dev, mut bar) = (None, 0);
    for (_, i) in bus.read_devs() {
        if let Some(res) = i.is_my_bar_addr(base) {
            dev = Some(i.clone());
            bar = res;
            break;
        }
    }
    if let Some(found_dev) = dev {
        return found_dev.bar_mmio_distribute(bar, mmio);
    }
    Ok(())
}

#[derive(Debug)]
pub struct RngPCIDevice {
    basic:Arc<RwLock<BasicConfig>>,
    bar:Arc<RwLock<BarAreaManager>>,
}

#[derive(Debug)]
pub struct BasicConfig{
    // pub vendor_id:u16,
    // pub device_id:u16,
    pub id:u32,
    pub command:u16,
    pub status:u16,
    pub revision_and_class:u32,
    // cache_line_size:u8,
    // latency_time:u8,
    // header_type:u8,
    // bist:u8,
    // pub card_cis_pointer:u32,
    pub subsystem_vendor_id:u16,
    pub subsystem_id:u16,
    // bar:BarAreaManager,
    // expansion_rom_bar:u32,
    pub capability_pointer:u8,
    // interrupt_line:u8,
    // interrupt_pin:u8,
    // min_gnt:u8,
    // max_lat:u8,

}

impl PciRegion for RngPCIDevice{
    fn read_u8(&self, offset: PciConfigAddress) -> HvResult<u8> {
        match EndpointField::from(offset as usize, 1){
            EndpointField::CapabilityPointer => {
                Ok(self.basic.read().capability_pointer)
            }
            _=>{
                warn!("This u8 read has not been implement:{:?}",offset);
                Ok(0)
            }
        }
    }

    fn write_u8(&self, offset: PciConfigAddress, value: u8) -> HvResult {
        match EndpointField::from(offset as usize, 1){
            _=>{ 
                warn!("This u8 write has not been implement:{:?}",offset);
                Ok(())
            }
        }
    }

    fn read_u16(&self, offset: PciConfigAddress) -> HvResult<u16> {
        match EndpointField::from( offset as usize, 2){
            EndpointField::Command => {
                Ok(self.basic.read().command)
            }
            EndpointField::Status => {
                Ok(self.basic.read().status)
            }
            EndpointField::SubsystemId => {
                Ok(self.basic.read().subsystem_id)
            }
            EndpointField::SubsystemVendorId => {
                Ok(self.basic.read().subsystem_vendor_id)
            }
            _=>{
                warn!("This u16 read has not been implement:{:?}",offset);
                Ok(0)
            }
        }
    }

    fn write_u16(&self, offset: PciConfigAddress, value: u16) -> HvResult {
        match EndpointField::from(offset as usize, 2){
            EndpointField::Command => {
                self.basic.write().command = value;
                Ok(())
            }
            EndpointField::Status => {
                self.basic.write().status = value;
                Ok(())
            }
            _=>{
                warn!("This u16 write has not been implement:{:?}",offset);
                Ok(())
            }
        }
    }

    fn read_u32(&self, offset: PciConfigAddress) -> HvResult<u32> {
        match EndpointField::from(offset as usize, 4){
            EndpointField::ID => {
                Ok(self.basic.read().id)
            }
            EndpointField::RevisionIDAndClassCode => {
                Ok(self.basic.read().revision_and_class)
            }
            EndpointField::Bar(n) => {
                Ok(0)
            }
            _=>{
                warn!("This u32 read has not been implement:{:?}",offset);
                Ok(0)
            }
        }
    }

    fn write_u32(&self, offset: PciConfigAddress, value: u32) -> HvResult {
        match EndpointField::from(offset as usize, 4){
            EndpointField::Bar(n)=>{
                Ok(())
            }
            _=>{
                warn!("This u32 write has not been implement:{:?}",offset);
                Ok(())
            }
        }
    }

}