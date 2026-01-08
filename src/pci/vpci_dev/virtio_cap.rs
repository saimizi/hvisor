use crate::pci::pci_struct::PciCapabilityRegion;

fn put_together(src:(u8,u8,u8,u8))->u32{
    let a =(src.0 as u32)<<24 |
    (src.1 as u32)<<16 |
    (src.2 as u32)<<8  |
    (src.3 as u32) ;
    info!("output:{:x}",a);
    a
}

pub enum VirtioCfgType {
    CommonCfg = 1,
    NotifyCfg = 2,
    IsrCfg = 3,
    DeviceCfg = 4,
    PciCfg = 5,
    SharedMemoryCfg = 8,
    VendorCfg = 9
}

pub struct VirtioPciCap{
    cap_vndr:u8,
    cap_next:u8,
    cap_len:u8,
    cfg_type:u8,
    bar:u8,
    id:u8,
    padding:[u8;2],
    offset:u32,
    length:u32

}

impl PciCapabilityRegion for VirtioPciCap{
    fn read(&self, offset: crate::pci::PciConfigAddress, size: usize) -> crate::error::HvResult<u32> {
        info!("read cap:{:x},size:{}",offset,size);
        if offset as usize %size != 0 {
            warn!("cap read is misalign!");
            return Ok(0);
        }
        if size == 1 {
            match offset {
                0 => return Ok(self.cap_vndr as u32) ,
                1 => return Ok(self.cap_next as u32),
                2 => return Ok(self.cap_len as u32),
                3 => return Ok(self.cfg_type as u32),
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
                2 => return Ok(put_together((0,0,self.cfg_type,self.cap_len))),
                4 => return Ok(put_together((0,0,self.id,self.bar))),
                _ => {
                    warn!("read u16 from unexpected area! offset:{}",offset);
                    return Ok(0);
                }
            }
        };
        if size == 4{
            match offset {
                0 => return Ok(put_together((self.cfg_type,self.cap_len,self.cap_next,self.cap_vndr))),
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
}

impl VirtioPciCap{
    pub fn new(config_type:VirtioCfgType,cap_next:u8,offset:u32,length:u32)->Self{
        Self { 
            cap_vndr:0x09, 
            cap_next, cap_len: 0x10, 
            cfg_type: config_type as u8, 
            bar: 0x04, id: 0x00, 
            padding: [0,0], 
            offset, 
            length 
        }
    }
}