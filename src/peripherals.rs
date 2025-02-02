// #[cfg(feature = "51")]
// use nrf51_pac as pac;

// #[cfg(feature = "52810")]
// use nrf52810_pac as pac;

// #[cfg(feature = "52832")]
// use nrf52832_pac as pac;

// #[cfg(feature = "52833")]
// use nrf52833_pac as pac;

// #[cfg(feature = "52840")]
// use nrf52840_pac as pac;

use nrf_pac::{
    self as pac,
    radio::{
        regs::{Int as RadioInt, Prefix0, Prefix1, Rxaddresses},
        vals::{Crcstatus, Endian, Len, Mode},
    },
    timer::{regs::Int as TimerInt, vals::Bitmode},
    TIMER0, TIMER1, TIMER2,
};

use core::sync::atomic::{compiler_fence, Ordering};

use crate::{
    app::{Addresses, FramedConsumer},
    payload::{PayloadR, PayloadW},
    Error,
};
pub(crate) use pac::{radio::vals::Txpower, radio::Radio, Interrupt};
// TODO
pub use pac::RADIO;

const CRC_INIT: u32 = 0x0000_FFFF;
const CRC_POLY: u32 = 0x0001_1021;
const NUM_PIPES: usize = 8;

#[inline]
fn bytewise_bit_swap(value: u32) -> u32 {
    value.reverse_bits().swap_bytes()
}

#[inline]
fn address_conversion(value: u32) -> u32 {
    value.reverse_bits()
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub(crate) enum RxPayloadState {
    Ack,
    NoAck,
    RepeatedAck,
    RepeatedNoAck,
    BadCRC,
}

pub struct EsbRadio<const OUT: usize, const IN: usize>
{
    radio: Radio,
    tx_grant: Option<PayloadR<OUT>>,
    rx_grant: Option<PayloadW<IN>>,
    last_crc: [u16; NUM_PIPES],
    last_pid: [u8; NUM_PIPES],
    cached_pipe: u8,
}

impl<const OUT: usize, const IN: usize> EsbRadio<OUT, IN>
{
    pub(crate) fn new(radio: Radio) -> Self {
        EsbRadio {
            radio,
            tx_grant: None,
            rx_grant: None,
            last_crc: [0; NUM_PIPES],
            last_pid: [0; NUM_PIPES],
            cached_pipe: 0,
        }
    }

    pub(crate) fn init(&mut self, max_payload: u8, tx_power: Txpower, addresses: &Addresses) {
        // Disables all interrupts, Nordic's code writes to all bits, seems to be okay
        self.radio.intenclr().write_value(RadioInt(0xFFFF_FFFF));
        self.radio.mode().write(|w| w.set_mode(Mode::NRF_2MBIT));
        let len_bits = if max_payload <= 32 { 6 } else { 8 };
        // Convert addresses to remain compatible with nRF24L devices
        let base0 = address_conversion(u32::from_le_bytes(addresses.base0));
        let base1 = address_conversion(u32::from_le_bytes(addresses.base1));
        let prefix0 = bytewise_bit_swap(u32::from_le_bytes(addresses.prefixes0));
        let prefix1 = bytewise_bit_swap(u32::from_le_bytes(addresses.prefixes1));

        self.radio.shorts().write(|w| {
            w.set_ready_start(true);
            w.set_end_disable(true);
            w.set_address_rssistart(true);
            w.set_disabled_rssistop(true);
        });

        // Enable fast ramp-up
        #[cfg(feature = "fast-ru")]
        self.radio.modecnf0.modify(|w| w.ru().fast());

        self.radio.txpower().write(|w| w.set_txpower(tx_power));

        self.radio.pcnf0().write(|w| {
            w.set_lflen(len_bits);
            w.set_s1len(3);
        });

        self.radio.pcnf1().write(|w| {
            w.set_maxlen(max_payload);
            // 4-Byte Base Address + 1-Byte Address Prefix
            w.set_balen(4);
            // Nordic's code doesn't use whitening, maybe enable in the future ?
            // w.set_whiteen(true);
            w.set_statlen(0);
            w.set_endian(Endian::BIG);
        });

        self.radio
            .crcinit()
            .write(|w| w.set_crcinit(CRC_INIT & 0x00FF_FFFF));

        self.radio
            .crcpoly()
            .write(|w| w.set_crcpoly(CRC_POLY & 0x00FF_FFFF));

        self.radio.crccnf().write(|w| w.set_len(Len::TWO));

        self.radio.base0().write_value(base0);
        self.radio.base1().write_value(base1);

        self.radio.prefix0().write_value(Prefix0(prefix0));
        self.radio.prefix1().write_value(Prefix1(prefix1));

        // NOTE(unsafe) `rf_channel` was checked to be between 0 and 100 during the creation of
        // the `Adresses` object
        self.radio
            .frequency()
            .write(|w| w.set_frequency(addresses.rf_channel));
    }

    // Clears the Disabled event to not retrigger the interrupt
    #[inline]
    pub(crate) fn clear_disabled_event(&mut self) {
        self.radio.events_disabled().write_value(0);
    }

    // Checks the Disabled event
    #[inline]
    pub(crate) fn check_disabled_event(&mut self) -> bool {
        self.radio.events_disabled().read() == 1
    }

    // Clears the End event to not retrigger the interrupt
    #[inline]
    pub(crate) fn clear_end_event(&mut self) {
        self.radio.events_end().write_value(0);
    }

    // Checks the Ready event
    #[inline]
    pub(crate) fn check_ready_event(&self) -> bool {
        self.radio.events_ready().read() == 1
    }

    // Clears the Ready event to not retrigger the interrupt
    #[inline]
    pub(crate) fn clear_ready_event(&mut self) {
        self.radio.events_ready().write_value(0);
    }

    // Disables Disabled interrupt
    #[inline]
    pub(crate) fn disable_disabled_interrupt(&mut self) {
        self.radio.intenclr().write(|w| w.set_disabled(true));
    }

    // Disables the radio and the `radio disabled` interrupt
    pub(crate) fn stop(&mut self, drop_grants: bool) {
        self.radio.shorts().modify(|w| {
            w.set_disabled_rxen(false);
            w.set_disabled_txen(false);
        });
        self.disable_disabled_interrupt();
        self.radio.tasks_disable().write_value(1);

        // Wait for the disable event to kick in, to make sure that the `task_disable` write won't
        // trigger an interrupt
        while self.radio.events_disabled().read() == 0 {}
        self.clear_disabled_event();

        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        if drop_grants {
            // Drop grants we might have
            self.tx_grant.take();
            self.rx_grant.take();
        }
    }

    // --------------- PTX methods --------------- //

    // Transmit a packet and setup interrupts.
    pub(crate) fn transmit(&mut self, payload: PayloadR<OUT>, ack: bool) {
        if ack {
            // Go to RX mode after the transmission
            self.radio.shorts().modify(|w| w.set_disabled_rxen(true));
        }
        self.radio.intenset().write(|w| w.set_disabled(true));
        // NOTE(unsafe) Pipe fits in 3 bits
        self.radio
            .txaddress()
            .write(|w| w.set_txaddress(payload.pipe()));
        // NOTE(unsafe) Pipe only goes from 0 through 7
        self.radio
            .rxaddresses()
            .write_value(Rxaddresses(1 << payload.pipe()));

        self.radio
            .packetptr()
            .write_value(payload.dma_pointer() as u32);
        self.radio.events_address().write_value(0);
        self.clear_disabled_event();
        self.clear_ready_event();
        self.clear_end_event();
        self.radio.events_payload().write_value(0); // do we need this ? Probably not

        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        self.radio.tasks_txen().write_value(1);
        self.tx_grant = Some(payload);
    }

    // Must be called after the end of TX if the user did not request an ack
    pub(crate) fn finish_tx_no_ack(&mut self) {
        // We could use `Acquire` ordering if could prove that a read occurred
        // "No re-ordering of reads and writes across this point is allowed."
        compiler_fence(Ordering::SeqCst);

        // Transmission completed, release packet. If we are here we should always have the tx_grant
        if let Some(grant) = self.tx_grant.take() {
            grant.release();
        }

        // It will be enabled again in a call to `transmit`.
        self.disable_disabled_interrupt();
    }

    // Must be called after the end of TX if the user requested for an ack.
    // Timers must be set accordingly by the upper stack
    pub(crate) fn prepare_for_ack(&mut self, mut rx_buf: PayloadW<IN>) {
        self.clear_ready_event();
        // We need a compiler fence here because the DMA will automatically start listening for
        // packets after the ramp-up is completed
        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);
        self.radio
            .packetptr()
            .write_value(rx_buf.dma_pointer() as u32);

        // Check if the radio turn around was faster than us
        debug_assert!(!self.check_ready_event(), "Missed window (PTX)");

        self.rx_grant = Some(rx_buf);
        // this already fired
        self.radio.shorts().modify(|w| w.set_disabled_rxen(false));

        // We don't release the packet here because we may need to retransmit
    }

    // Returns true if the ack was received successfully
    // The upper stack is responsible for checking and disabling the timeouts
    #[inline]
    pub(crate) fn check_ack(&mut self) -> Result<bool, Error> {
        let ret = self.radio.crcstatus().read().crcstatus() == Crcstatus::CRCOK;
        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);

        if ret {
            let (tx_grant, mut rx_grant) = (
                self.tx_grant.take().ok_or(Error::InternalError)?,
                self.rx_grant.take().ok_or(Error::InternalError)?,
            );

            let pipe = tx_grant.pipe();
            tx_grant.release();

            let rssi = self.radio.rssisample().read().rssisample();
            rx_grant.set_pipe(pipe);
            rx_grant.set_rssi(rssi);
            rx_grant.commit_all();
        } else {
            // Drop `tx_grant` and `rx_grant` so the upper stack can pass them again in the next
            // `transmit` and `prepare_for_ack`
            self.tx_grant.take();
            self.rx_grant.take();
        }
        Ok(ret)
    }

    // --------------- PRX methods --------------- //

    // Start listening for packets and setup necessary shorts and interrupts
    pub(crate) fn start_receiving(&mut self, mut rx_buf: PayloadW<IN>, enabled_pipes: u8) {
        // Start TX after receiving a packet as it might need an ack
        self.radio.shorts().modify(|w| w.set_disabled_txen(true));

        self.radio.intenset().write(|w| w.set_disabled(true));
        self.radio
            .rxaddresses()
            .write_value(Rxaddresses(enabled_pipes as u32));

        self.radio
            .packetptr()
            .write_value(rx_buf.dma_pointer() as u32);
        self.radio.events_address().write_value(0);
        self.clear_disabled_event();
        self.clear_ready_event();
        self.clear_end_event();
        self.radio.events_payload().write_value(0); // TODO: do we need this ? Probably not

        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        self.radio.tasks_rxen().write_value(1);

        self.rx_grant = Some(rx_buf);
    }

    // Check the received packet.
    #[inline]
    pub(crate) fn check_packet(
        &mut self,
        consumer: &mut FramedConsumer<OUT>,
    ) -> Result<RxPayloadState, Error> {
        // If the user didn't provide a packet to send, we will fall back to this empty ack packet
        static FALLBACK_ACK: [u8; 2] = [0, 0];

        if self.radio.crcstatus().read().crcstatus() == Crcstatus::CRCERROR {
            // Bad CRC, clear events and restart RX.
            self.stop(false);
            self.radio.shorts().modify(|w| w.set_disabled_txen(true));
            self.radio.intenset().write(|w| w.set_disabled(true));
            // "Preceding reads and writes cannot be moved past subsequent writes."
            compiler_fence(Ordering::Release);

            // NOTE(unsafe) 1 is a valid value to write to this register
            self.radio.tasks_rxen().write_value(1);
            return Ok(RxPayloadState::BadCRC);
        }
        // "Subsequent reads and writes cannot be moved ahead of preceding reads."
        compiler_fence(Ordering::Acquire);
        self.clear_ready_event();

        let pipe = self.radio.rxmatch().read().rxmatch() as usize;
        let crc = self.radio.rxcrc().read().rxcrc() as u16;
        let rx_grant = self.rx_grant.as_ref().ok_or(Error::InternalError)?;
        let (pid, ack) = (rx_grant.pid(), !rx_grant.no_ack());
        let repeated = (self.last_crc[pipe] == crc) && (self.last_pid[pipe] == pid);

        if ack {
            // This is a bit risky, the radio is turning around since before the beginning of the
            // method, we should have enough time if the Radio interrupt is top priority, otherwise
            // we might have a problem, should we disable the `disabled_txen` shorcut ? We might
            // have problems to acknowledge consistently if we do so.

            // NOTE(unsafe) Any byte value is valid for this register.
            self.radio.txaddress().write(|w| {
                w.set_txaddress(pipe as u8);
            });

            let mut dma_pointer = FALLBACK_ACK.as_ptr() as u32;

            if repeated {
                if pipe == self.cached_pipe as usize {
                    if let Some(grant) = &self.tx_grant {
                        dma_pointer = grant.dma_pointer() as u32;
                    }
                }
            } else {
                // Our last ack packet was received, release it and ask for a new one
                if let Some(grant) = self.tx_grant.take() {
                    grant.release();
                }

                // "No re-ordering of reads and writes across this point is allowed."
                compiler_fence(Ordering::SeqCst);

                if let Ok(payload) = consumer.read().map(PayloadR::new) {
                    dma_pointer = payload.dma_pointer() as u32;
                    self.tx_grant = Some(payload);
                }
            }

            // "Preceding reads and writes cannot be moved past subsequent writes."
            compiler_fence(Ordering::Release);

            // NOTE(unsafe) Any u32 is valid to write to this register.
            self.radio.packetptr().write_value(dma_pointer);

            // Check if the ramp-up was faster than us :/
            debug_assert!(!self.check_ready_event(), "Missed window (PRX)");

            // Disables the shortcut for `txen`, we already hit that.
            // Enables the shortcut for `rxen` to turn around to rx after the end of the ack
            // transmission.
            self.radio.shorts().modify(|w| {
                w.set_disabled_txen(false);
                w.set_disabled_rxen(true);
            });
        } else {
            // Stops the radio before transmission begins
            self.stop(false);
        }

        if repeated {
            if ack {
                return Ok(RxPayloadState::RepeatedAck);
            } else {
                return Ok(RxPayloadState::RepeatedNoAck);
            }
        }
        self.last_crc[pipe] = crc;
        self.last_pid[pipe] = pid;
        self.cached_pipe = pipe as u8;

        let mut grant = self.rx_grant.take().ok_or(Error::InternalError)?;
        let rssi = self.radio.rssisample().read().rssisample();
        grant.set_rssi(rssi);
        grant.set_pipe(pipe as u8);
        grant.commit_all();
        if ack {
            Ok(RxPayloadState::Ack)
        } else {
            Ok(RxPayloadState::NoAck)
        }
    }

    // Must be called after the end of the ack transmission.
    // `rx_buf` must only be `None` if a previous call to `check_packet` returned `RepeatedAck`
    pub(crate) fn complete_rx_ack(
        &mut self,
        mut rx_buf: Option<PayloadW<IN>>,
    ) -> Result<(), Error> {
        let dma_pointer = if let Some(mut grant) = rx_buf.take() {
            let pointer = grant.dma_pointer() as u32;
            self.rx_grant = Some(grant);
            pointer
        } else {
            self.rx_grant
                .as_mut()
                .ok_or(Error::InternalError)?
                .dma_pointer() as u32
        };

        // "No re-ordering of reads and writes across this point is allowed."
        compiler_fence(Ordering::SeqCst);

        self.radio.packetptr().write_value(dma_pointer);

        // We don't release the `tx_grant` here, because we don't know if it was really received.

        // Disables the shortcut for `rxen`, we already hit that.
        // Enables the shortcut for `txen` to turn around to tx after receiving a packet
        // transmission.
        self.radio.shorts().modify(|w| {
            w.set_disabled_rxen(false);
            w.set_disabled_txen(true);
        });
        Ok(())
    }

    // Must be called after `check_packet` returns `RxPayloadState::NoAck`.
    // `rx_buf` must only be `None` if a previous call to `check_packet` returned `RepeatedNoAck`
    pub(crate) fn complete_rx_no_ack(&mut self, mut rx_buf: Option<PayloadW<IN>>) {
        // Since we're in the no-ack branch, the previous value of `packetptr` still is the last
        // `rx_grant` that we still hold if `check_packet` returned `RepeatedNoAck`. Therefore, we
        // only need to update if `rx_buf` is `Some`.
        if let Some(mut grant) = rx_buf.take() {
            self.radio
                .packetptr()
                .write_value(grant.dma_pointer() as u32);
            self.rx_grant = Some(grant);
        }

        self.radio.shorts().modify(|w| w.set_disabled_txen(true));
        self.radio.intenset().write(|w| w.set_disabled(true));
        // "Preceding reads and writes cannot be moved past subsequent writes."
        compiler_fence(Ordering::Release);

        self.radio.tasks_rxen().write_value(1);
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Trait implemented for the nRF timer peripherals.
pub trait EsbTimer: sealed::Sealed {
    /// Initialize the timer with a 1MHz rate.
    fn init(&mut self);

    /// Configures the timer's interrupt used for the retransmit, to fire after a given time in
    /// micro seconds.
    fn set_interrupt_retransmit(&mut self, micros: u16);

    /// Acknowledges the retransmit interrupt.
    fn clear_interrupt_retransmit();

    /// Returns whether the retransmit interrupt is currently pending, atomically.
    fn is_retransmit_pending() -> bool;

    /// Configures the timer's interrupt used for the acknowledge timeout, to fire after a given
    /// time in micro seconds.
    fn set_interrupt_ack(&mut self, micros: u16);

    /// Acknowledges the ack timeout interrupt.
    fn clear_interrupt_ack();

    /// Returns whether the ack timeout interrupt is currently pending.
    fn is_ack_pending() -> bool;

    /// Stops the timer, atomically.
    fn stop();
}

use pac::timer::Timer;

pub trait PtrTimer {
    const ME: Timer;
    fn timer(&mut self) -> &mut Timer;
    /// # Safety
    ///
    /// You only take once
    unsafe fn take() -> Self;
}

pub struct Timer0 {
    timer: Timer,
}
pub struct Timer1 {
    timer: Timer,
}
pub struct Timer2 {
    timer: Timer,
}

impl PtrTimer for Timer0 {
    const ME: Timer = TIMER0;

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }

    unsafe fn take() -> Self {
        Self { timer: Self::ME }
    }
}

impl PtrTimer for Timer1 {
    const ME: Timer = TIMER1;

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }

    unsafe fn take() -> Self {
        Self { timer: Self::ME }
    }
}

impl PtrTimer for Timer2 {
    const ME: Timer = TIMER2;

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }

    unsafe fn take() -> Self {
        Self { timer: Self::ME }
    }
}

impl<T: PtrTimer + sealed::Sealed> EsbTimer for T {
    #[inline]
    fn init(&mut self) {
        // Disables all interrupts, Nordic's code writes to all bits, seems to be okay
        self.timer().intenclr().write_value(TimerInt(0xFFFF_FFFF));
        Self::stop();
        self.timer()
            .bitmode()
            .write(|w| w.set_bitmode(Bitmode::_32BIT));
        // 2^4 = 16
        // 16 MHz / 16 = 1 MHz = µs resolution
        self.timer().prescaler().write(|w| w.set_prescaler(4));
    }

    // CC[0] will be used for the retransmit timeout and CC[1] will be used for the ack
    // timeout

    #[inline]
    fn set_interrupt_retransmit(&mut self, micros: u16) {
        self.timer().cc(0).write_value(micros as u32);
        self.timer().events_compare(0).write_value(0);
        self.timer().intenset().write(|w| w.set_compare(0, true));

        // Clears and starts the counter
        self.timer().tasks_clear().write_value(1);
        self.timer().tasks_start().write_value(1);
    }

    #[inline]
    fn clear_interrupt_retransmit() {
        // NOTE(unsafe) This will be used for atomic operations, only
        let timer = Self::ME;

        timer.intenclr().write(|w| w.set_compare(0, false));
        timer.events_compare(0).write_value(0);

        Self::stop();
    }

    #[inline]
    fn is_retransmit_pending() -> bool {
        // NOTE(unsafe) This will be used for atomic operations, only
        let timer = Self::ME;

        timer.events_compare(0).read() == 1u32
    }

    fn set_interrupt_ack(&mut self, micros: u16) {
        // get current counter
        self.timer().tasks_capture(1).write_value(1);
        let current_counter = self.timer().cc(1).read();

        self.timer()
            .cc(1)
            .write_value(current_counter + micros as u32);
        self.timer().events_compare(1).write_value(0);
        self.timer().intenset().write(|w| w.set_compare(1, true));
    }

    #[inline]
    fn clear_interrupt_ack() {
        // NOTE(unsafe) This will be used for atomic operations, only
        let timer = Self::ME;

        timer.intenclr().write(|w| w.set_compare(1, false));
        timer.events_compare(1).write_value(0);
    }

    #[inline]
    fn is_ack_pending() -> bool {
        // NOTE(unsafe) This will be used for atomic operations, only
        let timer = Self::ME;

        timer.events_compare(1).read() == 1u32
    }

    #[inline]
    fn stop() {
        // NOTE(unsafe) This will be used for atomic operations, only
        let timer = Self::ME;

        timer.tasks_stop().write_value(1);
    }
}

impl sealed::Sealed for Timer0 {}
impl sealed::Sealed for Timer1 {}
impl sealed::Sealed for Timer2 {}

// #[cfg(not(feature = "51"))]
// impl_timer!(pac::TIMER0, pac::TIMER1, pac::TIMER2);

// #[cfg(feature = "51")]
// impl_timer!(pac::TIMER0);
