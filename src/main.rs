use anyhow::{anyhow, Context, Result};
use gpiod::{Chip, Lines, Options, Output};
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
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::fd::{AsFd, AsRawFd},
};

// Configuration
const ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20001);
const INHIBIT: Expiration = OneShot(TimeSpec::new(120, 0));
const PING_TIMEOUT: Expiration = OneShot(TimeSpec::new(30, 0));
const PET_ON: Expiration = OneShot(TimeSpec::new(0, 100_000_000));
const PET_OFF: Expiration = OneShot(TimeSpec::new(0, 900_000_000));

const GPIO_LINE: u32 = 25;
const GPIO_CHIP: &str = "gpiochip2";
const GPIO_CONSUMER: &str = "C3_Watchdog";

// --- ABSTRACTION LAYER (Hardware vs. Simulated) ---

enum PetterImpl {
    Real(Lines<Output>),
    Simulated,
}

struct Petter {
    impl_: PetterImpl,
    timer: TimerFd,
    state: bool, // true = ON, false = OFF
}

impl Petter {
    fn new() -> Result<Self> {
        let timer = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK)?;
        
        // AUTO-DETECTION: Attempt to open the real hardware. 
        // Fall back to Simulation if the chip or line request fails.
        let impl_ = match Chip::new(GPIO_CHIP) {
            Ok(chip) => {
                println!("Hardware GPIO chip found: {}", GPIO_CHIP);
                let opts = Options::output([GPIO_LINE])
                    .values([false])
                    .consumer(GPIO_CONSUMER);
                match chip.request_lines(opts) {
                    Ok(lines) => PetterImpl::Real(lines),
                    Err(e) => {
                        eprintln!("Failed to request GPIO line: {}", e);
                        println!("Falling back to SIMULATION mode.");
                        PetterImpl::Simulated
                    }
                }
            }
            Err(_) => {
                println!("Hardware GPIO chip NOT found (Desktop Environment Detected).");
                PetterImpl::Simulated
            }
        };

        if let PetterImpl::Simulated = impl_ {
            println!("Falling back to SIMULATION mode.");
        }

        Ok(Petter {
            impl_,
            timer,
            state: false,
        })
    }

    fn pet(&mut self) -> Result<()> {
        // Toggle state
        self.state = !self.state;
        let expiration = if self.state { PET_ON } else { PET_OFF };

        // 1. Update Timer
        self.timer.set(expiration, TimerSetTimeFlags::empty())?;

        // 2. Actuate Hardware (or Print)
        match &self.impl_ {
            PetterImpl::Real(lines) => {
                lines.set_values([self.state])?;
            },
            PetterImpl::Simulated => {
                #[cfg(debug_assertions)]
                println!("SIMULATED PET: State is now {}", self.state);
            }
        }
        Ok(())
    }

    fn on_pet(&mut self) -> Result<()> {
        self.timer.wait()?; 
        self.pet()
    }
}

// --- MAIN LOOP ---

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
        let mut buf = [0; 1];
        // Drain the socket to ensure mio triggers again on the next packet
        while match self.socket.recv_from(&mut buf) {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => return Err(e).context("Ping socket read failed"),
        } {}
        
        // Reset the safety timeout timer when petted
        if let (Some(OneShot(remaining)), OneShot(ping)) = (self.timer.get()?, PING_TIMEOUT) {
            if remaining < ping {
                self.timer.set(PING_TIMEOUT, TimerSetTimeFlags::empty())?;
            }
        }
        Ok(())
    }
}

// --- MAIN EVENT LOOP ---

fn main() -> Result<()> {
    println!("Starting Smart Watchdog...");

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);

    let mut pingee = Pingee::new()?;
    let mut petter = Petter::new()?;

    let mask = SigSet::from_iter([SIGTERM, SIGHUP, SIGINT]);
    mask.thread_block()?;
    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK)?;

    const PING: Token = Token(0);
    const PET: Token = Token(1);
    const TIMEOUT: Token = Token(2);
    const SIGNAL: Token = Token(3);

    poll.registry().register(&mut pingee.socket, PING, Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&petter.timer.as_fd().as_raw_fd()), PET, Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&pingee.timer.as_fd().as_raw_fd()), TIMEOUT, Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&sfd.as_raw_fd()), SIGNAL, Interest::READABLE)?;

    println!("Listening for heartbeats on {}", ADDRESS);
    petter.pet()?;

    'outer: loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                PING => pingee.on_ping()?,
                PET => petter.on_pet()?,
                TIMEOUT => {
                    eprintln!("CRITICAL: Watchdog Ping Timeout! System would reboot.");
                    break 'outer Err(anyhow!("Watchdog Timeout"));
                },
                SIGNAL => break 'outer Ok(()),
                _ => unreachable!(),
            }
        }
    }
}