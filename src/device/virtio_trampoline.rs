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
use crate::arch::cpu::{get_target_cpu, this_cpu_id};
use crate::consts::MAX_CPU_NUM;
use crate::consts::MAX_WAIT_TIMES;
use crate::device::irqchip::inject_irq;
use crate::event::send_event;
use crate::event::IPI_EVENT_WAKEUP_VIRTIO_DEVICE;
use crate::hypercall::SGI_IPI_ID;
use crate::pci::vpci_dev::virtio_queue::{GuestMemory, GuestSlice};
use crate::zone::root_zone;
use crate::zone::this_zone_id;
use crate::{error::HvResult, memory::MMIOAccess};
use alloc::collections::BTreeMap;
use core::fmt::Debug;
use core::fmt::Formatter;
use core::fmt::Result;
use core::sync::atomic::fence;
use core::sync::atomic::Ordering;
use spin::Mutex;

/// Save the irqs the virtio-device wants to inject. The format is <cpu_id, List<irq_id>>, and the first elem of List<irq_id> is the valid len of it.
pub static VIRTIO_IRQS: Mutex<BTreeMap<usize, [u64; MAX_DEVS + 1]>> = Mutex::new(BTreeMap::new());
// Controller of the shared memory the root linux's virtio device and hvisor shares.
pub static VIRTIO_BRIDGE: Mutex<VirtioBridgeRegion> = Mutex::new(VirtioBridgeRegion::default());
pub static VIRTIO_PCI_BRIDGE: Mutex<VirtioPCIBridge> = Mutex::new(VirtioPCIBridge::dummy());

const QUEUE_NOTIFY: usize = 0x50;
pub const MAX_REQ: u32 = 32;
pub const MAX_DEVS: usize = 8; // Attention: The max virtio-dev number for vm is 8 (loongarch64 needs 3 consoles and 3 disks for zgclab project).
pub const MAX_CPUS: usize = 32;
pub const MAX_VQ: usize = 16;

#[cfg(all(not(target_arch = "riscv64"), not(target_arch = "x86_64")))]
pub const IRQ_WAKEUP_VIRTIO_DEVICE: usize = 32 + 0x20;
pub const IRQ_WAKEUP_VIRTIO_PCI_CONFIG:usize = 32 + 0x21;
pub const IRQ_WAKEUP_VIRTIO_PCI_DATA:usize = 32 + 0x22;
#[cfg(target_arch = "riscv64")]
pub const IRQ_WAKEUP_VIRTIO_DEVICE: usize = 0x20;
#[cfg(target_arch = "x86_64")]
pub const IRQ_WAKEUP_VIRTIO_DEVICE: usize = 0x6;

/// non root zone's virtio request handler
pub fn mmio_virtio_handler(mmio: &mut MMIOAccess, base: usize) -> HvResult {
    // debug!("mmio virtio handler");
    let need_interrupt = if mmio.address == QUEUE_NOTIFY { 1 } else { 0 };
    if need_interrupt == 1 {
        trace!("notify !!!, cpu id is {}", this_cpu_id());
    }
    mmio.address += base;
    let mut dev = VIRTIO_BRIDGE.lock();
    while dev.is_req_list_full() {
        // When root linux's cpu is in el2's finish req handler and is getting the dev lock,
        // if we don't release dev lock, it will cause a dead lock.
        drop(dev);
        dev = VIRTIO_BRIDGE.lock();
    }
    let hreq = HvisorDeviceReq::new(
        this_cpu_id() as _,
        mmio.address as _,
        mmio.size as _,
        mmio.value as _,
        this_zone_id() as _,
        mmio.is_write,
        need_interrupt,
    );
    // debug!("non root sends req: {:#x?}", hreq);
    let (cfg_flags, cfg_values) = unsafe {
        (
            core::slice::from_raw_parts(dev.get_cfg_flags(), MAX_CPU_NUM),
            core::slice::from_raw_parts(dev.get_cfg_values(), MAX_CPU_NUM),
        )
    };
    let cpu_id = this_cpu_id() as usize;
    let old_cfg_flag = cfg_flags[cpu_id];
    // debug!("old cfg flag: {:#x?}", old_cfg_flag);
    dev.push_req(hreq);
    // If req list is empty, send sgi to root linux to wake up virtio device.
    #[cfg(not(target_arch = "loongarch64"))]
    if dev.need_wakeup() {
        debug!("need wakeup, sending ipi to wake up virtio device");
        send_event(
            get_target_cpu(IRQ_WAKEUP_VIRTIO_DEVICE, 0),
            SGI_IPI_ID as _,
            IPI_EVENT_WAKEUP_VIRTIO_DEVICE,
        );
    }
    drop(dev);
    let mut count: usize = 0;
    // if it is cfg request, current cpu should be blocked until gets the result
    if need_interrupt == 0 {
        // when virtio backend finish the req, it will add 1 to cfg_flag.
        while unsafe { core::ptr::read_volatile(&cfg_flags[cpu_id]) } == old_cfg_flag {
            // fence(Ordering::Acquire);
            count += 1;
            if count == MAX_WAIT_TIMES {
                warn!(
                    "virtio backend is too slow, please check it! addr: {:x} is_write: {:x?}",
                    mmio.address, mmio.is_write
                );
                fence(Ordering::Acquire);
            }
            if count == MAX_WAIT_TIMES * 10 {
                error!(
                    "virtio backend may have some problem, please check it! addr: {:x} is_write: {:x?}",
                    mmio.address, mmio.is_write
                );
                count = 0;
            }
        }
        if !mmio.is_write {
            // ensure cfg value is right.
            mmio.value = unsafe { core::ptr::read_volatile(&cfg_values[cpu_id]) as _ };
            // debug!("non root receives value: {:#x?}", mmio.value);
        }
    }
    // debug!("non root returns");
    Ok(())
}

/// When virtio req type is notify, root zone will send sgi to non root, \
/// and non root will call this function.
pub fn handle_virtio_irq() {
    let mut map = VIRTIO_IRQS.lock();
    let irq_list = map.get_mut(&this_cpu_id()).unwrap();
    let len = irq_list[0] as usize;
    for irq_id in irq_list[1..=len].iter() {
        inject_irq(*irq_id as _, false);
    }
    irq_list[0] = 0;
}

pub struct VirtioBridgeRegion {
    base_address: usize, // el1 and el2 shared region addr, el2 virtual address
    pub is_enable: bool,
}

impl VirtioBridgeRegion {
    // return a mut region
    pub fn region(&self) -> &mut VirtioBridge {
        if !self.is_enable {
            panic!("hvisor device region is not enabled!");
        }
        unsafe { &mut *(self.base_address as *mut VirtioBridge) }
    }
    // return a non mut region
    pub fn immut_region(&self) -> &VirtioBridge {
        if !self.is_enable {
            panic!("hvisor device region is not enabled!");
        }
        unsafe { &*(self.base_address as *const VirtioBridge) }
    }

    pub const fn default() -> Self {
        VirtioBridgeRegion {
            base_address: 0,
            is_enable: false,
        }
    }

    pub fn set_base_addr(&mut self, base_addr: usize) {
        self.base_address = base_addr;
        self.is_enable = true;
    }

    pub fn is_req_list_full(&self) -> bool {
        let region = self.immut_region();
        if ((region.req_rear + 1) & (MAX_REQ - 1)) == region.req_front {
            debug!("hvisor req queue full");
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn is_req_list_empty(&self) -> bool {
        let region = self.immut_region();
        fence(Ordering::Acquire);
        if region.req_rear == region.req_front {
            true
        } else {
            false
        }
    }

    pub fn is_res_list_empty(&self) -> bool {
        let region = self.immut_region();
        if region.res_rear == region.res_front {
            true
        } else {
            false
        }
    }
    // push a req to hvisor's req list
    pub fn push_req(&mut self, req: HvisorDeviceReq) {
        let region = self.region();
        region.req_list[(region.req_rear % MAX_REQ) as usize] = req;
        // Write barrier so that virtio device sees changes to req_list before change to req_idx
        fence(Ordering::SeqCst);
        region.req_rear = (region.req_rear + 1) % MAX_REQ;
        // Write barrier so that device can see change after this method returns
        // fence(Ordering::SeqCst);
    }

    pub fn get_cfg_flags(&self) -> *const u64 {
        let region = self.immut_region();
        region.cfg_flags.as_ptr()
    }

    pub fn get_cfg_values(&self) -> *const u64 {
        let region = self.immut_region();
        region.cfg_values.as_ptr()
    }

    pub fn need_wakeup(&self) -> bool {
        let region = self.immut_region();
        fence(Ordering::Acquire);
        if region.need_wakeup == 1 {
            true
        } else {
            false
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy,Debug)]
pub struct VirtqueueAreaInfo{
    pub desc_area: u64,
    pub avail_area: u64,
    pub used_area: u64
}

impl VirtqueueAreaInfo{
    pub fn new(desc:u64,avail:u64,used:u64)->Self{
        Self { desc_area: desc, avail_area: avail, used_area: used }
    }

    pub fn dummy()->Self{
        Self { desc_area: 0, avail_area: 0, used_area: 0 }
    }
}



// pub struct VirtioPCIBridge{
//     req_list:[VirtioPCIReq;128]
// }

type VirtioPCIBridgeRegion = GuestSlice<VirtqueueAreaInfo>;

#[repr(C)]
#[derive(Clone, Copy,Debug)]
pub struct VirtioPCIConfigInfo{
    features:u64,
    dev_id:u16,
    num_of_queues:u16,
    dtype:u16,
    padding:u16,
    vqs:[VirtqueueAreaInfo;MAX_VQ],
}

impl VirtioPCIConfigInfo{
    pub fn dummy()->Self{
        Self { features: 0, dev_id: 0, num_of_queues: 0, dtype: 0,padding:0, vqs: [VirtqueueAreaInfo::dummy();16] }
    }

    pub fn set_features(&mut self,val:u64){
        self.features = val;
    }

    pub fn set_dev_id(&mut self,val:u16){
        self.dev_id = val;
    }

    pub fn set_num_of_queues(&mut self,val:u16){
        self.num_of_queues = val;
    }

    pub fn set_dtype(&mut self,val:u16){
        self.dtype = val;
    }

    pub fn set_vqs(&mut self,idx:usize,val:VirtqueueAreaInfo){
        if idx >= MAX_VQ{
            error!("It's illegal to have idx:0x{:x} exceeding MAX_VQ",idx);
            return;
        }
        self.vqs[idx] = val;
    }

}

#[repr(C)]
#[derive(Copy,Clone,Debug)]
pub struct VirtioPCIDataInfo{
    dev_id:u16,
    queue_id:u16,
    cpu_id:u16,
    _padding:u16
}

impl VirtioPCIDataInfo{
    pub fn new(dev_id:u16,queue_id:u16)->Self{
        let cpu_id = this_cpu_id() as u16;
        Self { dev_id, queue_id ,cpu_id,_padding:0xff}
    }

    pub fn get_identifier(&self)->u64{
        (self.dev_id as u64) | ((self.queue_id as u64) << 16) | ((self.cpu_id as u64) << 32)
    }
}

pub struct VirtioPCIBridge {
    config:GuestMemory,
    // config:VirtioPCIConfigInfo,
    data:GuestMemory,
    // data:VirtioPCIConfigInfo,
}

impl VirtioPCIBridge{
    pub const fn dummy()->Self{
        Self { config: GuestMemory::dummy(), data: GuestMemory::dummy() }
    }

    pub fn init(&mut self,addr:usize){
        let size_of_config_info = 2*4+8+24*MAX_VQ;
        self.config.set_ptr(addr);
        self.config.set_len(size_of_config_info);
        self.data.set_ptr(addr + size_of_config_info);
        self.data.set_len(2*4);
    }

    pub fn write_dev_info(&mut self,config:VirtioPCIConfigInfo){
        self.config.write_obj(0, config);
    }

    pub fn read_dev_info(&self)->VirtioPCIConfigInfo{
        self.config.read_obj(0)
    }

    pub fn write_data_info(&mut self,data_info:VirtioPCIDataInfo){
        self.data.write_obj(0,data_info );
    }

    pub fn til_config_finish(&self){
        let mut info:VirtioPCIConfigInfo = self.config.read_obj(0);
        while info.dev_id == MAX_DEVS as u16{
            for _ in 0..1000{

            }
            info = self.config.read_obj(0);
        }
    }
}



impl VirtioPCIBridgeRegion{
    pub fn set_request_index(&self,index:u64,is_config:bool){
        let avail_area = if is_config{0}else {1};
        let request_index = VirtqueueAreaInfo{desc_area:index,avail_area,used_area:0};
        self.set(0, request_index);
    }

    pub fn find_request_slot(&self)->u64{
        return 1;
    }

    pub fn set_request(&self,req:VirtqueueAreaInfo,slot_index:u64){
        self.set(slot_index as usize, req);
    }

    pub fn show_addr(&self){
        info!("addr:{:x}",self.get_addr());
    }
}

/// El1 and EL2 shared region for virtio requests and results.
#[repr(C)]
pub struct VirtioBridge {
    /// The first elem of req list, only virtio device updates
    pub req_front: u32,
    /// The last elem's next place of req list, only hvisor updates
    pub req_rear: u32,
    /// The first elem of res list, only hvisor updates
    pub res_front: u32,
    /// The last elem's next place of res list, only virtio device updates
    pub res_rear: u32,
    pub req_list: [HvisorDeviceReq; MAX_REQ as usize],
    pub res_list: [HvisorDeviceRes; MAX_REQ as usize], // irqs
    cfg_flags: [u64; MAX_CPUS],
    cfg_values: [u64; MAX_CPUS],
    pub mmio_addrs: [u64; MAX_DEVS],
    pub mmio_avail: u8,
    pub need_wakeup: u8,
}

impl Debug for VirtioBridge {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("VirtioBridge")
            .field("req_front", &self.req_front)
            .field("req_rear", &self.req_rear)
            .field("res_front", &self.res_front)
            .field("res_rear", &self.res_rear)
            .finish()
    }
}

/// Hvisor device requests
#[repr(C)]
#[derive(Debug)]
pub struct HvisorDeviceReq {
    pub src_cpu: u64,
    address: u64,
    size: u64,
    pub value: u64,
    src_zone: u32,
    is_write: u8,
    pub need_interrupt: u8,
    _padding: u16,
}

#[repr(C)]
pub struct HvisorDeviceRes {
    pub target_zone: u32,
    pub irq_id: u32,
}

impl HvisorDeviceReq {
    pub fn new(
        src_cpu: u64,
        address: u64,
        size: u64,
        value: u64,
        src_zone: u32,
        is_write: bool,
        need_interrupt: u8,
    ) -> Self {
        let is_write = if is_write { 1 } else { 0 };
        Self {
            src_cpu,
            address,
            size,
            value,
            src_zone,
            is_write,
            need_interrupt,
            _padding: 0,
        }
    }
}
