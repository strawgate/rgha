//! Local containers through the docker CLI (also works with podman/nerdctl
//! that accept the same flags). Isolation depends on the OCI runtime:
//! `runsc` (gVisor) or Kata give a separate kernel; plain runc does not.

use async_trait::async_trait;
use tokio::process::Command;

use super::{Backend, Instance, JIT_ENV, Network, RUNNER_ENTRYPOINT, RunnerSpec};

const LABEL_CLASS: &str = "dev.rgha.class";
const LABEL_RUNNER: &str = "dev.rgha.runner";

pub struct DockerBackend {
    bin: String,
    image: String,
    runtime: Option<String>,
}

impl DockerBackend {
    pub fn new(bin: &str, image: &str, runtime: Option<String>) -> Self {
        Self { bin: bin.to_string(), image: image.to_string(), runtime }
    }

    /// `docker run` argv. The JIT config is passed by name only (`-e NAME`),
    /// so its value is inherited from our env and never appears in argv/`ps`.
    pub(crate) fn run_args(&self, spec: &RunnerSpec) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--name".into(),
            spec.name.clone(),
            "--label".into(),
            format!("{LABEL_CLASS}={}", spec.class),
            "--label".into(),
            format!("{LABEL_RUNNER}={}", spec.name),
            "--cpus".into(),
            format!("{}", spec.cpu_limit),
            "--memory".into(),
            format!("{}m", spec.memory_mib),
            // Keep Docker's default capability set: many workflows rely on
            // `sudo`. Isolation comes from the OCI runtime (gVisor/Kata).
            "--pids-limit".into(),
            "4096".into(),
            "-e".into(),
            JIT_ENV.into(),
        ];
        if let Some(rt) = &self.runtime {
            args.push("--runtime".into());
            args.push(rt.clone());
        }
        debug_assert!(matches!(spec.network, Network::Open), "validated in config");
        args.push(self.image.clone());
        args.push(RUNNER_ENTRYPOINT.into());
        args
    }

    async fn run(&self, args: &[String], env: Option<(&str, &str)>) -> anyhow::Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).kill_on_drop(true);
        if let Some((k, v)) = env {
            cmd.env(k, v);
        }
        let out = cmd.output().await?;
        if !out.status.success() {
            anyhow::bail!(
                "{} {} failed: {}",
                self.bin,
                args.first().map(String::as_str).unwrap_or(""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

#[async_trait]
impl Backend for DockerBackend {
    fn kind(&self) -> &'static str {
        "docker"
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        self.run(&["pull".into(), "-q".into(), self.image.clone()], None).await.map(|_| ())
    }

    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String> {
        // `docker run --timeout` doesn't exist; the controller enforces max
        // lifetime by stopping the container after `spec.timeout`.
        self.run(&self.run_args(spec), Some((JIT_ENV, &spec.jit_config))).await
    }

    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        match self.run(&["rm".into(), "-f".into(), id.to_string()], None).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("No such container") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        match self.run(&["wait".into(), id.to_string()], None).await {
            Ok(code) => Ok(code.parse().ok()),
            // --rm removed it before we started waiting: it already exited.
            Err(e) if e.to_string().contains("No such container") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        let out = self
            .run(
                &[
                    "ps".into(),
                    "--filter".into(),
                    format!("label={LABEL_CLASS}={class}"),
                    "--format".into(),
                    format!("{{{{.ID}}}}\t{{{{.Label \"{LABEL_RUNNER}\"}}}}"),
                ],
                None,
            )
            .await?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(id, name)| Instance { id: id.to_string(), runner_name: name.to_string() })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn run_args_never_contain_jit_value() {
        let b = DockerBackend::new("docker", "img:1", Some("runsc".into()));
        let spec = RunnerSpec {
            name: "c-1".into(),
            class: "c".into(),
            jit_config: "TOPSECRET".into(),
            cpu: 0.5,
            cpu_limit: 0.5,
            memory_mib: 768,
            timeout: Duration::from_secs(60),
            network: Network::Open,
        };
        let args = b.run_args(&spec);
        assert!(!args.iter().any(|a| a.contains("TOPSECRET")));
        assert!(args.windows(2).any(|w| w[0] == "--runtime" && w[1] == "runsc"));
        assert!(args.windows(2).any(|w| w[0] == "--memory" && w[1] == "768m"));
        assert_eq!(args.last().map(String::as_str), Some(RUNNER_ENTRYPOINT));
    }
}
