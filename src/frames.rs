//! Latest-frame capture on top of `lamco-pipewire`.
//!
//! `lamco-pipewire` hands frames over in one bounded queue
//! (`sync_channel(256)`) filled with `try_send`, which drops the NEWEST frame
//! when the queue is full. A consumer that pops one frame per screenshot
//! therefore gets the OLDEST queued frame: under screen activity every capture
//! is older than the last, and up to 256 full-size frames (~16-20 MB each) sit
//! in memory.
//!
//! `FrameSource` fixes that: a drainer thread owns the `PipeWireThreadManager`,
//! receives every frame as soon as it arrives, and keeps only the latest one
//! per stream in a `LatestSlot`. Commands for the PipeWire thread (stream
//! creation) are forwarded through the drainer, because the manager's frame
//! receiver cannot be shared between threads.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use lamco_pipewire::{PipeWireThreadCommand, PipeWireThreadManager, VideoFrame};
use tokio::sync::Notify;
use tokio::time::Instant;

/// How long the drainer blocks on the frame queue before it looks at its
/// command channel and stop flag again. Bounds command latency and shutdown.
const DRAIN_POLL: Duration = Duration::from_millis(20);
/// How many PipeWire stream state events are kept for error messages.
const STATE_LOG_LEN: usize = 16;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks leaves plain data behind;
    // keep going rather than poisoning every later capture.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A single-value "latest wins" slot with a publish counter.
///
/// `publish` replaces the value and bumps `seq`; readers note a `seq` and can
/// wait for a newer one. Older values are dropped as soon as a newer one is
/// published, so memory stays at one value per slot.
pub struct LatestSlot<T> {
    state: Mutex<SlotState<T>>,
    notify: Notify,
}

struct SlotState<T> {
    latest: Option<T>,
    seq: u64,
}

impl<T: Clone> LatestSlot<T> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                latest: None,
                seq: 0,
            }),
            notify: Notify::new(),
        }
    }

    /// Store `value` as the latest, dropping the previous one.
    pub fn publish(&self, value: T) {
        {
            let mut state = lock(&self.state);
            state.latest = Some(value);
            state.seq += 1;
        }
        self.notify.notify_waiters();
    }

    /// The latest value and its sequence number (0 = nothing published yet).
    pub fn latest(&self) -> Option<(T, u64)> {
        let state = lock(&self.state);
        state.latest.clone().map(|value| (value, state.seq))
    }

    fn newer_than(&self, seq: u64) -> Option<(T, u64)> {
        let state = lock(&self.state);
        if state.seq > seq {
            state.latest.clone().map(|value| (value, state.seq))
        } else {
            None
        }
    }

    /// Wait up to `timeout` for a value published after `seq`; returns the
    /// latest such value, or `None` when nothing newer arrived in time.
    pub async fn wait_newer(&self, seq: u64, timeout: Duration) -> Option<(T, u64)> {
        self.wait_newer_until(seq, Instant::now() + timeout).await
    }

    /// Wait until the stream settles: collect values published after `seq`
    /// until none arrives for `quiet` (measured from the last arrival) or
    /// `deadline` has passed since the call, and return the latest. `None`
    /// when nothing at all arrived before the deadline (a static screen).
    ///
    /// Returning the FIRST newer value is not enough for screenshots: the
    /// action's own cursor motion produces a cursor-only frame that is often
    /// already queued, before the application repainted.
    pub async fn settle(&self, seq: u64, quiet: Duration, deadline: Duration) -> Option<(T, u64)> {
        let end = Instant::now() + deadline;
        let mut latest: Option<(T, u64)> = None;
        loop {
            let (after, until) = match &latest {
                None => (seq, end),
                Some((_, last)) => (*last, end.min(Instant::now() + quiet)),
            };
            match self.wait_newer_until(after, until).await {
                Some(found) => latest = Some(found),
                None => return latest,
            }
            if Instant::now() >= end {
                return latest;
            }
        }
    }

    async fn wait_newer_until(&self, seq: u64, deadline: Instant) -> Option<(T, u64)> {
        loop {
            // Register interest BEFORE checking, so a publish between the
            // check and the await is not lost (`notify_waiters` only wakes
            // registered waiters).
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(found) = self.newer_than(seq) {
                return Some(found);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.newer_than(seq);
            }
        }
    }
}

type Slots = Arc<Mutex<HashMap<u32, Arc<LatestSlot<Arc<VideoFrame>>>>>>;

/// Owns the PipeWire thread (through the drainer) and the per-stream latest
/// frame slots.
pub struct FrameSource {
    commands: Option<std_mpsc::Sender<PipeWireThreadCommand>>,
    slots: Slots,
    state_log: Arc<Mutex<VecDeque<String>>>,
    stop: Arc<AtomicBool>,
    drainer: Option<JoinHandle<()>>,
}

/// Why the drainer loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainEnd {
    /// Stop flag or dropped command sender: a normal shutdown.
    Stopped,
    /// The PipeWire thread is gone (its frame channel disconnected).
    PipeWireExited,
}

impl FrameSource {
    /// Connect to PipeWire over the portal's remote fd and start the drainer.
    pub fn start(fd: OwnedFd) -> Result<Self> {
        let manager = PipeWireThreadManager::new(fd.into_raw_fd())
            .context("failed to start the PipeWire thread")?;
        Self::with_manager(manager)
    }

    fn with_manager(manager: PipeWireThreadManager) -> Result<Self> {
        let (commands, command_rx) = std_mpsc::channel();
        let slots: Slots = Arc::default();
        let state_log = Arc::new(Mutex::new(VecDeque::with_capacity(STATE_LOG_LEN)));
        let stop = Arc::new(AtomicBool::new(false));

        let drainer = {
            let slots = Arc::clone(&slots);
            let state_log = Arc::clone(&state_log);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("frame-drainer".to_owned())
                .spawn(move || {
                    if drain(manager, command_rx, slots, Arc::clone(&state_log), stop)
                        == DrainEnd::PipeWireExited
                    {
                        eprintln!("gnome-portal-bridge: the PipeWire thread exited");
                        lock(&state_log).push_back("PipeWire thread exited".to_owned());
                    }
                })
                .context("failed to spawn the PipeWire frame drainer")?
        };

        Ok(Self {
            commands: Some(commands),
            slots,
            state_log,
            stop,
            drainer: Some(drainer),
        })
    }

    /// Register a slot for `stream_id` and forward a stream command. The slot
    /// exists before the stream does, so no early frame is missed.
    pub fn send_stream_command(
        &self,
        stream_id: u32,
        command: PipeWireThreadCommand,
    ) -> Result<()> {
        lock(&self.slots)
            .entry(stream_id)
            .or_insert_with(|| Arc::new(LatestSlot::new()));
        self.commands
            .as_ref()
            .context("PipeWire frame source is shut down")?
            .send(command)
            .map_err(|_| anyhow::anyhow!("the PipeWire thread exited; see daemon.log"))
    }

    /// Whether the drainer (and with it the PipeWire thread) still runs. A dead
    /// source never delivers frames again and must be rebuilt.
    pub fn is_alive(&self) -> bool {
        self.drainer.as_ref().is_some_and(|d| !d.is_finished())
    }

    /// The latest-frame slot of a stream, if the stream was created.
    pub fn slot(&self, stream_id: u32) -> Option<Arc<LatestSlot<Arc<VideoFrame>>>> {
        lock(&self.slots).get(&stream_id).cloned()
    }

    /// Recent PipeWire stream state changes, for error messages.
    pub fn state_summary(&self) -> String {
        let log = lock(&self.state_log);
        if log.is_empty() {
            "no state events observed".to_owned()
        } else {
            format!(
                "state events: {}",
                log.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    }

    /// Stop the drainer (which shuts the PipeWire thread down) and wait for
    /// it up to `grace`. A drainer that does not finish in time is left
    /// behind: it is a plain thread and never keeps the process alive.
    pub async fn shutdown(mut self, grace: Duration) {
        self.signal_stop();
        let Some(drainer) = self.drainer.take() else {
            return;
        };
        let deadline = Instant::now() + grace;
        while !drainer.is_finished() {
            if Instant::now() >= deadline {
                eprintln!("gnome-portal-bridge: PipeWire teardown did not finish in time");
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = drainer.join();
    }

    fn signal_stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.commands = None;
    }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        // Not joined here (Drop cannot wait asynchronously); the drainer sees
        // the flag within one poll interval and winds PipeWire down itself.
        self.signal_stop();
    }
}

/// The drainer loop: forward commands, keep only the latest frame per stream,
/// remember state events. Ends on the stop flag or when the command sender is
/// dropped, then shuts the PipeWire thread down (on this thread, so the
/// caller never blocks on it).
fn drain(
    mut manager: PipeWireThreadManager,
    commands: std_mpsc::Receiver<PipeWireThreadCommand>,
    slots: Slots,
    state_log: Arc<Mutex<VecDeque<String>>>,
    stop: Arc<AtomicBool>,
) -> DrainEnd {
    let mut end = DrainEnd::Stopped;
    let route = |frame: VideoFrame| {
        // lamco-pipewire tags each frame with its stream id in `frame_id`.
        let Ok(stream_id) = u32::try_from(frame.frame_id) else {
            return;
        };
        let slot = lock(&slots).get(&stream_id).cloned();
        if let Some(slot) = slot {
            slot.publish(Arc::new(frame));
        }
    };

    'outer: while !stop.load(Ordering::Acquire) {
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    if let Err(error) = manager.send_command(command) {
                        eprintln!("gnome-portal-bridge: PipeWire command failed: {error}");
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => break 'outer,
            }
        }

        let events = manager.drain_state_events();
        if !events.is_empty() {
            let mut log = lock(&state_log);
            for event in events {
                if log.len() == STATE_LOG_LEN {
                    log.pop_front();
                }
                log.push_back(format!("stream {} -> {:?}", event.stream_id, event.state));
            }
        }

        let polled = std::time::Instant::now();
        match manager.recv_frame_timeout(DRAIN_POLL) {
            Some(frame) => {
                route(frame);
                // Empty whatever else is queued so only the newest survives.
                while let Some(frame) = manager.try_recv_frame() {
                    route(frame);
                }
            }
            // lamco maps both Timeout and Disconnected to None, but a timeout
            // only returns once the full poll elapsed: an earlier None means
            // the PipeWire thread dropped its sender (connect failure, panic).
            // Without this check the loop would spin at 100% CPU forever.
            None if returned_early(polled.elapsed()) => {
                end = DrainEnd::PipeWireExited;
                break;
            }
            None => {}
        }
    }

    if let Err(error) = manager.shutdown() {
        eprintln!("gnome-portal-bridge: PipeWire shutdown failed: {error}");
    }
    end
}

/// Whether an empty `recv_timeout(DRAIN_POLL)` came back before its timeout,
/// which `std::sync::mpsc` only does for a disconnected channel.
fn returned_early(elapsed: Duration) -> bool {
    elapsed < DRAIN_POLL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_slot_has_no_latest_and_times_out() {
        let slot: LatestSlot<u32> = LatestSlot::new();
        assert!(slot.latest().is_none());
        assert!(
            slot.wait_newer(0, Duration::from_millis(10))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn publish_keeps_only_the_latest_value() {
        let slot = LatestSlot::new();
        slot.publish(1u32);
        slot.publish(2);
        slot.publish(3);
        assert_eq!(slot.latest(), Some((3, 3)));
        // Anything newer than seq 1 resolves at once, to the LATEST value.
        assert_eq!(
            slot.wait_newer(1, Duration::from_millis(10)).await,
            Some((3, 3))
        );
    }

    #[tokio::test]
    async fn wait_newer_times_out_on_a_static_stream() {
        let slot = LatestSlot::new();
        slot.publish(7u32);
        let (_, seq) = slot.latest().unwrap();
        let started = std::time::Instant::now();
        assert!(
            slot.wait_newer(seq, Duration::from_millis(30))
                .await
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(30));
        // The stored frame is still there for the caller to use.
        assert_eq!(slot.latest(), Some((7, 1)));
    }

    #[tokio::test]
    async fn wait_newer_wakes_on_publish_from_another_thread() {
        let slot = Arc::new(LatestSlot::new());
        slot.publish(1u32);
        let publisher = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                slot.publish(2);
            })
        };
        let started = std::time::Instant::now();
        let got = slot.wait_newer(1, Duration::from_secs(5)).await;
        publisher.join().unwrap();
        assert_eq!(got, Some((2, 2)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn settle_returns_the_last_frame_of_a_burst_after_the_quiet_gap() {
        let slot = Arc::new(LatestSlot::new());
        slot.publish(0u32);
        let publisher = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                for value in 1..=3u32 {
                    slot.publish(value);
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        let started = std::time::Instant::now();
        let got = slot
            .settle(1, Duration::from_millis(70), Duration::from_millis(300))
            .await;
        let elapsed = started.elapsed();
        publisher.join().unwrap();
        // Frames at ~0/20/40 ms, then silence: the 40 ms frame once the
        // 70 ms gap passed, well before the 300 ms deadline.
        assert_eq!(got, Some((3, 4)));
        assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(300), "{elapsed:?}");
    }

    #[tokio::test]
    async fn settle_on_a_static_stream_returns_nothing_after_the_deadline() {
        let slot = LatestSlot::new();
        slot.publish(9u32);
        let started = std::time::Instant::now();
        let got = slot
            .settle(1, Duration::from_millis(30), Duration::from_millis(120))
            .await;
        assert_eq!(got, None);
        assert!(started.elapsed() >= Duration::from_millis(120));
        // The caller falls back to the stored (current) frame.
        assert_eq!(slot.latest(), Some((9, 1)));
    }

    #[test]
    fn early_empty_poll_means_disconnected() {
        assert!(returned_early(Duration::from_millis(1)));
        assert!(!returned_early(DRAIN_POLL));
    }

    /// A PipeWire thread that drops its frame sender must end the drainer
    /// (not spin) and mark the source dead.
    #[tokio::test]
    async fn drainer_exits_when_the_pipewire_thread_drops_its_sender() {
        let (raw_tx, raw_rx) = std_mpsc::channel();
        let manager = PipeWireThreadManager::new_direct(raw_rx, 4, 2);
        let source = FrameSource::with_manager(manager).unwrap();
        // The direct adapter numbers frames from 1; route frame 1 to a slot.
        lock(&source.slots).insert(1, Arc::new(LatestSlot::new()));
        raw_tx
            .send(lamco_pipewire::frame::RawFrameData {
                data: vec![0; 4 * 2 * 4],
                width: None,
                height: None,
                stride: None,
                format: None,
            })
            .unwrap();
        let slot = source.slot(1).unwrap();
        let (frame, _) = slot.wait_newer(0, Duration::from_secs(5)).await.unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert!(source.is_alive());

        drop(raw_tx);
        let started = std::time::Instant::now();
        while source.is_alive() && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!source.is_alive(), "drainer still running after disconnect");
        assert!(source.state_summary().contains("PipeWire thread exited"));
    }

    #[test]
    fn video_frames_fit_the_slot() {
        // The slot holds real lamco frames (shared, not copied).
        let slot = LatestSlot::new();
        let frame = Arc::new(VideoFrame::new(
            42,
            2560,
            1600,
            2560 * 4,
            lamco_pipewire::PixelFormat::BGRx,
            0,
        ));
        slot.publish(Arc::clone(&frame));
        let (stored, seq) = slot.latest().unwrap();
        assert!(Arc::ptr_eq(&stored, &frame));
        assert_eq!((stored.width, stored.height, seq), (2560, 1600, 1));
    }
}
