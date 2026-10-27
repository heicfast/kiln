//! Behavioral probe for seccomp enforcement: try to create an IPv4 TCP
//! socket and report via exit code.
//!
//! Exit codes:
//! * `0`  — `socket()` succeeded (connection refused still proves the socket
//!   was created; only its use failed)
//! * `42` — `socket()` itself was denied (`EACCES`/`EPERM`: a seccomp
//!   filter is in effect)
//!
//! Used by kiln-build's sandbox tests; also handy for manual debugging of
//! sandbox environments (`target/debug/examples/socket_probe; echo $?`).

fn main() {
    match std::net::TcpStream::connect("127.0.0.1:1") {
        Ok(_) => std::process::exit(0),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => std::process::exit(42),
        Err(_) => std::process::exit(0),
    }
}
