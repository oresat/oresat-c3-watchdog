### Initial Setup
- Ensure rust is up to date: `rustup update`
- Install the cross toolchain: `rustup target add armv7-unknown-linux-gnueabihf`
- Install cargo-deb: `cargo install cargo-deb`

If it complains about incompatible GLIBC versions consider using an older cross compiler toolchain
https://github.com/abhiTronix/raspberry-pi-cross-compilers

### Compile and run the watchdog:
`cargo r --target armv7-unknown-linux-gnueabihf --release`

### Build a debian package:
`cargo deb --target armv7-unknown-linux-gnueabihf`

Package will be in `target/armv7-unknown-linux-gnueabihf/debian/`

### Debugging the watchdog:
- Omit the --release flag to run in debug mode
- Prints petted and pinged times
  
`cargo run`

- Handy script that will ping the watchdog UDP port every 5 seconds
  
`test_ping.sh`

### Testing the watchdog:
`cargo test`

`cargo test -- --nocapture` for print statements in the test functions
