//! Keeps every native call on one thread at a time.
//!
//! The native registry is a process-wide global with no locking. Each owning
//! wrapper holds a [`Claim`]; the first claim pins the core to the current
//! thread, and the pin is released when the last claim drops. Since claims and
//! the objects holding them are `!Send`, every call made through a live object
//! happens on the pinned thread.

use std::marker::PhantomData;
use std::sync::Mutex;
use std::thread::{self, ThreadId};

use crate::{Error, Result};

static OWNER: Mutex<Option<(ThreadId, usize)>> = Mutex::new(None);

pub(crate) struct Claim {
    _not_send: PhantomData<*const ()>,
}

impl Claim {
    pub(crate) fn acquire() -> Result<Claim> {
        let me = thread::current().id();
        let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        match owner.as_mut() {
            Some((id, count)) if *id == me => *count += 1,
            Some(_) => return Err(Error::WrongThread),
            None => *owner = Some((me, 1)),
        }
        Ok(Claim {
            _not_send: PhantomData,
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, count)) = owner.as_mut() {
            *count -= 1;
            if *count == 0 {
                *owner = None;
            }
        }
    }
}
