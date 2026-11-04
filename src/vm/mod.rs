//! faber's side of its VMs (ADR-0026): the wire to the VM supervisor, what
//! `_faber` is willing to read from it, the socket client that speaks it, the
//! restart that moves the supervisor onto a new launcher binary, the read-only
//! tools disk each macOS clone boots with, the Linux VM that serves Docker, and
//! adopting, sweeping and reviving the VMs themselves.

pub mod admit;
pub mod adopt;
pub mod client;
pub mod engine;
pub mod grow;
pub mod job;
pub mod proto;
pub mod restart;
pub mod revive;
pub mod runner_image;
pub mod session;
pub mod tools_disk;
pub mod verify;
pub mod wall_proof;
