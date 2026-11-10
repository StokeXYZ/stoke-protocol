//! A pool's initialized ticks, sorted ascending in one growable account.
//! Plays the role of v4's `ticks` mapping plus `tickBitmap`: lookups are binary searches and
//! the next initialized tick in either direction is a neighbour in the array.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, system_instruction};
use meridian_core::{TickEntry, TicksHeader, TICKS_DISCRIMINATOR, TICKS_HEADER_SIZE, TICK_ENTRY_SIZE};

use crate::errors::PmError;

pub const INITIAL_TICK_CAPACITY: u32 = 8;
const GROW_BY: u32 = 8;

pub fn space_for(capacity: u32) -> usize {
    TICKS_HEADER_SIZE + capacity as usize * TICK_ENTRY_SIZE
}

pub struct TicksView<'a> {
    pub header: &'a mut TicksHeader,
    pub entries: &'a mut [TickEntry],
}

impl<'a> TicksView<'a> {
    pub fn load(data: &'a mut [u8], pool: &Pubkey) -> Result<Self> {
        let (h, rest) = data.split_at_mut(TICKS_HEADER_SIZE);
        let header: &mut TicksHeader = bytemuck::try_from_bytes_mut(h).map_err(|_| error!(PmError::WrongPoolAccount))?;
        require!(header.discriminator == TICKS_DISCRIMINATOR && header.pool == *pool, PmError::WrongPoolAccount);
        let cap = header.capacity as usize;
        let entries: &mut [TickEntry] = bytemuck::try_cast_slice_mut(&mut rest[..cap * TICK_ENTRY_SIZE])
            .map_err(|_| error!(PmError::WrongPoolAccount))?;
        Ok(Self { header, entries })
    }

    pub fn len(&self) -> usize {
        self.header.len as usize
    }

    fn find(&self, tick: i32) -> core::result::Result<usize, usize> {
        self.entries[..self.len()].binary_search_by_key(&tick, |e| e.tick)
    }

    /// The tick's info, or an all-zero one if it isn't initialized (like reading a v4 mapping).
    pub fn get(&self, tick: i32) -> TickEntry {
        match self.find(tick) {
            Ok(i) => self.entries[i],
            Err(_) => TickEntry { tick, ..Default::default() },
        }
    }

    pub fn get_mut(&mut self, tick: i32) -> Option<&mut TickEntry> {
        match self.find(tick) {
            Ok(i) => Some(&mut self.entries[i]),
            Err(_) => None,
        }
    }

    /// The entry for `tick`, inserting a zeroed one if it isn't there yet.
    pub fn get_or_insert(&mut self, tick: i32) -> Result<&mut TickEntry> {
        match self.find(tick) {
            Ok(i) => Ok(&mut self.entries[i]),
            Err(i) => {
                let len = self.len();
                require!(len < self.entries.len(), PmError::TickStoreFull);
                self.entries.copy_within(i..len, i + 1);
                self.entries[i] = TickEntry { tick, ..Default::default() };
                self.header.len += 1;
                Ok(&mut self.entries[i])
            }
        }
    }

    pub fn remove(&mut self, tick: i32) {
        if let Ok(i) = self.find(tick) {
            let len = self.len();
            self.entries.copy_within(i + 1..len, i);
            self.entries[len - 1] = TickEntry::default();
            self.header.len -= 1;
        }
    }

    /// Next initialized tick: the largest <= `tick` when `lte`, else the smallest > `tick`.
    pub fn next_initialized(&self, tick: i32, lte: bool) -> Option<i32> {
        let active = &self.entries[..self.len()];
        match (self.find(tick), lte) {
            (Ok(i), true) => Some(active[i].tick),
            (Err(i), true) => i.checked_sub(1).map(|j| active[j].tick),
            (Ok(i), false) => active.get(i + 1).map(|e| e.tick),
            (Err(i), false) => active.get(i).map(|e| e.tick),
        }
    }
}

/// Create a tick store (the account is a PDA, so the caller passes its signer seeds).
pub fn create<'info>(
    ticks: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    pool: &Pubkey,
    seeds: &[&[u8]],
) -> Result<()> {
    let space = space_for(INITIAL_TICK_CAPACITY);
    let lamports = Rent::get()?.minimum_balance(space);
    anchor_lang::solana_program::program::invoke_signed(
        &system_instruction::create_account(payer.key, ticks.key, lamports, space as u64, &crate::ID),
        &[payer.clone(), ticks.clone(), system_program.clone()],
        &[seeds],
    )?;
    let mut data = ticks.try_borrow_mut_data()?;
    let header: &mut TicksHeader = bytemuck::from_bytes_mut(&mut data[..TICKS_HEADER_SIZE]);
    header.discriminator = TICKS_DISCRIMINATOR;
    header.pool = *pool;
    header.len = 0;
    header.capacity = INITIAL_TICK_CAPACITY;
    Ok(())
}

/// Make room for at least `free` more ticks, growing the account (and its rent) if needed.
pub fn ensure_free<'info>(
    ticks: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    free: u32,
) -> Result<()> {
    let (len, cap) = {
        let data = ticks.try_borrow_data()?;
        let header: &TicksHeader = bytemuck::try_from_bytes(&data[..TICKS_HEADER_SIZE]).map_err(|_| error!(PmError::WrongPoolAccount))?;
        (header.len, header.capacity)
    };
    if len + free <= cap {
        return Ok(());
    }
    grow(ticks, payer, system_program, len + free + GROW_BY - cap)
}

/// Add `by` entries of capacity.
pub fn grow<'info>(ticks: &AccountInfo<'info>, payer: &AccountInfo<'info>, system_program: &AccountInfo<'info>, by: u32) -> Result<()> {
    let cap = {
        let data = ticks.try_borrow_data()?;
        let header: &TicksHeader = bytemuck::try_from_bytes(&data[..TICKS_HEADER_SIZE]).map_err(|_| error!(PmError::WrongPoolAccount))?;
        header.capacity
    };
    let new_cap = cap.checked_add(by).ok_or(PmError::MathOverflow)?;
    let space = space_for(new_cap);
    let needed = Rent::get()?.minimum_balance(space).saturating_sub(ticks.lamports());
    if needed > 0 {
        invoke(
            &system_instruction::transfer(payer.key, ticks.key, needed),
            &[payer.clone(), ticks.clone(), system_program.clone()],
        )?;
    }
    ticks.resize(space)?;
    let mut data = ticks.try_borrow_mut_data()?;
    let header: &mut TicksHeader = bytemuck::from_bytes_mut(&mut data[..TICKS_HEADER_SIZE]);
    header.capacity = new_cap;
    Ok(())
}
