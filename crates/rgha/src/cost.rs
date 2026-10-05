//! Per-job cost accounting: what the sandbox actually cost (per second) vs
//! what the same job would have cost on a per-minute GitHub-hosted runner.

use serde::Deserialize;

/// Per-second compute prices for a backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// USD per CPU unit per second, in the backend's CPU unit (Modal: core, metered as CPU-seconds used).
    pub cpu_per_sec: f64,
    /// USD per GiB of memory per second.
    pub gib_per_sec: f64,
    /// Smallest billable duration in seconds (EC2: 60, Modal: 1).
    #[serde(default)]
    pub min_billed_secs: f64,
}

impl Pricing {
    /// Modal Sandbox list prices (modal.com/pricing, checked 2026-10).
    pub const MODAL_SANDBOX: Pricing =
        Pricing { cpu_per_sec: 0.000_039_42, gib_per_sec: 0.000_006_67, min_billed_secs: 0.0 };
    /// Daytona list prices per vCPU-second and GiB-second (daytona.io/pricing,
    /// checked 2026-10). Daytona allocates whole vCPUs and GiB.
    pub const DAYTONA: Pricing = Pricing { cpu_per_sec: 0.000_014, gib_per_sec: 0.000_004_5, min_billed_secs: 0.0 };

    /// Cost of metered core-seconds and GiB-seconds.
    pub fn metered(&self, cpu_core_secs: f64, mem_gib_secs: f64) -> f64 {
        cpu_core_secs * self.cpu_per_sec + mem_gib_secs * self.gib_per_sec
    }

    pub fn cost(&self, cpu: f64, memory_mib: u32, secs: f64) -> f64 {
        let billed = secs.max(self.min_billed_secs).max(0.0);
        billed * (cpu * self.cpu_per_sec + (memory_mib as f64 / 1024.0) * self.gib_per_sec)
    }
}

/// GitHub-hosted runner pricing: every job is rounded up to a whole minute.
/// Default is Linux x64 2-core, $0.006/min (docs.github.com, 2026 pricing).
pub const GITHUB_LINUX_2CORE_PER_MIN: f64 = 0.006;

pub fn github_hosted_cost(job_secs: f64, per_minute: f64) -> f64 {
    let minutes = (job_secs.max(0.0) / 60.0).ceil().max(1.0);
    minutes * per_minute
}

/// Running totals for one runner class.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ledger {
    pub jobs: u64,
    pub sandbox_secs: f64,
    pub job_secs: f64,
    pub sandbox_usd: f64,
    pub github_usd: f64,
}

impl Ledger {
    pub fn record(&mut self, sandbox_secs: f64, job_secs: Option<f64>, sandbox_usd: f64, github_per_min: f64) {
        self.sandbox_secs += sandbox_secs;
        self.sandbox_usd += sandbox_usd;
        if let Some(j) = job_secs {
            self.jobs += 1;
            self.job_secs += j;
            self.github_usd += github_hosted_cost(j, github_per_min);
        }
    }

    pub fn savings_ratio(&self) -> Option<f64> {
        (self.sandbox_usd > 0.0).then(|| self.github_usd / self.sandbox_usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn short_low_cpu_job_is_far_cheaper_on_modal() {
        // 20s job + ~10s boot/registration overhead at 0.25 core / 512 MiB.
        let modal = Pricing::MODAL_SANDBOX.cost(0.25, 512, 30.0);
        let gh = github_hosted_cost(20.0, GITHUB_LINUX_2CORE_PER_MIN);
        assert!((gh - 0.006).abs() < 1e-12);
        assert!(modal < 0.0005, "modal={modal}");
        assert!(gh / modal > 10.0);
    }

    #[test]
    fn github_rounds_up_to_minutes() {
        assert_eq!(github_hosted_cost(0.0, 1.0), 1.0);
        assert_eq!(github_hosted_cost(60.0, 1.0), 1.0);
        assert_eq!(github_hosted_cost(60.5, 1.0), 2.0);
    }

    #[test]
    fn min_billed_applies() {
        let p = Pricing { cpu_per_sec: 1.0, gib_per_sec: 0.0, min_billed_secs: 60.0 };
        assert_eq!(p.cost(1.0, 0, 5.0), 60.0);
    }

    proptest! {
        #[test]
        fn cost_is_monotonic_in_time(a in 0.0f64..10_000.0, b in 0.0f64..10_000.0, cpu in 0.125f64..8.0, mem in 128u32..32_768) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(Pricing::MODAL_SANDBOX.cost(cpu, mem, lo) <= Pricing::MODAL_SANDBOX.cost(cpu, mem, hi));
        }

        #[test]
        fn github_never_cheaper_than_exact_minutes(secs in 0.0f64..100_000.0) {
            prop_assert!(github_hosted_cost(secs, 1.0) >= secs / 60.0);
        }
    }
}
