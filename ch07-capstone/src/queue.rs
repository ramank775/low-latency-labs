//! The feed-to-strategy queue and its two swaps. Each side is a trait so that
//! `--queue` changes a type and no line of either loop: the strategy calls
//! `drain`, the feed calls `try_push`, whichever queue is behind them.

use crate::{Stamped, BATCH};
use ch05_structures::{Consumer, Producer};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// `--queue spsc|mutex|mpsc`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Queue { Spsc, Mutex, Mpsc }

/// The feed's end.
pub trait QueueTx: Send {
    /// Hand one message over, or give it back if there is no room.
    fn try_push(&mut self, s: Stamped) -> Result<(), Stamped>;
}

/// The strategy's end.
pub trait QueueRx: Send {
    /// Everything present when it looked, at most `BATCH`, without waiting for more.
    fn drain(&mut self, f: impl FnMut(Stamped)) -> usize;
}

// ---------------------------------------------------------------- the default: the SPSC ring

impl QueueTx for Producer<Stamped, BATCH> {
    #[inline(always)]
    fn try_push(&mut self, s: Stamped) -> Result<(), Stamped> {
        Producer::try_push(self, s)
    }
}

impl QueueRx for Consumer<Stamped, BATCH> {
    #[inline(always)]
    fn drain(&mut self, f: impl FnMut(Stamped)) -> usize {
        Consumer::drain(self, f)          // one look at head, one store of tail
    }
}

// ---------------------------------------------------------------- --queue mutex

/// `Mutex<VecDeque<Stamped>>`: unbounded, so a backlog grows the deque by
/// doubling, and every push and pop takes the lock.
pub fn mutex() -> (MutexTx, MutexRx) {
    let q = Arc::new(Mutex::new(VecDeque::new()));
    (MutexTx(Arc::clone(&q)), MutexRx(q))
}

pub struct MutexTx(Arc<Mutex<VecDeque<Stamped>>>);
pub struct MutexRx(Arc<Mutex<VecDeque<Stamped>>>);

impl QueueTx for MutexTx {
    fn try_push(&mut self, s: Stamped) -> Result<(), Stamped> {
        self.0.lock().unwrap().push_back(s);          // may sleep on the futex if the strategy holds it
        Ok(())
    }
}

impl QueueRx for MutexRx {
    fn drain(&mut self, mut f: impl FnMut(Stamped)) -> usize {
        let n = self.0.lock().unwrap().len().min(BATCH);   // what is there now, like the ring's one look
        for _ in 0..n {
            let s = self.0.lock().unwrap().pop_front();     // the lock per message is the swap's price
            if let Some(s) = s { f(s); }                    // f runs with the lock released
        }
        n
    }
}

// ---------------------------------------------------------------- --queue mpsc

impl QueueTx for Sender<Stamped> {
    fn try_push(&mut self, s: Stamped) -> Result<(), Stamped> {
        self.send(s).map_err(|e| e.0)                  // unbounded: a new block every 31 messages
    }
}

impl QueueRx for Receiver<Stamped> {
    fn drain(&mut self, mut f: impl FnMut(Stamped)) -> usize {
        let mut n = 0;
        while n < BATCH {
            match self.try_recv() {
                Ok(s) => { f(s); n += 1; }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        n
    }
}
