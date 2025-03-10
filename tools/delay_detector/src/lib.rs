use cpu_time::ProcessTime;
use std::borrow::Cow;
use std::time::{Duration, Instant};
use tracing::{info, warn};

struct Snapshot {
    real_time: Duration,
    cpu_time: Duration,
}

struct SnapshotInstant {
    real_time: Instant,
    cpu_time: ProcessTime,
}

pub struct DelayDetector<'a> {
    min_delay: Duration,
    msg: Cow<'a, str>,
    started: Instant,
    started_cpu_time: ProcessTime,
    snapshots: Vec<((String, String), Snapshot)>,
    last_snapshot: Option<(String, SnapshotInstant)>,
}

impl<'a> DelayDetector<'a> {
    pub fn new(msg: Cow<'a, str>) -> Self {
        Self {
            msg,
            started: Instant::now(),
            started_cpu_time: ProcessTime::now(),
            snapshots: vec![],
            last_snapshot: None,
            min_delay: Duration::from_millis(50),
        }
    }

    pub fn min_delay(mut self, min_delay: Duration) -> Self {
        self.min_delay = min_delay;
        self
    }

    pub fn snapshot(&mut self, msg: &str) {
        let now = Instant::now();
        let cpu_time = ProcessTime::now();
        if let Some((s, started)) = self.last_snapshot.take() {
            self.snapshots.push((
                (s, msg.to_string()),
                Snapshot {
                    real_time: now - started.real_time,
                    cpu_time: started.cpu_time.elapsed(),
                },
            ));
        }
        self.last_snapshot = Some((msg.to_string(), SnapshotInstant { real_time: now, cpu_time }));
    }
}

impl<'a> Drop for DelayDetector<'a> {
    fn drop(&mut self) {
        let elapsed = self.started_cpu_time.elapsed();
        let elapsed_real = self.started.elapsed();
        let long_delay = self.min_delay * 10;
        if self.min_delay < elapsed && elapsed <= long_delay {
            info!(target: "delay_detector", "Took {:?} cpu_time, {:?} real_time processing {}", elapsed, elapsed_real, self.msg);
        }
        if elapsed > long_delay {
            warn!(target: "delay_detector", "LONG DELAY! Took {:?} cpu_time, {:?} real_time processing {}", elapsed, elapsed_real, self.msg);
            if self.last_snapshot.is_some() {
                self.snapshot("end");
            }
            self.snapshots.sort_by(|a, b| b.1.cpu_time.cmp(&a.1.cpu_time));
            for ((s1, s2), Snapshot { cpu_time, real_time }) in self.snapshots.drain(..) {
                info!(target: "delay_detector", "Took {:?} cpu_time, {:?} real_time between {} and {}", cpu_time, real_time, s1, s2);
            }
        }
    }
}
