//! Pure runner-pool bookkeeping: no I/O, so the scaling rules are unit- and
//! property-testable. The scaler drives it and performs the side effects.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Started and registered; waiting for GitHub to assign a job.
    Idle,
    /// Running a job.
    Busy,
}

#[derive(Debug, Clone)]
pub struct Runner {
    pub name: String,
    pub runner_id: i64,
    pub instance_id: String,
    pub state: State,
    pub created: Instant,
    pub idle_since: Instant,
    pub job_started: Option<Instant>,
    /// Set when the instance exited while busy, before GitHub's JobCompleted
    /// arrived; the runner stays in the pool until that message (or a grace
    /// period) so the job is recorded with its real result.
    pub exited_at: Option<Instant>,
}

/// What happened to a runner that left the pool, for cost accounting.
#[derive(Debug, Clone)]
pub struct Departed {
    pub runner: Runner,
    pub lifetime: Duration,
    pub job: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct Pool {
    runners: HashMap<String, Runner>,
    pub min_idle: u32,
    pub max_runners: u32,
}

impl Pool {
    pub fn new(min_idle: u32, max_runners: u32) -> Self {
        Self { runners: HashMap::new(), min_idle, max_runners }
    }

    pub fn len(&self) -> usize {
        self.runners.len()
    }

    pub fn get(&self, name: &str) -> Option<&Runner> {
        self.runners.get(name)
    }

    pub fn runners(&self) -> impl Iterator<Item = &Runner> {
        self.runners.values()
    }

    /// Runners we want: one per assigned job plus the warm buffer, capped.
    pub fn desired(&self, assigned_jobs: i64) -> usize {
        let assigned = assigned_jobs.max(0) as u64;
        (assigned + self.min_idle as u64).min(self.max_runners as u64) as usize
    }

    /// How many new runners to start now.
    pub fn deficit(&self, assigned_jobs: i64) -> usize {
        self.desired(assigned_jobs).saturating_sub(self.len())
    }

    pub fn insert(&mut self, name: String, runner_id: i64, instance_id: String, now: Instant) {
        self.runners.insert(
            name.clone(),
            Runner {
                name,
                runner_id,
                instance_id,
                state: State::Idle,
                created: now,
                idle_since: now,
                job_started: None,
                exited_at: None,
            },
        );
    }

    /// Records that a busy runner's instance has exited. Returns false if
    /// the runner is unknown or idle (the caller should remove it instead).
    pub fn mark_exited(&mut self, name: &str, now: Instant) -> bool {
        match self.runners.get_mut(name) {
            Some(r) if r.state == State::Busy => {
                r.exited_at.get_or_insert(now);
                true
            }
            _ => false,
        }
    }

    /// Exited runners whose JobCompleted hasn't arrived within `grace`.
    pub fn stale_exited(&self, grace: Duration, now: Instant) -> Vec<String> {
        self.runners
            .values()
            .filter(|r| r.exited_at.is_some_and(|t| now.saturating_duration_since(t) >= grace))
            .map(|r| r.name.clone())
            .collect()
    }

    /// Marks a runner busy. Returns false for runners we don't own.
    pub fn job_started(&mut self, name: &str, now: Instant) -> bool {
        match self.runners.get_mut(name) {
            Some(r) => {
                r.state = State::Busy;
                r.job_started = Some(now);
                true
            }
            None => false,
        }
    }

    /// Removes a runner (job completed, instance exited, or reaped).
    pub fn remove(&mut self, name: &str, now: Instant) -> Option<Departed> {
        let runner = self.runners.remove(name)?;
        // If the instance already exited, that is when billing stopped.
        let end = runner.exited_at.unwrap_or(now);
        Some(Departed {
            lifetime: end.saturating_duration_since(runner.created),
            job: runner.job_started.map(|s| end.saturating_duration_since(s)),
            runner,
        })
    }

    /// Idle runners beyond `desired` that have waited at least `ttl`, oldest
    /// first. Busy runners are never candidates.
    pub fn reap_candidates(&self, assigned_jobs: i64, ttl: Duration, now: Instant) -> Vec<String> {
        let surplus = self.len().saturating_sub(self.desired(assigned_jobs));
        if surplus == 0 {
            return vec![];
        }
        let mut idle: Vec<&Runner> = self
            .runners
            .values()
            .filter(|r| r.state == State::Idle && now.saturating_duration_since(r.idle_since) >= ttl)
            .collect();
        idle.sort_by_key(|r| r.idle_since);
        idle.into_iter().take(surplus).map(|r| r.name.clone()).collect()
    }
}

/// Adaptive warm-pool size: grows slowly while jobs keep starting cold,
/// shrinks slowly once they stop. Additive in both directions, so one big
/// burst nudges the pool up by one runner rather than by the burst size.
#[derive(Debug, Clone)]
pub struct AdaptiveWarm {
    pub floor: u32,
    pub max: u32,
    pub grow_every: Duration,
    pub shrink_every: Duration,
    target: u32,
    last_grow: Option<Instant>,
    /// Last cold start or shrink; shrinking waits `shrink_every` after it.
    quiet_since: Instant,
}

impl AdaptiveWarm {
    pub fn new(floor: u32, max: u32, grow_every: Duration, shrink_every: Duration, now: Instant) -> Self {
        Self { floor, max: max.max(floor), grow_every, shrink_every, target: floor, last_grow: None, quiet_since: now }
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    /// Moves the floor (e.g. a warm_schedule window opening or closing).
    /// Raising it lifts the target at once; lowering it lets the target
    /// shrink back gradually.
    pub fn set_floor(&mut self, floor: u32) {
        self.floor = floor;
        self.max = self.max.max(floor);
        self.target = self.target.max(floor);
    }

    /// Updates the target given whether jobs are waiting without a runner.
    pub fn observe(&mut self, cold_starts: bool, now: Instant) -> u32 {
        if cold_starts {
            self.quiet_since = now;
            let can_grow = self.last_grow.is_none_or(|t| now.saturating_duration_since(t) >= self.grow_every);
            if self.target < self.max && can_grow {
                self.target += 1;
                self.last_grow = Some(now);
            }
        } else if self.target > self.floor && now.saturating_duration_since(self.quiet_since) >= self.shrink_every {
            self.target -= 1;
            self.quiet_since = now;
        }
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn scale_from_zero_and_back() {
        let t0 = Instant::now();
        let mut p = Pool::new(0, 5);
        assert_eq!(p.deficit(0), 0);
        assert_eq!(p.deficit(2), 2);
        p.insert("a".into(), 1, "i-a".into(), t0);
        p.insert("b".into(), 2, "i-b".into(), t0);
        assert_eq!(p.deficit(2), 0);
        assert!(p.job_started("a", t0 + Duration::from_secs(3)));
        let d = p.remove("a", t0 + Duration::from_secs(23)).unwrap();
        assert_eq!(d.job, Some(Duration::from_secs(20)));
        assert_eq!(d.lifetime, Duration::from_secs(23));
        // Job for "b" got cancelled: b is surplus but not yet past ttl.
        let ttl = Duration::from_secs(60);
        assert!(p.reap_candidates(0, ttl, t0 + Duration::from_secs(30)).is_empty());
        assert_eq!(p.reap_candidates(0, ttl, t0 + Duration::from_secs(61)), vec!["b".to_string()]);
    }

    #[test]
    fn warm_pool_and_cap() {
        let p = Pool::new(2, 3);
        assert_eq!(p.desired(0), 2);
        assert_eq!(p.desired(10), 3);
        assert_eq!(p.desired(-5), 2);
    }

    #[test]
    fn exit_before_job_completed_waits_for_the_message() {
        let t0 = Instant::now();
        let mut p = Pool::new(0, 5);
        p.insert("idle".into(), 1, "i".into(), t0);
        p.insert("busy".into(), 2, "j".into(), t0);
        p.job_started("busy", t0);
        assert!(!p.mark_exited("idle", t0), "idle runners are removed directly");
        assert!(p.mark_exited("busy", t0 + Duration::from_secs(10)));
        assert!(p.stale_exited(Duration::from_secs(120), t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(p.stale_exited(Duration::from_secs(120), t0 + Duration::from_secs(130)), vec!["busy".to_string()]);
        // JobCompleted removes it later; durations end at the exit.
        let d = p.remove("busy", t0 + Duration::from_secs(20)).unwrap();
        assert_eq!(d.job, Some(Duration::from_secs(10)));
        assert_eq!(d.lifetime, Duration::from_secs(10));
    }

    #[test]
    fn busy_runners_are_never_reaped() {
        let t0 = Instant::now();
        let mut p = Pool::new(0, 5);
        p.insert("a".into(), 1, "i".into(), t0);
        p.job_started("a", t0);
        assert!(p.reap_candidates(0, Duration::ZERO, t0 + Duration::from_secs(999)).is_empty());
    }

    #[test]
    fn adaptive_warm_grows_and_shrinks_one_at_a_time() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut w = AdaptiveWarm::new(0, 3, s(60), s(300), t0);
        assert_eq!(w.observe(true, t0), 1, "first cold start: +1");
        assert_eq!(w.observe(true, t0 + s(10)), 1, "rate-limited");
        assert_eq!(w.observe(true, t0 + s(60)), 2);
        assert_eq!(w.observe(true, t0 + s(120)), 3);
        assert_eq!(w.observe(true, t0 + s(180)), 3, "capped at max");
        assert_eq!(w.observe(false, t0 + s(400)), 3, "not quiet long enough");
        assert_eq!(w.observe(false, t0 + s(480)), 2, "quiet for shrink_every: -1");
        assert_eq!(w.observe(false, t0 + s(500)), 2);
        assert_eq!(w.observe(false, t0 + s(780)), 1);
        assert_eq!(w.observe(false, t0 + s(1080)), 0);
        assert_eq!(w.observe(false, t0 + s(9999)), 0, "floor");
    }

    proptest! {
        /// Target stays within [floor, max] and moves by at most 1 per observation.
        #[test]
        fn adaptive_warm_bounds(floor in 0u32..3, extra in 0u32..5, events in proptest::collection::vec((any::<bool>(), 0u64..400), 0..60)) {
            let t0 = Instant::now();
            let mut w = AdaptiveWarm::new(floor, floor + extra, Duration::from_secs(60), Duration::from_secs(300), t0);
            let mut now = t0;
            let mut prev = w.target();
            for (cold, dt) in events {
                now += Duration::from_secs(dt);
                let t = w.observe(cold, now);
                prop_assert!(t >= floor && t <= floor + extra);
                prop_assert!(t.abs_diff(prev) <= 1);
                if cold { prop_assert!(t >= prev, "never shrinks during cold starts"); }
                prev = t;
            }
        }

        /// Never plan more runners than max, and never reap below desired.
        #[test]
        fn invariants(min_idle in 0u32..4, max in 0u32..8, assigned in -2i64..20,
                      n_idle in 0usize..8, n_busy in 0usize..8, age in 0u64..300) {
            let t0 = Instant::now();
            let mut p = Pool::new(min_idle.min(max), max);
            for i in 0..n_idle { p.insert(format!("i{i}"), i as i64, String::new(), t0); }
            for i in 0..n_busy { let n = format!("b{i}"); p.insert(n.clone(), 100 + i as i64, String::new(), t0); p.job_started(&n, t0); }
            prop_assert!(p.len() + p.deficit(assigned) <= (max as usize).max(p.len()));
            let reaped = p.reap_candidates(assigned, Duration::from_secs(60), t0 + Duration::from_secs(age));
            prop_assert!(p.len() - reaped.len() >= p.desired(assigned).min(p.len()));
            for name in &reaped { prop_assert_eq!(p.get(name).unwrap().state, State::Idle); }
        }
    }
}
