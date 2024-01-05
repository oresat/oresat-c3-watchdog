Ensure rust is up to date: `rustup update`
Install the cross toolchain: `rustup target add armv7-unknown-linux-gnueabihf`
Install cargo-deb: `cargo install cargo-deb`

If it complains about incompatible GLIBC versions consider using an older cross compiler toolchain
https://github.com/abhiTronix/raspberry-pi-cross-compilers

Compile and run the watchdog:
`cargo r --target armv7-unknown-linux-gnueabihf`

Build a debian package:
`cargo deb --target armv7-unknown-linux-gnueabihf`
Package will be in `target/armv7-unknown-linux-gnueabihf/debian/`
