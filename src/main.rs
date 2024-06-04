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
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::fd::{AsFd, AsRawFd},
};

const ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20001);
const INHIBIT: Expiration = OneShot(TimeSpec::new(120, 0));
const PING_TIMEOUT: Expiration = OneShot(TimeSpec::new(30, 0));
const PET_ON: Expiration = OneShot(TimeSpec::new(0, 100_000_000));
const PET_OFF: Expiration = OneShot(TimeSpec::new(0, 900_000_000));

const PIN: u32 = 25;
const CHIP: &str = "gpiochip2";

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
        let chip = Chip::new(CHIP).context("Failed to get GPIO chip")?;
        //FIXME: Since the consumer is set instead of name, this clears on program exit. Once names
        //are updated, this check should be reinstated.
        //let label = "PET_WDT";
        //let consumer = chip.line_info(pin)?.consumer;
        //ensure!(label == consumer, "Invalid GPIO Pin label, expected {:?}, found {:?}", label, consumer);
        let opts = Options::output([PIN]).values([false]);
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
            println!("PETTED at {} with value {}", chrono::Local::now().timestamp_millis(), value);
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
        println!("PINGED at {}", chrono::Local::now().timestamp_millis());
        Ok(())
    }
}

fn main() -> Result<()> {
    #[cfg(debug_assertions)]
    let build_type = "Debug";
    #[cfg(not(debug_assertions))]
    let build_type = "Release";
    println!("This is a {} build.", build_type);

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

#[cfg(test)]
mod tests{
    use super::*;

    #[test]
    fn test_pet() -> Result<()>{
        //let mut petter = Petter::new()?;
        let chip = Chip::new(CHIP).context("Failed to get GPIO chip")?;
        let output_opts = Options::output([PIN]).values([false]);
        let input_opts = Options::input([PIN]);

        let test_lane_output = chip.request_lines(output_opts).context("Failed to get GPIO pin")?;
        test_lane_output.set_values([true])?;
        drop(test_lane_output);

        let test_lane_input = chip.request_lines(input_opts).context("Failed to get GPIO pin")?;
        let value = test_lane_input.get_values([false;1])?; // Get the line value
        println!("READ VALUE: {}", value[0]);
        drop(test_lane_input);

        //let test_lane_output = chip.request_lines(output_opts).context("Failed to get GPIO pin")?;


        //petter.hand = chip.request_lines(input_opts).context("Failed to get GPIO pin")?;

        //let mut petter = Petter::new()?;

        // Read this to get sim value
        // cat /sys/devices/platform/gpio-sim.0/gpiochip2/sim_gpio25/value

        // Or do read with libgpiod
        // gpioget --bias=as-is gpiochip2 25
        // Need to take down the line and grab it again though.
        
        // Works but not really useful
        /*
        let mut line_value = petter.hand.get_values([false;1])?;
        println!("Line before pet(): {}", line_value[0]);
        assert_eq!(line_value[0], false);

        petter.pet()?;

        let line_value = petter.hand.get_values([false;1])?;
        println!("Line After pet(): {}", line_value[0]);
        assert_eq!(line_value[0], true);

        petter.pet()?;

        let line_value = petter.hand.get_values([false;1])?;
        println!("Line After pet(): {}", line_value[0]);
        assert_eq!(line_value[0], false);
        //nix::unistd::sleep(10000);
        */

        Ok(())
    }
}


