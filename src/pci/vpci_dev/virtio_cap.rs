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
                    warn!("read multiplier!!!");
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
    pub fn new(config_type:VirtioCfgType,cap_next:u8,cap_len:u8,offset:u32,length:u32,handler:Option<PciCapabilityHandler>)->Self{
        Self { 
            cap_vndr:0x09, 
            cap_next, cap_len, 
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
    queue_reset:u16,
    config_changed:bool
}

impl VirtioPciCommonCfg{
    pub fn new() -> Self{
        VirtioPciCommonCfg { 
            device_feature_select: 0,
            device_feature: (0,65),
            driver_feature_select: 0,
            driver_feature: (0,0), 
            config_msix_vector: 0, 
            num_queue: 1, 
            device_status: 0, 
            config_generation: 0, 
            queue_select: 0, 
            queue_size: 256, 
            queue_msix_vector: 0, 
            queue_enable: 1, 
            queue_notify_off: 0, 
            queue_desc: 0, 
            queue_driver: 0, 
            queue_device: 0, 
            queue_notify_data: 0, 
            queue_reset: 0,
            config_changed: false
        }
    }
}

impl VirtioPciCommonCfg{
    pub fn write_into(&mut self,mmio_ac:&MMIOAccess)->bool{
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        let value = mmio_ac.value;
        info!("----write in common cfg !!! addr:{:x},size:{:x},value:{:x}----",addr,size,value);
        if size == 1{
            match addr {
                0x14 => {
                    self.device_status = value as u8;
                    return true
                }
                _ => {
                    warn!("write:size is misalign!");
                    return false
                }
            }
        }

        if size == 4{
            match addr {
                0x00 => {
                    self.device_feature_select = value as u32;
                    return true
                }
                0x08 => {
                    self.driver_feature_select = value as u32;
                    return true
                }
                0x0c => {
                    if self.driver_feature_select == 0 {
                        self.driver_feature.0 = value as u32;
                    }else{
                        self.driver_feature.1 = value as u32;
                    }
                    return true;
                }
                _=>{
                    warn!("write:not implement yet!addr:{:x}",addr);
                    return false;
                }
            }
        }
        false
    }
}

impl AreaInBar for VirtioPciCommonCfg{
    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult {
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        info!("----read in common cfg !!! addr:{:x},size:{:x}----",addr,size);
        if size == 1{
            match addr {
                0x14 => {
                    mmio_ac.value = self.device_status as usize;
                }
                0x15 => {
                    if self.config_changed{
                        self.config_generation += 1;
                    }
                    mmio_ac.value = self.config_generation as usize;
                }
                _ => {
                    warn!("read:size is misalign!");
                    return Ok(());
                }
            }
            info!("read from common cfg:0x{:x}",mmio_ac.value);
        }
        
        if size == 2{
            match addr {
                0x12 => {
                    mmio_ac.value = self.num_queue as usize;
                    // panic!("hhhh");
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
           if self.write_into(mmio_ac){
            self.config_changed = true;
            return Ok(())
           }
           warn!("the write has not reached the common config!");
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
    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult;

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
                return area.write().read(mmio_ac);
            }
        }
        warn!("we didn't find the access result!");
        Ok(())
    }
}

pub struct MsixCap{
    cap_id:u8,
    cap_next:u8,
    message_control:u16,
    table_bar:u8,
    table_offset:u32,
    pending_bar:u8,
    pending_offset:u32,
}

impl MsixCap{
    pub fn new(next:u8,table_size:u16)->Self{
        let mut res = Self { 
            cap_id:0x11,
            cap_next: next, 
            message_control: 0x0, 
            table_bar: 0x01, 
            table_offset: 0x0000_0000, 
            pending_bar: 0x01, 
            pending_offset: 0x0000_08000 
        };
        res.set_table_size(table_size);
        res
    }

    pub fn get_table_mesg(&self) -> u32{
        (self.table_offset << 3) | (self.table_bar as u32)
    }

    pub fn get_pending_mesg(&self) -> u32{
        (self.pending_offset << 3) | (self.pending_bar as u32)
    }

    pub fn set_table_mesg(&mut self,mesg:u32){
        self.table_offset = mesg >> 3;
        self.table_bar = (mesg & 0x0000_0003) as u8;
    }
    pub fn set_pending_mesg(&mut self,mesg:u32){
        self.pending_offset = mesg >> 3;
        self.pending_bar = (mesg & 0x0000_0003) as u8;
    }

    pub fn set_table_size(&mut self,size:u16){
        if size > 2048{
            warn!("msix table size cannot larger than 2048");
            return;
        }
        let mask = 0xf800;
        self.message_control &= mask;
        self.message_control |= size;

    }
}

impl PciCapabilityRegion for MsixCap{
       fn read(&self, offset: crate::pci::PciConfigAddress, size: usize) -> crate::error::HvResult<u32> {
        info!("read cap:{:x},size:{}",offset,size);
        if offset as usize %size != 0 {
            warn!("cap read is misalign!");
            return Ok(0);
        }
        if size == 1 {
            match offset {
                0x00 => return Ok(self.cap_id as u32),
                0x01 => return Ok(self.cap_next as u32),
                _ => {
                    warn!("read u8 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        if size == 2 {
            match offset {
                0x00 => return Ok(self.cap_id as u32 | (self.cap_next as u32) << 8),
                0x02 => return Ok(self.message_control as u32),
                _ => {
                    warn!("read u16 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        if size == 4{
            match offset {
                0x00 => return Ok((self.cap_id as u32)|(self.cap_next as u32)<< 8 |(self.message_control as u32)<< 16),
                0x04 => return Ok(self.get_table_mesg()),
                0x08 => return Ok(self.get_pending_mesg()),
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
        if size == 1{
            warn!("there is no writeable field with size 1!")
        }
        if size == 2{
            match offset {
                0x02 => self.message_control = value as u16,
                _ => {
                    warn!("write into unexpected area! offset:{}", offset)
                }
                
            }
            return Ok(());
        }

        if size == 4{
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

#[derive(Clone)]
struct MsixTableEntry{
    pub message_address: u32,
    pub message_upper_address: u32,
    pub msg_data:u32,
    pub vector_control:u32
}


impl MsixTableEntry{
    pub fn dummy()->Self{
        Self { 
            message_address: 0,
            message_upper_address: 0,
            msg_data: 0, 
            vector_control: 0 
        }
    }
}

pub struct MsixTable{
    table:Vec<MsixTableEntry>
}

impl MsixTable{
    pub fn new(size:usize)->Self{
        let mut vec = Vec::new();
        vec.resize(size, MsixTableEntry::dummy());
        Self { table: vec }
    }
}

impl AreaInBar for MsixTable{

    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult {
        info!("misx table read:{:x?}",mmio_ac);
        // let size = mmio_ac.size;
        let offset = mmio_ac.address;
        // 16 is the size of entry
        let index = offset /16;
        let offset_in_entry = offset % 16;
        // let mut res;
        match offset_in_entry {
            0x00 => mmio_ac.value = self.table[index].message_address as usize,
            0x04 => mmio_ac.value = self.table[index].message_upper_address as usize,
            0x08 => mmio_ac.value = self.table[index].msg_data as usize,
            0x0c => mmio_ac.value = self.table[index].vector_control as usize,
            _=>{
                warn!("access address is misalign!");
            }
        }
        // mmio_ac.value = res;
        Ok(())
    }

    fn write(&mut self,mmio_ac:&MMIOAccess) -> HvResult {
        info!("msix table write:{:x?}",mmio_ac);
        if mmio_ac.size != 4{
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
            _=>{
                warn!("access address is misalign!");
            } 
        }
        Ok(())
    }
}