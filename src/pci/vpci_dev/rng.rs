use alloc::sync::Arc;
use spin::rwlock::RwLock;

use crate::cpu_data::this_zone;
use crate::pci::vpci_dev::capability_handler::virtio_common_cfg_handler;
use crate::pci::vpci_dev::standard::mmio_vdev_standard_handler;
use crate::pci::vpci_dev::virtio_cap::{MAPTI_INTERCEPTOR, MsixCap, MsixTable, VirtioISRCap, VirtioNotifyCap, VirtioPciCap, VirtioPciCommonCfg, Virtqueue};
// use crate::percpu::this_zone;
use crate::{error::HvResult, pci::pci_struct::VirtualPciConfigSpace};
use crate::pci::pci_struct::{ArcRwLockVirtualPciConfigSpace, CapabilityType, PciCapability, PciCapabilityRegion};
use crate::pci::pci_access::{BaseClass, DeviceId, DeviceRevision, EndpointField, Interface, PciMemType, SubClass, VendorId};
use crate::pci::PciConfigAddress;
use super::{PciConfigAccessStatus, VpciDeviceHandler};
use crate::memory::frame::Frame;
use crate::pci::vpci_dev::{Bar, VirtMsiXCap};
use crate::memory::MMIOAccess;
/*
0000000 1af4 1044 0406 0010 0001 00ff 0008 0000
0000010 0000 0000 1000 1004 0000 0000 0000 0000
0000020 400c 0000 0080 0000 0000 0000 0000 0000
0000030 0000 0000 0098 0000 0000 0000 0127 0000
0000040 0009 0110 0004 0000 0000 0000 1000 0000
0000050 4009 0310 0004 0000 1000 0000 1000 0000
0000060 5009 0410 0004 0000 2000 0000 1000 0000
0000070 6009 0214 0004 0000 3000 0000 1000 0000
0000080 0004 0000 7009 0514 0000 0000 0000 0000
0000090 0000 0000 0000 0000 8411 8001 0001 0000
00000a0 0801 0000 0000 0000 0000 0000 0000 0000
00000b0 0000 0000 0000 0000 0000 0000 0000 0000
*
0000100

*/

const VIRTIO_RNG_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_RNG_DEVICE_ID: u16 = 0x1044;
const PCI_STS_CAPS: u16 = 0x0010; // bit 4
const RNG_REVISION: u8 = 0x01; 
const PCI_DEV_CLASS_OTHER: u32 = 0x00ff0000;
const PCI_CFG_CAPS: usize = 0x34;
const PCI_CAP_ID_VNDR: u8 = 0x09;
const PCI_CAP_ID_MSIX: u8 = 0x11;
const RNG_CFG_VNDR_CAP: u8 = 0x98;
const CAP_UNKONWN_POS:u8 = 0x84;
const CAP_UNKONWN_ID:u8 = 0x09;
const CAP_UNKONWN_U16:u16 = 0x0514;
const CAP_NOTIFY_POS:u8 = 0x70;
const CAP_NOTIFY_ID:u8 = 0x09;
const CAP_NOTIFY_U16:u16 = 0x0214;
const CAP_DEVICECFG_POS:u8 = 0x60;
const CAP_DEVICECFG_U16:u16 = 0x0410;
const CAP_DEVICECFG_ID:u8 = 0x09;
const CAP_ISR_POS:u8 = 0x50;
const CAP_ISR_U16:u16 = 0x0310;
const CAP_ISR_ID:u8 = 0x09;
const CAP_COMMONCFG_POS:u8 = 0x40;
const CAP_COMMONCFG_U16:u16 = 0x0110;
const CAP_COMMONCFG_ID:u8 = 0x09;
const CAP_MSIX_POS:u8 = 0x98;
const CAP_MSIX_ID:u8 = 0x11;
const CAP_MSIX_MSGCON:u16 = 8001;
// const STANDARD_CFG_VNDR_LEN: u8 = 0x20;
// const STANDARD_CFG_MSIX_CAP: usize = 0x60; // VNDR_CAP + VNDR_LEN
// const STANDARD_MSIX_VECTORS: u16 = 16;
const RNG_CFG_SIZE: usize = 0x100;

pub(crate) const DEFAULT_CSPACE_U32: [u32; RNG_CFG_SIZE / 4] = {
    let mut arr = [0u32; RNG_CFG_SIZE / 4];
    // DEVICE ID ----- VENDOR ID
    arr[0x00 / 4] = (VIRTIO_RNG_DEVICE_ID as u32) << 16 | VIRTIO_RNG_VENDOR_ID as u32;
    // Status ------ Command
    arr[0x04 / 4] = (PCI_STS_CAPS as u32) << 16;
    // Class ----- Revision ID
    arr[0x08 / 4] = PCI_DEV_CLASS_OTHER | (RNG_REVISION as u32);
    // Subsystem ID ----- Subsystem vendor ID
    arr[0x2c / 4] = (VIRTIO_RNG_DEVICE_ID as u32) << 16 | VIRTIO_RNG_VENDOR_ID as u32;
    // capability pointer = 0x98
    arr[PCI_CFG_CAPS / 4] = CAP_MSIX_POS as u32;
    // capability 0 = {id = MSIX;next_ptr = 0x84} 
    arr[CAP_MSIX_POS as usize / 4] = (CAP_MSIX_MSGCON as u32) << 16
        | (CAP_UNKONWN_POS as u32) << 8
        | CAP_MSIX_ID as u32;
    // capability 1 = {id = UNKONWN;next_ptr = 0x70}
    arr[CAP_UNKONWN_POS as usize / 4] = (CAP_UNKONWN_U16 as u32) << 16
        | (CAP_NOTIFY_POS as u32) << 8
        | (CAP_UNKONWN_ID as u32);
    // capability 2 = {id = NOTIFY;next_ptr = 0x60}
    arr[CAP_NOTIFY_POS as usize / 4] = (CAP_NOTIFY_U16 as u32) << 16
        | (CAP_DEVICECFG_POS as u32) << 8
        | (CAP_NOTIFY_ID as u32);
    // capability 3 = {id = DEVICECFG;next_ptr = 0x50}
    arr[CAP_DEVICECFG_POS as usize / 4] = (CAP_DEVICECFG_U16 as u32) << 16
        | (CAP_ISR_POS as u32) << 8
        | (CAP_DEVICECFG_ID as u32);
    // capability 4 = {id = ISR;next_ptr = 0x40}
    arr[CAP_ISR_POS as usize / 4] = (CAP_ISR_U16 as u32) << 16
        | (CAP_COMMONCFG_POS as u32) << 8
        | (CAP_ISR_ID as u32);
    // capability 5 = {id = COMMONCFG;next_ptr = 0x0}
    arr[CAP_COMMONCFG_POS as usize / 4] = (CAP_COMMONCFG_U16 as u32) << 16
        | (0x0) << 8
        | (CAP_COMMONCFG_ID as u32); 
    // arr[STANDARD_CFG_MSIX_CAP / 4] = (0x00u32) << 8 | PCI_CAP_ID_MSIX as u32;
    // arr[(STANDARD_CFG_MSIX_CAP + 0x4) / 4] = 1;
    // arr[(STANDARD_CFG_MSIX_CAP + 0x8) / 4] = ((0x10 * STANDARD_MSIX_VECTORS) as u32) | 1;
    arr
};

/// Handler for standard virtual PCI devices
pub struct VirtioRngHandler;

impl VpciDeviceHandler for VirtioRngHandler {

    fn read_cfg(&self, space: ArcRwLockVirtualPciConfigSpace, offset: PciConfigAddress, size: usize) -> HvResult<PciConfigAccessStatus> {
        // info!("virt pci standard read_cfg, offset {:#x}, size {:#x}", offset, size);
        // let mut space_guard = _space.write();
        match EndpointField::from(offset as usize, size) {
            EndpointField::ID => {
                Ok(PciConfigAccessStatus::Done(0x0000_0000_1044_1af4))
            }
            
            // EndpointField::CapabilityPointer =>{
            //     Ok(PciConfigAccessStatus::Done(_space.get(EndpointField::CapabilityPointer) as usize))
            // }
            EndpointField::Bar(n)=>{
                info!("Bar read:{}",n);
                let bar = &space.get_bararr()[n];
                if n == 0{
                    info!("Bar 0 has been read!");
                    return Ok(PciConfigAccessStatus::Done(0x0000_0001));
                }
                // return Ok(PciConfigAccessStatus::Done(0x0));
                if(bar.get_size_read()){
                    return Ok(PciConfigAccessStatus::Done(bar.get_size() as usize))
                }else{
                    // loop{}
                    info!("Bar read virtual:{:x}",bar.get_virtual_value());
                    // loop{}
                    return Ok(PciConfigAccessStatus::Done(bar.get_virtual_value() as usize))
                }
            }
            EndpointField::CapabilityPointer=>{
                info!("Cap ptr read!");
                return Ok(PciConfigAccessStatus::Done(0x74));
            }
            EndpointField::Command=>{
                info!("cmd read!");
                return Ok(PciConfigAccessStatus::Done(0x0010_0406));
            }
            EndpointField::RevisionIDAndClassCode=>{
                return Ok(PciConfigAccessStatus::Done(0xff00_0000));
            }
            EndpointField::Unknown(x)=>{
                warn!("----unknown read!!!:0x{:x}----",x);
                return Ok(PciConfigAccessStatus::Default);
            }
            _ => {
                Ok(PciConfigAccessStatus::Default)
            }
        }
    }

    fn write_cfg(&self, space: ArcRwLockVirtualPciConfigSpace, offset: PciConfigAddress, size: usize, value: usize) -> HvResult<PciConfigAccessStatus> {
        // info!("virt pci standard write_cfg, offset {:#x}, size {:#x}, value {:#x}", offset, size, value);
        // let mut space_guard = space.write();
        match EndpointField::from(offset as usize, size) {
            EndpointField::ID => {
                Ok(PciConfigAccessStatus::Reject)
            }
            // EndpointField::Command => {
            //     space.set(EndpointField::Command, value as u32);
            //     Ok(PciConfigAccessStatus::Done(value))
            // }
            // EndpointField::Bar0=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar0, 0xffff_f000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            // EndpointField::Bar1=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar1, 0xffff_f000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            // EndpointField::Bar2=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar2, 0xffff_f000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            // EndpointField::Bar3=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar3, 0xffff_f000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            // EndpointField::Bar4=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar4, 0xffff_c000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            // EndpointField::Bar5=>{
            //     if(value == 0xffff_ffff){
            //         space.set(EndpointField::Bar5, 0xffff_f000);
            //         Ok(PciConfigAccessStatus::Done(value))
            //     }else{
            //         Ok(PciConfigAccessStatus::Perform)
            //     }
            // }
            EndpointField::Bar(n)=>{
                if(value == 0xffff_ffff){
                    
                    space.write().set_bar_size_read(n);
                    Ok(PciConfigAccessStatus::Done(0x0))
                }else if value == 0x0 {
                    Ok(PciConfigAccessStatus::Done(0x0))   
                }
                else{
                    
                    let b = &space.get_bararr()[n];
                    let zone = this_zone();
                    let mut guard = zone.write();
                    // warn!("rng write mmio region");
                    guard.mmio_region_register(value , b.get_size() as usize, rng_mmio_handler, value);
                    drop(guard);
                    // warn!("done");
                    space.write().clear_bar_size_read(n); 
                    space.with_bar_ref_mut(n, |bar|{
                        bar.set_virtual_value(value as u64);
                    });
                    // let mut a =space.get_bararr();
                    // info!("write {} virtual:{:x}",n,value);
                    // a[n].set_virtual_value(value as u64);
                    // info!("bar:{:?}",b);
                    Ok(PciConfigAccessStatus::Done(0x0))
                }
            }
            _ => {
                Ok(PciConfigAccessStatus::Default)
            }
        }
    }


    fn vdev_init(&self, mut dev: VirtualPciConfigSpace) -> VirtualPciConfigSpace {
        // Set config_value
        let id: (DeviceId, VendorId) = (0x1044, 0x1af4);
        let revision: DeviceRevision = 0xFFu8;
        let base_class: BaseClass = 0x0;
        let sub_class: SubClass = 0x0;
        let interface: Interface = 0x0;
        dev.with_config_value_mut(|config_value| {
            config_value.set_id(id);
            config_value.set_class_and_revision_id((base_class, sub_class, interface, revision));
        });
        // let a = dev.get_bar_ref_mut(1);
        // // a.set_size(0x4000);
        // a.set_size(0xffff_c000);
        // a.set_bar_type(PciMemType::Mem32);
        // let b = dev.get_bar_ref_mut(4);
        // b.set_size(0xffff_0000);
        // b.set_bar_type(PciMemType::Mem64Low);
        // b.set_prefetchable(true);
        // let c = dev.get_bar_ref_mut(5);
        // c.set_size(0x0);
        // c.set_bar_type(PciMemType::Mem64High);
        // c.set_prefetchable(true);
        // // Set bararr   
        // let your_addr = 0x0;
        // let size = 0x1000;
        dev.with_bararr_mut(|bararr| {
            // bararr[0].config_init(PciMemType::Mem32, false, size as u64, your_addr);
            bararr[1].config_init(PciMemType::Mem32, false, 0x4000, 0x0);
            bararr[4].config_init(PciMemType::Mem64Low, true, 0x10000, 0x0);
            bararr[5].config_init(PciMemType::Mem64High, true, 0x0, 0x0);
        });

        let virtio_common_cap = VirtioPciCap::new(
            super::virtio_cap::VirtioCfgType::CommonCfg,0,0x10,0x0,0x1000,Some(virtio_common_cfg_handler));
        let virtio_isr_cap = VirtioPciCap::new(
            super::virtio_cap::VirtioCfgType::IsrCfg, 0x40,0x10, 0x1000, 0x1000,None);
        // let virtio_notify_cap = VirtioPciCap::new(
        //     super::virtio_cap::VirtioCfgType::NotifyCfg(0x04), 0x50,0x14, 0x2000, 0x1000,None);
        let virtio_notify_cap = VirtioNotifyCap::new(0x50,0x2000,0x1000);
        let locked_notify = Arc::new(RwLock::new(virtio_notify_cap));
        let msix_cap = MsixCap::new(0x60,0x10);
        // 0x98 is an arbitrary value, used here only for demonstration purposes
        // please don't forget to set next cap pointer if next cap exists
        // let msi_cap_offset = 0x98;
        // let mut msi_cap = VirtMsiXCap::new(msi_cap_offset);
        // msi_cap.set_next_cap_pointer(0x00);
        dev.with_access_mut(|access| {
            // access.set_bits(
            //     (msi_cap_offset as usize)..(msi_cap_offset as usize + msi_cap.get_size()) as usize,
            // );
            access.set_bits(0x40..0x50);
            access.set_bits(0x50..0x60);
            access.set_bits(0x60..0x70);
            access.set_bits(0x70..0x90);
        });
        let commcfg = Arc::new(RwLock::new(VirtioPciCommonCfg::new()));
        let msix_table:Arc<RwLock<MsixTable>> = Arc::new(RwLock::new(MsixTable::new(0x10,dev.get_bdf().requester_id() as usize)));
        unsafe {
            MAPTI_INTERCEPTOR = Some(msix_table.clone());
        }
        let isrcfg:Arc<RwLock<VirtioISRCap>> = Arc::new(RwLock::new(VirtioISRCap::new()));
        isrcfg.write().set_isr(1);
        let vq:Arc<RwLock<Virtqueue>> = Arc::new(RwLock::new(Virtqueue::new(msix_table.clone())));
        commcfg.write().insert_queue(vq.clone());
        locked_notify.write().insert_queue(vq);
        // let test = Arc::new(RwLock::new(VirtioPciCommonCfg::new()));
        dev.with_cap_mut(|capabilities| {
            // capabilities.insert(
            //     msi_cap_offset,
            //     PciCapability::new_virt(CapabilityType::MsiX, Arc::new(RwLock::new(msi_cap))),
            // );
            capabilities.insert_cap(0x40, 
                PciCapability::new_virt(CapabilityType::Vendor, Arc::new(RwLock::new(virtio_common_cap)))
            );
            capabilities.register_bar_area(0x04, 0x0000, 0x1000, commcfg);
            capabilities.insert_cap(0x50, 
                PciCapability::new_virt(CapabilityType::Vendor, Arc::new(RwLock::new(virtio_isr_cap)))
            );
            capabilities.register_bar_area(0x04, 0x1000, 0x1000, isrcfg);
            // capabilities.register_bar_area(0x04,0x1000,0x1000,test);
            capabilities.insert_cap(0x60, 
                PciCapability::new_virt(CapabilityType::Vendor,locked_notify.clone())
            );
            capabilities.register_bar_area(0x04, 0x2000, 0x1000, locked_notify);
            capabilities.insert_cap(0x74, 
                    PciCapability::new_virt(CapabilityType::MsiX, Arc::new(RwLock::new(msix_cap)))
            );
            capabilities.register_bar_area(0x01, 0x0000, 0x800 , msix_table);
        });

        dev.with_access_mut(|access| {
            access.set_bits(0x34..0x38);
        });
        // msix_table.write().init_msix_intid();
        // dev.
        dev
    }
    
}

/// Static handler instance for standard virtual PCI devices
pub const HANDLER: VirtioRngHandler = VirtioRngHandler;

pub fn rng_mmio_handler(mmio: &mut MMIOAccess, base: usize) -> HvResult {
    // error!("i receive mmio!{:x?},base:{:x?}",mmio,base);
    let zone = this_zone();
    let zone_lock = zone.read();
    let bus = zone_lock.vpci_bus();
    let (mut dev,mut bar) = (None,0);
    for (b,i) in bus.read_devs(){
        if let Some(res) = i.is_my_bar_addr(base) {
            // warn!("we found the device!{:?}",b);
            dev = Some(i.clone());
            bar = res;
            break;
        }
    }
    if let Some(found_dev) = dev{
        return found_dev.bar_mmio_distribute(bar, mmio);
    }
    Ok(())
}