use crate::packet::Addresses;
use crate::peripherals::{ESBRadio, ESBTimer, RADIO};
use crate::Error;
use crate::State;
use bbqueue::{
    framed::{FrameConsumer, FrameProducer, FrameGrantW, FrameGrantR},
    ArrayLength, BBBuffer,
};
use core::result::Result;

// | SW USE                        |               ACTUAL DMA PART                                    |
// | rssi - 1 byte | pipe - 1 byte | length - 1 byte | pid_no_ack - 1 byte | payload - 1 to 252 bytes |

pub struct PayloadHeader {
    rssi: u8,
    pipe: u8,
    length: u8,
    pid_no_ack: u8,
}

pub type PhBytes = [u8; 4];
impl PayloadHeader {

    fn to_bytes(self) -> PhBytes {

        [
            self.rssi,
            self.pipe,

            // DO NOT REORDER!
            self.length,
            self.pid_no_ack,
        ]
    }

    fn from_bytes(bytes: &PhBytes) -> Self {
        Self {
            rssi: bytes[Self::rssi_idx()],
            pipe: bytes[Self::pipe_idx()],
            length: bytes[Self::length_idx()],
            pid_no_ack: bytes[Self::pid_no_ack_idx()],
        }
    }

    const fn rssi_idx() -> usize {
        0
    }

    const fn pipe_idx() -> usize {
        1
    }

    const fn length_idx() -> usize {
        // DO NOT CHANGE! HW DEPENDANT
        2
    }

    const fn pid_no_ack_idx() -> usize {
        // DO NOT CHANGE! HW DEPENDANT
        3
    }

    const fn header_size() -> usize {
        core::mem::size_of::<PhBytes>()
    }

    const fn dma_payload_offset() -> isize {
        2
    }
}

pub struct PayloadR<N: ArrayLength<u8>> {
    pub grant: FrameGrantR<'static, N>,
}

impl<N> PayloadR<N>
where
    N: ArrayLength<u8>
{
    pub fn new(
        raw_grant: FrameGrantR<'static, N>,
    ) -> Self {
        Self {
            grant: raw_grant
        }
    }

    pub fn get_header(&self) -> PayloadHeader {
        const LEN: usize = PayloadHeader::header_size();
        let mut bytes = [0u8; LEN];
        bytes.copy_from_slice(&self.grant[..LEN]);
        PayloadHeader::from_bytes(&bytes)
    }

    pub unsafe fn dma_pointer(&mut self) -> *const u8 {
        self.grant
            .deref()
            .as_ptr()
            .offset(PayloadHeader::dma_payload_offset())
    }

    pub fn pipe(&self) -> u8 {
        self.grant[PayloadHeader::pipe_idx()]
    }

    pub fn release(self) {
        self.grant.release()
    }
}

use core::ops::{Deref, DerefMut};

impl<N> Deref for PayloadR<N>
where
    N: ArrayLength<u8>
{
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.grant[PayloadHeader::header_size()..]
    }
}

pub struct PayloadW<N: ArrayLength<u8>> {
    pub grant: FrameGrantW<'static, N>,
}

impl<N> PayloadW<N>
where
    N: ArrayLength<u8>
{
    pub(crate) fn new_from_app(
        mut raw_grant: FrameGrantW<'static, N>,
        header: PayloadHeader, // HMMMMMM
    ) -> Self {
        raw_grant[..PayloadHeader::header_size()].copy_from_slice(&header.to_bytes());
        Self {
            grant: raw_grant
        }
    }

    pub(crate) fn new_from_radio(
        raw_grant: FrameGrantW<'static, N>,
    ) -> Self {
        Self {
            grant: raw_grant,
        }
    }

    pub(crate) fn update_header(
        &mut self,
        header: PayloadHeader
    ) {
        self.grant[..PayloadHeader::header_size()].copy_from_slice(&header.to_bytes());
    }

    pub unsafe fn dma_pointer(&mut self) -> *mut u8 {
        self.grant
            .deref_mut()
            .as_mut_ptr()
            .offset(PayloadHeader::dma_payload_offset())
    }

    pub fn pipe(&self) -> u8 {
        self.grant[PayloadHeader::pipe_idx()]
    }

    pub fn payload_len(&self) -> usize {
        self.grant[PayloadHeader::length_idx()] as usize
    }

    pub fn commit_all(self) {
        let payload_len = self.payload_len();
        self.grant.commit(payload_len + PayloadHeader::header_size())
    }

    pub fn commit(mut self, used: usize) {
        let max_payload_len = self.payload_len();
        let payload_len = core::cmp::min(used, max_payload_len);
        self.grant[PayloadHeader::length_idx()] = payload_len as u8;

        self.grant.commit(payload_len + PayloadHeader::header_size())
    }
}

impl<N> Deref for PayloadW<N>
where
    N: ArrayLength<u8>
{
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.grant[PayloadHeader::header_size()..]
    }
}

impl<N> DerefMut for PayloadW<N>
where
    N: ArrayLength<u8>,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.grant[PayloadHeader::header_size()..]
    }
}