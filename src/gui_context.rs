use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::{Shared, TrackInfo};

/// What the editor tells the host. Any thread but the audio thread may use it, as often as it likes:
/// changes travel to the host through a queue the wrapper empties on the thread the format requires.
#[derive(Clone)]
pub struct GuiContext(Arc<Channel>);

impl GuiContext {
    pub(crate) fn new(shared: Weak<dyn Shared>) -> Self {
        GuiContext(Arc::new(Channel { shared, queue: Queue::default(), dirty: AtomicBool::new(false), wake: RwLock::new(None), track: Mutex::new(None) }))
    }

    /// Starts a gesture on `id`, e.g. when a drag begins, so the host groups what follows into one undo step.
    pub fn begin(&self, id: u32) {
        if self.0.known(id) {
            self.0.send(Event::Begin(id));
        }
    }

    /// Sets the shared value at once and tells the host. Between `begin` and `end` for automation writing.
    pub fn set(&self, id: u32, plain: f64) {
        let Some(shared) = self.0.shared.upgrade() else { return };
        let Some(info) = shared.params().iter().find(|p| p.id == id) else { return };
        shared.set_param(id, plain);
        self.0.send(Event::Value(id, info.clamp(plain)));
    }

    pub fn end(&self, id: u32) {
        if self.0.known(id) {
            self.0.send(Event::End(id));
        }
    }

    /// Tells the host the state changed in a way no parameter shows, so it saves the project.
    pub fn mark_dirty(&self) {
        self.0.dirty.store(true, Ordering::Release);
        self.0.wake(Wake::Dirty);
    }

    /// Asks the host to resize the editor to `w` by `h` at a scale of 1.0; false when it refuses.
    pub fn request_resize(&self, _w: u32, _h: u32) -> bool {
        false
    }

    /// What the host said about the track the plugin is on, if anything.
    pub fn track(&self) -> Option<TrackInfo> {
        self.0.track.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Keeps `info` as the latest and hands it to the shared half.
    pub(crate) fn set_track(&self, info: TrackInfo) {
        *self.0.track.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());
        if let Some(shared) = self.0.shared.upgrade() {
            shared.set_track(info);
        }
    }

    /// Lets the wrapper hear of new work; `wake` must not block and may be called from any thread
    /// but the audio thread.
    pub(crate) fn connect(&self, wake: Waker) {
        *self.0.wake.write().unwrap_or_else(|e| e.into_inner()) = Some(wake);
    }

    /// After this returns no `wake` runs any more, though clones of this context may live on.
    pub(crate) fn disconnect(&self) {
        *self.0.wake.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Hands the queued events to `deliver` in order until it refuses one, which stays first in the
    /// queue. Never blocks or allocates. A call while another runs, from any thread, returns at once.
    pub(crate) fn drain(&self, deliver: impl FnMut(Event) -> bool) {
        self.0.queue.drain(deliver)
    }

    pub(crate) fn take_dirty(&self) -> bool {
        self.0.dirty.swap(false, Ordering::AcqRel)
    }
}

type Waker = Box<dyn Fn(Wake) + Send + Sync>;

struct Channel {
    /// Weak, since the shared half may keep this context.
    shared: Weak<dyn Shared>,
    queue: Queue,
    dirty: AtomicBool,
    wake: RwLock<Option<Waker>>,
    track: Mutex<Option<TrackInfo>>,
}

impl Channel {
    fn known(&self, id: u32) -> bool {
        self.shared.upgrade().is_some_and(|shared| shared.params().iter().any(|p| p.id == id))
    }

    fn send(&self, event: Event) {
        self.queue.push(event);
        self.wake(Wake::Events);
    }

    fn wake(&self, what: Wake) {
        if let Some(wake) = self.wake.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            wake(what);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Event {
    Begin(u32),
    /// A plain value.
    Value(u32, f64),
    End(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Wake {
    Events,
    Dirty,
}

const CAPACITY: usize = 1024;

/// A ring that the one draining thread empties without locking, and that producers fill one at a
/// time. Once the ring is full, events wait in `overflow` in order, where a new value replaces the
/// one still waiting for the same id unless a begin or end of that id came between: a long drag the
/// host does not collect costs one entry, and no begin or end is ever dropped.
struct Queue {
    slots: Box<[Slot]>,
    /// Advanced by the draining thread only.
    head: AtomicUsize,
    /// Advanced only with `overflow` locked.
    tail: AtomicUsize,
    overflow: Mutex<VecDeque<Event>>,
    overflowing: AtomicBool,
    draining: AtomicBool,
}

#[derive(Default)]
struct Slot {
    /// `kind << 32 | id`.
    tag: AtomicU64,
    value: AtomicU64,
}

const BEGIN: u64 = 0;
const VALUE: u64 = 1;
const END: u64 = 2;

impl Default for Queue {
    fn default() -> Self {
        Queue {
            slots: (0..CAPACITY).map(|_| Slot::default()).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            overflow: Mutex::new(VecDeque::new()),
            overflowing: AtomicBool::new(false),
            draining: AtomicBool::new(false),
        }
    }
}

impl Queue {
    fn push(&self, event: Event) {
        let mut overflow = self.overflow.lock().unwrap_or_else(|e| e.into_inner());
        self.spill(&mut overflow);
        if overflow.is_empty() && self.try_push(event) {
            return;
        }
        if let Event::Value(id, value) = event {
            for waiting in overflow.iter_mut().rev() {
                match waiting {
                    Event::Value(other, old) if *other == id => {
                        *old = value;
                        return;
                    }
                    Event::Begin(other) | Event::End(other) if *other == id => break,
                    _ => {}
                }
            }
        }
        overflow.push_back(event);
        self.overflowing.store(true, Ordering::Release);
    }

    /// Callers hold `overflow`.
    fn try_push(&self, event: Event) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail - self.head.load(Ordering::Acquire) == CAPACITY {
            return false;
        }
        let (kind, id, value) = match event {
            Event::Begin(id) => (BEGIN, id, 0.0),
            Event::Value(id, value) => (VALUE, id, value),
            Event::End(id) => (END, id, 0.0),
        };
        let slot = &self.slots[tail % CAPACITY];
        slot.tag.store(kind << 32 | id as u64, Ordering::Relaxed);
        slot.value.store(value.to_bits(), Ordering::Relaxed);
        self.tail.store(tail + 1, Ordering::Release);
        true
    }

    /// Moves what waits in `overflow` into the ring as far as it fits; callers hold `overflow`.
    fn spill(&self, overflow: &mut VecDeque<Event>) -> bool {
        let mut moved = false;
        while let Some(&event) = overflow.front() {
            if !self.try_push(event) {
                break;
            }
            overflow.pop_front();
            moved = true;
        }
        if overflow.is_empty() {
            self.overflowing.store(false, Ordering::Release);
        }
        moved
    }

    fn drain(&self, mut deliver: impl FnMut(Event) -> bool) {
        if self.draining.swap(true, Ordering::Acquire) {
            return;
        }
        loop {
            let head = self.head.load(Ordering::Relaxed);
            if head == self.tail.load(Ordering::Acquire) {
                // A producer holding the lock wakes the wrapper again once it is done.
                if self.overflowing.load(Ordering::Acquire)
                    && let Ok(mut overflow) = self.overflow.try_lock()
                    && self.spill(&mut overflow)
                {
                    continue;
                }
                break;
            }
            let slot = &self.slots[head % CAPACITY];
            let tag = slot.tag.load(Ordering::Relaxed);
            let (kind, id) = (tag >> 32, tag as u32);
            let event = match kind {
                BEGIN => Event::Begin(id),
                VALUE => Event::Value(id, f64::from_bits(slot.value.load(Ordering::Relaxed))),
                _ => Event::End(id),
            };
            if !deliver(event) {
                break;
            }
            self.head.store(head + 1, Ordering::Release);
        }
        self.draining.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained(queue: &Queue) -> Vec<Event> {
        let mut events = Vec::new();
        queue.drain(|event| {
            events.push(event);
            true
        });
        events
    }

    #[test]
    fn events_come_out_in_the_order_they_went_in() {
        let queue = Queue::default();
        for event in [Event::Begin(3), Event::Value(3, -6.0), Event::End(3)] {
            queue.push(event);
        }
        assert_eq!(drained(&queue), [Event::Begin(3), Event::Value(3, -6.0), Event::End(3)]);
        assert_eq!(drained(&queue), []);
    }

    #[test]
    fn a_refused_event_stays_first() {
        let queue = Queue::default();
        queue.push(Event::Begin(1));
        queue.push(Event::End(1));
        let mut first = Vec::new();
        queue.drain(|event| {
            first.push(event);
            false
        });
        assert_eq!(first, [Event::Begin(1)]);
        assert_eq!(drained(&queue), [Event::Begin(1), Event::End(1)]);
    }

    #[test]
    fn a_full_queue_keeps_every_begin_and_end_and_the_last_value_of_each_stretch() {
        let queue = Queue::default();
        queue.push(Event::Begin(1));
        for i in 0..CAPACITY + 500 {
            queue.push(Event::Value(1, i as f64));
            queue.push(Event::Value(2, -(i as f64)));
        }
        queue.push(Event::End(1));
        queue.push(Event::Begin(1));
        queue.push(Event::Value(1, 7.0));
        queue.push(Event::Value(1, 8.0));
        queue.push(Event::End(1));

        let events = drained(&queue);
        assert_eq!(events.len(), CAPACITY + 6);
        assert_eq!(events[0], Event::Begin(1));
        let last = CAPACITY + 499;
        assert_eq!(
            events[CAPACITY..],
            [Event::Value(2, -(last as f64)), Event::Value(1, last as f64), Event::End(1), Event::Begin(1), Event::Value(1, 8.0), Event::End(1)]
        );
        assert_eq!(drained(&queue), []);
        queue.push(Event::Begin(4));
        assert_eq!(drained(&queue), [Event::Begin(4)], "the ring is used again once the overflow is gone");
    }

    #[test]
    fn a_drain_inside_a_drain_returns_at_once() {
        let queue = Queue::default();
        queue.push(Event::Begin(1));
        queue.push(Event::End(1));
        let mut outer = Vec::new();
        queue.drain(|event| {
            assert_eq!(drained(&queue), []);
            outer.push(event);
            true
        });
        assert_eq!(outer, [Event::Begin(1), Event::End(1)]);
    }
}
