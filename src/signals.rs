//! Owned native signal registration and cancellation delivery.
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{Handle, Signals},
};
use std::{
    io,
    thread::{self, JoinHandle},
};
use workspace_overlay::Cancellation;

pub struct SignalsGuard {
    handle: Handle,
    worker: Option<JoinHandle<()>>,
}
impl SignalsGuard {
    pub fn new(cancel: Cancellation) -> io::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        let handle = signals.handle();
        let worker = thread::Builder::new()
            .name("overlay-signals".into())
            .spawn(move || {
                for _ in signals.forever() {
                    cancel.cancel();
                }
            })?;
        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }
    pub fn close(&mut self) -> io::Result<()> {
        self.handle.close();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("signal delivery thread panicked"))?;
        }
        Ok(())
    }
}
impl Drop for SignalsGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
