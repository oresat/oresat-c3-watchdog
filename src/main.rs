use anyhow::{anyhow, bail, Context, Result};
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
    array::IntoIter,
    io::ErrorKind,
    iter::Cycle,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    os::fd::{AsFd, AsRawFd},
};

const ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 20001);
const INHIBIT: Expiration = OneShot(TimeSpec::new(120, 0));
const PING_TIMEOUT: Expiration = OneShot(TimeSpec::new(30, 0));
const PET_ON: Expiration = OneShot(TimeSpec::new(0, 100_000_000));
const PET_OFF: Expiration = OneShot(TimeSpec::new(0, 900_000_000));

// pet every 1s (0.1s high, 0.9s low)
// wait 120s
// if port hasn't been pinged in the last 30s, die

// Specifically die to sigterm/sighup/sigint
// set line low on death

struct Petter {
    hand: Lines<Output>,
    timer: TimerFd,
    values: Cycle<IntoIter<(bool, Expiration), 2>>,
}

impl Petter {
    fn new() -> Result<Self> {
        let pin = 25;
        let chip = Chip::new("gpiochip2").context("Failed to get GPIO chip")?;
        //FIXME: Since the consumer is set instead of name, this clears on program exit. Once names
        //are updated, this check should be reinstated.
        //let label = "PET_WDT";
        //let consumer = chip.line_info(pin)?.consumer;
        //ensure!(label == consumer, "Invalid GPIO Pin label, expected {:?}, found {:?}", label, consumer);
        let opts = Options::output([pin]).values([false]);
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
        Ok(())
    }
}

fn main() -> Result<()> {
    let mut poll = Poll::new()?;
    let registry = poll.registry();
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
