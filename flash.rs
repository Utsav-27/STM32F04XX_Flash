use core::mem;
use cortex_m::interrupt;
use stm32f0xx_hal::stm32::FLASH;

pub use traits::{Error, FlashPage, Read, Result, WriteErase};

mod traits:

pub const FLASH_START: usize = 0x0800_0000;

pub const PAGE_SIZE: u32 = 1024;
pub const NUM_PAGES: u32 = 32; // our chip, others up to 64

const FLASH_KEY1: u32 = 0x4567_0123;
const FLASH_KEY2: u32 = 0xCDEF_89AB;

impl FlashPage {
    pub const fn to_address(&self) -> usize {
        FLASH_START + self.0 * PAGE_SIZE as usize
    }
}

impl FlashExt for FLASH {
    fn unlock(self) -> core::result::Result<UnlockedFlash, FLASH> {
        // wait while memory interface is busy
        while self.sr.read().bsy().bit_is_set() {}

        // Unlock Flash
        self.keyr.write(|w| w.fkeyr().bits(FLASH_KEY1));
        self.keyr.write(|w| w.fkeyr().bits(FLASH_KEY2));

        // Verify Success
        if self.cr.read().lock().bit_is_clear() {
            Ok(UnlockedFlash { f: self })
        } else {
            Err(self)
        }
    }
}

pub trait FlashExt {
    // Unlocks Flash memory for erasure and writing
    fn unlock(self) -> core::result::Result<UnlockedFlash, FLASH>;
}

pub struct UnlockedFlash {
    f: FLASH,
}

impl UnlockedFlash {
    pub fn lock(self) -> FLASH {
        self.f.cr.modify(|_, w| w.lock().set_bit());
        self.f
    }
}

impl Read for UnlockedFlash {
    type NativeType = u8;
    fn read_native(&self, address: usize, array: &mut [Self::NativeType]) {
        let mut address = address as *const Self::NativeType;
        for data in array {
            unsafe {
                *data = core::ptr::read(address);
                address = address.add(1);
            }
        }
    }

    fn read(&self, address: usize, buf: &mut [u8]) {
        self.read_native(address, buf);
    }
}
impl WriteErase for UnlockedFlash {
    type NativeType = u16;

    fn status(&self) -> Result {
        let sr = self.f.sr.read();
        if sr.bsy().bit_is_set() {
            return Err(Error::Busy);
        }
        if sr.pgerr().bit_is_set() {
            return Err(Error::ProgrammingError);
        }
        if sr.wrprt().bit_is_set() {
            return Err(Error::WriteProtectionError);
        }
        Ok(())
    }

    fn erase_page(&mut self, page: FlashPage) -> Result {
        if page.0 >= NUM_PAGES as usize {
            return Err(Error::PageOutOfRange);
        }

        // Wait, while the memory interface is busy.
        while self.f.sr.read().bsy().bit_is_set() {}
        self.clear_errors();

        // We absoluty can't have any access to Flash while preparing the
        // erase, or the process will be interrupted. This includes any
        // access to the vector table or interrupt handlers that might be
        // caused by an interrupt.
        interrupt::free(|_| {
            self.f.cr.modify(|_, w| w.per().set_bit());
            self.f
                .ar
                .write(|w| unsafe { w.bits(page.to_address() as u32) });
            self.f.cr.modify(|_, w| w.strt().set_bit());
        });
        let result = self.wait();

        if self.f.sr.read().eop().bit_is_set() {
            self.f.sr.write(|w| w.eop().set_bit());
        } else {
            return Err(Error::Eop);
        }
        self.f.cr.modify(|_, w| w.per().clear_bit());

        result
    }

    fn write_native(&mut self, address: usize, array: &[Self::NativeType]) -> Result {
        // wait while memory interface is busy
        while self.f.sr.read().bsy().bit_is_set() {}
        self.clear_errors();

        // set the PG bit in flash cr register
        self.f.cr.modify(|_, w| w.pg().set_bit());

        // Possible to program half word (16 bit)
        let mut address = address as *mut u16;
        for &word in array {
            interrupt::free(|_| unsafe {
                address.write_volatile(word);
                address = address.add(1);
            });

            self.wait()?;

            if self.f.sr.read().eop().bit_is_set() {
                self.f.sr.write(|w| w.eop().set_bit());
            }
        }
        self.f.cr.modify(|_, w| w.pg().clear_bit());
        Ok(())
    }

    /// provide address which does not conflict with data or code address
    fn write(&mut self, address: usize, data: &[u8]) -> Result {
        let address_offset = address % mem::align_of::<Self::NativeType>();
        let unaligned_size = (mem::size_of::<Self::NativeType>() - address_offset)
            % mem::size_of::<Self::NativeType>();

        if unaligned_size > 0 {
            let unaligned_data = &data[..unaligned_size];
            // Handle unaligned address data, make it into a native write
            let mut data = 0xffffu16;
            for b in unaligned_data {
                data = (data >> 8) | ((*b as Self::NativeType) << 8);
            }
            let unaligned_address = address - address_offset;
            let native = &[data];
            self.write_native(unaligned_address, native)?;
        }

        // Handle aligned address data
        let aligned_data = &data[unaligned_size..];
        let mut aligned_address = if unaligned_size > 0 {
            address - address_offset + mem::size_of::<Self::NativeType>()
        } else {
            address
        };
        let mut chunks = aligned_data.chunks_exact(mem::size_of::<Self::NativeType>());

        for exact_chunk in &mut chunks {
            // Write chunks
            let native = &[Self::NativeType::from_ne_bytes(
                exact_chunk.try_into().unwrap(),
            )];
            self.write_native(aligned_address, native)?;
            aligned_address += mem::size_of::<Self::NativeType>();
        }
        let rem = chunks.remainder();

        if !rem.is_empty() {
            let mut data = 0xffffu16;
            // Write remainder
            for b in rem.iter().rev() {
                data = (data << 8) | *b as Self::NativeType;
            }

            let native = &[data];
            self.write_native(aligned_address, native)?;
        }
        Ok(())
    }
}

impl UnlockedFlash {
    fn clear_errors(&mut self) {
        self.f
            .sr
            .modify(|_, w| w.pgerr().set_bit().wrprt().set_bit());
    }

    fn wait(&self) -> Result {
        while self.f.sr.read().bsy().bit_is_set() {}
        self.status()
    }
}