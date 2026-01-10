use core::array::from_fn;

use alloc::{sync::Arc, vec::Vec};
use bitvec::index::BitSel;
use spin::rwlock::RwLock;

use crate::{error::HvResult, memory::{GuestPhysAddr, MMIOAccess, mmio}, pci::{pci_access::Bar, pci_struct::PciCapabilityRegion, vpci_dev::capability_handler::{self, virtio_common_cfg_handler}}, percpu::this_zone};

pub type PciCapabilityHandler = fn (&mut MMIOAccess,usize) -> HvResult;

fn put_together(src:(u8,u8,u8,u8))->u32{
    let a =(src.0 as u32)<<24 |
    (src.1 as u32)<<16 |
    (src.2 as u32)<<8  |
    (src.3 as u32) ;
    info!("output:{:x}",a);
    a
}

#[derive(Clone, Copy)]
pub enum VirtioCfgType {
    CommonCfg,
    NotifyCfg(u32),
    IsrCfg,
    DeviceCfg,
    PciCfg,
    SharedMemoryCfg,
    VendorCfg
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

pub struct VirtioPciCap{
    cap_vndr:u8,
    cap_next:u8,
    cap_len:u8,
    cfg_type:VirtioCfgType,
    bar:u8,
    id:u8,
    padding:[u8;2],
    offset:u32,
    length:u32,

    handler:Option<PciCapabilityHandler>
}

impl PciCapabilityRegion for VirtioPciCap{
    fn read(&self, offset: crate::pci::PciConfigAddress, size: usize) -> crate::error::HvResult<u32> {
        info!("read cap:{:x},size:{}",offset,size);
        if offset as usize %size != 0 {
            warn!("cap read is misalign!");
            return Ok(0);
        }
        match self.cfg_type {
            VirtioCfgType::NotifyCfg(multiplier) => {
                if (offset,size) == (16,4){
                    return Ok(multiplier);
                }
            }
            _ => ()
        };
        if size == 1 {
            match offset {
                0 => return Ok(self.cap_vndr as u32) ,
                1 => return Ok(self.cap_next as u32),
                2 => return Ok(self.cap_len as u32),
                3 => return Ok(u8::from(self.cfg_type) as u32),
                4 => return Ok(self.bar as u32),
                5 => return Ok(self.id as u32),
                _ => {
                    warn!("read u8 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        if size == 2 {
            match offset {
                0 => return Ok(put_together((0,0,self.cap_next,self.cap_vndr))),
                2 => return Ok(put_together((0,0,self.cfg_type.into(),self.cap_len))),
                4 => return Ok(put_together((0,0,self.id,self.bar))),
                _ => {
                    warn!("read u16 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        if size == 4{
            match offset {
                0 => return Ok(put_together((self.cfg_type.into(),self.cap_len,self.cap_next,self.cap_vndr))),
                4 => return Ok(put_together((0,0,self.id,self.bar))),
                8 => return Ok(self.offset),
                12 => return Ok(self.length),
                _ => {
                    warn!("read u32 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        warn!("size is not any of 1,2,4!");
        Ok(0)
    }

    fn write(&mut self, offset: crate::pci::PciConfigAddress, size: usize, value: u32) -> crate::error::HvResult {
        Ok(())
    }

    fn get_offset(&self) -> crate::pci::PciConfigAddress {
        0
    }

    fn get_size(&self) -> usize {
        self.cap_len as usize
    }

    fn next_cap(&self) -> crate::error::HvResult<crate::pci::PciConfigAddress> {
        Ok(self.cap_next as u64)
    }

    fn bar_usage_info(&self) -> Option<BarUsageInfo> {
        if let Some(handler) = self.handler{
            let bar_usage = BarUsageInfo::new(self.offset as usize, self.length as usize, self.bar, handler);
            return Some(bar_usage);
        }
        None
    }
}

impl VirtioPciCap{
    pub fn new(config_type:VirtioCfgType,cap_next:u8,offset:u32,length:u32,handler:Option<PciCapabilityHandler>)->Self{
        Self { 
            cap_vndr:0x09, 
            cap_next, cap_len: 0x10, 
            cfg_type: config_type, 
            bar: 0x04, id: 0x00, 
            padding: [0,0], 
            offset, 
            length,
            handler
        }
    }

    // pub fn bar_area_init(&self,start_address:usize)->HvResult{

    // }
}

const VIRTIO_F_VERSION_1:usize = 32;
pub struct VirtioPciCommonCfg{
    device_feature_select:u32,
    device_feature:(u32,u32),
    driver_feature_select:u32,
    driver_feature:(u32,u32),
    config_msix_vector:u16,
    num_queue:u16,
    device_status:u8,
    config_generation:u8,

    queue_select:u16,
    queue_size:u16,
    queue_msix_vector:u16,
    queue_enable:u16,
    queue_notify_off:u16,
    queue_desc:u64,
    queue_driver:u64,
    queue_device:u64,
    queue_notify_data:u16,
    queue_reset:u16
}

impl VirtioPciCommonCfg{
    pub fn new() -> Self{
        VirtioPciCommonCfg { 
            device_feature_select: 0,
            device_feature: (0,1),
            driver_feature_select: 0,
            driver_feature: (0,0), 
            config_msix_vector: 0, 
            num_queue: 0, 
            device_status: 0, 
            config_generation: 0, 
            queue_select: 0, 
            queue_size: 0, 
            queue_msix_vector: 0, 
            queue_enable: 0, 
            queue_notify_off: 0, 
            queue_desc: 0, 
            queue_driver: 0, 
            queue_device: 0, 
            queue_notify_data: 0, 
            queue_reset: 0 
        }
    }
}

impl AreaInBar for VirtioPciCommonCfg{
    fn read(&self, mmio_ac:&mut MMIOAccess) -> HvResult {
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        info!("read in common cfg !!! addr:{:x},size:{:x}",addr,size);
        if size == 1{
            match addr {
                0x14 => {
                    mmio_ac.value = self.device_status as usize;
                }
                _ => {
                    warn!("read:size is misalign!");
                    return Ok(());
                }
            }
            info!("read from common cfg:0x{:x}",mmio_ac.value);
        }

        if size == 4{
            match addr {
                0x04 => {
                    if self.device_feature_select == 0{
                        mmio_ac.value = self.device_feature.0 as usize;
                    }else{
                        mmio_ac.value = self.device_feature.1 as usize;
                    }
                    // return Ok(());
                }   
                _ => {
                    warn!("read:not implement yet!addr:{:x}",mmio_ac.value);
                    return Ok(());
                }
            }
            info!("read from common cfg:0x{:x}",mmio_ac.value);
        }
        Ok(())        
    }

    fn write(&mut self,mmio_ac:& MMIOAccess) -> HvResult {
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        let value = mmio_ac.value;
        info!("write in common cfg !!! addr:{:x},size:{:x},value:{:x}",addr,size,value);
        if size == 1{
            match addr {
                0x14 => {
                    self.device_status = value as u8;
                    return Ok(())
                }
                _ => {
                    warn!("write:size is misalign!");
                    return Ok(())
                }
            }
        }

        if size == 4{
            match addr {
                0x00 => {
                    self.device_feature_select = value as u32;
                    return Ok(());
                }
                _=>{
                    warn!("write:not implement yet!addr:{:x}",addr);
                    return Ok(());
                }
            }
        }
        Ok(())   
    }
}

pub struct BarUsageInfo{
    base:usize,
    length:usize,
    bar:u8,
    handler:PciCapabilityHandler
}

impl BarUsageInfo{
    fn new(base:usize,length:usize,bar:u8,handler:PciCapabilityHandler)->Self{
        Self { base, length, bar, handler }
    }
}

pub trait AreaInBar: Send + Sync{
    fn read(&self, mmio_ac:&mut MMIOAccess) -> HvResult;

    fn write(&mut self,mmio_ac:&MMIOAccess) -> HvResult;
}

pub struct BarAreaManager{
    area:[Vec<(GuestPhysAddr,usize,Arc<RwLock<dyn AreaInBar>>)>;6]
}

impl BarAreaManager{
    pub fn new()->Self{
        BarAreaManager {
            area:from_fn(|_|{Vec::new()})
        }
    }

    pub fn insert(&mut self,bar:usize,addr:GuestPhysAddr,size:usize,area:Arc<RwLock<dyn AreaInBar>>){
        self.area[bar].push((addr,size,area));
    }

    fn find_cap(&self,bar:usize,addr:GuestPhysAddr,size:usize) -> Option<&(GuestPhysAddr,usize,Arc<RwLock<dyn AreaInBar>>)>{
        let res = self.area[bar].iter()
        .filter(|&e|{
            e.0<= addr && e.0+e.1>=addr+size
        }).max_by_key(|(k,_,_)| k);
        res
    }

    pub fn handle_bar_access(&self,bar:usize,mmio_ac:&mut MMIOAccess) -> HvResult{
        let target_cap = self.find_cap(bar,mmio_ac.address, mmio_ac.size);
        if let Some((_,_,area)) = target_cap{
            if mmio_ac.is_write{
                return area.write().write(mmio_ac);
            }else{
                return area.read().read(mmio_ac);
            }
        }
        warn!("we didn't find the access result!");
        Ok(())
    }
}