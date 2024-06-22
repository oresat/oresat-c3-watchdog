// Build release version (No Print statements): cargo build --release
// Build Debug version: cargo build
 
// Run Tests: cargo test
// Run Tests with print statements: cargo test -- --nocapture
    
use anyhow::{anyhow, bail, Context, Result};
use gpiod::{Chip, Lines, Options, Output};
use gpiosim::{Bank, Direction, Level, Sim};
use mio::{net::UdpSocket, unix::SourceFd, Events, Interest, Poll, Token};
use nix::sys::{
    signal::{SIGHUP, SIGINT, SIGTERM},
    signalfd::{SfdFlags, SigSet, SignalFd},
    time::TimeSpec,
    timerfd::{
        ClockId,
        Expiration::{self, OneShot},
        TimerFd, TimerFlags, TimerSetTimeFlags,
    },
};
use std::{
    array::IntoIter,
    io::ErrorKind,
    iter::Cycle,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::fd::{AsFd, AsRawFd}, time::UNIX_EPOCH,
};

const ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20001);
const INHIBIT: Expiration = OneShot(TimeSpec::new(120, 0));
const PING_TIMEOUT: Expiration = OneShot(TimeSpec::new(30, 0));
const PET_ON: Expiration = OneShot(TimeSpec::new(0, 100_000_000));
const PET_OFF: Expiration = OneShot(TimeSpec::new(0, 900_000_000));

const GPIO_LABEL: &str = "PET_WDT";
const GPIO_LINE: u32 = 25;
const GPIO_CHIP: &str = "gpiochip2";
const GPIO_CONSUMER: &str = "C3_Watchdog";

// pet every 1s (0.1s high, 0.9s low)
// wait 120s
// if port hasn't been pinged in the last 30s, die

// Specifically die to sigterm/sighup/sigint
// set line low on death


fn timestamp_millis() -> u128 {
    let now = std::time::SystemTime::now();
    let millis = now.duration_since(UNIX_EPOCH).unwrap().as_millis();
    return millis;
}

struct Petter {
    hand: Lines<Output>,
    timer: TimerFd,
    values: Cycle<IntoIter<(bool, Expiration), 2>>,
}

impl Petter {
    fn new(gpio_chip: &str, gpio_label: &str, gpio_line: u32) -> Result<Self> {

        let chip = Chip::new(gpio_chip).context("Failed to get GPIO chip")?;

        let read_label = chip.line_info(gpio_line)?.name;
        anyhow::ensure!(read_label == gpio_label, "Invalid GPIO LINE label, expected {:?}, found {:?}", gpio_label, read_label);

        let opts = Options::output([GPIO_LINE])
            .values([false])
            .consumer(GPIO_CONSUMER);
        let line = chip.request_lines(opts).context("Failed to get GPIO pin")?;

        Ok(Petter {
            hand: line,
            timer: TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK)?,
            values: [(true, PET_ON), (false, PET_OFF)].into_iter().cycle(),
        })
    }

    fn pet(&mut self) -> Result<()> {
        // functions as a toggle
        if let Some((value, duration)) = self.values.next() {
            self.hand.set_values([value])?;
            self.timer.set(duration, TimerSetTimeFlags::empty())?;
            #[cfg(debug_assertions)]
            println!("PETTED at {} ms with value {}", timestamp_millis(), value);
        } else {
            bail!("Unexpected iterator in Petter")
        }
        Ok(())
    }


    fn on_pet(&mut self) -> Result<()> {
        self.timer.wait()?; // TODO: read and assert 1?
        self.pet()
    }
}

impl Drop for Petter {
    fn drop(&mut self) {
        let _ = self.hand.set_values([false]);
    }
}

struct Pingee {
    socket: UdpSocket,
    timer: TimerFd,
}

impl Pingee {
    fn new() -> Result<Self> {
        let timer = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK)?;
        timer.set(INHIBIT, TimerSetTimeFlags::empty())?;
        Ok(Self {
            socket: UdpSocket::bind(ADDRESS)?,
            timer,
        })
    }

    fn on_ping(&self) -> Result<()> {
        // We don't care about the contents - bytes longer than buf are discarded
        let mut buf = [0; 1];
        // Read until there's no more packets, otherwise mio won't see the socket as readable again
        while match self.socket.recv_from(&mut buf) {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => return Err(e).context("Ping socket read failed")
        } {};
        if let (Some(OneShot(remaining)), OneShot(ping)) = (self.timer.get()?, PING_TIMEOUT) {
            if remaining < ping {
                self.timer.set(PING_TIMEOUT, TimerSetTimeFlags::empty())?;
            }
        } else {
            bail!("Unexpected ping timeout timer")
        }
        #[cfg(debug_assertions)]
        println!("PINGED at {} ms", timestamp_millis());
        Ok(())
    }
}


fn simulate_gpio(line_offset: u32, line_label: &str) -> Sim {
    #[cfg(debug_assertions)]
    println!("Simulating the GPIO.");

    let sim = gpiosim::builder()
    .with_name("watchdog_sim")
    .with_bank(
        Bank::new(32, "sim_bank")
            .name(line_offset, line_label)
    )
    .live();

    sim.expect("Failed to simulate GPIO")
}

fn main() -> Result<()> {
    #[cfg(debug_assertions)]
    println!("This is a Debug build.");

    // // get command line arguments
    let args: Vec<String> = std::env::args().collect();

    let _sim: Sim;
    let gpio_chip : String;

    if args.iter().any(|arg| arg == "sim") {
        _sim = simulate_gpio(GPIO_LINE, GPIO_LABEL);
        let chips = gpiod::Chip::list_devices();
        let sim_chip_index = chips.unwrap().len() -1;
        gpio_chip = "gpiochip".to_string() + &(sim_chip_index.to_string());
    }
    else{
       gpio_chip = GPIO_CHIP.to_string();
    }

    let mut petter = Petter::new(&gpio_chip,GPIO_LABEL,GPIO_LINE)?;

    let mut poll = Poll::new()?;
    let registry = poll.registry();
    let mut events = Events::with_capacity(128);

    let mut pingee = Pingee::new()?;
    let mask = SigSet::from_iter([SIGTERM, SIGHUP, SIGINT]);
    mask.thread_block()?;
    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK)?;

    const PING: Token = Token(0);
    const PET: Token = Token(1);
    const TIMEOUT: Token = Token(2);
    const SIGNAL: Token = Token(3);

    registry.register(&mut pingee.socket, PING, Interest::READABLE)?;
    registry.register(
        &mut SourceFd(&petter.timer.as_fd().as_raw_fd()),
        PET,
        Interest::READABLE,
    )?;
    registry.register(
        &mut SourceFd(&pingee.timer.as_fd().as_raw_fd()),
        TIMEOUT,
        Interest::READABLE,
    )?;
    registry.register(&mut SourceFd(&sfd.as_raw_fd()), SIGNAL, Interest::READABLE)?;

    petter.pet()?;
    'outer: loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                PING => pingee.on_ping()?,
                PET => petter.on_pet()?,
                TIMEOUT => break 'outer Err(anyhow!("Ping timeout")),
                SIGNAL => break 'outer Ok(()),
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests{
    use super::*;

    #[test]
    fn test_simulated_pet() -> Result<()>{

        // Setup Simulated GPIO
        let sim = simulate_gpio(GPIO_LINE, GPIO_LABEL);
        let chips = gpiod::Chip::list_devices();
        let sim_chip_index = chips.unwrap().len() -1;
        let gpio_chip = "gpiochip".to_string() + &(sim_chip_index.to_string());

        let chip = sim.chips();
        let c = &chip[0];

        // Initialize the petter
        let mut petter = Petter::new(&gpio_chip,GPIO_LABEL,GPIO_LINE)?;

        // Check simulated Line level Before Petting the watchdog
        let line_level = c.get_level(GPIO_LINE).unwrap();
        assert_eq!(line_level, gpiosim::Level::Low);

        petter.pet()?;
        // Check simulated Line level after pet 1
        let line_level = c.get_level(GPIO_LINE).unwrap();
        assert_eq!(line_level, gpiosim::Level::High);

        petter.pet()?;
        // Check simulated Line level after pet 2
        let line_level = c.get_level(GPIO_LINE).unwrap();
        assert_eq!(line_level, gpiosim::Level::Low);

        Ok(())
    }
}
