//! The node's start-up and shutdown steps: the shared [`xkr_term::steps`] reporter (its default
//! subject is the node).

pub use xkr_term::steps::{
    Kind, Mode, Reporter, STOPPING_NOTE, SignalAction, Sink, Step, StepHandle, begin, check_status,
    install, install_startup, process_read_bytes, reporter, signal_action, step, step_on,
};
