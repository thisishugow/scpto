//! Effect executors: perform the I/O requested by `Effect`s and report
//! results as `BgEvent`s. One module per domain.

pub mod edit;
pub mod external;
pub mod fs;
pub mod preview;
pub mod session;
pub mod state;
pub mod sync;
pub mod timer;
pub mod transfer;
