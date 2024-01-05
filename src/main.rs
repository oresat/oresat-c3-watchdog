use anyhow::{anyhow, bail, Result};
use gpiod::{Chip, Lines, Options, Output};
use mio::{event, net::UdpSocket, unix::SourceFd, Events, Interest, Poll, Registry, Token};
use nix::sys::{
    signal,
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
    io,
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
        let chip = Chip::new("gpiochip2")?;
        //FIXME: Since the consumer is set instead of name, this clears on program exit. Once names
        //are updated, this check should be reinstated.
        //let label = "PET_WDT";
        //let consumer = chip.line_info(pin)?.consumer;
        //ensure!(label == consumer, "Invalid GPIO Pin label, expected {:?}, found {:?}", label, consumer);
        let opts = Options::output([pin]).values([false]);
        let line = chip.request_lines(opts)?;

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

impl event::Source for Petter {
    fn register(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
        SourceFd(&self.timer.as_fd().as_raw_fd()).register(r, t, i)
    }

    fn reregister(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
        SourceFd(&self.timer.as_fd().as_raw_fd()).reregister(r, t, i)
    }

    fn deregister(&mut self, r: &Registry) -> io::Result<()> {
        SourceFd(&self.timer.as_fd().as_raw_fd()).deregister(r)
    }
}

impl Drop for Petter {
    fn drop(&mut self) {
        let _ = self.hand.set_values([false]);
    }
}

fn main() -> Result<()> {
    let mut poll = Poll::new()?;
    let registry = poll.registry();
    let mut events = Events::with_capacity(128);

    let mut request = UdpSocket::bind(ADDRESS)?;
    const REQUEST: Token = Token(0);
    registry.register(&mut request, REQUEST, Interest::READABLE)?;

    let mut petter = Petter::new()?;
    const PET: Token = Token(1);
    registry.register(&mut petter, PET, Interest::READABLE)?;

    let timeout = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK)?;
    timeout.set(INHIBIT, TimerSetTimeFlags::empty())?;
    const TIMEOUT: Token = Token(2);
    registry.register(
        &mut SourceFd(&timeout.as_fd().as_raw_fd()),
        TIMEOUT,
        Interest::READABLE,
    )?;

    let mut mask = SigSet::empty();
    mask.add(signal::SIGTERM);
    mask.add(signal::SIGHUP);
    mask.add(signal::SIGINT);
    mask.thread_block()?;
    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK)?;
    const SIGNAL: Token = Token(3);
    registry.register(&mut SourceFd(&sfd.as_raw_fd()), SIGNAL, Interest::READABLE)?;

    petter.pet()?;
    'outer: loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                REQUEST => {
                    let mut buf = [0; 128]; // FIXME size 1? we don't care what
                    request.recv_from(&mut buf)?;
                    if let (Some(OneShot(remaining)), OneShot(ping)) =
                        (timeout.get()?, PING_TIMEOUT)
                    {
                        if remaining < ping {
                            timeout.set(PING_TIMEOUT, TimerSetTimeFlags::empty())?;
                        }
                    } else {
                        bail!("Unexpected ping timeout timer")
                    }
                }
                PET => petter.on_pet()?,
                TIMEOUT => break 'outer Err(anyhow!("Ping timeout")),
                SIGNAL => break 'outer Ok(()),
                _ => unreachable!(),
            }
        }
    }
}
