//! Process exit codes for TestDet.
//!
//! The numeric values are part of the CLI contract (§7.12): `0` ok, `1` config
//! invalid, `2` transport failure, `3` usage, `4` internal.

/// The process exit code a command returns to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    /// Everything executed; no transport failures.
    Ok = 0,
    /// Config invalid; nothing ran.
    ConfigInvalid = 1,
    /// One or more requests failed at transport level. Task 18 (`run`) is
    /// the first producer of this code.
    #[allow(dead_code)]
    TransportFailure = 2,
    /// Usage error (bad flag, unknown subcommand, unknown operation id).
    Usage = 3,
    /// Internal error (I/O, panic caught at the boundary).
    Internal = 4,
}

impl Exit {
    /// The numeric value `std::process::ExitCode` maps onto.
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }
}
