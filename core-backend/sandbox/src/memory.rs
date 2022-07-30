// This file is part of Gear.

// Copyright (C) 2021-2022 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! sp-sandbox extensions for memory.

use gear_core::memory::{Error, HostPointer, Memory, PageNumber, WasmPageNumber};
use sp_sandbox::SandboxMemory;

/// Wrapper for sp_sandbox::Memory.
pub struct MemoryWrap(sp_sandbox::default_executor::Memory);

impl MemoryWrap {
    /// Wrap sp_sandbox::Memory for Memory trait.
    pub fn new(mem: sp_sandbox::default_executor::Memory) -> Self {
        MemoryWrap(mem)
    }
}

/// Memory interface for the allocator.
impl Memory for MemoryWrap {
    fn grow(&mut self, pages: WasmPageNumber) -> Result<PageNumber, Error> {
        self.0
            .grow(pages.0)
            .map(|prev| prev.into())
            .map_err(|_| Error::OutOfBounds)
    }

    fn size(&self) -> WasmPageNumber {
        self.0.size().into()
    }

    fn write(&mut self, offset: usize, buffer: &[u8]) -> Result<(), Error> {
        self.0
            .set(offset as u32, buffer)
            .map_err(|_| Error::MemoryAccessError)
    }

    fn read(&self, offset: usize, buffer: &mut [u8]) -> Result<(), Error> {
        self.0
            .get(offset as u32, buffer)
            .map_err(|_| Error::MemoryAccessError)
    }

    fn data_size(&self) -> usize {
        self.0.size() as usize * WasmPageNumber::size()
    }

    unsafe fn get_buffer_host_addr_unsafe(&self) -> HostPointer {
        self.0.get_buff()
    }
}

/// can't be tested outside the node runtime
#[cfg(test)]
mod tests {
    use super::*;
    use gear_backend_common::{assert_err, assert_ok};
    use gear_core::memory::AllocationsContext;

    fn new_test_memory(static_pages: u32, max_pages: u32) -> (AllocationsContext, MemoryWrap) {
        use sp_sandbox::SandboxMemory as WasmMemory;

        let memory = MemoryWrap::new(
            WasmMemory::new(static_pages, Some(max_pages)).expect("Memory creation failed"),
        );

        (
            AllocationsContext::new(Default::default(), static_pages.into(), max_pages.into()),
            memory,
        )
    }

    #[test]
    fn smoky() {
        let (mut mem, mut mem_wrap) = new_test_memory(16, 256);

        assert_ok!(mem.alloc(16.into(), &mut mem_wrap), 16.into());

        // there is a space for 14 more
        for _ in 0..14 {
            assert_ok!(mem.alloc(16.into(), &mut mem_wrap));
        }

        // no more mem!
        assert_err!(mem.alloc(1.into(), &mut mem_wrap), Error::OutOfBounds);

        // but we free some
        assert_ok!(mem.free(137.into()));

        // and now can allocate page that was freed
        assert_ok!(mem.alloc(1.into(), &mut mem_wrap), 137.into());

        // if we have 2 in a row we can allocate even 2
        assert_ok!(mem.free(117.into()));
        assert_ok!(mem.free(118.into()));

        assert_ok!(mem.alloc(2.into(), &mut mem_wrap), 117.into());

        // but if 2 are not in a row, bad luck
        assert_ok!(mem.free(117.into()));
        assert_ok!(mem.free(158.into()));

        assert_err!(mem.alloc(2.into(), &mut mem_wrap), Error::OutOfBounds);
    }
}
