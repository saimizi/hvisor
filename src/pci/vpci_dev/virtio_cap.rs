use core::{array::from_fn, ptr::{self, write_bytes}, slice::{from_raw_parts, from_raw_parts_mut}, sync::atomic::fence};

use aarch64_cpu::registers::VTCR_EL2::SH0::Non;
use alloc::{sync::Arc, vec::Vec};
use bitvec::index::BitSel;
use spin::rwlock::RwLock;

use crate::{device::irqchip::inject_irq, error::HvResult, memory::{GuestPhysAddr, MMIOAccess, mmio}, pci::{pci_access::Bar, pci_struct::PciCapabilityRegion, vpci_dev::{capability_handler::{self, virtio_common_cfg_handler}, virtio_queue::{AvailRing, DescriptorTable, VirtqUsed, VirtqUsedElem}}}, percpu::this_zone};

pub type PciCapabilityHandler = fn (&mut MMIOAccess,usize) -> HvResult;

const VIRTQ_DESC_F_NEXT:u16 = 1;

pub static mut MAPTI_INTERCEPTOR:Option<Arc<RwLock<MsixTable>>> = None;

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

pub struct Virtqueue{
    queue_size:u16,
    queue_msix_vector:u16,
    queue_enable:u16,
    queue_notify_off:u16,
    queue_desc:u64,
    queue_driver:u64,
    queue_device:u64,
    queue_notify_data:u16,
    queue_reset:u16,
    misx_table:Arc<RwLock<MsixTable>>,

    desc_table:Option<DescriptorTable>,
    used_area:Option<VirtqUsed>,
    avail_area:Option<AvailRing>,
    last_avail:usize
    // desc_table:Arc<RwLock<DescriptorTable>>
}

impl Virtqueue{
    pub fn new(msix:Arc<RwLock<MsixTable>>)->Self{
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
            misx_table:msix,
            desc_table:None,
            used_area:None,
            avail_area:None,
            last_avail:0
            // desc_table:Arc::new(RwLock::new(DescriptorTable::new(256)))
        }
    }

    pub fn notify_driver(&self){
        self.misx_table.read().inject_irq(self.queue_msix_vector as usize);
    }

    pub fn set_desc_area(&mut self){
        let base = self.queue_desc as usize;
        match self.desc_table {
            Some(ref mut x)=>{
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None=>{
                let desc = DescriptorTable::new(base, self.queue_size as usize);
                self.desc_table = Some(desc);      
            }
        }
    }

    pub fn set_avail_area(&mut self){
        let base = self.queue_driver as usize;
        match self.avail_area {
            Some(ref mut x)=>{
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None=>{
                let avail = AvailRing::new(base, self.queue_size);
                self.avail_area = Some(avail);      
            }
        }
    }

    pub fn set_used_area(&mut self){
        let base = self.queue_device as usize;
        match self.used_area {
            Some(ref mut x)=>{
                warn!("this arm should not be entered");
                x.set_ptr(base);
            }
            None=>{
                let desc = VirtqUsed::new(base, self.queue_size);
                self.used_area = Some(desc);      
            }
        }
    }

    pub fn consume_avail_with_zero(&self)->Option<()>{
        let avail_area = self.avail_area?;
        let used_area = self.used_area?;
        let desc_area = self.desc_table?;

        let avail_idx = avail_area.get_idx();
        let used_idx = used_area.get_idx();
        let queue_size = self.queue_size;
        for i in used_idx..avail_idx{
            let idx = i%queue_size;
            let avail_ring_content = avail_area.get_ring_content(idx as usize);
            let desc = desc_area.get(avail_ring_content as usize);
            // info!("desc read:{:x?}",desc);
            unsafe {
                write_bytes(desc.addr as *mut u8, '0' as u8, desc.len as usize);
            }
            let used_item = VirtqUsedElem::new(avail_ring_content as u32, desc.len);
            used_area.write_ring(idx as usize, used_item);
            // used_area.write_ring((idx +1) as usize, used_item);
            // used_area.write_ring((idx +2) as usize, used_item);
            // used_area.write_ring((idx +3) as usize, used_item);
            // used_area.write_ring((idx +4) as usize, used_item);
        }

        used_area.set_idx(avail_idx);
        return Some(());
    }
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

    queue_select:usize,
    // queue_size:u16,
    queue_list:Vec<Arc<RwLock<Virtqueue>>>,
    // queue_msix_vector:u16,
    // queue_enable:u16,
    // queue_notify_off:u16,
    // queue_desc:u64,
    // queue_driver:u64,
    // queue_device:u64,
    // queue_notify_data:u16,
    // queue_reset:u16,
    config_changed:bool
}

impl VirtioPciCommonCfg{
    pub fn new() -> Self{
        let mut cfg = VirtioPciCommonCfg { 
            device_feature_select: 0,
            device_feature: (0,65),
            driver_feature_select: 0,
            driver_feature: (0,0), 
            config_msix_vector: 0x0, 
            num_queue: 0, 
            device_status: 0, 
            config_generation: 0, 
            queue_select: 0, 
            // queue_size: 256, 
            queue_list: Vec::new(),
            // queue_msix_vector: 0, 
            // queue_enable: 1, 
            // queue_notify_off: 0, 
            // queue_desc: 0, 
            // queue_driver: 0, 
            // queue_device: 0, 
            // queue_notify_data: 0, 
            // queue_reset: 0,
            config_changed: false
        };
        // for _ in 0..queue_num{
        //     cfg.add_to_queue();
        // }
        cfg
    }

    pub fn insert_queue(&mut self, qu:Arc<RwLock<Virtqueue>>){
        self.queue_list.push(qu);
        self.num_queue+=1;
    }

    // pub fn add_to_queue(&mut self){
    //     let empty_queue:Arc<RwLock<Virtqueue>> = Arc::new(RwLock::new(Virtqueue::new()));
    //     self.insert_queue(empty_queue);
    // }
    
}

impl VirtioPciCommonCfg{
    pub fn write_into(&mut self,mmio_ac:&MMIOAccess)->bool{
        let addr = mmio_ac.address;
        let size = mmio_ac.size;
        let value = mmio_ac.value;
        // info!("----write in common cfg !!! addr:{:x},size:{:x},value:{:x}----",addr,size,value);
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

        if size == 2{
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
                    info!("queue No.{} has msix vector: 0x{:x}", self.queue_select,value);
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
                0x20 => {
                    let mut queue= self.queue_list[self.queue_select].write();
                    queue.queue_desc = value as u64;
                    // queue.set_desc_area();
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
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
        }
        
        if size == 2{
            match addr {
                0x10 =>{
                    mmio_ac.value = self.config_msix_vector as usize;
                }
                0x12 => {
                    mmio_ac.value = self.num_queue as usize;
                    // panic!("hhhh");
                }
                0x18 => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_size as usize;
                }
                0x1a => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_enable as usize;
                }
                0x1c => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_notify_off as usize;
                }
                0x1e => {
                    mmio_ac.value = self.queue_list[self.queue_select].read().queue_notify_off as usize;
                }
                _ => {
                    warn!("read:size is misalign!");
                    return Ok(());
                }
            }
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
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
            // info!("read from common cfg:0x{:x}",mmio_ac.value);
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
        // info!("read cap:{:x},size:{}",offset,size);
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

#[derive(Clone,Debug)]
struct MsixTableEntry{
    pub message_address: u32,
    pub message_upper_address: u32,
    pub msg_data:u32,
    pub vector_control:u32,
    pub intid:Option<usize>,
}

impl MsixTableEntry{
    pub fn activate_irq(&self){
        info!("entry:{:x?}",self);
        // inject_irq(0x2001, false);
        match self.intid {
            Some(x)=>{
                inject_irq(x, false);
            }
            None=>{
                warn!("this msix vector has not gotten a intid:{:x?}",self);
            }
        }
        // inject_irq(irq_id, is_hardware)
    }
}

impl MsixTableEntry{
    pub fn dummy()->Self{
        Self { 
            message_address: 0,
            message_upper_address: 0,
            msg_data: 0, 
            vector_control: 0,
            intid:None
        }
    }
}

#[derive(Debug)]
pub struct MsixTable{
    table:Vec<MsixTableEntry>,
    device_id:usize,
    event_id:Vec<(usize,usize)>,
}

impl MsixTable{
    pub fn new(size:usize,deviceid:usize)->Self{
        let mut vec = Vec::new();
        vec.resize(size, MsixTableEntry::dummy());
        Self { 
            table: vec,
            device_id: deviceid,
            event_id:Vec::new()
        }
    }

    pub fn inject_irq(&self,vector_index:usize){
        self.table[vector_index].activate_irq();
    }

    pub fn intercept_its(&mut self,deviceid:usize,event_id:usize,intid:usize){
        if deviceid == self.device_id {
            warn!("MAPTI's deviceid != current deviceid!");
        }
        // for i in self.table.iter_mut(){
        //     if i.msg_data == event_id as u32{
        //         i.intid = Some(intid);
        //     }
        // }
        self.event_id.push((event_id,intid));
        // warn!("we can't find a vector with this event_id:0x{:x}",event_id);
    }

    pub fn init_msix_intid(&mut self,index:usize){
        // unsafe {
        //     MAPTI_INTERCEPTOR = None
        // }
        // info!("init msix intid Self:{:x?}",self);
        let selected_vector = &mut self.table[index];
        for i in self.event_id.iter(){
            if selected_vector.msg_data as usize == i.0{
                selected_vector.intid = Some(i.1);
                break;
            }

        }
        
    }
}

impl AreaInBar for MsixTable{

    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult {
        // info!("misx table read:{:x?}",mmio_ac);
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
        // info!("msix table write:{:x?}",mmio_ac);
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
        self.init_msix_intid(index);
        Ok(())
    }
}


pub struct VirtioISRCap{
    isr:u32
}

impl VirtioISRCap{
    pub fn new() -> Self{
        Self { isr: 0 }
    }

    pub fn set_isr(&mut self, value:u32){
        self.isr = value
    }
}

impl AreaInBar for VirtioISRCap{
    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult {
        info!("ISR read:{:x?}",mmio_ac);
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

    fn write(&mut self,mmio_ac:&MMIOAccess) -> HvResult {
        warn!("isr should not be write");
        return Ok(());
    }
}

pub struct VirtioNotifyCap{
    cap:VirtioPciCap,
    queue_list:Vec<(usize,Arc<RwLock<Virtqueue>>)>
}

impl VirtioNotifyCap {
    pub fn new(next:u8,offset:u32,length:u32) -> Self{
        let cap = VirtioPciCap::new(VirtioCfgType::NotifyCfg(0x04),next,0x14,offset,offset,None);
        Self { 
            cap, 
            queue_list: Vec::new(),
        }
    }

    pub fn insert_queue(&mut self ,qu:Arc<RwLock<Virtqueue>>){
        let queue_notify_off = qu.read().queue_notify_off as u32;
        // let offset;
        if let VirtioCfgType::NotifyCfg(multiplier) = self.cap.cfg_type{
            let offset = self.cap.offset + queue_notify_off*multiplier;
            self.queue_list.push((offset as usize,qu));
        }

        error!("Notify cap has to have NotifyCfg type!");
    }    

    // pub fn get_queue(&self,offset:usize)->Arc<RwLock<Virtqueue>>{
    //     let res = self.queue_list.iter().filter(|(x,_)|x==&offset).cloned().collect();
    // }

    fn get_queues(
    &self,
    offset:usize
    ) -> impl Iterator<Item = &Arc<RwLock<Virtqueue>>>
    {
        self.queue_list.iter()
        .filter(move |(x,_)| *x==offset)
        .map(|(_,res)|res)
    }


}

impl PciCapabilityRegion for VirtioNotifyCap{
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

impl AreaInBar for VirtioNotifyCap{
    fn read(&mut self, mmio_ac:&mut MMIOAccess) -> HvResult {
        warn!("Notify read has not been implemented yet");
        Ok(())
    }

    fn write(&mut self,mmio_ac:&MMIOAccess) -> HvResult {
        let offset = mmio_ac.address;
        // info!("get into notify:");
        // for i in self.queue_list.iter(){
        //     info!("the address registered:0x{:x}",i.0);
        // }
        for i in self.get_queues(offset){
            let desc = i.read().queue_desc;
            let avail = i.read().queue_driver;
            let used = i.read().queue_device;
            i.read().consume_avail_with_zero();
            // VringAvail::show(avail);
            // VringDesc::show(desc, 0, 1);
            // VringUsed::show(used);
            // info!("device get kicked: queue desc address:0x{:x}",desc);
            fence(core::sync::atomic::Ordering::SeqCst);
            i.read().notify_driver();
        }
        Ok(())
    }
}

struct VringAvail;

impl VringAvail{
    pub fn show(addr:u64){
        let avail_area = unsafe {
            from_raw_parts(addr as *mut u16, 100)
        };
        let flags = avail_area[0];
        let idx = avail_area[1];
        info!("Vring Avail:addr:0x{:x}flags:0x{:x},idx:0x{:x}",addr,flags,idx);
        // info!("ring content:")
        for i in 0..idx{
            info!("avail area: {}",i);
        }
    }
}

// struct DescriptorTable{
//     list:Vec<VringDesc>
// }

// impl DescriptorTable{
//     pub fn new(size:usize)->Self{
//         DescriptorTable { list: Vec::with_capacity(size) }
//     }

//     pub fn get_desc_chain(&self,idx:usize)->impl Iterator<Item = &VringDesc>{
//         let mut ans = Vec::new();
//         ans.push(idx);
//         let mut idx = idx;
//         loop{
//             match self.list[idx].next_idx() {
//                 Some(x)=>{
//                     ans.push(x);
//                     idx = x;
//                 }
//                 None=>{
//                     break;
//                 }
//             }
//         }
//         ans.into_iter().map(move |x| &self.list[x])
//     }

//     pub fn get_desc_chain_length(&self,idx:usize)->usize{
//         let chain = self.get_desc_chain(idx);
//         let mut ans = 0;
//         for i in chain{
//             ans += i.get_len();
//         }
//         ans
//     }
// }

struct VringDesc{
    addr:u64,
    len:u32,
    flags:u16,
    next:u16,
}

impl VringDesc{

    pub fn new(addr:u64)->Self{
        let u32_are = unsafe {
            from_raw_parts(addr as *mut u32, 16)
        };
        let u16_are = unsafe {
            from_raw_parts(addr as *mut u16, 16)
        };
        Self { addr,len: u32_are[2], flags:u16_are[6], next: u16_are[7] }
    }

    pub fn show(addr:u64,idx:usize,len:usize){
        let desc_area = unsafe {
            from_raw_parts(addr as *mut u64, 16)
        };
        let u32_are = unsafe {
            from_raw_parts(addr as *mut u32, 16)
        };
        let u16_are = unsafe {
            from_raw_parts(addr as *mut u16, 16)
        };
        info!("desc area: 0x{:x} 0x{:x} 0x{:x} 0x{:x}",desc_area[0],u32_are[2],u16_are[6],u16_are[7]);
        
        info!("desc area: 0x{:x} 0x{:x}",desc_area[2],desc_area[3]);

        info!("desc area: 0x{:x} 0x{:x}",desc_area[4],desc_area[5]);
        info!("desc area: 0x{:x} 0x{:x}",desc_area[6],desc_area[7]);
        info!("desc area: 0x{:x} 0x{:x}",desc_area[8],desc_area[9]);
        info!("desc area: 0x{:x} 0x{:x}",desc_area[10],desc_area[11]);
    }

    
    fn has_next(&self)->bool{
        self.flags & VIRTQ_DESC_F_NEXT == 1
    }

    pub fn next_idx(&self)->Option<usize>{
        if self.has_next(){
            return Some(self.next as usize);
        }else {
            return None;
        }
    }

    // pub fn get_len(&self)->usize{
    //     self.len
    // }
}

struct VringUsed;

impl VringUsed{
    pub fn show(addr:u64){
        let used_area = unsafe {
            from_raw_parts(addr as *mut u16, 100)
        };
        let flags = used_area[0];
        let idx = used_area[1];
        info!("Vring used:addr:0x{:x} flags:0x{:x},idx:0x{:x}",addr,flags,idx);
        // info!("ring content:")
        // for i in 0..idx{
        //     info!("avail area: {}",i);
        // }
    }
    pub fn fake_use(addr:u64){
        let used_area = unsafe {
            from_raw_parts_mut(addr as *mut u16, 20)
        };
        used_area[1] = 1;
        used_area[2] = 0;
        used_area[3] = 0;
        used_area[4] = 0x1000;

    }
}