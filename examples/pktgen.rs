#![no_std]
#![no_main]

extern crate panic_itm;

use cortex_m_rt::{entry, exception};
use core::cell::RefCell;
use core::default::Default;

use cortex_m::asm;
use cortex_m::interrupt::Mutex;

#[cfg(feature = "stm32f4xx-hal")]
use stm32f4xx_hal::{
    gpio::GpioExt,
    stm32::{Peripherals, CorePeripherals, SYST, interrupt},
};

#[cfg(feature = "stm32f7xx-hal")]
use stm32f7xx_hal::{
    gpio::GpioExt,
    device::{Peripherals, CorePeripherals, SYST, interrupt},
};

use core::fmt::Write;
use cortex_m_semihosting::hio;

use stm32_eth::{Eth, RingEntry, TxError};


const SRC_MAC: [u8; 6] = [0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];
const DST_MAC: [u8; 6] = [0x00, 0x00, 0xBE, 0xEF, 0xDE, 0xAD];
const ETH_TYPE: [u8; 2] = [0x80, 0x00];

static TIME: Mutex<RefCell<usize>> = Mutex::new(RefCell::new(0));
static ETH_PENDING: Mutex<RefCell<bool>> = Mutex::new(RefCell::new(false));


#[entry]
fn main() -> ! {
    let mut stdout = hio::hstdout().unwrap();

    let p = Peripherals::take().unwrap();
    let mut cp = CorePeripherals::take().unwrap();

    setup_systick(&mut cp.SYST);

    writeln!(stdout, "Enabling ethernet...").unwrap();
    stm32_eth::setup(&p.RCC, &p.SYSCFG);
    let gpioa = p.GPIOA.split();
    let gpiob = p.GPIOB.split();
    let gpioc = p.GPIOC.split();
    let gpiog = p.GPIOG.split();
    stm32_eth::setup_pins(
        gpioa.pa1, gpioa.pa2, gpioa.pa7, gpiob.pb13, gpioc.pc1,
        gpioc.pc4, gpioc.pc5, gpiog.pg11, gpiog.pg13
    );

    let mut rx_ring: [RingEntry<_>; 16] = Default::default();
    let mut tx_ring: [RingEntry<_>; 8] = Default::default();
    let mut eth = Eth::new(
        p.ETHERNET_MAC, p.ETHERNET_DMA,
        &mut rx_ring[..], &mut tx_ring[..]
    );
    eth.enable_interrupt(&mut cp.NVIC);

    // Main loop
    let mut last_stats_time = 0usize;
    let mut rx_bytes = 0usize;
    let mut rx_pkts = 0usize;
    let mut tx_bytes = 0usize;
    let mut tx_pkts = 0usize;
    let mut last_status = None;

    loop {
        let time: usize = cortex_m::interrupt::free(|cs| {
            *TIME.borrow(cs)
                .borrow()
        });

        // print stats every 30 seconds
        if time >= last_stats_time + 30 {
            let t = time - last_stats_time;
            writeln!(
                stdout, "T={}\tRx:\t{} KB/s\t{} pps\tTx:\t{} KB/s\t{} pps",
                time,
                rx_bytes / 1024 / t, rx_pkts / t,
                tx_bytes / 1024 / t, tx_pkts / t
            ).unwrap();
            // Reset
            rx_bytes = 0;
            rx_pkts = 0;
            tx_bytes = 0;
            tx_pkts = 0;
            last_stats_time = time;
        }

        // Link change detection
        let status = eth.status();
        if last_status.map(|last_status| last_status != status)
            .unwrap_or(true)
        {
            if ! status.link_detected() {
                writeln!(
                    stdout,
                    "Ethernet: no link detected"
                ).unwrap();
            } else {
                writeln!(
                    stdout,
                    "Ethernet: link detected with {} Mbps/{}",
                    status.speed(),
                    match status.is_full_duplex() {
                        Some(true) => "FD",
                        Some(false) => "HD",
                        None => "?",
                    }
                ).unwrap();
            }

            last_status = Some(status);
        }

        cortex_m::interrupt::free(|cs| {
            let mut eth_pending =
                ETH_PENDING.borrow(cs)
                .borrow_mut();
            *eth_pending = false;
        });

        // handle rx packet
        {
            let mut recvd = 0usize;
            while let Ok(pkt) = eth.recv_next() {
                rx_bytes += pkt.len();
                rx_pkts += 1;
                pkt.free();

                recvd += 1;
                if recvd > 16 {
                    // Break arbitrarily to process tx eventually
                    break;
                }
            }
        }
        if ! eth.rx_is_running() {
            writeln!(stdout, "RX stopped").unwrap();
        }

        // fill tx queue
        const SIZE: usize = 1500;
        if status.link_detected() {
            'egress: loop {
                let r = eth.send(SIZE, |buf| {
                    buf[0..6].copy_from_slice(&DST_MAC);
                    buf[6..12].copy_from_slice(&SRC_MAC);
                    buf[12..14].copy_from_slice(&ETH_TYPE);
                });

                //writeln!(stdout, "loop").unwrap();

                match r {
                    Ok(()) => {
                        tx_bytes += SIZE;
                        tx_pkts += 1;
                    }
                    Err(TxError::WouldBlock) => break 'egress,
                }
            }
        }

        writeln!(stdout, "next").unwrap();

        cortex_m::interrupt::free(|cs| {
            let eth_pending =
                ETH_PENDING.borrow(cs)
                .borrow_mut();
            if ! *eth_pending {
                asm::wfi();
            }
        });
    }
}

fn setup_systick(syst: &mut SYST) {
    syst.set_reload(100 * SYST::get_ticks_per_10ms());
    syst.enable_counter();
    syst.enable_interrupt();

    if ! SYST::is_precise() {
        let mut stderr = hio::hstderr().unwrap();
        writeln!(
            stderr,
            "Warning: SYSTICK with source {:?} is not precise",
            syst.get_clock_source()
        ).unwrap();
    }
}

#[exception]
fn SysTick() {
    cortex_m::interrupt::free(|cs| {
        let mut time =
            TIME.borrow(cs)
            .borrow_mut();
        *time += 1;
    })
}

#[interrupt]
fn ETH() {
    cortex_m::interrupt::free(|cs| {
        let mut eth_pending =
            ETH_PENDING.borrow(cs)
            .borrow_mut();
        *eth_pending = true;
    });

    // Clear interrupt flags
    let p = unsafe { Peripherals::steal() };
    stm32_eth::eth_interrupt_handler(&p.ETHERNET_DMA);
}
