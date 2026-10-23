//! Bounded test-build evidence. Never compiled into the production package.
//! Allocation/serialization are RPC-owner work; audio owners only try-lock,
//! take a preallocated block, copy bounded arrays and try-send it back.
use super::{CHANNELS, FRAMES, Guard, Shared, Stamp, identity, monotonic_ns};
use crate::audiochunk::AudioChunk;
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

const CAPACITY: usize = 128;
const SAMPLES: usize = CHANNELS * FRAMES;
const PCM_BYTES: usize = SAMPLES * 3;

#[derive(Debug)]
struct Block {
    kind: &'static str,
    epoch: [u64; 3],
    frames: [u64; 2],
    times: [u64; 5],
    identity: identity::Identity,
    trip: Option<super::Trip>,
    inhibited: bool,
    shape_valid: bool,
    clipped: usize,
    candidate: Vec<u64>,
    forwarded: Vec<u64>,
    pcm: Vec<u8>,
}
impl Default for Block {
    fn default() -> Self {
        Self {
            kind: "guard",
            epoch: [0; 3],
            frames: [0; 2],
            times: [0; 5],
            identity: identity::Identity::default(),
            trip: None,
            inhibited: true,
            shape_valid: false,
            clipped: 0,
            candidate: vec![0; SAMPLES],
            forwarded: vec![0; SAMPLES],
            pcm: vec![0; PCM_BYTES],
        }
    }
}
#[derive(Debug)]
struct Session {
    id: u64,
    begin_ns: u64,
    contention_at_start: u64,
    free: Receiver<Box<Block>>,
    ready: Sender<Box<Block>>,
    records: Receiver<Box<Block>>,
    lost: AtomicU64,
    in_flight: AtomicU64,
}
#[derive(Debug, Default)]
struct Current {
    high_water: u64,
    session: Option<Arc<Session>>,
    retired: Option<Arc<Session>>,
}
#[derive(Debug, Default)]
pub(crate) struct Capture {
    current: Mutex<Current>,
    contention: AtomicU64,
}
pub(super) struct Pending {
    session: Arc<Session>,
    block: Option<Box<Block>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.block.is_some() {
            self.session.lost.fetch_add(1, Ordering::SeqCst);
        }
        self.session.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Pending {
    fn finish(mut self) {
        self.block.as_mut().unwrap().times[4] = monotonic_ns();
        if self
            .session
            .ready
            .try_send(self.block.take().unwrap())
            .is_err()
        {
            self.session.lost.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub(super) fn guard(
        mut self,
        guard: &Guard,
        chunk: &AudioChunk,
        decision_ns: u64,
        published_ns: u64,
    ) {
        let block = self.block.as_mut().unwrap();
        block.epoch = [guard.epoch, guard.backend, guard.reset];
        block.frames[1] = guard.frames;
        block.times[2] = decision_ns;
        block.times[3] = published_ns;
        block.identity = guard.identity.unwrap_or_default();
        block.trip = guard.trip;
        block.inhibited = guard.activation_inhibited();
        block.shape_valid = shape(chunk);
        copy_bits(chunk, &mut block.forwarded);
        self.finish();
    }
}
fn shape(chunk: &AudioChunk) -> bool {
    chunk.frames == FRAMES
        && chunk.valid_frames == FRAMES
        && chunk.channels == CHANNELS
        && chunk.waveforms.len() == CHANNELS
        && chunk.waveforms.iter().all(|w| w.len() == FRAMES)
}
fn copy_bits(chunk: &AudioChunk, target: &mut [u64]) {
    for (channel, wave) in chunk.waveforms.iter().take(CHANNELS).enumerate() {
        for (frame, value) in wave.iter().take(FRAMES).enumerate() {
            target[channel * FRAMES + frame] = (*value as f64).to_bits();
        }
    }
}
impl Capture {
    fn begin(&self) -> Option<Pending> {
        let Ok(current) = self.current.try_lock() else {
            self.contention.fetch_add(1, Ordering::SeqCst);
            return None;
        };
        let session = current.session.as_ref()?;
        let Ok(block) = session.free.try_recv() else {
            session.lost.fetch_add(1, Ordering::SeqCst);
            return None;
        };
        session.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(Pending {
            session: session.clone(),
            block: Some(block),
        })
    }
    pub(super) fn guard(&self, chunk: &AudioChunk, frame: u64) -> Option<Pending> {
        let begin = monotonic_ns();
        let mut pending = self.begin()?;
        let block = pending.block.as_mut().unwrap();
        block.kind = "guard";
        block.frames[0] = frame;
        block.times[0] = begin;
        copy_bits(chunk, &mut block.candidate);
        block.times[1] = monotonic_ns();
        Some(pending)
    }
    pub(crate) fn converted(
        &self,
        stamp: &Stamp,
        clipped: usize,
        pcm: &[u8],
        begin: u64,
        end: u64,
    ) {
        let Some(mut pending) = self.begin() else {
            return;
        };
        let block = pending.block.as_mut().unwrap();
        block.kind = "conversion";
        block.epoch = [stamp.epoch, stamp.backend, stamp.reset];
        block.frames = [stamp.frame_begin, stamp.frame_end];
        block.times[..4].copy_from_slice(&[begin, end, end, end]);
        block.shape_valid = pcm.len() == PCM_BYTES;
        block.clipped = clipped;
        if block.shape_valid {
            block.pcm.copy_from_slice(pcm);
        }
        pending.finish();
    }
    fn start(&self, id: u64) -> Result<Value, &'static str> {
        if id == 0 || id > i64::MAX as u64 {
            return Err("invalid_id");
        }
        // Retain timed-out writers until an RPC owner can reclaim the pool.
        // The audio thread must never become its final Arc owner and free it.
        let mut current = self.current.lock().map_err(|_| "poisoned")?;
        if current.session.is_some()
            || id <= current.high_water
            || current.retired.as_ref().is_some_and(|s| {
                s.in_flight.load(Ordering::SeqCst) != 0 || Arc::strong_count(s) != 1
            })
        {
            return Err("busy_or_reused_id");
        }
        current.retired.take(); // Reclamation belongs to this RPC thread.
        let (sender, free) = bounded(CAPACITY);
        for _ in 0..CAPACITY {
            sender.try_send(Box::new(Block::default())).unwrap();
        }
        let (ready, records) = bounded(CAPACITY);
        let contention_at_start = self.contention.load(Ordering::SeqCst);
        let begin_ns = monotonic_ns();
        current.high_water = id;
        current.session = Some(Arc::new(Session {
            id,
            begin_ns,
            contention_at_start,
            free,
            ready,
            records,
            lost: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
        }));
        Ok(json!({"id": id, "capacity": CAPACITY, "begin_ns": begin_ns}))
    }
    fn stop(&self, id: u64, shared: &Shared) -> Result<Value, &'static str> {
        let session = {
            let mut current = self.current.lock().map_err(|_| "poisoned")?;
            if current.session.as_ref().is_none_or(|s| s.id != id) {
                return Err("wrong_id");
            }
            let session = current.session.take().unwrap();
            current.retired = Some(session.clone());
            session
        };
        // This wait is on the RPC owner, never on an audio owner. A stuck
        // writer makes the retained capture incomplete, never a passing prefix.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while session.in_flight.load(Ordering::SeqCst) != 0 && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let in_flight = session.in_flight.load(Ordering::SeqCst);
        let lost = session.lost.load(Ordering::SeqCst);
        let contention = self.contention.load(Ordering::SeqCst) - session.contention_at_start;
        let end_ns = monotonic_ns();
        let records: Vec<_> = session.records.try_iter().map(|block| {
            let common = json!({"kind": block.kind, "epoch": block.epoch, "frames": block.frames,
                "times_ns": block.times, "shape_valid": block.shape_valid});
            let mut result = common.as_object().unwrap().clone();
            if block.kind == "guard" {
                let bits = |values: &[u64]| values.iter().map(|v| format!("{v:016x}")).collect::<Vec<_>>();
                result.extend(json!({"candidate_bits": bits(&block.candidate),
                    "forwarded_bits": bits(&block.forwarded), "activation_inhibited": block.inhibited,
                    "graph_sha256": identity::hex(block.identity.graph),
                    "profile_sha256": identity::hex(block.identity.profile),
                    "trip": block.trip.map(|t| json!({"generation": t.generation, "reason": t.reason as u64,
                        "channel": t.channel, "frame": t.frame, "at_ns": t.at_ns}))}).as_object().unwrap().clone());
            } else {
                let pcm: String = block.pcm.iter().map(|v| format!("{v:02x}")).collect();
                result.extend(json!({"pcm_s24_3_le_hex": pcm, "clipped": block.clipped}).as_object().unwrap().clone());
            }
            Value::Object(result)
        }).collect();
        let host = shared.host.as_ref().ok_or("missing_host")?;
        Ok(
            json!({"schema": 1, "test_build": true, "id": id, "capacity": CAPACITY,
            "fork_revision": identity::FORK_REVISION, "process": host.process,
            "boot_id": host.boot, "time_namespace": host.namespace,
            "begin_ns": session.begin_ns, "end_ns": end_ns,
            "in_flight": in_flight, "lost": lost, "contention": contention,
            "complete": in_flight == 0 && lost == 0 && contention == 0,
            "records": records}),
        )
    }
}
#[derive(Deserialize)]
enum Request {
    StartProtectionTestCapture(u64),
    StopProtectionTestCapture(u64),
}
pub(super) fn reply(shared: &Shared, text: &str, key: &str) -> Option<String> {
    if !["StartProtectionTestCapture", "StopProtectionTestCapture"].contains(&key) {
        return None;
    }
    let result = if text.len() > 256 {
        Err("oversized")
    } else {
        match serde_json::from_str::<Request>(text) {
            Ok(Request::StartProtectionTestCapture(id)) => shared.capture.start(id),
            Ok(Request::StopProtectionTestCapture(id)) => shared.capture.stop(id, shared),
            Err(_) => Err("invalid"),
        }
    };
    Some(
        match result {
            Ok(value) => json!({key: {"result": "Ok", "value": value}}),
            Err(error) => json!({key: {"result": "Error", "error": error}}),
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protection::tests::{chunk, configuration};
    fn fixture() -> Guard {
        let mut conf = configuration();
        conf.protection_identity = Some(identity::prepare(&conf).unwrap());
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&conf).unwrap();
        guard
    }
    #[test]
    fn capture_rpc_rejects_bad_and_reused_ids_without_losing_session() {
        let shared = Shared::default();
        for text in [
            r#"{"StartProtectionTestCapture":0}"#,
            r#"{"StartProtectionTestCapture":true}"#,
            r#"{"StartProtectionTestCapture":1,"extra":2}"#,
        ] {
            let reply = shared.protection_reply(text).unwrap();
            assert!(reply.contains("Error"));
        }
        assert!(
            shared
                .protection_reply(r#"{"StartProtectionTestCapture":1}"#)
                .unwrap()
                .contains("Ok")
        );
        assert!(
            shared
                .protection_reply(r#"{"StartProtectionTestCapture":2}"#)
                .unwrap()
                .contains("Error")
        );
        assert!(
            shared
                .protection_reply(r#"{"StopProtectionTestCapture":2}"#)
                .unwrap()
                .contains("Error")
        );
        assert!(
            shared
                .protection_reply(r#"{"StopProtectionTestCapture":1}"#)
                .unwrap()
                .contains("Ok")
        );
        assert!(
            shared
                .protection_reply(r#"{"StartProtectionTestCapture":1}"#)
                .unwrap()
                .contains("Error")
        );
    }

    #[test]
    fn unfinished_writer_cannot_be_returned_as_complete() {
        let guard = fixture();
        guard.shared.capture.start(1).unwrap();
        let pending = guard.shared.capture.begin().unwrap();
        let weak = Arc::downgrade(&pending.session);
        let value = guard.shared.capture.stop(1, &guard.shared).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["in_flight"], 1);
        assert!(guard.shared.capture.start(2).is_err());
        drop(pending);
        assert!(weak.upgrade().is_some()); // Audio did not free the pool.
        guard.shared.capture.start(2).unwrap();
        assert!(weak.upgrade().is_none()); // RPC reclaimed it.
    }

    #[test]
    fn exact_candidate_forwarded_and_conversion_fixture() {
        let mut guard = fixture();
        guard.shared.capture.start(1).unwrap();
        let mut input = chunk();
        input.waveforms[0][17] = 1.0;
        guard.process(&mut input);
        let stamp = input.protection.take().unwrap();
        let begin = monotonic_ns();
        let mut pcm = vec![0; PCM_BYTES];
        let result = crate::utils::conversions::chunk_to_buffer_rawbytes(
            input,
            &mut pcm,
            &crate::config::BinarySampleFormat::S24_3_LE,
        );
        let end = monotonic_ns();
        guard
            .shared
            .capture
            .converted(&stamp, result.1, &pcm, begin, end);
        let value = guard.shared.capture.stop(1, &guard.shared).unwrap();
        assert_eq!(value["complete"], true);
        assert_eq!(value["records"].as_array().unwrap().len(), 2);
        let record = &value["records"][0];
        assert_eq!(record["candidate_bits"][17], "3ff0000000000000");
        assert!(
            record["forwarded_bits"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v == "0000000000000000")
        );
        assert_eq!(record["trip"]["frame"], 17);
        assert_eq!(record["activation_inhibited"], true);
        assert_eq!(value["records"][1]["clipped"], 0);
        if let Some(path) = std::env::var_os("P4_CAPTURE_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
    }
    #[test]
    fn overflow_and_contention_never_wait_or_become_complete() {
        let mut guard = fixture();
        guard.shared.capture.start(1).unwrap();
        for _ in 0..CAPACITY + 1 {
            guard.process(&mut chunk());
        }
        let value = guard.shared.capture.stop(1, &guard.shared).unwrap();
        assert_eq!(value["lost"], 1);
        assert_eq!(value["complete"], false);
        assert!(guard.shared.capture.start(1).is_err());
        guard.shared.capture.start(2).unwrap();
        {
            let _lock = guard.shared.capture.current.lock().unwrap();
            assert!(guard.shared.capture.begin().is_none());
        }
        assert_eq!(
            guard.shared.capture.stop(2, &guard.shared).unwrap()["complete"],
            false
        );
    }
    #[test]
    fn incomplete_shape_and_nonfinite_bits_are_retained() {
        let mut guard = fixture();
        guard.shared.capture.start(1).unwrap();
        let mut input = chunk();
        input.waveforms[0][0] = f64::NAN;
        input.waveforms.pop();
        guard.process(&mut input);
        let value = guard.shared.capture.stop(1, &guard.shared).unwrap();
        assert_eq!(value["records"][0]["shape_valid"], false);
        assert_eq!(value["records"][0]["candidate_bits"][0], "7ff8000000000000");
        assert_eq!(value["records"][0]["trip"]["reason"], 2);
    }
}
